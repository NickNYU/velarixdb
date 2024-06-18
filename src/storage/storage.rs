use crate::bucket::{Bucket, BucketID, BucketMap};
use crate::cfg::Config;
use crate::compactors::{self, Compactor};
use crate::consts::{
    BUCKETS_DIRECTORY_NAME, DEFAULT_COMPACTION_FLUSH_LISTNER_INTERVAL_MILLI, DEFAULT_FLUSH_DATA_CHANNEL_SIZE,
    DEFAULT_FLUSH_SIGNAL_CHANNEL_SIZE, HEAD_ENTRY_KEY, META_DIRECTORY_NAME, SIZE_OF_U32, SIZE_OF_U64, SIZE_OF_U8,
    TAIL_ENTRY_KEY, TOMB_STONE_MARKER, VALUE_LOG_DIRECTORY_NAME, WRITE_BUFFER_SIZE,
};
use crate::err::Error;
use crate::err::Error::*;
use crate::filter::BloomFilter;
use crate::flusher::{FlushDataMemTable, Flusher};
use crate::fs::{DataFileNode, DataFs, FileNode, IndexFileNode, IndexFs};
use crate::index::{Index, IndexFile};
use crate::key_range::KeyRange;
use crate::memtable::{Entry, InMemoryTable};
use crate::meta::Meta;
use crate::range::RangeIterator;
use crate::sst::{DataFile, Table};
use crate::types::{
    self, BloomFilterHandle, BucketMapHandle, FlushSignal, ImmutableMemTable, Key, KeyRangeHandle, ValOffset,
};
use crate::value_log::ValueLog;
use async_broadcast::broadcast;
use chrono::Utc;
use crossbeam_skiplist::SkipMap;
use indexmap::IndexMap;
use log::error;
use std::time::Duration;
use std::{borrow::Borrow, path::PathBuf};
use std::{hash::Hash, sync::Arc};
use tokio::fs::{self, read_dir, OpenOptions};
use tokio::io::{self, AsyncSeekExt, AsyncWriteExt, SeekFrom};
use tokio::time::sleep;
use tokio::{
    spawn,
    sync::{
        mpsc::{self, Receiver, Sender},
        RwLock,
    },
};

#[derive(Debug)]
pub struct DataStore<'a, K>
where
    K: Hash + Ord + Send + Sync + Clone,
{
    pub dir: DirPath,
    pub active_memtable: InMemoryTable<K>,
    pub filters: BloomFilterHandle,
    pub val_log: ValueLog,
    pub buckets: BucketMapHandle,
    pub key_range: KeyRangeHandle,
    pub compactor: Compactor,
    pub meta: Meta,
    pub flusher: Flusher,
    pub config: Config,
    pub range_iterator: Option<RangeIterator<'a>>,
    pub read_only_memtables: ImmutableMemTable<K>,
    pub flush_data_sender: ChanSender,
    pub flush_data_recevier: ChanRecv,
    pub flush_signal_sender: ChanSender,
    pub flush_signal_receiver: ChanRecv,
    pub tombstone_compaction_sender: ChanSender,
    pub tombstone_compaction_rcv: ChanRecv,
}

// TODO: REVIEW LOCK MECHANISM FOR BUCKET MAP

#[derive(Debug, Clone)]
pub enum ChanSender {
    FlushDataSender(Arc<RwLock<tokio::sync::mpsc::Sender<FlushDataMemTable>>>),
    TombStoneCompactionNoticeSender(tokio::sync::mpsc::Sender<BucketMap>),
    FlushNotificationSender(async_broadcast::Sender<FlushSignal>),
}

#[derive(Debug)]
pub enum ChanRecv {
    FlushDataRecv(Arc<RwLock<tokio::sync::mpsc::Receiver<FlushDataMemTable>>>),
    TombStoneCompactionNoticeRcv(Arc<RwLock<tokio::sync::mpsc::Receiver<BucketMap>>>),
    FlushNotificationRecv(async_broadcast::Receiver<FlushSignal>),
}

#[derive(Clone, Debug)]
pub struct DirPath {
    pub root: PathBuf,
    pub val_log: PathBuf,
    pub buckets: PathBuf,
    pub meta: PathBuf,
}

#[derive(Clone, Copy, Debug)]
pub enum SizeUnit {
    Bytes,
    Kilobytes,
    Megabytes,
    Gigabytes,
}

impl<'a> DataStore<'a, Key> {
    pub async fn new(dir: PathBuf) -> Result<DataStore<'a, Key>, Error> {
        let dir = DirPath::build(dir);
        let default_config = Config::default();

