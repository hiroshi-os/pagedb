use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;

use crate::btree::BTree;
use crate::bufpool::BufPool;
use crate::page::{Error, Result};
use crate::wal::{WalCommitRecord, WalManager, WalOpType, WalWriteOp};

/// Tagged value format for leaf nodes:
/// Put: [1, value_bytes...]
/// Tombstone: [2]
pub const TAG_PUT: u8 = 1;
pub const TAG_TOMBSTONE: u8 = 2;

/// MVCC internal key encoding: user_key ‖ (u64::MAX - commit_ts) (big-endian)
/// Ensures newest commit_ts for a user_key sorts FIRST in the B+tree.
pub fn encode_mvcc_key(user_key: &[u8], commit_ts: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(user_key.len() + 8);
    key.extend_from_slice(user_key);
    let inverted = u64::MAX - commit_ts;
    key.extend_from_slice(&inverted.to_be_bytes());
    key
}

pub fn decode_mvcc_key(encoded: &[u8]) -> Result<(&[u8], u64)> {
    if encoded.len() < 8 {
        return Err(Error::Corruption("MVCC key too short".into()));
    }
    let user_len = encoded.len() - 8;
    let user_key = &encoded[..user_len];
    let inverted = u64::from_be_bytes(encoded[user_len..].try_into().unwrap_or([0; 8]));
    let commit_ts = u64::MAX - inverted;
    Ok((user_key, commit_ts))
}

pub fn encode_mvcc_val(val: Option<&[u8]>) -> Vec<u8> {
    match val {
        Some(bytes) => {
            let mut v = Vec::with_capacity(1 + bytes.len());
            v.push(TAG_PUT);
            v.extend_from_slice(bytes);
            v
        }
        None => vec![TAG_TOMBSTONE],
    }
}

pub fn decode_mvcc_val(val: &[u8]) -> Result<Option<Vec<u8>>> {
    if val.is_empty() {
        return Err(Error::Corruption("Empty MVCC value".into()));
    }
    match val[0] {
        TAG_PUT => Ok(Some(val[1..].to_vec())),
        TAG_TOMBSTONE => Ok(None),
        other => Err(Error::Corruption(format!("Invalid MVCC tag {other}"))),
    }
}

/// Active snapshot registry tracking open transactions for VACUUM
pub struct SnapshotRegistry {
    snapshots: Mutex<Vec<u64>>, // active start_ts list
}

impl SnapshotRegistry {
    pub fn new() -> Self {
        Self {
            snapshots: Mutex::new(Vec::new()),
        }
    }

    pub fn register(&self, start_ts: u64) {
        let mut list = self.snapshots.lock();
        list.push(start_ts);
    }

    pub fn unregister(&self, start_ts: u64) {
        let mut list = self.snapshots.lock();
        if let Some(pos) = list.iter().position(|&ts| ts == start_ts) {
            list.swap_remove(pos);
        }
    }

    pub fn min_active_ts(&self) -> Option<u64> {
        let list = self.snapshots.lock();
        list.iter().copied().min()
    }
}

pub struct MvccEngine {
    pub btree: Arc<BTree>,
    pub bufpool: Arc<BufPool>,
    pub wal: Arc<Mutex<WalManager>>,
    pub commit_lock: Mutex<()>,
    pub last_commit_ts: AtomicU64,
    pub visible_ts: AtomicU64,
    pub snapshot_registry: Arc<SnapshotRegistry>,
}

impl MvccEngine {
    pub fn new(
        btree: Arc<BTree>,
        bufpool: Arc<BufPool>,
        wal: Arc<Mutex<WalManager>>,
        initial_commit_ts: u64,
    ) -> Self {
        Self {
            btree,
            bufpool,
            wal,
            commit_lock: Mutex::new(()),
            last_commit_ts: AtomicU64::new(initial_commit_ts),
            visible_ts: AtomicU64::new(initial_commit_ts),
            snapshot_registry: Arc::new(SnapshotRegistry::new()),
        }
    }

