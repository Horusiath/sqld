use crate::read::BatchReader;
use crate::wal::WalFileReader;
use crate::write::BatchWriter;
use anyhow::anyhow;
use arc_swap::ArcSwap;
use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::operation::get_object::builders::GetObjectFluentBuilder;
use aws_sdk_s3::operation::list_objects::builders::ListObjectsFluentBuilder;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use bytes::{Bytes, BytesMut};
use std::collections::BTreeMap;
use std::io::SeekFrom;
use std::ops::{Deref, Range};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncSeekExt;
use tokio::sync::watch::{channel, Receiver, Sender};
use tokio::time::{timeout_at, Instant};
use uuid::Uuid;

pub type Result<T> = anyhow::Result<T>;

#[derive(Debug)]
pub struct Replicator {
    pub client: Client,

    /// Frame number, incremented whenever a new frame is written from SQLite.
    next_frame_no: Arc<AtomicU32>,
    /// Last frame which has been requested to be sent to S3.
    /// Always: [last_sent_frame_no] <= [next_frame_no].
    last_sent_frame_no: Arc<AtomicU32>,
    /// Last frame which has been confirmed as sent to S3.
    /// Always: [last_committed_frame_no] <= [last_sent_frame_no].
    last_committed_frame_no: Receiver<Result<u32>>,
    flush_trigger: Sender<()>,

    pub page_size: usize,
    generation: Arc<ArcSwap<uuid::Uuid>>,
    pub commits_in_current_generation: Arc<AtomicU32>,
    verify_crc: bool,
    pub bucket: String,
    pub db_path: String,
    pub db_name: String,

    use_compression: bool,
    max_frames_per_batch: usize,
}

#[derive(Debug)]
pub struct FetchedResults {
    pub pages: Vec<(i32, Bytes)>,
    pub next_marker: Option<String>,
}

#[derive(Debug)]
pub enum RestoreAction {
    None,
    SnapshotMainDbFile,
    ReuseGeneration(uuid::Uuid),
}

#[derive(Clone, Debug)]
pub struct Options {
    pub create_bucket_if_not_exists: bool,
    pub verify_crc: bool,
    pub use_compression: bool,
    pub aws_endpoint: Option<String>,
    pub db_id: Option<String>,
    pub bucket_name: String,
    pub max_frames_per_batch: usize,
    pub max_batch_interval: Duration,
}

impl Default for Options {
    fn default() -> Self {
        let aws_endpoint = std::env::var("LIBSQL_BOTTOMLESS_ENDPOINT").ok();
        let bucket_name =
            std::env::var("LIBSQL_BOTTOMLESS_BUCKET").unwrap_or_else(|_| "bottomless".to_string());
        let db_id = std::env::var("LIBSQL_BOTTOMLESS_DATABASE_ID").ok();
        Options {
            create_bucket_if_not_exists: false,
            verify_crc: true,
            use_compression: false,
            max_batch_interval: Duration::from_secs(15),
            max_frames_per_batch: 64,
            db_id,
            aws_endpoint,
            bucket_name,
        }
    }
}

impl Replicator {
    pub const UNSET_PAGE_SIZE: usize = usize::MAX;

    pub async fn new<S: Into<String>>(db_path: S) -> Result<Self> {
        Self::create(db_path, Options::default()).await
    }