        let store = DataStore::with_default_capacity_and_config(
            dir.clone(),
            SizeUnit::Bytes,
            WRITE_BUFFER_SIZE,
            default_config,
        )
        .await?;
        let started = store.trigger_background_tasks()?;
        println!("Background jobs started: {}", started);
        return Ok(store);
    }

    pub async fn new_with_custom_config(dir: PathBuf, config: Config) -> Result<DataStore<'a, Key>, Error> {
        let dir = DirPath::build(dir);
        DataStore::with_default_capacity_and_config(dir.clone(), SizeUnit::Bytes, WRITE_BUFFER_SIZE, config).await
    }

    pub fn trigger_background_tasks(&self) -> Result<bool, Error> {
        // Start background job to check for tombstone compaction condition at regular intervals 20 days
        if let ChanRecv::TombStoneCompactionNoticeRcv(rcx) = &self.tombstone_compaction_rcv {
            self.compactor
                .tombstone_compaction_condition_background_checker(Arc::clone(rcx));
        }

        // Start background job to check for compaction condition at regular intervals 20 days
        self.compactor.start_periodic_background_compaction(
            Arc::clone(&self.buckets),
            Arc::clone(&self.filters),
            Arc::clone(&self.key_range),
        );

        if let ChanRecv::FlushNotificationRecv(rcx) = &self.flush_signal_receiver {
            self.compactor.start_flush_listner(
                rcx.clone(),
                Arc::clone(&self.buckets),
                Arc::clone(&self.filters),
                Arc::clone(&self.key_range),
            );
        }

        return Ok(true);
    }

    /// A Result indicating success or an `Error` if an error occurred.
    pub async fn put(&mut self, key: &str, value: &str, existing_v_offset: Option<ValOffset>) -> Result<bool, Error> {
        // Convert the key and value into Vec<u8> from given &str.
        let key = &key.as_bytes().to_vec();
        let value = &value.as_bytes().to_vec();
        let created_at = Utc::now().timestamp_millis() as u64;
        let is_tombstone = false;
        let v_offset;
        if let Some(v_off) = existing_v_offset {
            v_offset = v_off;
        } else {
            v_offset = self.val_log.append(key, value, created_at, is_tombstone).await?;
        }

        if self.active_memtable.is_full(HEAD_ENTRY_KEY.len()) {
            let capacity = self.active_memtable.capacity();
            let size_unit = self.active_memtable.size_unit();
            let false_positive_rate = self.active_memtable.false_positive_rate();
            let head_offset = self.active_memtable.entries.iter().max_by_key(|e| e.value().0);

            // reset head in vLog
            self.val_log.set_head(head_offset.to_owned().unwrap().value().0);
            let head_entry = Entry::new(
                HEAD_ENTRY_KEY.to_vec(),
                head_offset.unwrap().value().0,
                Utc::now().timestamp_millis() as u64,
                false,
            );

            let _ = self.active_memtable.insert(&head_entry);
            self.active_memtable.read_only = true;
            self.read_only_memtables.write().await.insert(
                InMemoryTable::generate_table_id(),
                Arc::new(RwLock::new(self.active_memtable.to_owned())),
            );

            if self.read_only_memtables.read().await.len() >= self.config.max_buffer_write_number {
                let rd_table = self.read_only_memtables.read().await;
                for (table_id, table_to_flush) in rd_table.iter() {
                    let table = Arc::clone(table_to_flush);
                    let table_id_clone = table_id.clone();
                    //let flush_data_sender_clone = self.flush_data_sender.clone();
                    let mut flusher = self.flusher.clone();

                    let flush_signal_clone = self.flush_signal_sender.clone();
                    // Prevent write block
                    spawn(async move {
                        if let ChanSender::FlushNotificationSender(signal_sender) = flush_signal_clone {
                            flusher.flush_handler(table_id_clone.to_owned(), table.to_owned(), signal_sender.clone());
                        }
                    });
                }
            }

            self.active_memtable =
                InMemoryTable::with_specified_capacity_and_rate(size_unit, capacity, false_positive_rate);
        }
        let entry = Entry::new(key.to_vec(), v_offset, created_at, is_tombstone);
        self.active_memtable.insert(&entry)?;
        Ok(true)
    }

    // A Result indicating success or an `io::Error` if an error occurred.
    pub async fn get(&self, key: &str) -> Result<(Vec<u8>, u64), Error> {
        let key = key.as_bytes().to_vec();
        let mut offset = 0;
        let mut most_recent_insert_time = 0;

        //Step 1 > Check the active memtable
        if let Ok(Some((value_offset, creation_date, is_tombstone))) = self.active_memtable.get(&key) {
            offset = value_offset;
            most_recent_insert_time = creation_date;
            if is_tombstone {
                return Err(KeyFoundAsTombstoneInMemtableError);
            }
        } else {
            //Step 2 > Check the read only memtable
            let mut is_deleted = false;
            for (_, m_table) in self.read_only_memtables.read().await.iter() {
                if let Ok(Some((value_offset, creation_date, is_tombstone))) = m_table.read().await.get(&key) {
                    if creation_date > most_recent_insert_time {
                        offset = value_offset;
                        most_recent_insert_time = creation_date;
                        is_deleted = is_tombstone;
                    }
                }
            }
            if most_recent_insert_time > 0 && is_deleted {
                return Err(KeyFoundAsTombstoneInMemtableError);
            } else if most_recent_insert_time == 0 {
                //Step 3 > Check the sstables
                let key_range_r_lock = &self.key_range.read().await;
                let sstables_within_key_range = key_range_r_lock.filter_sstables_by_biggest_key(&key);
                if sstables_within_key_range.is_empty() {
                    return Err(KeyNotFoundInAnySSTableError);
                }
                let bloom_filter_read_lock = &self.filters.read().await;
                let filters_within_key_range = BloomFilter::bloom_filters_within_key_range(
                    bloom_filter_read_lock,
                    sstables_within_key_range.to_vec(),
                );
                if filters_within_key_range.is_empty() {
                    return Err(KeyNotFoundByAnyBloomFilterError);
                }

                let sstable_paths = BloomFilter::sstables_within_key_range(filters_within_key_range, &key);
                match sstable_paths {
                    Some(sstables_within_key_range) => {
                        for sstable in sstables_within_key_range.iter() {
                            let sparse_index =
                                Index::new(sstable.index_file.path.clone(), sstable.index_file.file.clone());
                            let block_offset_res = sparse_index.get(&key).await;
                            match block_offset_res {
                                Ok(None) => continue,
                                Ok(result) => {
                                    if let Some(block_offset) = result {
                                        let sst_res = sstable.get(block_offset, &key).await;
                                        match sst_res {
                                            Ok(None) => continue,
                                            Ok(result) => {
                                                if let Some((value_offset, created_at, is_tombstone)) = result {
                                                    if created_at > most_recent_insert_time {
                                                        offset = value_offset;
                                                        most_recent_insert_time = created_at;
                                                        is_deleted = is_tombstone;
                                                    }
                                                }
                                            }
                                            Err(err) => error!("{}", err),
                                        }
                                    }
                                }
                                Err(err) => error!("{}", err),
                            }
                        }
                        if most_recent_insert_time > 0 && is_deleted {
                            return Err(KeyFoundAsTombstoneInSSTableError);
                        }
                    }
                    None => {
                        return Err(KeyNotFoundInAnySSTableError);
                    }
                }
            }
        }
        // most_recent_insert_time cannot be zero unless did not find this key in any sstable
        if most_recent_insert_time > 0 {
            // Step 5: Read value from value log based on offset
            let value = self.val_log.get(offset).await?;
            match value {
                Some((v, is_tombstone)) => {
                    if is_tombstone {
                        return Err(KeyFoundAsTombstoneInValueLogError);
                    }
                    return Ok((v, most_recent_insert_time));
                }
                None => return Err(KeyNotFoundInValueLogError),
            };
        }

        Err(NotFoundInDB)
    }
    pub async fn delete(&mut self, key: &str) -> Result<bool, Error> {
        // Return error if not
        self.get(key).await?;

        // Convert the key and value into Vec<u8> from given &str.
        let key = &key.as_bytes().to_vec();
        let value = &TOMB_STONE_MARKER.to_le_bytes().to_vec();
        let created_at = Utc::now().timestamp_millis() as u64;
        let is_tombstone = true;

        let v_offset = self.val_log.append(key, value, created_at, is_tombstone).await?;

        // then check if memtable is full
        if self.active_memtable.is_full(HEAD_ENTRY_KEY.len()) {
            let capacity = self.active_memtable.capacity();
            let size_unit = self.active_memtable.size_unit();
            let false_positive_rate = self.active_memtable.false_positive_rate();
            let head_offset = self.active_memtable.entries.iter().max_by_key(|e| e.value().0);
            let head_entry = Entry::new(
                HEAD_ENTRY_KEY.to_vec(),
                head_offset.unwrap().value().0,
                Utc::now().timestamp_millis() as u64,
                is_tombstone,
            );
            let _ = self.active_memtable.insert(&head_entry);
            self.active_memtable.read_only = true;
            self.read_only_memtables.write().await.insert(
                InMemoryTable::generate_table_id(),
                Arc::new(RwLock::new(self.active_memtable.to_owned())),
            );

            if self.read_only_memtables.read().await.len() >= self.config.max_buffer_write_number {
                let rd_table = self.read_only_memtables.read().await;
                for (table_id, table_to_flush) in rd_table.iter() {
                    let table = Arc::clone(table_to_flush);
                    let table_id_clone = table_id.clone();
                    let flush_data_sender_clone = self.flush_data_sender.clone();
                    // Prevent write block
                    spawn(async move {
                        if let ChanSender::FlushDataSender(sender) = flush_data_sender_clone {
                            if let Err(err) = sender.write().await.send((table_id_clone, table)).await {
                                println!("Could not send flush data to channel {:?}", err);
                            }
                        }
                    });
                }
            }
            self.active_memtable =
                InMemoryTable::with_specified_capacity_and_rate(size_unit, capacity, false_positive_rate);
        }

        let entry = Entry::new(key.to_vec(), v_offset.try_into().unwrap(), created_at, is_tombstone);
        self.active_memtable.insert(&entry)?;
        Ok(true)
    }

    pub async fn update(&mut self, key: &str, value: &str) -> Result<bool, Error> {
        // Call set method defined in DataStore.
        self.put(key, value, None).await
    }

    pub async fn clear(&'a mut self) -> Result<DataStore<'a, types::Key>, Error> {
        let capacity = self.active_memtable.capacity();

        let size_unit = self.active_memtable.size_unit();

        self.active_memtable.clear();

        self.buckets.write().await.clear_all().await;

        self.val_log.clear_all().await;

        DataStore::with_capacity_and_rate(self.dir.clone(), size_unit, capacity, self.config.to_owned()).await
    }

    async fn with_default_capacity_and_config(
        dir: DirPath,
        size_unit: SizeUnit,
        capacity: usize,
        config: Config,
    ) -> Result<DataStore<'a, types::Key>, Error> {
        Self::with_capacity_and_rate(dir, size_unit, capacity, config).await
    }

    async fn with_capacity_and_rate(
        dir: DirPath,
        size_unit: SizeUnit,
        capacity: usize,
        config: Config,
    ) -> Result<DataStore<'a, types::Key>, Error> {
        let vlog_path = &dir.clone().val_log;
        let buckets_path = dir.buckets.clone();
        let vlog_exit = vlog_path.exists();
        let vlog_empty = !vlog_exit || fs::metadata(vlog_path).await.map_err(GetFileMetaDataError)?.len() == 0;
        let key_range = KeyRange::new();
        let mut vlog = ValueLog::new(vlog_path).await?;
        let meta = Meta::new(&dir.meta);
        if vlog_empty {
            let mut active_memtable =
                InMemoryTable::with_specified_capacity_and_rate(size_unit, capacity, config.false_positive_rate);

            // if ValueLog is empty then we want to insert both tail and head
            let created_at = Utc::now().timestamp_millis() as u64;

            let tail_offset = vlog
                .append(&TAIL_ENTRY_KEY.to_vec(), &vec![], created_at, false)
                .await?;
            let tail_entry = Entry::new(TAIL_ENTRY_KEY.to_vec(), tail_offset, created_at, false);

            let head_offset = vlog
                .append(&HEAD_ENTRY_KEY.to_vec(), &vec![], created_at, false)
                .await?;
            let head_entry = Entry::new(HEAD_ENTRY_KEY.to_vec(), head_offset, created_at, false);

            vlog.set_head(head_offset);
            vlog.set_tail(tail_offset);

            // insert tail and head to memtable
            active_memtable.insert(&tail_entry.to_owned())?;
            active_memtable.insert(&head_entry.to_owned())?;
            let buckets = BucketMap::new(buckets_path);
            let (flush_data_sender, flush_data_rec) = mpsc::channel(DEFAULT_FLUSH_DATA_CHANNEL_SIZE);
            let (flush_signal_sender, flush_signal_rec) = broadcast(DEFAULT_FLUSH_SIGNAL_CHANNEL_SIZE);
            let (comp_sender, comp_rec) = mpsc::channel(1);
            let read_only_memtables = IndexMap::new();

            let filters_ref: Arc<RwLock<Vec<BloomFilter>>> = Arc::new(RwLock::new(Vec::new()));
            let buckets_ref = Arc::new(RwLock::new(buckets.to_owned()));
            let key_range_ref = Arc::new(RwLock::new(key_range));
            let read_only_memtables_ref = Arc::new(RwLock::new(read_only_memtables));

            let flusher = Flusher::new(
                read_only_memtables_ref.clone(),
                buckets_ref.clone(),
                filters_ref.clone(),
                key_range_ref.clone(),
                config.enable_ttl,
                config.entry_ttl_millis,
            );

            return Ok(DataStore {
                active_memtable,
                val_log: vlog,
                filters: filters_ref,
                buckets: buckets_ref,
                dir,
                key_range: key_range_ref,
                compactor: Compactor::new(
                    config.enable_ttl,
                    config.entry_ttl_millis,
                    config.tombstone_ttl,
                    config.background_compaction_interval,
                    config.compactor_flush_listener_interval,
                    config.tombstone_compaction_interval,
                    config.compaction_strategy,
                    compactors::CompactionReason::MaxSize,
                ),
                config: config.clone(),
                meta,
                flusher,
                read_only_memtables: read_only_memtables_ref,
                tombstone_compaction_sender: ChanSender::TombStoneCompactionNoticeSender(comp_sender),
                tombstone_compaction_rcv: ChanRecv::TombStoneCompactionNoticeRcv(Arc::new(RwLock::new(comp_rec))),
                range_iterator: None,
                flush_data_sender: ChanSender::FlushDataSender(Arc::new(RwLock::new(flush_data_sender))),
                flush_data_recevier: ChanRecv::FlushDataRecv(Arc::new(RwLock::new(flush_data_rec))),
                flush_signal_sender: ChanSender::FlushNotificationSender(flush_signal_sender),
                flush_signal_receiver: ChanRecv::FlushNotificationRecv(flush_signal_rec),
            });
        }

        let mut recovered_buckets: IndexMap<BucketID, Bucket> = IndexMap::new();
        let mut filters: Vec<BloomFilter> = Vec::new();
        let mut most_recent_head_timestamp = 0;
        let mut most_recent_head_offset = 0;

        let mut most_recent_tail_timestamp = 0;
        let mut most_recent_tail_offset = 0;

        let mut buckets_stream = read_dir(buckets_path.to_owned())
            .await
            .map_err(|err| BucketDirectoryOpenError {
                path: buckets_path.to_owned(),
                error: err,
            })?;

        while let Some(buckets_dir) = buckets_stream
            .next_entry()
            .await
            .map_err(|err| BucketDirectoryOpenError {
                path: buckets_path.to_owned(),
                error: err,
            })?
        {
            let sstable_path = buckets_dir.path().join("sstable_{timestamp}");

            let mut sst_files = Vec::new();
            let sstable_stream = read_dir(sstable_path.to_owned())
                .await
                .map_err(|err| SSTableFileOpenError {
                    path: sstable_path.to_owned(),
                    error: err,
                });

            for mut entry in sstable_stream.into_iter() {
                // Use for loop directly on the stream
                let file_path = entry.next_entry().await.unwrap().unwrap().path();

                if file_path.is_file() {
                    sst_files.push(file_path);
                }
            }

            // Can't guarantee order that the files are retrived so sort for order
            sst_files.sort();
            // Extract bucket id
            let bucket_id = Self::get_bucket_id_from_full_bucket_path(sstable_path.to_owned().as_path().to_owned());

            // We expect two files, data file and index file
            if sst_files.len() < 2 {
                return Err(InvalidSSTableDirectoryError {
                    input_string: sstable_path.as_path().to_owned().to_string_lossy().to_string(),
                });
            }
            let data_file_path = sst_files[1].to_owned();
            let index_file_path = sst_files[0].to_owned();

            // TODO: extract from file path
            let created_at = Utc::now();

            let sst_file = Table {
                dir: sstable_path.as_path().to_owned(),
                hotness: 1,
                created_at: created_at.timestamp_millis() as u64,
                //TODO// instead of unwrapping this can return a file already exisit error, handle it
                data_file: DataFile {
                    file: DataFileNode::new(data_file_path.to_owned(), crate::fs::FileType::SSTable)
                        .await
                        .unwrap(),
                    path: data_file_path,
                },
                index_file: IndexFile {
                    file: IndexFileNode::new(index_file_path.to_owned(), crate::fs::FileType::Index)
                        .await
                        .unwrap(),
                    path: index_file_path,
                },
                size: 0, // TODO
                entries: Arc::new(SkipMap::new()),
            };

            let bucket_uuid = uuid::Uuid::parse_str(&bucket_id).map_err(|err| InvaidUUIDParseString {
                input_string: bucket_id,
                error: err,
            })?;
            // If bucket already exist in recovered bucket then just append sstable to its sstables vector
            if let Some(b) = recovered_buckets.get(&bucket_uuid) {
                let temp_sstables = b.sstables.clone();
                temp_sstables.write().await.push(sst_file.clone());
                let updated_bucket = Bucket::new_with_id_dir_average_and_sstables(
                    buckets_dir.path(),
                    bucket_uuid,
                    temp_sstables.read().await.clone(),
                    0,
                )
                .await?;
                recovered_buckets.insert(bucket_uuid, updated_bucket);
            } else {
                // Create new bucket
                let updated_bucket = Bucket::new_with_id_dir_average_and_sstables(
                    buckets_dir.path(),
                    bucket_uuid,
                    vec![sst_file.clone()],
                    0,
                )
                .await?;
                recovered_buckets.insert(bucket_uuid, updated_bucket);
            }

            let sstable_from_file = sst_file.load_entries_from_file().await?;
            let sstable = sstable_from_file.unwrap();
            // Fetch the most recent write offset so it can
            // use it to recover entries not written into sstables from value log
            let head_entry = sstable.get_value_from_entries(HEAD_ENTRY_KEY);

            let tail_entry = sstable.get_value_from_entries(TAIL_ENTRY_KEY);

            // update head
            if let Some((head_offset, date_created, _)) = head_entry {
                if date_created > most_recent_head_timestamp {
                    most_recent_head_offset = head_offset;
                    most_recent_head_timestamp = date_created;
                }
            }

            // update tail
            if let Some((tail_offset, date_created, _)) = tail_entry {
                if date_created > most_recent_tail_timestamp {
                    most_recent_tail_offset = tail_offset;
                    most_recent_tail_timestamp = date_created;
                }
            }

            let mut bf = Table::build_filter_from_sstable(&sstable.entries);
            bf.set_sstable(sst_file.clone());
            // update bloom filters
            filters.push(bf)

            // Process sst_files here (logic similar to standard fs)
        }

        let mut buckets_map = BucketMap::new(buckets_path.clone());
        for (bucket_id, b) in recovered_buckets.iter() {
            let mut bucket_map_with_reference: IndexMap<BucketID, Bucket> = IndexMap::new();
            bucket_map_with_reference.insert(*bucket_id, b.clone());
            buckets_map.set_buckets(bucket_map_with_reference);
        }

        // store vLog head and tail in memory
        vlog.set_head(most_recent_head_offset);
        vlog.set_tail(most_recent_tail_offset);

        // recover memtable
        let recover_result = DataStore::recover_memtable(
            size_unit,
            capacity,
            config.false_positive_rate,
            &dir.val_log,
            most_recent_head_offset,
        )
        .await;

        let (flush_data_sender, flush_data_rec) = mpsc::channel(DEFAULT_FLUSH_DATA_CHANNEL_SIZE);
        let (flush_signal_sender, flush_signal_rec) = broadcast(DEFAULT_FLUSH_SIGNAL_CHANNEL_SIZE);
        let (tomb_comp_sender, tomb_comp_rec) = mpsc::channel(1);
        match recover_result {
            Ok((active_memtable, read_only_memtables)) => {
                let buckets_map_ref = Arc::new(RwLock::new(buckets_map.to_owned()));
                let bloom_filter_ref = Arc::new(RwLock::new(filters));
                //TODO:  we also need to recover this from sstable
                let key_range_ref = Arc::new(RwLock::new(key_range.to_owned()));
                let read_only_memtables_ref = Arc::new(RwLock::new(read_only_memtables));

                let flusher = Flusher::new(
                    read_only_memtables_ref.clone(),
                    buckets_map_ref.clone(),
                    bloom_filter_ref.clone(),
                    key_range_ref.clone(),
                    config.enable_ttl,
                    config.entry_ttl_millis,
                );

                Ok(DataStore {
                    active_memtable,
                    val_log: vlog,
                    dir,
                    buckets: buckets_map_ref,
                    filters: bloom_filter_ref,
                    key_range: key_range_ref,
                    meta,
                    flusher,
                    compactor: Compactor::new(
                        config.enable_ttl,
                        config.entry_ttl_millis,
                        config.tombstone_ttl,
                        config.background_compaction_interval,
                        config.compactor_flush_listener_interval,
                        config.tombstone_compaction_interval,
                        config.compaction_strategy,
                        compactors::CompactionReason::MaxSize,
                    ),
                    config: config.clone(),
                    read_only_memtables: read_only_memtables_ref,
                    tombstone_compaction_sender: ChanSender::TombStoneCompactionNoticeSender(tomb_comp_sender),
                    range_iterator: None,
                    tombstone_compaction_rcv: ChanRecv::TombStoneCompactionNoticeRcv(Arc::new(RwLock::new(
                        tomb_comp_rec,
                    ))),
                    flush_data_sender: ChanSender::FlushDataSender(Arc::new(RwLock::new(flush_data_sender))),
                    flush_data_recevier: ChanRecv::FlushDataRecv(Arc::new(RwLock::new(flush_data_rec))),
                    flush_signal_sender: ChanSender::FlushNotificationSender(flush_signal_sender),
                    flush_signal_receiver: ChanRecv::FlushNotificationRecv(flush_signal_rec),
                })
            }
            Err(err) => Err(MemTableRecoveryError(Box::new(err))),
        }
    }
    async fn recover_memtable(
        size_unit: SizeUnit,
        capacity: usize,
        false_positive_rate: f64,
        vlog_path: &PathBuf,
        head_offset: usize,
    ) -> Result<
        (
            InMemoryTable<types::Key>,
            IndexMap<Vec<u8>, Arc<RwLock<InMemoryTable<types::Key>>>>,
        ),
        Error,
    > {
        let mut read_only_memtables: IndexMap<Vec<u8>, Arc<RwLock<InMemoryTable<Vec<u8>>>>> = IndexMap::new();
        let mut active_memtable =
            InMemoryTable::with_specified_capacity_and_rate(size_unit, capacity, false_positive_rate);

        let mut vlog = ValueLog::new(&vlog_path.clone()).await?;
        let mut most_recent_offset = head_offset;
        let entries = vlog.recover(head_offset).await?;

        for e in entries {
            let entry = Entry::new(e.key.to_owned(), most_recent_offset, e.created_at, e.is_tombstone);
            // Since the most recent offset is the offset we start reading entries from in value log
            // and we retrieved this from the sstable, therefore should not re-write the initial entry in
            // memtable since it's already in the sstable
            if most_recent_offset != head_offset {
                if active_memtable.is_full(e.key.len()) {
                    // Make memtable read only
                    active_memtable.read_only = true;
                    read_only_memtables.insert(
                        InMemoryTable::generate_table_id(),
                        Arc::new(RwLock::new(active_memtable.to_owned())),
                    );
                    active_memtable =
                        InMemoryTable::with_specified_capacity_and_rate(size_unit, capacity, false_positive_rate);
                }
                active_memtable.insert(&entry)?;
            }
            most_recent_offset += SIZE_OF_U32// Key Size -> for fetching key length
                        +SIZE_OF_U32// Value Length -> for fetching value length
                        + SIZE_OF_U64 // Date Length
                        + SIZE_OF_U8 // tombstone marker
                        + e.key.len() // Key Length
                        + e.value.len(); // Value Length
        }

        Ok((active_memtable, read_only_memtables))
    }
    // Flush all memtables
    pub async fn flush_all_memtables(&mut self) -> Result<(), Error> {
        // Flush active memtable
        let hotness = 1;
        self.flush_memtable(Arc::new(RwLock::new(self.active_memtable.to_owned())), hotness)
            .await?;

        // Flush all read-only memtables
        let memtable_lock = self.read_only_memtables.read().await;
        let memtable_iterator = memtable_lock.iter();
        let mut read_only_memtables = Vec::new();
        for (_, mem) in memtable_iterator {
            read_only_memtables.push(Arc::clone(&mem))
        }
        drop(memtable_lock);
        for memtable in read_only_memtables {
            self.flush_memtable(memtable, hotness).await?;
        }

        // Sort bloom filter by hotness after flushing read-only memtables
        self.filters
            .write()
            .await
            .sort_by(|a, b| b.get_sst().get_hotness().cmp(&a.get_sst().get_hotness()));

        // clear the memtables
        self.active_memtable.clear();
        self.read_only_memtables = Arc::new(RwLock::new(IndexMap::new()));
        Ok(())
    }

    async fn flush_memtable(&mut self, memtable: Arc<RwLock<InMemoryTable<Key>>>, hotness: u64) -> Result<(), Error> {
        let sstable_path = self
            .buckets
            .write()
            .await
            .insert_to_appropriate_bucket(Arc::new(Box::new(memtable.read().await.to_owned())), hotness)
            .await?;

        // Write the memtable to disk as SSTables
        // Insert to bloom filter
        let mut bf = memtable.read().await.get_bloom_filter();
        bf.set_sstable(sstable_path.clone());
        self.filters.write().await.push(bf);

        let biggest_key = memtable.read().await.find_biggest_key()?;
        let smallest_key = memtable.read().await.find_smallest_key()?;
        self.key_range.write().await.set(
            sstable_path.get_data_file_path(),
            smallest_key,
            biggest_key,
            sstable_path,
        );

        Ok(())
    }

    pub async fn run_compaction(&mut self) -> Result<(), Error> {
        Compactor::handle_compaction(
            Arc::clone(&self.buckets),
            Arc::clone(&self.filters.clone()),
            Arc::clone(&self.key_range),
            &self.compactor.config,
        )
        .await
    }

    fn get_bucket_id_from_full_bucket_path(full_path: PathBuf) -> String {
        let full_path_as_str = full_path.to_string_lossy().to_string();
        let mut bucket_id = String::new();
        // Find the last occurrence of "bucket" in the file path
        if let Some(idx) = full_path_as_str.rfind("bucket") {
            // Extract the substring starting from the index after the last occurrence of "bucket"
            let uuid_part = &full_path_as_str[idx + "bucket".len()..];
            if let Some(end_idx) = uuid_part.find('/') {
                // Extract the UUID
                let uuid = &uuid_part[..end_idx];
                bucket_id = uuid.to_string();
            }
        }
        bucket_id
    }
}