    pub fn visible_ts(&self) -> u64 {
        self.visible_ts.load(Ordering::SeqCst)
    }

    pub fn last_commit_ts(&self) -> u64 {
        self.last_commit_ts.load(Ordering::SeqCst)
    }

    /// Read value for user_key visible at snapshot `start_ts`
    pub fn get_at(&self, user_key: &[u8], start_ts: u64) -> Result<Option<Vec<u8>>> {
        // Since inverted commit_ts is used, scanning from encode_mvcc_key(user_key, start_ts)
        // gives the newest version ≤ start_ts first!
        let start_encoded = encode_mvcc_key(user_key, start_ts);
        let end_encoded = encode_mvcc_key(user_key, 0); // 0 inverted is u64::MAX (latest possible scan bound)

        let versions = self
            .btree
            .scan(start_encoded.as_slice()..=end_encoded.as_slice())?;
        for (enc_k, enc_v) in versions {
            let (k, ts) = decode_mvcc_key(&enc_k)?;
            if k == user_key && ts <= start_ts {
                return decode_mvcc_val(&enc_v);
            }
        }
        Ok(None)
    }

    /// Range scan for keys visible at snapshot `start_ts`
    pub fn scan_at(
        &self,
        start_user_key: Option<&[u8]>,
        end_user_key: Option<&[u8]>,
        start_ts: u64,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        // Leaf scan across btree, keeping the newest version ≤ start_ts for each distinct user key
        let all = self.btree.scan(..)?;
        let mut results = Vec::new();
        let mut curr_user_key: Option<Vec<u8>> = None;
        let mut chosen = false;

        for (enc_k, enc_v) in all {
            let (u_key, ts) = decode_mvcc_key(&enc_k)?;

            // Check range boundaries
            if let Some(start) = start_user_key {
                if u_key < start {
                    continue;
                }
            }
            if let Some(end) = end_user_key {
                if u_key >= end {
                    continue;
                }
            }

            match &curr_user_key {
                Some(prev) if prev.as_slice() == u_key => {
                    // Subsequent version of current key; skip if already found or older
                }
                _ => {
                    // New user key encountered
                    curr_user_key = Some(u_key.to_vec());
                    chosen = false;
                }
            }

            if !chosen && ts <= start_ts {
                chosen = true;
                if let Some(val) = decode_mvcc_val(&enc_v)? {
                    results.push((u_key.to_vec(), val));
                }
            }
        }

        Ok(results)
    }

