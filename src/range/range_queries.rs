// NOTE: STCS can handle range queries but scans within identified SSTables might be neccessary.
// Data for your range might be spread across multiple SSTables. Even with a successful bloom filter check,
// each identified SSTable might still contain data outside your desired range. For heavily range query-focused workloads, LCS or TWSC should be considered
// Although this stratedy is not available for now, It will be implmented in the future

use crate::consts::{DEFAULT_ALLOW_PREFETCH, DEFAULT_PREFETCH_SIZE};
use crate::err::StorageEngineError;
use crate::memtable::{Entry, InMemoryTable};
use crate::sparse_index::SparseIndex;
use crate::sstable::SSTable;
use crate::storage_engine::StorageEngine;
use crate::types::{self, CreationTime, IsTombStone, Key, ValOffset, Value};
use crate::value_log::ValueLog;
use futures::future::join_all;
use indexmap::IndexMap;
use log::{error, info};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::{cmp::Ordering, collections::HashMap, sync::Arc};
use tokio::sync::broadcast::error;
use tokio_stream::{self as stream, StreamExt};
#[derive(Debug, Clone)]
pub struct FetchedEntry {
    pub key: Key,
    pub val: Value,
}

#[derive(Debug, Clone)]
pub struct RangeIterator<'a> {
    pub start: &'a [u8],
    pub current: usize,
    pub end: &'a [u8],
    pub allow_prefetch: bool,
    pub prefetch_entries_size: usize,
    pub prefetch_entries: Vec<FetchedEntry>,
    pub keys: Vec<Entry<Key, ValOffset>>,
    pub v_log: ValueLog,
}

impl<'a> RangeIterator<'a> {
    fn new(
        start: &'a [u8],
        end: &'a [u8],
        allow_prefetch: bool,
        prefetch_entries_size: usize,
        keys: Vec<Entry<Key, ValOffset>>,
        v_log: ValueLog,
    ) -> Self {
        Self {
            start,
            current: 0,
            end,
            allow_prefetch,
            prefetch_entries_size,
            prefetch_entries: Vec::new(),
            keys,
            v_log,
        }
    }

    // READ -> https://tokio.rs/tokio/tutorial/streams
    pub async fn next(&mut self) -> impl stream::Stream<Item = FetchedEntry> {
        if self.allow_prefetch {
            if self.current_is_at_end_prefetched_keys() {
                if self.current_is_at_last_key() {
                    return stream::iter(vec![]);
                }
                match self.prefetch_entries().await {
                    Err(err) => {
                        error!("{}", StorageEngineError::RangeScanError(Box::new(err)));
                        return stream::iter(vec![]);
                    }
                    _ => {}
                }
            }
            let entry = self.current_entry();
            self.current += 1;
            return stream::iter(vec![entry]);
        }
        // handle not allow prefetch
        return stream::iter(vec![]);
    }
    pub fn prev(&mut self) -> Option<FetchedEntry> {
        None
    }
    pub fn key<K>(&mut self) -> Option<K> {
        None
    }

    pub fn value<V>(&mut self) -> Option<V> {
        None
    }

    // Move the iterator to the end of the collection.
    pub fn end(&mut self) -> Option<FetchedEntry> {
        None
    }

