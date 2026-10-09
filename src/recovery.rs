use std::sync::Arc;

use parking_lot::Mutex;

use crate::btree::BTree;
use crate::bufpool::BufPool;
use crate::checkpoint::Checkpointer;
use crate::mvcc::MvccEngine;
use crate::page::{MetaPage, PageType, Result, META_PAGE_ID};
use crate::pager::Pager;
use crate::wal::WalManager;

pub struct Recovery;

impl Recovery {
    /// Crash recovery process:
    /// 1. Run DWB recovery if `pagedb.dwb` exists and is valid (repairing any torn data pages).
    /// 2. Read MetaPage from Page 0.
    /// 3. Replay WAL commit records starting from last_checkpoint_lsn.
    /// 4. Apply each commit record whose commit_ts > last_checkpoint_commit_ts to the B+tree.
    ///    Redo is idempotent because versions encode commit_ts in the key!
    /// 5. Return initialized engine with last_commit_ts and visible_ts.
    pub fn recover_and_open(
        mut pager: Pager,
        bufpool_frames: usize,
        wal_segment_bytes: usize,
        checkpointer: &Checkpointer,
    ) -> Result<Arc<MvccEngine>> {
        // Step 1: DWB repair
        let _repaired = checkpointer.recover_dwb_if_needed(&mut pager)?;

        let db_dir = pager.path().parent().unwrap_or(std::path::Path::new("."));
        let wal_dir = db_dir.join("wal");

        let total_pages = pager.total_pages()?;
        let (root_id, next_id, chk_lsn, chk_ts) = if total_pages == 0 {
            // Empty database: initialize Page 0 (MetaPage) and Page 1 (Leaf root)
            let mut meta_page = crate::page::Page::new();
            MetaPage::init_new(&mut meta_page, 1, 2);
            pager.write_page(META_PAGE_ID, &mut meta_page)?;

            let mut root_page = crate::page::Page::new();
            crate::page::SlottedNode::new(&mut root_page, 1, PageType::Leaf);
            pager.write_page(1, &mut root_page)?;
            pager.sync_all()?;

            (1, 2, 0, 0)
        } else {
            let meta_p = pager.read_page(META_PAGE_ID)?;
            let meta = MetaPage::read(&meta_p)?;
            (
                meta.root_page_id,
                meta.next_page_id,
                meta.last_checkpoint_lsn,
                meta.last_checkpoint_commit_ts,
            )
        };

        let bufpool = Arc::new(BufPool::new(pager, bufpool_frames));
        let btree = Arc::new(BTree::new(bufpool.clone(), root_id, next_id));
        let mut wal_mgr = WalManager::open(&wal_dir, wal_segment_bytes)?;

        // Step 3 & 4: Replay WAL from chk_lsn
        let (records, max_lsn, max_commit_ts) = wal_mgr.recover_from(chk_lsn)?;

        let final_commit_ts = max_commit_ts.unwrap_or(chk_ts).max(chk_ts);
        if let Some(lsn) = max_lsn {
            wal_mgr.set_next_lsn(lsn + 1);
        }

        let mvcc = Arc::new(MvccEngine::new(
            btree.clone(),
            bufpool.clone(),
            Arc::new(Mutex::new(wal_mgr)),
            final_commit_ts,
        ));

        // Replay commit records
        for rec in records {
            if rec.commit_ts > chk_ts {
                mvcc.apply_wal_commit(&rec)?;
            }
        }

        Ok(mvcc)
    }
}