    /// Commit transaction writes:
    /// (a) check first-committer-wins conflicts
    /// (b) assign commit_ts = last_commit_ts + 1
    /// (c) append WAL record & fsync
    /// (d) apply to B+tree
    /// (e) publish visible_ts
    /// (f) release commit mutex
    pub fn commit(
        &self,
        _start_ts: u64,
        writes: &std::collections::BTreeMap<Vec<u8>, Option<Vec<u8>>>,
        sync_always: bool,
    ) -> Result<u64> {
        let _commit_guard = self.commit_lock.lock();

        // (a) First-committer-wins conflict check:
        // Skip conflict check ONLY if unsafe-no-conflict-check feature is enabled (negative control test)
        #[cfg(not(feature = "unsafe-no-conflict-check"))]
        {
            let start_ts = _start_ts;
            for user_key in writes.keys() {
                // Check if any version has commit_ts > start_ts
                let latest_enc = encode_mvcc_key(user_key, u64::MAX);
                let oldest_enc = encode_mvcc_key(user_key, 0);
                let existing = self
                    .btree
                    .scan(latest_enc.as_slice()..=oldest_enc.as_slice())?;
                for (enc_k, _) in existing {
                    let (k, existing_ts) = decode_mvcc_key(&enc_k)?;
                    if k == user_key.as_slice() && existing_ts > start_ts {
                        return Err(Error::Conflict {
                            key: user_key.clone(),
                        });
                    }
                }
            }
        }

        // (b) Assign commit_ts
        let commit_ts = self.last_commit_ts.load(Ordering::SeqCst) + 1;

        // (c) Prepare WAL record
        let mut wal_ops = Vec::with_capacity(writes.len());
        for (k, v) in writes {
            match v {
                Some(val) => wal_ops.push(WalWriteOp {
                    op_type: WalOpType::Put,
                    key: k.clone(),
                    val: val.clone(),
                }),
                None => wal_ops.push(WalWriteOp {
                    op_type: WalOpType::Delete,
                    key: k.clone(),
                    val: Vec::new(),
                }),
            }
        }

        let _lsn = {
            let mut wal_mgr = self.wal.lock();
            let next_lsn = wal_mgr.next_lsn();
            let record = WalCommitRecord {
                lsn: next_lsn,
                commit_ts,
                writes: wal_ops,
            };
            wal_mgr.append_record(&record)?;
            if sync_always {
                wal_mgr.sync()?;
            }
            next_lsn
        };

        // (d) Apply versions to B+tree
        for (k, v) in writes {
            let enc_k = encode_mvcc_key(k, commit_ts);
            let enc_v = encode_mvcc_val(v.as_deref());
            self.btree.insert(&enc_k, &enc_v)?;
        }

        // (e) Update last_commit_ts and publish visible_ts
        self.last_commit_ts.store(commit_ts, Ordering::SeqCst);
        self.visible_ts.store(commit_ts, Ordering::SeqCst);

        Ok(commit_ts)
    }

    /// Apply an already committed WAL record to tree (used by redo recovery)
    /// Redo is idempotent because versions encode commit_ts!
    pub fn apply_wal_commit(&self, record: &WalCommitRecord) -> Result<()> {
        for op in &record.writes {
            let enc_k = encode_mvcc_key(&op.key, record.commit_ts);
            let enc_v = match op.op_type {
                WalOpType::Put => encode_mvcc_val(Some(&op.val)),
                WalOpType::Delete => encode_mvcc_val(None),
            };
            self.btree.insert(&enc_k, &enc_v)?;
        }

        self.last_commit_ts.fetch_max(record.commit_ts, Ordering::SeqCst);
        self.visible_ts.fetch_max(record.commit_ts, Ordering::SeqCst);
        Ok(())
    }

    /// Vacuum: removes versions that no active snapshot can see.
    pub fn vacuum(&self) -> Result<usize> {
        let _commit_guard = self.commit_lock.lock();
        let min_active = self
            .snapshot_registry
            .min_active_ts()
            .unwrap_or_else(|| self.visible_ts());

        let all = self.btree.scan(..)?;
        let mut keys_to_delete = Vec::new();
        let mut curr_user_key: Option<Vec<u8>> = None;
        let mut kept_snapshot_version = false;

        for (enc_k, enc_v) in all {
            let (u_key, ts) = decode_mvcc_key(&enc_k)?;

            match &curr_user_key {
                Some(prev) if prev.as_slice() == u_key => {}
                _ => {
                    curr_user_key = Some(u_key.to_vec());
                    kept_snapshot_version = false;
                }
            }

            if ts > min_active {
                // Must keep: newer than active snapshot
                continue;
            }

            if !kept_snapshot_version {
                // Keep the single newest version ≤ min_active
                kept_snapshot_version = true;
                // If this newest version is a tombstone and no snapshot needs it, drop it
                if decode_mvcc_val(&enc_v)? == None {
                    keys_to_delete.push(enc_k);
                }
            } else {
                // Any older versions below min_active are unreachable by any current or future transaction!
                keys_to_delete.push(enc_k);
            }
        }

        let deleted_count = keys_to_delete.len();
        for k in keys_to_delete {
            self.btree.delete(&k)?;
        }

        Ok(deleted_count)
    }
}