    pub async fn create<S: Into<String>>(db_path: S, options: Options) -> Result<Self> {
        let mut loader = aws_config::from_env();
        if let Some(endpoint) = options.aws_endpoint.as_deref() {
            loader = loader.endpoint_url(endpoint);
        }
        let bucket = options.bucket_name.clone();
        let conf = aws_sdk_s3::config::Builder::from(&loader.load().await)
            .force_path_style(true)
            .build();
        let client = Client::from_conf(conf);
        let generation = Arc::new(ArcSwap::new(Arc::new(Self::generate_generation())));
        tracing::debug!("Generation {}", generation.load());

        match client.head_bucket().bucket(&bucket).send().await {
            Ok(_) => tracing::info!("Bucket {} exists and is accessible", bucket),
            Err(SdkError::ServiceError(err)) if err.err().is_not_found() => {
                if options.create_bucket_if_not_exists {
                    tracing::info!("Bucket {} not found, recreating", bucket);
                    client.create_bucket().bucket(&bucket).send().await?;
                } else {
                    tracing::error!("Bucket {} does not exist", bucket);
                    return Err(SdkError::ServiceError(err).into());
                }
            }
            Err(e) => {
                tracing::error!("Bucket checking error: {}", e);
                return Err(e.into());
            }
        }

        let db_path = db_path.into();
        let db_name = {
            let db_id = options.db_id.unwrap_or_default();
            let name = match db_path.rfind('/') {
                Some(index) => &db_path[index + 1..],
                None => &db_path,
            };
            db_id + name
        };

        let (flush_trigger, mut flush_trigger_rx) = channel(());
        let (last_committed_frame_no_sender, last_committed_frame_no) = channel(Ok(0));

        let next_frame_no = Arc::new(AtomicU32::new(1));
        let last_sent_frame_no = Arc::new(AtomicU32::new(0));
        let commits_in_current_generation = Arc::new(AtomicU32::new(0));

        let _backup_job = {
            let mut flush_manager = FlushManager::new(
                client.clone(),
                generation.clone(),
                commits_in_current_generation.clone(),
                format!("{}-wal", &db_path),
                bucket.clone(),
                db_name.clone(),
                options.max_frames_per_batch,
                options.use_compression,
            );
            let next_frame_no = next_frame_no.clone();
            let last_sent_frame_no = last_sent_frame_no.clone();
            let batch_interval = options.max_batch_interval;
            tokio::spawn(async move {
                loop {
                    let timeout = Instant::now() + batch_interval;
                    let trigger = match timeout_at(timeout, flush_trigger_rx.changed()).await {
                        Ok(Ok(())) => true,
                        Ok(Err(_)) => {
                            return;
                        }
                        Err(_) => true, // timeout reached
                    };
                    if trigger {
                        let next_frame = next_frame_no.load(Ordering::Acquire);
                        let last_sent_frame =
                            last_sent_frame_no.swap(next_frame - 1, Ordering::Acquire);
                        let frames = (last_sent_frame + 1)..next_frame;
                        let res = flush_manager.flush(frames).await;
                        if let Err(_) = last_committed_frame_no_sender.send(res) {
                            return;
                        }
                    }
                }
            })
        };

        Ok(Self {
            client,
            bucket,
            page_size: Self::UNSET_PAGE_SIZE,
            generation,
            commits_in_current_generation,
            next_frame_no,
            last_sent_frame_no,
            flush_trigger,
            last_committed_frame_no,
            verify_crc: options.verify_crc,
            db_path,
            db_name,
            use_compression: options.use_compression,
            max_frames_per_batch: options.max_frames_per_batch,
        })
    }

    pub fn next_frame_no(&self) -> u32 {
        self.next_frame_no.load(Ordering::Acquire)
    }

    pub fn last_sent_frame_no(&self) -> u32 {
        self.last_sent_frame_no.load(Ordering::Acquire)
    }

    /// Waits until the commit for a given frame_no or higher was given.
    pub async fn wait_until_committed(&mut self, frame_no: u32) -> Result<u32> {
        loop {
            {
                let last_committed = self.last_committed_frame_no.borrow();
                match last_committed.deref() {
                    Ok(last_committed) if *last_committed >= frame_no => {
                        return Ok(*last_committed)
                    }
                    Ok(_) => {}
                    Err(e) => return Err(anyhow!("Failed to flush frames: {}", e)),
                }
            }
            self.last_committed_frame_no.changed().await?;
        }
    }

    pub fn commits_in_current_generation(&self) -> u32 {
        self.commits_in_current_generation.load(Ordering::Acquire)
    }