    pub async fn prefetch_entries(&mut self) -> Result<(), StorageEngineError> {
        let keys: Vec<Entry<Key, ValOffset>>;
        if self.current + self.prefetch_entries_size <= self.keys.len() {
            keys = (&self.keys[self.current..self.current + self.prefetch_entries_size]).to_vec();
            // self.current += self.prefetch_entries_size
        } else {
            keys = (&self.keys[self.current..]).to_vec();
        }
        let entries = self.fetch_entries_in_parralel(&keys).await;
        match entries {
            Ok(e) => Ok(self.prefetch_entries.extend(e)),
            Err(err) => Err(err),
        }
    }
    pub fn current_entry(&self) -> FetchedEntry {
        self.prefetch_entries[self.current].to_owned()
    }
    pub fn current_is_at_end_prefetched_keys(&self) -> bool {
        self.current >= self.prefetch_entries.len()
    }
    pub fn current_is_at_last_key(&self) -> bool {
        self.current >= self.keys.len()
    }
    pub async fn fetch_entries_in_parralel(
        &self,
        keys: &'a Vec<Entry<Key, ValOffset>>,
    ) -> Result<Vec<FetchedEntry>, StorageEngineError> {
        let mut entries_map: BTreeMap<Key, Value> = BTreeMap::new();
        let tokio_owned_keys = keys.to_owned();
        let tokio_owned_v_log = Arc::new(self.v_log.to_owned());
        let tasks = tokio_owned_keys.into_iter().map(|entry| {
            let v_log = Arc::clone(&tokio_owned_v_log);
            tokio::spawn(async move {
                // We only use the snapshot of vlog to prevent modification while transaction is ongoing
                let entry_from_vlog = v_log.get(entry.val_offset).await;
                match entry_from_vlog {
                    Ok(val_opt) => match val_opt {
                        Some((val, is_deleted)) => return Ok((entry.key, val, is_deleted)),
                        None => {
                            return Err(StorageEngineError::KeyNotFoundInValueLogError);
                        }
                    },
                    Err(err) => return Err(err),
                };
            })
        });

        let all_results = join_all(tasks).await;
        for tokio_response in all_results {
            match tokio_response {
                Ok(entry) => match entry {
                    Ok((key, val, is_deleted)) => {
                        if !is_deleted {
                            entries_map.insert(key, val);
                        }
                    }
                    Err(err) => {
                        error!("{:?}", err)
                    }
                },
                Err(err) => error!("{}", err),
            }
        }
        let mut prefetched_entries = Vec::new();
        for (key, val) in entries_map {
            prefetched_entries.push(FetchedEntry { key, val })
        }
        Ok(prefetched_entries)
    }
}

impl Default for RangeIterator<'_> {
    fn default() -> Self {
        RangeIterator {
            start: &[0],
            current: 0,
            end: &[0],
            allow_prefetch: DEFAULT_ALLOW_PREFETCH,
            prefetch_entries_size: DEFAULT_PREFETCH_SIZE,
            prefetch_entries: Vec::new(),
            keys: Vec::new(),
            v_log: ValueLog {
                file_path: PathBuf::new(),
                head_offset: 0,
                tail_offset: 0,
            },
        }
    }
}

impl<'a> StorageEngine<'a, Key> {
    // Start if the range query
    pub async fn seek(
        &'static mut self,
        start: &'a [u8],
        end: &'a [u8],
    ) -> Result<&'a RangeIterator, StorageEngineError> {
        let mut merger = Merger::new();
        // check entries within active memtable
        if !self.active_memtable.index.is_empty() {
            if self
                .active_memtable
                .index
                .lower_bound(std::ops::Bound::Included(start))
                .is_some()
                || self
                    .active_memtable
                    .index
                    .upper_bound(std::ops::Bound::Included(end))
                    .is_some()
            {
                merger.merge_entries(
                    self.active_memtable
                        .clone()
                        .index
                        .iter()
                        .filter(|e| InMemoryTable::is_entry_within_range(e, start, end))
                        .map(|e| {
                            Entry::new(e.key().to_vec(), e.value().0, e.value().1, e.value().2)
                        })
                        .collect::<Vec<Entry<Key, ValOffset>>>(),
                );
            }
        }
        // check inactive memtable
        if !self.read_only_memtables.read().await.is_empty() {
            let read_only_memtables = self.read_only_memtables.read().await.clone();

            for (_, memtable) in read_only_memtables {
                let memtable_ref = memtable.read().await;
                if memtable_ref
                    .index
                    .lower_bound(std::ops::Bound::Included(start))
                    .is_some()
                    || memtable_ref
                        .index
                        .upper_bound(std::ops::Bound::Included(end))
                        .is_some()
                {
                    merger.merge_entries(
                        memtable_ref
                            .clone()
                            .index
                            .iter()
                            .filter(|e| InMemoryTable::is_entry_within_range(e, start, end))
                            .map(|e| {
                                Entry::new(e.key().to_vec(), e.value().0, e.value().1, e.value().2)
                            })
                            .collect::<Vec<Entry<Key, ValOffset>>>(),
                    );
                }
            }
        }

