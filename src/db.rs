use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::checkpoint::Checkpointer;
use crate::mvcc::MvccEngine;
use crate::page::{META_PAGE_ID, Result};
use crate::pager::Pager;
use crate::recovery::Recovery;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

pub struct Db {
    #[allow(dead_code)]
    dir: PathBuf,
    mvcc: Arc<MvccEngine>,
    checkpointer: Checkpointer,
    sync_mode: SyncMode,
}

impl Db {
    pub fn open<P: AsRef<Path>>(path: P, options: Options) -> Result<Self> {
        let dir = path.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;

        let data_file = dir.join("pagedb.data");
        let pager = Pager::open(&data_file)?;
        let checkpointer = Checkpointer::new(&dir);

        let mvcc = Recovery::recover_and_open(
            pager,
            options.page_cache_frames,
            options.wal_segment_bytes,
            &checkpointer,
        )?;

        Ok(Self {
            dir,
            mvcc,
            checkpointer,
            sync_mode: options.sync,
        })
    }

    pub fn begin(&self) -> Txn<'_> {
        let start_ts = self.mvcc.visible_ts();
        self.mvcc.snapshot_registry.register(start_ts);
        Txn {
            db: self,
            start_ts,
            writes: BTreeMap::new(),
            committed: false,
        }
    }

    pub fn checkpoint(&self) -> Result<()> {
        self.checkpointer
            .checkpoint(&self.mvcc, &self.mvcc.bufpool, META_PAGE_ID)
    }

    pub fn vacuum(&self) -> Result<usize> {
        self.mvcc.vacuum()
    }

    pub fn check_invariants(&self) -> Result<crate::btree::TreeStats> {
        self.mvcc.btree.check_invariants()
    }

    pub fn visible_ts(&self) -> u64 {
        self.mvcc.visible_ts()
    }
}

pub struct Txn<'a> {
    db: &'a Db,
    start_ts: u64,
    writes: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    committed: bool,
}

impl<'a> Txn<'a> {
    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        if key.len() > crate::page::MAX_KEY_SIZE {
            return Err(crate::page::Error::KeyTooLarge {
                size: key.len(),
                max: crate::page::MAX_KEY_SIZE,
            });
        }
        if value.len() > crate::page::MAX_VALUE_SIZE {
            return Err(crate::page::Error::ValueTooLarge {
                size: value.len(),
                max: crate::page::MAX_VALUE_SIZE,
            });
        }
        self.writes.insert(key.to_vec(), Some(value.to_vec()));
        Ok(())
    }

    pub fn delete(&mut self, key: &[u8]) -> Result<()> {
        self.writes.insert(key.to_vec(), None);
        Ok(())
    }

    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        // Read-your-writes overlay
        if let Some(local_val) = self.writes.get(key) {
            return Ok(local_val.clone());
        }
        self.db.mvcc.get_at(key, self.start_ts)
    }

    pub fn scan<'b, R>(&self, range: R) -> Result<Vec<(Vec<u8>, Vec<u8>)>>
    where
        R: std::ops::RangeBounds<&'b [u8]>,
    {
        let start_key = match range.start_bound() {
            std::ops::Bound::Included(&b) => Some(b),
            std::ops::Bound::Excluded(&b) => Some(b),
            std::ops::Bound::Unbounded => None,
        };
        let end_key = match range.end_bound() {
            std::ops::Bound::Included(&b) => Some(b),
            std::ops::Bound::Excluded(&b) => Some(b),
            std::ops::Bound::Unbounded => None,
        };

        let base = self.db.mvcc.scan_at(start_key, end_key, self.start_ts)?;

        // Overlay txn-local writes
        let mut map: BTreeMap<Vec<u8>, Option<Vec<u8>>> = BTreeMap::new();
        for (k, v) in base {
            map.insert(k, Some(v));
        }

        for (k, v) in &self.writes {
            if range.contains(&k.as_slice()) {
                map.insert(k.clone(), v.clone());
            }
        }

        let mut res = Vec::new();
        for (k, v) in map {
            if let Some(val) = v {
                res.push((k, val));
            }
        }
        Ok(res)
    }

    pub fn commit(mut self) -> Result<u64> {
        if self.writes.is_empty() {
            self.committed = true;
            return Ok(self.start_ts);
        }

        let sync_always = matches!(self.db.sync_mode, SyncMode::Always);
        let commit_ts = self.db.mvcc.commit(self.start_ts, &self.writes, sync_always)?;
        self.committed = true;

        // Auto-checkpoint if dirty frames exceed 70% threshold
        if self.db.mvcc.bufpool.needs_checkpoint() {
            let _ = self.db.checkpoint();
        }

        Ok(commit_ts)
    }

    pub fn abort(mut self) {
        self.writes.clear();
        self.committed = true;
    }
}

impl<'a> Drop for Txn<'a> {
    fn drop(&mut self) {
        self.db.mvcc.snapshot_registry.unregister(self.start_ts);
    }
}
