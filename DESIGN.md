# Design Document: pagedb

## 1. Overview and Architecture
`pagedb` is an embedded B+tree storage engine engineered in safe Rust (`#![forbid(unsafe_code)]`), implementing:
- Slotted page architecture (8 KiB pages) with dynamic cell compaction
- In-memory Buffer Pool with CLOCK eviction and a strict **No-Steal** policy
- Multi-Version Concurrency Control (MVCC) with Snapshot Isolation (first-committer-wins)
- Write-Ahead Log (WAL) with redo-only recovery
- Double-Write Buffer (DWB) preventing torn page corruptions during checkpoints

## 2. Page & Memory Layout
Every page is 8,192 bytes. Page 0 is the metadata page.
```
+-------------------------------------------------------------------------+
| Common Page Header (24 Bytes)                                           |
|   0..4: PageId (u32 LE)                                                 |
|   4: PageType (u8: Meta=0, Leaf=1, Internal=2, Free=3)                  |
|   5..8: Reserved                                                        |
|   8..16: LSN (u64 LE)                                                   |
|   16..20: CRC32C (u32 LE, calculated with this field zeroed)           |
|   20..24: Reserved Flags                                                |
+-------------------------------------------------------------------------+
| Slotted Node Header (16 Bytes)                                          |
|   24..26: num_slots (u16 LE)                                            |
|   26..28: free_space_offset (u16 LE)                                    |
|   28..32: next_sibling (u32 LE)                                         |
|   32..36: prev_sibling (u32 LE)                                         |
|   36..40: rightmost_child (u32 LE)                                      |
+-------------------------------------------------------------------------+
| Slot Directory (grows UPWARDS: 40 + i * 4)                              |
|   offset: u16, length: u16                                              |
+-------------------------------------------------------------------------+
| Free Space (unallocated contiguous bytes)                               |
+-------------------------------------------------------------------------+
| Cell Heap (grows DOWNWARDS from 8192)                                   |
|   Leaf Cell: key_len (u16) | val_len (u16) | key bytes | val bytes      |
|   Internal Cell: child_id (u32) | key_len (u16) | key bytes             |
+-------------------------------------------------------------------------+
```

## 3. Buffer Pool & No-Steal Eviction
The buffer pool manages `N` frames using the CLOCK replacement algorithm.
- **No-Steal Policy**: Dirty pages are strictly prohibited from being evicted to the data file prior to a checkpoint.
- If all frames are pinned or dirty, the pool returns `Error::BufferPoolFull` rather than deadlocking or stealing dirty pages.
- When dirty pages exceed 70% of frames, an automatic checkpoint is scheduled.
- **Why No-Steal makes recovery redo-only**: Because uncommitted dirty pages never reach the data file on disk, the database never needs to undo uncommitted changes from disk during crash recovery. Hence, ARIES-style undo logging and compensation log records (CLRs) are unnecessary; recovery only needs to redo committed operations that occurred after the last checkpoint.

## 4. MVCC Key Encoding & Snapshot Isolation
User keys are mapped to internal keys using inverted commit timestamps:
```
internal_key = user_key ‖ (u64::MAX - commit_ts) (big-endian)
```
- Because timestamps are inverted, the newest version for any `user_key` sorts first in B+tree scans.
- Values are tagged: `[1, val_bytes...]` for `Put`, and `[2]` for `Tombstone`.
- When a transaction begins, it snapshots `visible_ts`. Any version with `commit_ts <= start_ts` is visible.
- Writes are buffered privately in a txn-local `BTreeMap` until commit.

### Commit Path:
1. Acquire the global commit mutex.
2. **First-committer-wins**: For each key in the local write set, scan for any existing version with `commit_ts > start_ts`. If found, abort with `Error::Conflict { key }`.
3. Assign `commit_ts = last_commit_ts + 1`.
4. Append a framed WAL commit record (`{lsn, commit_ts, writes}` with CRC32C) and `fsync()`.
5. Apply versions to the B+tree.
6. Atomically update and publish `visible_ts`.
7. Release commit mutex.

## 5. Checkpoint & Double-Write Buffer (DWB) Walkthrough
To protect against torn 8 KiB page writes (e.g. power failure during sector writes), checkpoints execute via `pagedb.dwb`:
1. Acquire commit mutex (quiescing incoming commits).
2. Collect dirty pages from buffer pool.
3. Serialize dirty pages + updated MetaPage to `pagedb.dwb` with header magic and CRC32C; execute `fsync()` on DWB file.
4. Overwrite pages in place in `pagedb.data`; execute `fsync()` on data file.
5. Delete WAL segments older than the checkpoint.
6. Release commit mutex.

### Crash Point Analysis:
- **Crash before DWB fsync (Step 3)**: The DWB file is either missing, incomplete, or fails CRC validation. The data file was untouched. Recovery ignores DWB and replays WAL.
- **Crash after DWB fsync but before data file fsync completes (Step 4)**: The data file may contain torn pages (invalid page CRC). Recovery reads `pagedb.dwb`, verifies DWB CRC, and copies valid pages over `pagedb.data`, repairing the torn sectors before opening the engine.
- **Crash after data file fsync (Step 4+)**: Data file is intact. Recovery reads MetaPage and replays any subsequent WAL commit records.

## 6. What Broke During Development & Fixes
1. **Leaf split sibling chain pointer**: When resetting a leaf node during a split, the `prev_sibling` pointer was initially zeroed out instead of preserved from the original leaf. This caused an invariant failure during sibling chain traversal in `check_invariants()`, which was caught by the invariant checker and fixed.
2. **Slice bounds in range scan**: Dynamic slice range bounds (`RangeBounds<&'b [u8]>`) required precise lifetime propagation across `BTree::scan` and `Txn::scan` to avoid unsized `[u8]` compilation errors.
3. **Negative control detection**: The torn-write simulator initially selected pages at random across the data file even when no checkpoint had executed. It was refined to target pages present in the DWB, proving that without DWB repair (`--features unsafe-skip-dwb`), 10/20 runs catch hard CRC corruption.