    /// Returns number of frames waiting to be replicated.
    pub fn pending_frames(&self) -> u32 {
        self.next_frame_no() - self.last_sent_frame_no() - 1
    }

    // The database can use different page size - as soon as it's known,
    // it should be communicated to the replicator via this call.
    // NOTICE: in practice, WAL journaling mode does not allow changing page sizes,
    // so verifying that it hasn't changed is a panic check. Perhaps in the future
    // it will be useful, if WAL ever allows changing the page size.
    pub fn set_page_size(&mut self, page_size: usize) -> Result<()> {
        tracing::trace!("Setting page size from {} to {}", self.page_size, page_size);
        if self.page_size != Self::UNSET_PAGE_SIZE && self.page_size != page_size {
            return Err(anyhow::anyhow!(
                "Cannot set page size to {}, it was already set to {}",
                page_size,
                self.page_size
            ));
        }
        self.page_size = page_size;
        Ok(())
    }

    // Gets an object from the current bucket
    fn get_object(&self, key: String) -> GetObjectFluentBuilder {
        self.client.get_object().bucket(&self.bucket).key(key)
    }

    // Lists objects from the current bucket
    fn list_objects(&self) -> ListObjectsFluentBuilder {
        self.client.list_objects().bucket(&self.bucket)
    }

    fn reset_frames(&mut self, frame_no: u32) {
        let last_sent = self.last_sent_frame_no();
        self.next_frame_no.store(frame_no + 1, Ordering::Release);
        self.last_sent_frame_no
            .store(last_sent.min(frame_no), Ordering::Release);
    }

    // Generates a new generation UUID v7, which contains a timestamp and is binary-sortable.
    // This timestamp goes back in time - that allows us to list newest generations
    // first in the S3-compatible bucket, under the assumption that fetching newest generations
    // is the most common operation.
    // NOTICE: at the time of writing, uuid v7 is an unstable feature of the uuid crate
    fn generate_generation() -> uuid::Uuid {
        let (seconds, nanos) = uuid::timestamp::Timestamp::now(uuid::NoContext).to_unix();
        let (seconds, nanos) = (253370761200 - seconds, 999999999 - nanos);
        let synthetic_ts = uuid::Timestamp::from_unix(uuid::NoContext, seconds, nanos);
        uuid::Uuid::new_v7(synthetic_ts)
    }

    // Starts a new generation for this replicator instance
    pub fn new_generation(&mut self) {
        tracing::debug!("New generation started: {}", self.generation);
        self.set_generation(Self::generate_generation());
    }

    // Sets a generation for this replicator instance. This function
    // should be called if a generation number from S3-compatible storage
    // is reused in this session.
    pub fn set_generation(&mut self, generation: uuid::Uuid) {
        self.generation.swap(Arc::new(generation));
        self.commits_in_current_generation
            .store(0, Ordering::Release);
        self.reset_frames(0);
        tracing::debug!("Generation set to {}", self.generation);
    }

    // Returns the current last valid frame in the replicated log
    pub fn peek_last_valid_frame(&self) -> u32 {
        self.next_frame_no().saturating_sub(1)
    }

    // Sets the last valid frame in the replicated log.
    pub fn register_last_valid_frame(&mut self, frame: u32) {
        let last_valid_frame = self.peek_last_valid_frame();
        if frame != last_valid_frame {
            if last_valid_frame != 0 {
                tracing::error!(
                    "[BUG] Local max valid frame is {}, while replicator thinks it's {}",
                    frame,
                    last_valid_frame
                );
            }
            self.reset_frames(frame);
        }
    }

    /// Submit next `frame_count` of frames to be replicated.
    pub fn submit_frames(&mut self, frame_count: u32) {
        let prev = self.next_frame_no.fetch_add(frame_count, Ordering::SeqCst);
        let last_sent = self.last_sent_frame_no();
        let last_known = prev + frame_count - 1;
        if last_known - last_sent >= self.max_frames_per_batch as u32 {
            tracing::trace!("Triggering flush for frames {}..{}", last_sent, last_known);
            self.request_flush();
        }
    }

