use std::path::PathBuf;

use tokio::{
    fs::OpenOptions,
    io::{self, AsyncReadExt, AsyncWriteExt},
};

use crate::{
    consts::{EOF, SIZE_OF_U32},
    err::StorageEngineError,
};
use StorageEngineError::*;
type Offset = u32;
struct SparseIndexEntry {
    key_prefix: u32,
    key: Vec<u8>,
    offset: u32,
}

pub struct SparseIndex {
    entries: Vec<SparseIndexEntry>,
    file_path: PathBuf,
}

pub struct RangeOffset {
    pub start_offset: Offset,
    pub end_offset: Offset,
}

impl RangeOffset {
    pub fn new(start: Offset, end: Offset) -> Self {
        Self {
            start_offset: start,
            end_offset: end,
        }
    }
}

impl SparseIndex {
    pub async fn new(file_path: PathBuf) -> Self {
        Self {
            file_path,
            entries: Vec::new(),
        }
    }

    pub fn insert(&mut self, key_prefix: u32, key: Vec<u8>, offset: u32) {
        self.entries.push(SparseIndexEntry {
            key_prefix,
            key,
            offset,
        })
    }

    pub async fn write_to_file(&self) -> Result<(), StorageEngineError> {
        let file_path = PathBuf::from(&self.file_path);
        let mut file = OpenOptions::new()
            .append(true)
            .open(file_path.clone())
            .await
            .map_err(|err| SSTableFileOpenError {
                path: file_path.clone(),
                error: err,
            })?;
        for entry in &self.entries {
            let entry_len = entry.key.len() + SIZE_OF_U32 + SIZE_OF_U32;

            let mut entry_vec = Vec::with_capacity(entry_len);

            //add key len
            entry_vec.extend_from_slice(&(entry.key_prefix).to_le_bytes());

            //add key
            entry_vec.extend_from_slice(&entry.key);

            //add value offset
            entry_vec.extend_from_slice(&(entry.offset as u32).to_le_bytes());
            assert!(entry_len == entry_vec.len(), "Incorrect entry size");

            file.write_all(&entry_vec)
                .await
                .map_err(|err| IndexFileWriteError(err))?;

            file.flush().await.map_err(|err| IndexFileFlushError(err))?;
        }
        Ok(())
    }

    pub(crate) async fn get(&self, searched_key: &[u8]) -> Result<Option<u32>, StorageEngineError> {
        let mut block_offset = -1;
        // Open the file in read mode
        let file_path = PathBuf::from(&self.file_path);
        let mut file = OpenOptions::new()
            .read(true)
            .open(file_path.clone())
            .await
            .map_err(|err| SSTableFileOpenError {
                path: file_path.clone(),
                error: err,
            })?;

        // read bloom filter to check if the key possbly exists in the sstable
        // search sstable for key
        loop {
            let mut key_len_bytes = [0; SIZE_OF_U32];
            let mut bytes_read =
                file.read(&mut key_len_bytes)
                    .await
                    .map_err(|err| SSTableFileReadError {
                        path: file_path.clone(),
                        error: err,
                    })?;
            // If the end of the file is reached and no match is found, return non
            if bytes_read == 0 {
                if block_offset == -1 {
                    return Ok(None);
                }
                return Ok(Some(block_offset as u32));
            }
            let key_len = u32::from_le_bytes(key_len_bytes);
            let mut key = vec![0; key_len as usize];
            bytes_read = file
                .read(&mut key)
                .await
                .map_err(|err| IndexFileReadError(err))?;
            if bytes_read == 0 {
                return Err(UnexpectedEOF(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    EOF,
                )));
            }
            let mut key_offset_bytes = [0; SIZE_OF_U32];
            bytes_read =
                file.read(&mut key_offset_bytes)
                    .await
                    .map_err(|err| SSTableFileReadError {
                        path: file_path.clone(),
                        error: err,
                    })?;
            if bytes_read == 0 {
                return Err(UnexpectedEOF(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    EOF,
                )));
            }

            let offset = u32::from_le_bytes(key_offset_bytes);
            match key.cmp(&searched_key.to_vec()) {
                std::cmp::Ordering::Less => block_offset = offset as i32,
                std::cmp::Ordering::Equal => {
                    return Ok(Some(offset));
                }
                std::cmp::Ordering::Greater => {
                    // if all index keys are greater than the searched key then return none
                    if block_offset == -1 {
                        return Ok(None);
                    }
                    return Ok(Some(block_offset as u32));
                }
            }
        }
    }

    pub(crate) async fn get_offset_range(
        &self,
        start_key: &[u8],
        end_key: &[u8],
    ) -> Result<RangeOffset, StorageEngineError> {
        let mut range_offset = RangeOffset::new(0, 0);
        // Open the file in read mode
        let file_path = PathBuf::from(&self.file_path);
        let mut file = OpenOptions::new()
            .read(true)
            .open(file_path.clone())
            .await
            .map_err(|err| SSTableFileOpenError {
                path: file_path.clone(),
                error: err,
            })?;

        // read bloom filter to check if the key possbly exists in the sstable
        // search sstable for key
        loop {
            let mut key_len_bytes = [0; SIZE_OF_U32];
            let mut bytes_read =
                file.read(&mut key_len_bytes)
                    .await
                    .map_err(|err| SSTableFileReadError {
                        path: file_path.clone(),
                        error: err,
                    })?;
            // If the end of the file is reached and no match is found, return non
            if bytes_read == 0 {
                return Ok(range_offset);
            }
            let key_len = u32::from_le_bytes(key_len_bytes);
            let mut key = vec![0; key_len as usize];
            bytes_read = file
                .read(&mut key)
                .await
                .map_err(|err| IndexFileReadError(err))?;
            if bytes_read == 0 {
                return Err(UnexpectedEOF(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    EOF,
                )));
            }
            let mut key_offset_bytes = [0; SIZE_OF_U32];
            bytes_read =
                file.read(&mut key_offset_bytes)
                    .await
                    .map_err(|err| SSTableFileReadError {
                        path: file_path.clone(),
                        error: err,
                    })?;
            if bytes_read == 0 {
                return Err(UnexpectedEOF(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    EOF,
                )));
            }

            let offset = u32::from_le_bytes(key_offset_bytes);
            match key.cmp(&start_key.to_vec()) {
                std::cmp::Ordering::Greater => match key.cmp(&end_key.to_vec()) {
                    std::cmp::Ordering::Greater => {
                        range_offset.end_offset = offset;
                        return Ok(range_offset);
                    }
                    _ => range_offset.end_offset = offset,
                },
                _ => range_offset.start_offset = offset,
            }
        }
    }
}