        let sstables_within_range = {
            let mut sstable_path = HashMap::new();

            for b in self.bloom_filters.read().await.to_owned().into_iter() {
                let bf_inner = b.to_owned();
                let bf_sstable = bf_inner.sstable_path.to_owned().unwrap();
                let data_path = bf_sstable.data_file_path.to_str().unwrap();
                if bf_inner.contains(&start.to_vec()) || bf_inner.contains(&end.to_vec()) {
                    sstable_path.insert(data_path.to_owned(), bf_sstable.to_owned());
                }
            }

            let key_range = self.key_range.read().await;
            let paths_from_key_range = key_range.range_scan(&start.to_vec(), &end.to_vec());
            if !paths_from_key_range.is_empty() {
                for range in paths_from_key_range.iter() {
                    if !sstable_path
                        .contains_key(range.full_sst_path.data_file_path.to_str().unwrap())
                    {
                        sstable_path.insert(
                            range
                                .full_sst_path
                                .data_file_path
                                .to_str()
                                .unwrap()
                                .to_owned(),
                            range.full_sst_path.to_owned(),
                        );
                    }
                }
            }
            sstable_path
        };

        for (_, sst) in sstables_within_range {
            let sparse_index = SparseIndex::new(sst.index_file_path.clone()).await;
            match sparse_index.get_block_offset_range(&start, &end).await {
                Ok(range_offset) => {
                    let sst = SSTable::new_with_exisiting_file_path(
                        sst.dir.to_owned(),
                        sst.data_file_path.to_owned(),
                        sst.index_file_path.to_owned(),
                    );
                    match sst.range(range_offset).await {
                        Ok(sstable_entries) => merger.merge_entries(sstable_entries),
                        Err(err) => return Err(err),
                    }
                }
                Err(err) => return Err(StorageEngineError::RangeScanError(Box::new(err))),
            }
        }
        self.range_iterator = RangeIterator::<'a>::new(
            start,
            end,
            self.config.allow_prefetch,
            self.config.prefetch_size,
            merger.entries,
            self.val_log.clone(),
        );
        Ok(&self.range_iterator)
    }
}

pub struct Merger {
    entries: Vec<Entry<Key, ValOffset>>,
}

impl Merger {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    fn merge_entries(&mut self, entries_to_merge: Vec<Entry<Key, ValOffset>>) {
        let mut merged_indexes = Vec::new();
        let e1 = &self.entries;
        let e2 = entries_to_merge;

        let (mut i, mut j) = (0, 0);
        // Compare elements from both arrays and merge them
        while i < e1.len() && j < e2.len() {
            match e1[i].key.cmp(&e2[j].key) {
                Ordering::Less => {
                    merged_indexes.push(e1[i].to_owned());
                    i += 1;
                }
                Ordering::Equal => {
                    if e1[i].created_at > e2[j].created_at {
                        merged_indexes.push(e1[i].to_owned());
                    } else {
                        merged_indexes.push(e2[j].to_owned());
                    }
                    i += 1;
                    j += 1;
                }
                Ordering::Greater => {
                    merged_indexes.push(e2[j].to_owned());
                    j += 1;
                }
            }
        }

        // If there are any remaining entries in e1, append them
        while i < e1.len() {
            merged_indexes.push(e1[i].to_owned());
            i += 1;
        }

        // If there are any remaining entries in e2, append them
        while j < e2.len() {
            merged_indexes.push(e2[j].to_owned());
            j += 1;
        }
    }
}