    pub fn request_flush(&self) {
        let _ = self.flush_trigger.send(());
    }

    // Marks all recently flushed pages as committed and updates the frame number
    // holding the newest consistent committed transaction.
    pub async fn finalize_commit(&mut self, last_frame: u32, checksum: u64) -> Result<()> {
        // Last consistent frame is persisted in S3 in order to be able to recover
        // from failured that happen in the middle of a commit, when only some
        // of the pages that belong to a transaction are replicated.
        let last_consistent_frame_key = format!("{}-{}/.consistent", self.db_name, self.generation);
        tracing::trace!("Finalizing frame: {}, checksum: {}", last_frame, checksum);
        // Information kept in this entry: [last consistent frame number: 4 bytes][last checksum: 8 bytes]
        let mut consistent_info = BytesMut::with_capacity(16);
        consistent_info.extend_from_slice(&(self.page_size as u32).to_be_bytes());
        consistent_info.extend_from_slice(&last_frame.to_be_bytes());
        consistent_info.extend_from_slice(&checksum.to_be_bytes());
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(last_consistent_frame_key)
            .body(ByteStream::from(Bytes::from(consistent_info)))
            .send()
            .await?;
        tracing::trace!("Commit successful");
        Ok(())
    }

    // Drops uncommitted frames newer than given last valid frame
    pub fn rollback_to_frame(&mut self, last_valid_frame: u32) {
        // NOTICE: O(size), can be optimized to O(removed) if ever needed
        self.reset_frames(last_valid_frame);
        tracing::debug!("Rolled back to {}", last_valid_frame);
    }

    // Tries to read the local change counter from the given database file
    async fn read_change_counter(reader: &mut tokio::fs::File) -> Result<[u8; 4]> {
        use tokio::io::AsyncReadExt;
        let mut counter = [0u8; 4];
        reader.seek(std::io::SeekFrom::Start(24)).await?;
        reader.read_exact(&mut counter).await?;
        Ok(counter)
    }

    // Tries to read the local page size from the given database file
    async fn read_page_size(reader: &mut tokio::fs::File) -> Result<usize> {
        use tokio::io::AsyncReadExt;
        reader.seek(SeekFrom::Start(16)).await?;
        let page_size = reader.read_u16().await?;
        if page_size == 1 {
            Ok(65536)
        } else {
            Ok(page_size as usize)
        }
    }

    // Returns the compressed database file path and its change counter, extracted
    // from the header of page1 at offset 24..27 (as per SQLite documentation).
    pub async fn compress_main_db_file(&self) -> Result<(&'static str, [u8; 4])> {
        use tokio::io::AsyncWriteExt;
        let compressed_db = "db.gz";
        let mut reader = tokio::fs::File::open(&self.db_path).await?;
        let mut writer = async_compression::tokio::write::GzipEncoder::new(
            tokio::fs::File::create(compressed_db).await?,
        );
        tokio::io::copy(&mut reader, &mut writer).await?;
        writer.shutdown().await?;
        let change_counter = Self::read_change_counter(&mut reader).await?;
        Ok((compressed_db, change_counter))
    }

