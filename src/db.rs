use crate::page::Result;
use std::path::Path;

#[derive(Debug, Clone)]
pub enum SyncMode {
    Always,
    Periodic,
    None,
}

#[derive(Debug, Clone)]
pub struct Options {
    pub page_cache_frames: usize,
    pub wal_segment_bytes: usize,
    pub sync: SyncMode,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            page_cache_frames: 4096,
            wal_segment_bytes: 64 * 1024 * 1024,
            sync: SyncMode::Always,
        }
    }
}

pub struct Db;

impl Db {
    pub fn open<P: AsRef<Path>>(_path: P, _options: Options) -> Result<Self> {
        Ok(Self)
    }

    pub fn begin(&self) -> Txn {
        Txn
    }
}

pub struct Txn;

impl Txn {
    pub fn put(&mut self, _key: &[u8], _value: &[u8]) -> Result<()> {
        Ok(())
    }

    pub fn delete(&mut self, _key: &[u8]) -> Result<()> {
        Ok(())
    }

    pub fn get(&self, _key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }

    pub fn commit(self) -> Result<u64> {
        Ok(1)
    }
}