impl DirPath {
    pub(crate) fn build(root_path: PathBuf) -> Self {
        let root = root_path;
        let val_log = root.join(VALUE_LOG_DIRECTORY_NAME);
        let buckets = root.join(BUCKETS_DIRECTORY_NAME);
        let meta = root.join(META_DIRECTORY_NAME);
        Self {
            root,
            val_log,
            buckets,
            meta,
        }
    }
}
impl SizeUnit {
    pub(crate) const fn to_bytes(&self, value: usize) -> usize {
        match self {
            SizeUnit::Bytes => value,
            SizeUnit::Kilobytes => value * 1024,
            SizeUnit::Megabytes => value * 1024 * 1024,
            SizeUnit::Gigabytes => value * 1024 * 1024 * 1024,
        }
    }
}

#[cfg(test)]
mod tests {

    use std::thread;

    use crate::compactors::CompState;
    use crate::consts::DEFAULT_COMPACTION_FLUSH_LISTNER_INTERVAL_MILLI;

    use super::*;
    use futures::future::join_all;
    use log::info;
    use tokio::time::{sleep, Duration};

    use rand::distributions::Alphanumeric;
    use rand::{thread_rng, Rng};
    use std::io::{self, Write};
    use tokio::test;
    fn init() {
        let res = env_logger::builder().is_test(true).try_init();
        match res {
            Ok(_) => {}
            Err(err) => println!("err {}", err),
        }
    }

