use crate::wal::WalFileReader;
use anyhow::{anyhow, Result};
use arc_swap::ArcSwap;
use std::ops::Range;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::Sender;
use uuid::Uuid;

#[derive(Debug)]
pub(crate) struct WalCopier {
    wal: Option<WalFileReader>,
    outbox: Sender<String>,
    use_compression: bool,
    max_frames_per_batch: usize,
    wal_path: String,
    bucket: String,
    db_name: Arc<str>,
    generation: Arc<ArcSwap<Uuid>>,
}

impl WalCopier {
    pub fn new(
        bucket: String,
        db_name: Arc<str>,
        generation: Arc<ArcSwap<Uuid>>,
        db_path: &str,
        max_frames_per_batch: usize,
        use_compression: bool,
        outbox: Sender<String>,
    ) -> Self {
        WalCopier {
            wal: None,
            bucket,
            db_name,
            generation,
            wal_path: format!("{}-wal", db_path),
            outbox,
            max_frames_per_batch,
            use_compression,
        }
    }

    pub async fn flush(&mut self, frames: Range<u32>) -> Result<u32> {
        tracing::trace!("flushing frames [{}..{})", frames.start, frames.end);
        if frames.is_empty() {
            tracing::trace!("Trying to flush empty frame range");
            return Ok(frames.start - 1);
        }
        let wal = {
            if self.wal.is_none() {
                self.wal = WalFileReader::open(&self.wal_path).await?;
            }
            if let Some(wal) = self.wal.as_mut() {
                wal
            } else {
                return Err(anyhow!("WAL file not found: \"{:?}\"", self.wal_path));
            }
        };
        let generation = self.generation.load_full();
        let dir = format!("{}/{}-{}", self.bucket, self.db_name, generation);
        if frames.start == 1 {
            // before writing the first batch of frames - init directory
            // and store .meta object with basic info
            tracing::trace!("initializing local backup directory: {:?}", dir);
            tokio::fs::create_dir_all(&dir).await?;
            let meta_path = format!("{}/.meta", dir);
            let mut meta_file = tokio::fs::File::create(&meta_path).await?;
            let buf = {
                let page_size = wal.page_size();
                let crc = wal.checksum();
                let mut buf = [0u8; 12];
                buf[0..4].copy_from_slice(page_size.to_be_bytes().as_slice());
                buf[4..].copy_from_slice(crc.to_be_bytes().as_slice());
                buf
            };
            meta_file.write_all(buf.as_ref()).await?;
            meta_file.flush().await?;
            let msg = format!("{}-{}/.meta", self.db_name, generation);
            if self.outbox.send(msg).await.is_err() {
                return Err(anyhow!("couldn't initialize local backup dir: {}", dir));
            }
        }
        tracing::trace!("Flushing {} frames locally.", frames.len());

        for start in frames.clone().step_by(self.max_frames_per_batch) {
            let end = (start + self.max_frames_per_batch as u32).min(frames.end);
            let len = (end - start) as usize;
            let fdesc = format!(
                "{}-{}/{:012}-{:012}",
                self.db_name,
                generation,
                start,
                end - 1
            );
            let mut out = tokio::fs::File::create(&format!("{}/{}", self.bucket, fdesc)).await?;

            wal.seek_frame(start).await?;
            if self.use_compression {
                let mut gzip = async_compression::tokio::write::GzipEncoder::new(&mut out);
                wal.copy_frames(&mut gzip, len).await?;
                gzip.shutdown().await?;
            } else {
                wal.copy_frames(&mut out, len).await?;
                out.shutdown().await?;
            }
            if tracing::enabled!(tracing::Level::DEBUG) {
                let file_len = out.metadata().await?.len();
                tracing::debug!(
                    "written frames {:012}-{:012} into local file using {} bytes",
                    start,
                    end - 1,
                    file_len
                );
            }
            drop(out);
            if self.outbox.send(fdesc).await.is_err() {
                tracing::warn!(
                    "WAL local cloning ended prematurely. Last cloned frame no.: {}",
                    end - 1
                );
                return Ok(end - 1);
            }
        }
        Ok(frames.end - 1)
    }
}
