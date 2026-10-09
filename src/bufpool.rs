use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;

use crate::page::{Error, Page, PageId, Result};
use crate::pager::Pager;

pub struct Frame {
    pub page_id: AtomicU32,
    pub is_dirty: AtomicBool,
    pub pin_count: AtomicU32,
    pub reference_bit: AtomicBool,
    pub data: Mutex<Page>,
}

impl Frame {
    pub fn new() -> Self {
        Self {
            page_id: AtomicU32::new(0),
            is_dirty: AtomicBool::new(false),
            pin_count: AtomicU32::new(0),
            reference_bit: AtomicBool::new(false),
            data: Mutex::new(Page::new()),
        }
    }
}

impl Default for Frame {
    fn default() -> Self {
        Self::new()
    }
}

pub struct BufPoolInner {
    pub pager: Pager,
    pub frames: Vec<Arc<Frame>>,
    pub page_table: HashMap<PageId, usize>, // page_id -> frame_index
    pub clock_hand: usize,
    pub num_frames: usize,
}

pub struct BufPool {
    pub inner: Mutex<BufPoolInner>,
}

impl BufPool {
    pub fn new(pager: Pager, num_frames: usize) -> Self {
        let mut frames = Vec::with_capacity(num_frames);
        for _ in 0..num_frames {
            frames.push(Arc::new(Frame::new()));
        }
        Self {
            inner: Mutex::new(BufPoolInner {
                pager,
                frames,
                page_table: HashMap::new(),
                clock_hand: 0,
                num_frames,
            }),
        }
    }

    /// Number of dirty frames currently in the buffer pool
    pub fn dirty_count(&self) -> usize {
        let guard = self.inner.lock();
        guard
            .frames
            .iter()
            .filter(|f| f.is_dirty.load(Ordering::Acquire))
            .count()
    }

    /// Check if dirty page count exceeds 70% threshold
    pub fn needs_checkpoint(&self) -> bool {
        let guard = self.inner.lock();
        let dirty = guard
            .frames
            .iter()
            .filter(|f| f.is_dirty.load(Ordering::Acquire))
            .count();
        dirty * 100 >= guard.num_frames * 70
    }

    /// Fetch a page for reading, pinning it
    pub fn fetch_page(&self, page_id: PageId) -> Result<PageReadGuard> {
        let mut inner = self.inner.lock();

        // 1. Is it already in pool?
        if let Some(&frame_idx) = inner.page_table.get(&page_id) {
            let frame = inner.frames[frame_idx].clone();
            frame.pin_count.fetch_add(1, Ordering::SeqCst);
            frame.reference_bit.store(true, Ordering::Release);
            return Ok(PageReadGuard { frame });
        }

        // 2. Need to evict a frame using CLOCK
        let victim_idx = Self::find_victim(&mut inner)?;
        let frame = inner.frames[victim_idx].clone();

        // Read page from pager
        let disk_page = inner.pager.read_page(page_id)?;

        // Update frame data
        {
            let mut page_data = frame.data.lock();
            *page_data = disk_page;
        }

        // Update page table
        inner.page_table.insert(page_id, victim_idx);
        frame.page_id.store(page_id, Ordering::Release);
        frame.is_dirty.store(false, Ordering::Release);
        frame.reference_bit.store(true, Ordering::Release);
        frame.pin_count.store(1, Ordering::SeqCst);

        Ok(PageReadGuard { frame })
    }

    /// Fetch or allocate a page for writing, pinning it
    pub fn fetch_page_mut(&self, page_id: PageId) -> Result<PageWriteGuard> {
        let mut inner = self.inner.lock();

        if let Some(&frame_idx) = inner.page_table.get(&page_id) {
            let frame = inner.frames[frame_idx].clone();
            frame.pin_count.fetch_add(1, Ordering::SeqCst);
            frame.reference_bit.store(true, Ordering::Release);
            return Ok(PageWriteGuard { frame });
        }

        let victim_idx = Self::find_victim(&mut inner)?;
        let frame = inner.frames[victim_idx].clone();

        let total = inner.pager.total_pages()?;
        let disk_page = if page_id < total {
            inner.pager.read_page(page_id)?
        } else {
            let mut p = Page::new();
            p.set_page_id(page_id);
            p
        };

        {
            let mut page_data = frame.data.lock();
            *page_data = disk_page;
        }

        inner.page_table.insert(page_id, victim_idx);
        frame.page_id.store(page_id, Ordering::Release);
        frame.is_dirty.store(false, Ordering::Release);
        frame.reference_bit.store(true, Ordering::Release);
        frame.pin_count.store(1, Ordering::SeqCst);

        Ok(PageWriteGuard { frame })
    }