    // Replicates local WAL pages to S3, if local WAL is present.
    // This function is called under the assumption that if local WAL
    // file is present, it was already detected to be newer than its
    // remote counterpart.
    pub async fn maybe_replicate_wal(&mut self) -> Result<()> {
        let mut wal_file = match WalFileReader::open(&format!("{}-wal", &self.db_path)).await {
            Ok(Some(file)) => file,
            _ => {
                tracing::info!("Local WAL not present - not replicating");
                return Ok(());
            }
        };

        let frame_count = wal_file.frame_count().await;
        tracing::trace!("Local WAL pages: {}", frame_count);
        let checksum = wal_file.checksum();
        tracing::trace!("Local WAL checksum: {}", checksum);
        let mut last_written_frame = 0;
        for i in 0..frame_count {
            wal_file.seek_frame(i).await?;
            let header = wal_file.read_frame_header().await?;
            self.submit_frames(1);
            if header.is_committed() {
                self.request_flush();
                last_written_frame = self.wait_until_committed(i).await?;
            }
        }
        if last_written_frame > 0 {
            self.finalize_commit(last_written_frame, checksum).await?;
        }
        let pending_frames = self.pending_frames();
        if pending_frames != 0 {
            tracing::warn!("Uncommited WAL entries: {}", pending_frames);
        }
        tracing::info!("Local WAL replicated");
        Ok(())
    }

    // Check if the local database file exists and contains data
    async fn main_db_exists_and_not_empty(&self) -> bool {
        let file = match tokio::fs::File::open(&self.db_path).await {
            Ok(file) => file,
            Err(_) => return false,
        };
        match file.metadata().await {
            Ok(metadata) => metadata.len() > 0,
            Err(_) => false,
        }
    }

    // Sends the main database file to S3 - if -wal file is present, it's replicated
    // too - it means that the local file was detected to be newer than its remote
    // counterpart.
    pub async fn snapshot_main_db_file(&mut self) -> Result<()> {
        if !self.main_db_exists_and_not_empty().await {
            tracing::debug!("Not snapshotting, the main db file does not exist or is empty");
            return Ok(());
        }
        tracing::debug!("Snapshotting {}", self.db_path);

        let change_counter = if self.use_compression {
            // TODO: find a way to compress ByteStream on the fly instead of creating
            // an intermediary file.
            let (compressed_db_path, change_counter) = self.compress_main_db_file().await?;
            let key = format!("{}-{}/db.gz", self.db_name, self.generation);
            self.client
                .put_object()
                .bucket(&self.bucket)
                .key(key)
                .body(ByteStream::from_path(compressed_db_path).await?)
                .send()
                .await?;
            change_counter
        } else {
            self.client
                .put_object()
                .bucket(&self.bucket)
                .key(format!("{}-{}/db.db", self.db_name, self.generation))
                .body(ByteStream::from_path(&self.db_path).await?)
                .send()
                .await?;
            let mut reader = tokio::fs::File::open(&self.db_path).await?;
            Self::read_change_counter(&mut reader).await?
        };

        /* FIXME: we can't rely on the change counter in WAL mode:
         ** "In WAL mode, changes to the database are detected using the wal-index and
         ** so the change counter is not needed. Hence, the change counter might not be
         ** incremented on each transaction in WAL mode."
         ** Instead, we need to consult WAL checksums.
         */
        let change_counter_key = format!("{}-{}/.changecounter", self.db_name, self.generation);
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(change_counter_key)
            .body(ByteStream::from(Bytes::copy_from_slice(&change_counter)))
            .send()
            .await?;
        tracing::debug!("Main db snapshot complete");
        Ok(())
    }

    // Returns newest replicated generation, or None, if one is not found.
    // FIXME: assumes that this bucket stores *only* generations for databases,
    // it should be more robust and continue looking if the first item does not
    // match the <db-name>-<generation-uuid>/ pattern.
    pub async fn find_newest_generation(&self) -> Option<uuid::Uuid> {
        let prefix = format!("{}-", self.db_name);
        let response = self
            .list_objects()
            .prefix(prefix)
            .max_keys(1)
            .send()
            .await
            .ok()?;
        let objs = response.contents()?;
        let key = objs.first()?.key()?;
        let key = match key.find('/') {
            Some(index) => &key[self.db_name.len() + 1..index],
            None => key,
        };
        tracing::debug!("Generation candidate: {}", key);
        uuid::Uuid::parse_str(key).ok()
    }