    fn generate_random_string(length: usize) -> String {
        let rng = thread_rng();
        rng.sample_iter(&Alphanumeric).take(length).map(|c| c as char).collect()
    }

    // Generate test to find keys after compaction
    #[tokio::test]
    async fn datastore_create_asynchronous() {
        let path = PathBuf::new().join("bump1");
        let s_engine = DataStore::new(path.clone()).await.unwrap();

        // // Specify the number of random strings to generate
        let num_strings = 50000; // 100k

        // Specify the length of each random string
        let string_length = 5;
        // Generate random strings and store them in a vector
        let mut random_strings: Vec<String> = Vec::with_capacity(num_strings);
        for _ in 0..num_strings {
            let random_string = generate_random_string(string_length);
            random_strings.push(random_string);
        }
        // for k in random_strings.clone() {
        //    s_engine.put(&k, "boyode", None).await.unwrap();
        // }

        let sg = Arc::new(RwLock::new(s_engine));

        let tasks = random_strings.iter().map(|k| {
            let s_engine = Arc::clone(&sg);
            let k = k.clone();
            tokio::spawn(async move {
                let mut value = s_engine.write().await;
                value.put(&k, "boy", None).await
            })
        });

        let all_results = join_all(tasks).await;
        for tokio_response in all_results {
            match tokio_response {
                Ok(entry) => match entry {
                    Ok(is_inserted) => {
                        println!("Insertion Completed");
                        assert_eq!(is_inserted, true)
                    }
                    Err(err) => assert!(false, "{}", err.to_string()),
                },
                Err(err) => {
                    assert!(false, "{}", err.to_string())
                }
            }
        }
        println!("Write completed ");
        sleep(Duration::from_millis(
            DEFAULT_COMPACTION_FLUSH_LISTNER_INTERVAL_MILLI * 3,
        ))
        .await;
        println!("About to start reading");
        // println!("Compaction completed !");
        random_strings.sort();
        let tasks = random_strings
            .get(0..(num_strings / 2))
            .unwrap_or_default()
            .iter()
            .map(|k| {
                let s_engine = Arc::clone(&sg);
                let key = k.clone();
                tokio::spawn(async move {
                    let value = s_engine.read().await;
                    let nn = value.get(&key).await;
                    return nn;
                })
            });
        let all_results = join_all(tasks).await;
        for tokio_response in all_results {
            match tokio_response {
                Ok(entry) => match entry {
                    Ok(v) => {
                        println!("Found  {}", String::from_utf8_lossy(&v.0))
                        //assert_eq!(v.0, b"boy");
                    }
                    Err(err) => assert!(false, "Error: {}", err.to_string()),
                },
                Err(err) => {
                    assert!(false, "{}", err.to_string())
                }
            }
        }

        let _ = fs::remove_dir_all(path.clone()).await;
    }