    fn find_victim(inner: &mut BufPoolInner) -> Result<usize> {
        let num_frames = inner.num_frames;
        let mut attempts = 0;

        while attempts < num_frames * 2 {
            let idx = inner.clock_hand;
            inner.clock_hand = (inner.clock_hand + 1) % num_frames;

            let frame = &inner.frames[idx];
            let is_pinned = frame.pin_count.load(Ordering::SeqCst) > 0;
            let is_dirty = frame.is_dirty.load(Ordering::Acquire);

            if !is_pinned && !is_dirty {
                if frame.reference_bit.load(Ordering::Acquire) {
                    frame.reference_bit.store(false, Ordering::Release);
                } else {
                    let old_page_id = frame.page_id.load(Ordering::Acquire);
                    inner.page_table.remove(&old_page_id);
                    return Ok(idx);
                }
            }

            attempts += 1;
        }

        Err(Error::BufferPoolFull)
    }

    /// Collect dirty pages without writing directly to disk
    pub fn collect_dirty_pages(&self) -> Vec<(PageId, Page)> {
        let inner = self.inner.lock();
        let mut dirty_pages = Vec::new();

        for frame in &inner.frames {
            if frame.is_dirty.load(Ordering::Acquire) {
                let page_clone = frame.data.lock().clone();
                dirty_pages.push((frame.page_id.load(Ordering::Acquire), page_clone));
            }
        }
        dirty_pages
    }

    pub fn mark_all_clean(&self) {
        let inner = self.inner.lock();
        for frame in &inner.frames {
            if frame.is_dirty.load(Ordering::Acquire) {
                frame.is_dirty.store(false, Ordering::Release);
            }
        }
    }

    /// Flush all dirty pages directly to disk (used during clean shutdown)
    pub fn flush_all_dirty(&self) -> Result<Vec<(PageId, Page)>> {
        let mut inner = self.inner.lock();
        let mut dirty_pages = Vec::new();

        for frame in &inner.frames {
            if frame.is_dirty.load(Ordering::Acquire) {
                let page_clone = frame.data.lock().clone();
                dirty_pages.push((frame.page_id.load(Ordering::Acquire), page_clone));
            }
        }

        for (page_id, mut page) in dirty_pages.clone() {
            inner.pager.write_page(page_id, &mut page)?;
        }
        inner.pager.sync_all()?;

        for frame in &inner.frames {
            if frame.is_dirty.load(Ordering::Acquire) {
                frame.is_dirty.store(false, Ordering::Release);
            }
        }

        Ok(dirty_pages)
    }
}

pub struct PageReadGuard {
    frame: Arc<Frame>,
}

impl PageReadGuard {
    pub fn with_page<R, F: FnOnce(&Page) -> R>(&self, f: F) -> R {
        let guard = self.frame.data.lock();
        f(&guard)
    }

    pub fn clone_page(&self) -> Page {
        self.frame.data.lock().clone()
    }
}

impl Drop for PageReadGuard {
    fn drop(&mut self) {
        self.frame.pin_count.fetch_sub(1, Ordering::SeqCst);
    }
}

pub struct PageWriteGuard {
    frame: Arc<Frame>,
}

impl PageWriteGuard {
    pub fn with_page_mut<R, F: FnOnce(&mut Page) -> R>(&mut self, f: F) -> R {
        self.frame.is_dirty.store(true, Ordering::Release);
        let mut guard = self.frame.data.lock();
        f(&mut guard)
    }

    pub fn with_page<R, F: FnOnce(&Page) -> R>(&self, f: F) -> R {
        let guard = self.frame.data.lock();
        f(&guard)
    }
}

impl Drop for PageWriteGuard {
    fn drop(&mut self) {
        self.frame.pin_count.fetch_sub(1, Ordering::SeqCst);
    }
}