    // Tries to fetch the remote database change counter from given generation
    pub async fn get_remote_change_counter(&self, generation: &uuid::Uuid) -> Result<[u8; 4]> {
        use bytes::Buf;
        let mut remote_change_counter = [0u8; 4];
        if let Ok(response) = self
            .get_object(format!("{}-{}/.changecounter", self.db_name, generation))
            .send()
            .await
        {
            response
                .body
                .collect()
                .await?
                .copy_to_slice(&mut remote_change_counter)
        }
        Ok(remote_change_counter)
    }

    // Tries to fetch the last consistent frame number stored in the remote generation
    pub async fn get_last_consistent_frame(
        &self,
        generation: &uuid::Uuid,
    ) -> Result<(u32, u32, u64)> {
        use bytes::Buf;
        Ok(
            match self
                .get_object(format!("{}-{}/.consistent", self.db_name, generation))
                .send()
                .await
                .ok()
            {
                Some(response) => {
                    let mut collected = response.body.collect().await?;
                    (
                        collected.get_u32(),
                        collected.get_u32(),
                        collected.get_u64(),
                    )
                }
                None => (0, 0, 0),
            },
        )
    }

    // Returns the number of pages stored in the local WAL file, or 0, if there aren't any.
    async fn get_local_wal_page_count(&mut self) -> u32 {
        match WalFileReader::open(&format!("{}-wal", &self.db_path)).await {
            Ok(None) => 0,
            Ok(Some(wal)) => {
                let page_size = wal.page_size();
                if self.set_page_size(page_size as usize).is_err() {
                    return 0;
                }
                wal.frame_count().await
            }
            Err(_) => 0,
        }
    }

    // Parses the frame and page number from given key.
    // Format: <db-name>-<generation>/<frame-number>
    fn parse_frame_page_crc(key: &str) -> Option<u32> {
        let frame_delim = key.rfind('/')?;
        let frameno = key[(frame_delim + 1)..].parse::<u32>().ok()?;
        Some(frameno)
    }