    #[tokio::test]
    async fn datastore_create_synchronous() {
        let path = PathBuf::new().join("bump2");
        let mut s_engine = DataStore::new(path.clone()).await.unwrap();

        // Specify the number of random strings to generate
        let num_strings = 1000;

        // Specify the length of each random string
        let string_length = 10;
        // Generate random strings and store them in a vector
        let mut random_strings: Vec<String> = Vec::new();
        for _ in 0..num_strings {
            let random_string = generate_random_string(string_length);
            random_strings.push(random_string);
        }

        // Insert the generated random strings
        for (_, s) in random_strings.iter().enumerate() {
            s_engine.put(s, "boyode", None).await.unwrap();
        }
        // let compactor = Compactor::new();

        let compaction_opt = s_engine.run_compaction().await;
        match compaction_opt {
            Ok(_) => {
                println!("Compaction is successful");
                println!(
                    "Length of bucket after compaction {:?}",
                    s_engine.buckets.read().await.buckets.len()
                );
                println!(
                    "Length of bloom filters after compaction {:?}",
                    s_engine.filters.read().await.len()
                );
            }
            Err(err) => {
                println!("Error during compaction {}", err)
            }
        }

        // random_strings.sort();
        for k in random_strings {
            let result = s_engine.get(&k).await;
            match result {
                Ok((value, _)) => {
                    assert_eq!(value, b"boyode");
                }
                Err(_) => {
                    assert!(false, "No err should be found");
                }
            }
        }

        // let _ = fs::remove_dir_all(path.clone()).await;
        // sort to make fetch random
    }

