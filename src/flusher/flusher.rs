use crate::types;
use crate::{
    bloom_filter::BloomFilter, cfg::Config, compaction::BucketMap, err::StorageEngineError,
    key_offseter::KeyRange, memtable::InMemoryTable, storage_engine::ExRw,
};
use indexmap::IndexMap;
use std::sync::Arc;

type K = types::Key;

pub type InActiveMemtableID = Vec<u8>;
pub type InActiveMemtable = ExRw<InMemoryTable<K>>;
pub type FlushDataMemTable = (InActiveMemtableID, InActiveMemtable);

use tokio::spawn;
use tokio::sync::mpsc::Receiver;


#[derive(Debug)]
pub struct FlushUpdateMsg {
    pub flushed_memtable_id: InActiveMemtableID,
    pub buckets: BucketMap,
    pub bloom_filters: Vec<BloomFilter>,
    pub key_range: KeyRange,
}

#[derive(Debug)]
pub enum FlushResponse {
    Success {
        table_id: Vec<u8>,
        updated_bucket_map: BucketMap,
        updated_bloom_filters: Vec<BloomFilter>,
        key_range: KeyRange,
    },
    Failed {
        reason: StorageEngineError,
    },
}

#[derive(Debug, Clone)]
pub struct Flusher {
    pub(crate) read_only_memtable: ExRw<IndexMap<K, ExRw<InMemoryTable<K>>>>,
    pub(crate) table_to_flush: ExRw<InMemoryTable<K>>,
    pub(crate) table_id: Vec<u8>,
    pub(crate) bucket_map: ExRw<BucketMap>,
    pub(crate) bloom_filters: ExRw<Vec<BloomFilter>>,
    pub(crate) key_range: ExRw<KeyRange>,
    pub(crate) use_ttl: bool,
    pub(crate) entry_ttl: u64,
}

impl Flusher {
    pub fn new(
        read_only_memtable: ExRw<IndexMap<K, ExRw<InMemoryTable<K>>>>,
        table_to_flush: ExRw<InMemoryTable<K>>,
        table_id: Vec<u8>,
        bucket_map: ExRw<BucketMap>,
        bloom_filters: ExRw<Vec<BloomFilter>>,
        key_range: ExRw<KeyRange>,
        use_ttl: bool,
        entry_ttl: u64,
    ) -> Self {
        Self {
            read_only_memtable,
            table_to_flush,
            table_id,
            bucket_map,
            bloom_filters,
            key_range,
            use_ttl,
            entry_ttl,
        }
    }

    pub async fn flush(&mut self) -> Result<(), StorageEngineError> {
        let flush_data = self;
        let table = Arc::clone(&flush_data.table_to_flush);
        // let table_id = &flush_data.table_id;
        if table.read().await.index.is_empty() {
            println!("Cannot flush an empty table");
            return Err(StorageEngineError::FailedToInsertToBucket(
                "Cannot flush an empty table".to_string(),
            ));
        }

        let table_bloom_filter = &mut table.read().await.bloom_filter.to_owned();
        let table_biggest_key = table.read().await.find_biggest_key()?;
        let table_smallest_key = table.read().await.find_smallest_key()?;
        let hotness = 1;
        let sstable_path = flush_data
            .bucket_map
            .write()
            .await
            .insert_to_appropriate_bucket(&table.read().await.to_owned(), hotness)
            .await?;

        let data_file_path = sstable_path.get_data_file_path().clone();

        flush_data.key_range.write().await.set(
            data_file_path,
            table_smallest_key,
            table_biggest_key,
            sstable_path.clone(),
        );

        table_bloom_filter.set_sstable_path(sstable_path);
        flush_data
            .bloom_filters
            .write()
            .await
            .push(table_bloom_filter.to_owned());

        // sort bloom filter by hotness
        flush_data.bloom_filters.write().await.sort_by(|a, b| {
            b.get_sstable_path()
                .get_hotness()
                .cmp(&a.get_sstable_path().get_hotness())
        });

        Ok(())
    }

    pub fn flush_data_collector(
        &self,
        rcx: ExRw<Receiver<FlushDataMemTable>>,
        buckets: ExRw<BucketMap>,
        bloom_filters: ExRw<Vec<BloomFilter>>,
        key_range: ExRw<KeyRange>,
        read_only_memtable: ExRw<IndexMap<K, ExRw<InMemoryTable<K>>>>,
        config: Config,
    ) {
        let rcx_clone = Arc::clone(&rcx);

        spawn(async move {
            let current_buckets = &buckets;
            let current_bloom_filters = &bloom_filters;
            let current_key_range = &key_range;
            let current_read_only_memtables = &read_only_memtable;
            while let Some((table_id, table_to_flush)) = rcx_clone.write().await.recv().await {
                let mut flusher = Flusher::new(
                    Arc::clone(&read_only_memtable),
                    table_to_flush,
                    table_id.to_owned(),
                    Arc::clone(&current_buckets),
                    Arc::clone(&current_bloom_filters),
                    Arc::clone(&current_key_range),
                    config.enable_ttl,
                    config.entry_ttl_millis,
                );

                match flusher.flush().await {
                    Ok(_) => {
                        current_read_only_memtables
                            .write()
                            .await
                            .shift_remove(&table_id);
                    }
                    // Handle failure case here
                    Err(err) => {
                        println!("Flush error: {}", err);
                    }
                }
            }
        });
    }
}