    // Restores the database state from given remote generation
    pub async fn restore_from(&mut self, generation: Uuid) -> Result<RestoreAction> {
        use tokio::io::AsyncWriteExt;

        // Check if the database needs to be restored by inspecting the database
        // change counter and the WAL size.
        let local_counter = match tokio::fs::File::open(&self.db_path).await {
            Ok(mut db) => {
                // While reading the main database file for the first time,
                // page size from an existing database should be set.
                if let Ok(page_size) = Self::read_page_size(&mut db).await {
                    self.set_page_size(page_size)?;
                }
                Self::read_change_counter(&mut db).await.unwrap_or([0u8; 4])
            }
            Err(_) => [0u8; 4],
        };

        let remote_counter = self.get_remote_change_counter(&generation).await?;
        tracing::debug!("Counters: l={:?}, r={:?}", local_counter, remote_counter);

        let (page_size, last_consistent_frame, checksum) =
            self.get_last_consistent_frame(&generation).await?;
        tracing::debug!(
            "Last consistent remote frame: {}; checksum: {:x}, page_size: {}",
            last_consistent_frame,
            checksum,
            page_size,
        );
        if page_size != 0 {
            self.set_page_size(page_size as usize)?;
        }

        let wal_pages = self.get_local_wal_page_count().await;
        match local_counter.cmp(&remote_counter) {
            std::cmp::Ordering::Equal => {
                tracing::debug!(
                    "Consistent: {}; wal pages: {}",
                    last_consistent_frame,
                    wal_pages
                );
                match wal_pages.cmp(&last_consistent_frame) {
                    std::cmp::Ordering::Equal => {
                        tracing::info!(
                            "Remote generation is up-to-date, reusing it in this session"
                        );
                        self.reset_frames(wal_pages + 1);
                        return Ok(RestoreAction::ReuseGeneration(generation));
                    }
                    std::cmp::Ordering::Greater => {
                        tracing::info!("Local change counter matches the remote one, but local WAL contains newer data, which needs to be replicated");
                        return Ok(RestoreAction::SnapshotMainDbFile);
                    }
                    std::cmp::Ordering::Less => (),
                }
            }
            std::cmp::Ordering::Greater => {
                tracing::info!("Local change counter is larger than its remote counterpart - a new snapshot needs to be replicated");
                return Ok(RestoreAction::SnapshotMainDbFile);
            }
            std::cmp::Ordering::Less => (),
        }

        tokio::fs::rename(&self.db_path, format!("{}.bottomless.backup", self.db_path))
            .await
            .ok(); // Best effort
        let mut main_db_writer = tokio::fs::File::create(&self.db_path).await?;
        // If the db file is not present, the database could have been empty

        let main_db_path = if self.use_compression {
            format!("{}-{}/db.gz", self.db_name, generation)
        } else {
            format!("{}-{}/db.db", self.db_name, generation)
        };

        if let Ok(db_file) = self.get_object(main_db_path).send().await {
            let mut body_reader = db_file.body.into_async_read();
            if self.use_compression {
                let mut decompress_reader = async_compression::tokio::bufread::GzipDecoder::new(
                    tokio::io::BufReader::new(body_reader),
                );
                tokio::io::copy(&mut decompress_reader, &mut main_db_writer).await?;
            } else {
                tokio::io::copy(&mut body_reader, &mut main_db_writer).await?;
            }
            main_db_writer.flush().await?;
        }
        tracing::info!("Restored the main database file");

        let mut next_marker = None;
        let prefix = format!("{}-{}/", self.db_name, generation);
        tracing::debug!("Overwriting any existing WAL file: {}-wal", &self.db_path);
        tokio::fs::remove_file(&format!("{}-wal", &self.db_path))
            .await
            .ok();
        tokio::fs::remove_file(&format!("{}-shm", &self.db_path))
            .await
            .ok();

        let mut applied_wal_frame = false;
        loop {
            let mut list_request = self.list_objects().prefix(&prefix);
            if let Some(marker) = next_marker {
                list_request = list_request.marker(marker);
            }
            let response = list_request.send().await?;
            let objs = match response.contents() {
                Some(objs) => objs,
                None => {
                    tracing::debug!("No objects found in generation {}", generation);
                    break;
                }
            };
            let mut prev_crc = 0;
            let mut pending_pages = BTreeMap::new();
            for obj in objs {
                let key = obj
                    .key()
                    .ok_or_else(|| anyhow::anyhow!("Failed to get key for an object"))?;
                tracing::debug!("Loading {}", key);
                let frame = self.get_object(key.into()).send().await?;

                let mut frameno = match Self::parse_frame_page_crc(key) {
                    Some(result) => result,
                    None => {
                        if !key.ends_with(".gz")
                            && !key.ends_with(".db")
                            && !key.ends_with(".consistent")
                            && !key.ends_with(".changecounter")
                        {
                            tracing::warn!("Failed to parse frame/page from key {}", key);
                        }
                        continue;
                    }
                };
                if frameno > last_consistent_frame {
                    tracing::warn!("Remote log contains frame {} larger than last consistent frame ({}), stopping the restoration process",
                                frameno, last_consistent_frame);
                    break;
                }
                let crc = if self.verify_crc {
                    Some(prev_crc)
                } else {
                    None
                };
                let page_size = self.page_size;
                let mut reader =
                    BatchReader::new(frameno, frame.body, page_size, self.use_compression, crc);
                while let Some(frame) = reader.next_frame_header().await? {
                    tracing::debug!(
                        "Restoring next frame {} as main db page {}",
                        frameno,
                        frame.pgno
                    );
                    let buf = pending_pages.entry(frame.pgno).or_insert_with(|| {
                        let mut v = Vec::with_capacity(page_size);
                        unsafe { v.set_len(page_size) };
                        v
                    });
                    reader.next_page(buf.as_mut()).await?;
                    if frame.is_committed() {
                        let pending_pages = std::mem::replace(&mut pending_pages, BTreeMap::new());
                        let page_count = pending_pages.len();
                        for (pgno, data) in pending_pages {
                            let offset = (pgno - 1) as u64 * (page_size as u64);
                            main_db_writer.seek(SeekFrom::Start(offset)).await?;
                            main_db_writer.write_all(&data).await?;
                            // should we flush here? In theory if we don't recover fully we scrap
                            // anyway
                        }
                        tracing::debug!("Restored {} pages into main DB file.", page_count);
                    }
                    prev_crc = frame.crc;
                    frameno += 1;
                }
                main_db_writer.flush().await?;
                applied_wal_frame = true;
            }
            next_marker = response
                .is_truncated()
                .then(|| objs.last().map(|elem| elem.key().unwrap().to_string()))
                .flatten();
            if next_marker.is_none() {
                tracing::trace!("Restored DB from S3 backup using generation {}", generation);
                break;
            }
        }

        if applied_wal_frame {
            Ok::<_, anyhow::Error>(RestoreAction::SnapshotMainDbFile)
        } else {
            Ok::<_, anyhow::Error>(RestoreAction::None)
        }
    }