    #[tokio::test]
    async fn datastore_compaction_asynchronous() {
        let path = PathBuf::new().join("bump3");
        let s_engine = DataStore::new(path.clone()).await.unwrap();

        // Specify the number of random strings to generate
        let num_strings = 50000;

        // Specify the length of each random string
        let string_length = 10;
        // Generate random strings and store them in a vector
        let mut random_strings: Vec<String> = Vec::new();
        for _ in 0..num_strings {
            let random_string = generate_random_string(string_length);
            random_strings.push(random_string);
        }
        // for k in random_strings.clone() {
        //     s_engine.put(&k, "boyode").await.unwrap();
        // }
        let sg = Arc::new(RwLock::new(s_engine));
        let binding = random_strings.clone();
        let tasks = binding.iter().map(|k| {
            let s_engine = Arc::clone(&sg);
            let k = k.clone();
            tokio::spawn(async move {
                let mut value = s_engine.write().await;
                value.put(&k, "boyode", None).await
            })
        });

        //Collect the results from the spawned tasks
        for task in tasks {
            tokio::select! {
                result = task => {
                    match result{
                        Ok(v_opt)=>{
                            match v_opt{
                                Ok(v) => {
                                    assert_eq!(v, true)
                                },
                                Err(_) => { assert!(false, "No err should be found")},
                            }
                             }
                        Err(_) =>  assert!(false, "No err should be found") }
                    //println!("{:?}",result);
                }
            }
        }

        // sort to make fetch random
        random_strings.sort();
        let key = &random_strings[0];

        let get_res1 = sg.read().await.get(key).await;
        let get_res2 = sg.read().await.get(key).await;
        let get_res3 = sg.read().await.get(key).await;
        let get_res4 = sg.read().await.get(key).await;
        match get_res1 {
            Ok(v) => {
                assert_eq!(v.0, b"boyode");
            }
            Err(_) => {
                assert!(false, "No error should be found");
            }
        }

        match get_res2 {
            Ok(v) => {
                assert_eq!(v.0, b"boyode");
            }
            Err(_) => {
                assert!(false, "No error should be found");
            }
        }

        match get_res3 {
            Ok(v) => {
                assert_eq!(v.0, b"boyode");
            }
            Err(_) => {
                assert!(false, "No error should be found");
            }
        }
        match get_res4 {
            Ok(v) => {
                assert_eq!(v.0, b"boyode");
            }
            Err(_) => {
                assert!(false, "No error should be found");
            }
        }

        let del_res = sg.write().await.delete(key).await;
        match del_res {
            Ok(v) => {
                assert_eq!(v, true)
            }
            Err(_) => {
                assert!(false, "No error should be found");
            }
        }

        let get_res2 = sg.read().await.get(key).await;
        match get_res2 {
            Ok(_) => {
                assert!(false, "Should not be found after compaction")
            }
            Err(err) => {
                assert_eq!(Error::KeyFoundAsTombstoneInMemtableError.to_string(), err.to_string())
            }
        }

        let _ = sg.write().await.flush_all_memtables().await;
        sg.write().await.active_memtable.clear();

        // We expect tombstone to be flushed to an sstable at this point
        let get_res2 = sg.read().await.get(key).await;
        match get_res2 {
            Ok(_) => {
                assert!(false, "Should not be found after compaction")
            }
            Err(err) => {
                assert_eq!(Error::KeyFoundAsTombstoneInSSTableError.to_string(), err.to_string())
            }
        }

        let compaction_opt = sg.write().await.run_compaction().await;
        // Insert the generated random strings
        // let compactor = Compactor::new();
        // let compaction_opt = sg.write().await.run_compaction().await;
        match compaction_opt {
            Ok(_) => {
                println!("Compaction is successful");
                println!(
                    "Length of bucket after compaction {:?}",
                    sg.read().await.buckets.read().await.buckets.len()
                );
                println!(
                    "Length of bloom filters after compaction {:?}",
                    sg.read().await.filters.read().await.len()
                );
            }
            Err(err) => {
                info!("Error during compaction {}", err)
            }
        }

        // Insert the generated random strings
        let get_res3 = sg.read().await.get(key).await;
        match get_res3 {
            Ok(_) => {
                assert!(false, "Deleted key should be found as tumbstone");
            }

            Err(err) => {
                println!("{}", err);
                if err.to_string() != KeyFoundAsTombstoneInSSTableError.to_string()
                    && err.to_string() != KeyNotFoundInAnySSTableError.to_string()
                {
                    println!("{}", err);
                    assert!(false, "Key should be mapped to tombstone or deleted from all sstables")
                }
            }
        }
        let _ = fs::remove_dir_all(path.clone()).await;
    }

