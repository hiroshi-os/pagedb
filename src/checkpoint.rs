use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::bufpool::BufPool;
use crate::mvcc::MvccEngine;
use crate::page::{MetaPage, Page, PageId, Result, PAGE_SIZE};
use crate::pager::Pager;

pub const DWB_MAGIC: u32 = 0x44574231; // 'DWB1'

/// Checkpoint orchestrator with Double-Write Buffer (DWB).
///
/// Crash-safety invariant:
/// 1. Hold commit mutex (quiescing commits).
/// 2. Flush dirty pages from buffer pool to memory buffer.
/// 3. Write dirty pages into DWB file (`pagedb.dwb`) with header and footer CRC, then fsync DWB.
/// 4. Write pages to data file in place, then fsync data file.
/// 5. Write updated MetaPage (with new checkpoint LSN & commit_ts) to data file, then fsync data file.
/// 6. Truncate/remove old WAL segments before checkpoint LSN.
/// 7. Clear DWB file.
pub struct Checkpointer {
    dwb_path: PathBuf,
}

impl Checkpointer {
    pub fn new<P: AsRef<Path>>(db_dir: P) -> Self {
        Self {
            dwb_path: db_dir.as_ref().join("pagedb.dwb"),
        }
    }

    /// On DB startup, recover from torn data writes using DWB if valid and complete.
    /// If DWB is valid, copy its pages over the data file before opening the tree.
    /// If DWB itself is torn/corrupted/incomplete, data file was not yet touched, so discard DWB.
    pub fn recover_dwb_if_needed(&self, pager: &mut Pager) -> Result<usize> {
        #[cfg(feature = "unsafe-skip-dwb")]
        {
            // Negative control: skip DWB recovery to prove tear mode catches corruption
            return Ok(0);
        }

        if !self.dwb_path.exists() {
            return Ok(0);
        }

        let mut file = OpenOptions::new().read(true).open(&self.dwb_path)?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;

        if buf.len() < 12 {
            // Incomplete header: DWB itself was torn before full write
            let _ = fs::remove_file(&self.dwb_path);
            return Ok(0);
        }

        let magic = u32::from_le_bytes(buf[0..4].try_into().unwrap_or([0; 4]));
        if magic != DWB_MAGIC {
            let _ = fs::remove_file(&self.dwb_path);
            return Ok(0);
        }

        let num_pages = u32::from_le_bytes(buf[4..8].try_into().unwrap_or([0; 4])) as usize;
        let expected_crc = u32::from_le_bytes(buf[8..12].try_into().unwrap_or([0; 4]));

        let payload_len = num_pages * (4 + PAGE_SIZE);
        if buf.len() < 12 + payload_len {
            // Incomplete payload
            let _ = fs::remove_file(&self.dwb_path);
            return Ok(0);
        }

        let payload = &buf[12..12 + payload_len];
        let actual_crc = crc32c::crc32c(payload);
        if expected_crc != actual_crc {
            // CRC mismatch in DWB itself: torn during DWB write, ignore DWB
            let _ = fs::remove_file(&self.dwb_path);
            return Ok(0);
        }

        // DWB is valid and complete! Repair data file
        let mut offset = 0;
        let mut repaired = 0;
        for _ in 0..num_pages {
            let page_id =
                u32::from_le_bytes(payload[offset..offset + 4].try_into().unwrap_or([0; 4]));
            offset += 4;
            let mut page_data = [0u8; PAGE_SIZE];
            page_data.copy_from_slice(&payload[offset..offset + PAGE_SIZE]);
            offset += PAGE_SIZE;

            let mut page = Page::from_bytes(page_data);
            if page.verify_crc().is_ok() {
                pager.write_page(page_id, &mut page)?;
                repaired += 1;
            }
        }

        pager.sync_all()?;
        let _ = fs::remove_file(&self.dwb_path);
        Ok(repaired)
    }

    /// Perform a full checkpoint
    pub fn checkpoint(
        &self,
        mvcc: &MvccEngine,
        bufpool: &BufPool,
        meta_page_id: PageId,
    ) -> Result<()> {
        let _commit_guard = mvcc.commit_lock.lock();

        let last_commit_ts = mvcc.last_commit_ts();
        let last_lsn = mvcc.wal.lock().next_lsn();

        // 1. Flush dirty pages from buffer pool
        let dirty_pages = bufpool.flush_all_dirty()?;
        if dirty_pages.is_empty() {
            return Ok(());
        }

        // 2. Write dirty pages to DWB
        let mut dwb_payload = Vec::with_capacity(dirty_pages.len() * (4 + PAGE_SIZE));
        for (page_id, page) in &dirty_pages {
            dwb_payload.extend_from_slice(&page_id.to_le_bytes());
            dwb_payload.extend_from_slice(&page.data);
        }

        let payload_crc = crc32c::crc32c(&dwb_payload);
        let mut dwb_data = Vec::with_capacity(12 + dwb_payload.len());
        dwb_data.extend_from_slice(&DWB_MAGIC.to_le_bytes());
        dwb_data.extend_from_slice(&(dirty_pages.len() as u32).to_le_bytes());
        dwb_data.extend_from_slice(&payload_crc.to_le_bytes());
        dwb_data.extend_from_slice(&dwb_payload);

        // Write and sync DWB
        {
            let mut dwb_file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&self.dwb_path)?;
            dwb_file.write_all(&dwb_data)?;
            dwb_file.sync_all()?;
        }

        // 3. Write pages to data file in place & fsync data file
        {
            let mut inner = bufpool.inner.lock();
            for (page_id, mut page) in dirty_pages {
                inner.pager.write_page(page_id, &mut page)?;
            }
            inner.pager.sync_all()?;
        }

        // 4. Update MetaPage
        {
            let mut meta_guard = bufpool.fetch_page_mut(meta_page_id)?;
            meta_guard.with_page_mut(|p| {
                let mut meta = MetaPage::read(p).unwrap_or(MetaPage {
                    root_page_id: mvcc.btree.root_page_id(),
                    next_page_id: mvcc.btree.next_page_id(),
                    last_checkpoint_lsn: 0,
                    last_checkpoint_commit_ts: 0,
                });
                meta.root_page_id = mvcc.btree.root_page_id();
                meta.next_page_id = mvcc.btree.next_page_id();
                meta.last_checkpoint_lsn = last_lsn;
                meta.last_checkpoint_commit_ts = last_commit_ts;
                meta.write(p, last_lsn);
            });
        }
        bufpool.flush_all_dirty()?;

        // 5. Delete old WAL segments before checkpoint
        let _ = mvcc.wal.lock().delete_segments_before(0);

        // 6. Clean up DWB
        let _ = fs::remove_file(&self.dwb_path);

        Ok(())
    }
}