    // Restores the database state from newest remote generation
    pub async fn restore(&mut self) -> Result<RestoreAction> {
        let newest_generation = match self.find_newest_generation().await {
            Some(gen) => gen,
            None => {
                tracing::debug!("No generation found, nothing to restore");
                return Ok(RestoreAction::SnapshotMainDbFile);
            }
        };

        tracing::info!("Restoring from generation {}", newest_generation);
        self.restore_from(newest_generation).await
    }
}

pub struct Context {
    pub replicator: Replicator,
    pub runtime: tokio::runtime::Runtime,
}

struct FlushManager {
    wal: Option<WalFileReader>,
    client: Client,
    use_compression: bool,
    max_frames_per_batch: usize,
    wal_path: String,
    bucket: String,
    db_name: String,
    generation: Arc<ArcSwap<Uuid>>,
    commits_in_current_generation: Arc<AtomicU32>,
}

impl FlushManager {
    fn new(
        client: Client,
        generation: Arc<ArcSwap<Uuid>>,
        commits_in_current_generation: Arc<AtomicU32>,
        wal_path: String,
        bucket: String,
        db_name: String,
        max_frames_per_batch: usize,
        use_compression: bool,
    ) -> Self {
        FlushManager {
            wal: None,
            client,
            use_compression,
            max_frames_per_batch,
            wal_path,
            bucket,
            db_name,
            generation,
            commits_in_current_generation,
        }
    }

    async fn flush(&mut self, frames: Range<u32>) -> Result<u32> {
        if frames.is_empty() {
            tracing::trace!("Trying to flush empty frame range");
            return Ok(frames.start - 1);
        }
        let wal_file = {
            if self.wal.is_none() {
                self.wal = WalFileReader::open(&self.wal_path).await?;
            }
            if let Some(wal) = self.wal.as_mut() {
                wal
            } else {
                return Err(anyhow!("WAL file not found: {}", self.wal_path));
            }
        };
        tracing::trace!("Flushing {} frames", frames.len());
        self.commits_in_current_generation
            .fetch_add(1, Ordering::SeqCst);
        //wal_file.checksum_verification().await?;
        for start in frames.clone().step_by(self.max_frames_per_batch) {
            let end = (start + self.max_frames_per_batch as u32).min(frames.end);
            let mut writer = BatchWriter::new(self.use_compression, start..end);
            if let Some(body) = writer.read_frames(wal_file).await? {
                let generation = self.generation.load();
                let key = format!("{}-{}/{:012}", self.db_name, &generation, start);
                self.client
                    .put_object()
                    .bucket(&self.bucket)
                    .key(key)
                    .body(body.into())
                    .send()
                    .await?;
                tracing::trace!("Frame range [{}..{}) has been sent to S3", start, end);
            }
        }
        Ok(frames.end - 1)
    }
}