    #[tokio::test]
    async fn datastore_update_asynchronous() {
        let path = PathBuf::new().join("bump4");
        let mut s_engine = DataStore::new(path.clone()).await.unwrap();

        // Specify the number of random strings to generate
        let num_strings = 6000;

        // Specify the length of each random string
        let string_length = 10;
        // Generate random strings and store them in a vector
        let mut random_strings: Vec<String> = Vec::new();
        for _ in 0..num_strings {
            let random_string = generate_random_string(string_length);
            random_strings.push(random_string);
        }
        for k in random_strings.clone() {
            s_engine.put(&k, "boyode", None).await.unwrap();
        }
        let sg = Arc::new(RwLock::new(s_engine));
        let binding = random_strings.clone();
        let tasks = binding.iter().map(|k| {
            let s_engine = Arc::clone(&sg);
            let k = k.clone();
            tokio::spawn(async move {
                let mut value = s_engine.write().await;
                value.put(&k, "boyode", None).await
            })
        });

        // Collect the results from the spawned tasks
        for task in tasks {
            tokio::select! {
                result = task => {
                    match result{
                        Ok(v_opt)=>{
                            match v_opt{
                                Ok(v) => {
                                    assert_eq!(v, true)
                                },
                                Err(_) => { assert!(false, "No err should be found")},
                            }
                             }
                        Err(_) =>  assert!(false, "No err should be found") }
                }
            }
        }
        // // sort to make fetch random
        random_strings.sort();
        let key = &random_strings[0];
        let updated_value = "updated_key";

        let get_res = sg.read().await.get(key).await;
        match get_res {
            Ok(v) => {
                assert_eq!(v.0, b"boyode");
            }
            Err(_) => {
                assert!(false, "No error should be found");
            }
        }

        let update_res = sg.write().await.update(key, updated_value).await;
        match update_res {
            Ok(v) => {
                assert_eq!(v, true)
            }
            Err(_) => {
                assert!(false, "No error should be found");
            }
        }
        let _ = sg.write().await.flush_all_memtables().await;
        sg.write().await.active_memtable.clear();

        let get_res = sg.read().await.get(key).await;
        match get_res {
            Ok((value, _)) => {
                assert_eq!(value, updated_value.as_bytes().to_vec())
            }
            Err(_) => {
                assert!(false, "Should not run")
            }
        }

        // // Run compaction
        let compaction_opt = sg.write().await.run_compaction().await;
        match compaction_opt {
            Ok(_) => {
                println!("Compaction is successful");
                println!(
                    "Length of bucket after compaction {:?}",
                    sg.read().await.buckets.read().await.buckets.len()
                );
                println!(
                    "Length of bloom filters after compaction {:?}",
                    sg.read().await.filters.read().await.len()
                );
            }
            Err(err) => {
                info!("Error during compaction {}", err)
            }
        }

        let get_res = sg.read().await.get(key).await;
        match get_res {
            Ok((value, _)) => {
                assert_eq!(value, updated_value.as_bytes().to_vec())
            }
            Err(_) => {
                assert!(false, "Should not run")
            }
        }
        let _ = fs::remove_dir_all(path.clone()).await;
    }

    #[tokio::test]
    async fn datastore_deletion_asynchronous() {
        let path = PathBuf::new().join("bump5");
        let s_engine = DataStore::new(path.clone()).await.unwrap();

        // Specify the number of random strings to generate
        let num_strings = 60000;

        // Specify the length of each random string
        let string_length = 10;
        // Generate random strings and store them in a vector
        let mut random_strings: Vec<String> = Vec::new();
        for _ in 0..num_strings {
            let random_string = generate_random_string(string_length);
            random_strings.push(random_string);
        }
        // for k in random_strings.clone() {
        //     s_engine.put(&k, "boyode").await.unwrap();
        // }
        let sg = Arc::new(RwLock::new(s_engine));
        let binding = random_strings.clone();
        let tasks = binding.iter().map(|k| {
            let s_engine = Arc::clone(&sg);
            let k = k.clone();
            tokio::spawn(async move {
                let mut value = s_engine.write().await;
                value.put(&k, "boyode", None).await
            })
        });
        let key = "aunkanmi";
        let _ = sg.write().await.put(key, "boyode", None).await;
        // // Collect the results from the spawned tasks
        for task in tasks {
            tokio::select! {
                result = task => {
                    match result{
                        Ok(v_opt)=>{
                            match v_opt{
                                Ok(v) => {
                                    assert_eq!(v, true)
                                },
                                Err(_) => { assert!(false, "No err should be found")},
                            }
                             }
                        Err(_) =>  assert!(false, "No err should be found") }
                }
            }
        }
        // sort to make fetch random
        random_strings.sort();
        let get_res = sg.read().await.get(key).await;
        match get_res {
            Ok((value, _)) => {
                assert_eq!(value, "boyode".as_bytes().to_vec());
            }
            Err(err) => {
                assert_ne!(key.as_bytes().to_vec(), err.to_string().as_bytes().to_vec());
            }
        }

        let del_res = sg.write().await.delete(key).await;
        match del_res {
            Ok(v) => {
                assert_eq!(v, true);
            }
            Err(err) => {
                assert!(err.to_string().is_empty())
            }
        }

        let _ = sg.write().await.flush_all_memtables().await;

        let get_res = sg.read().await.get(key).await;
        match get_res {
            Ok((_, _)) => {
                assert!(false, "Should not be executed")
            }
            Err(err) => {
                assert_eq!(KeyFoundAsTombstoneInSSTableError.to_string(), err.to_string())
            }
        }

        let compaction_opt = sg.write().await.run_compaction().await;
        match compaction_opt {
            Ok(_) => {
                println!("Compaction is successful");
                println!(
                    "Length of bucket after compaction {:?}",
                    sg.read().await.buckets.read().await.buckets.len()
                );
                println!(
                    "Length of bloom filters after compaction {:?}",
                    sg.read().await.filters.read().await.len()
                );
            }
            Err(err) => {
                info!("Error during compaction {}", err)
            }
        }

        // Insert the generated random strings
        println!("trying to get this after compaction {}", key);
        let get_res = sg.read().await.get(key).await;
        match get_res {
            Ok((_, _)) => {
                assert!(false, "Should not ne executed")
            }
            Err(err) => {
                if err.to_string() != KeyFoundAsTombstoneInSSTableError.to_string()
                    && err.to_string() != KeyNotFoundInAnySSTableError.to_string()
                {
                    assert!(false, "Key should be mapped to tombstone or deleted from all sstables")
                }
            }
        }
        let _ = fs::remove_dir_all(path.clone()).await;
    }
}