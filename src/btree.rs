use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use crate::bufpool::BufPool;
use crate::page::{Error, PageId, PageType, Result, SlottedNode};

#[derive(Debug, Clone)]
pub struct TreeStats {
    pub height: usize,
    pub leaf_pages: usize,
    pub internal_pages: usize,
    pub total_keys: usize,
    pub fill_factor: f64,
}

pub struct BTree {
    bufpool: Arc<BufPool>,
    root_page_id: AtomicU32,
    next_page_id: AtomicU32,
}

impl BTree {
    pub fn new(bufpool: Arc<BufPool>, root_page_id: PageId, next_page_id: PageId) -> Self {
        Self {
            bufpool,
            root_page_id: AtomicU32::new(root_page_id),
            next_page_id: AtomicU32::new(next_page_id),
        }
    }

    pub fn root_page_id(&self) -> PageId {
        self.root_page_id.load(Ordering::SeqCst)
    }

    pub fn next_page_id(&self) -> PageId {
        self.next_page_id.load(Ordering::SeqCst)
    }

    pub fn set_root_page_id(&self, root: PageId) {
        self.root_page_id.store(root, Ordering::SeqCst);
    }

    fn allocate_page(&self, ptype: PageType) -> Result<PageId> {
        let new_id = self.next_page_id.fetch_add(1, Ordering::SeqCst);
        let mut guard = self.bufpool.fetch_page_mut(new_id)?;
        guard.with_page_mut(|p| {
            SlottedNode::new(p, new_id, ptype);
        });
        Ok(new_id)
    }

    /// Point lookup
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let root = self.root_page_id();
        let mut curr_id = root;

        loop {
            let guard = self.bufpool.fetch_page(curr_id)?;
            let page = guard.clone_page();
            let mut page_clone = page;
            let node = SlottedNode::from_page(&mut page_clone);

            match node.page.page_type()? {
                PageType::Leaf => {
                    let count = node.num_slots();
                    for i in 0..count {
                        let (k, v) = node.get_leaf_cell(i)?;
                        match k.cmp(key) {
                            std::cmp::Ordering::Equal => return Ok(Some(v.to_vec())),
                            std::cmp::Ordering::Greater => return Ok(None),
                            std::cmp::Ordering::Less => {}
                        }
                    }
                    return Ok(None);
                }
                PageType::Internal => {
                    let count = node.num_slots();
                    let mut next_id = node.rightmost_child();
                    for i in 0..count {
                        let (child, sep_key) = node.get_internal_cell(i)?;
                        if key < sep_key {
                            next_id = child;
                            break;
                        }
                    }
                    curr_id = next_id;
                }
                _ => return Err(Error::Corruption("Invalid node page type".into())),
            }
        }
    }

    /// Insert a key-value pair. Handles splits recursively.
    pub fn insert(&self, key: &[u8], val: &[u8]) -> Result<()> {
        let root = self.root_page_id();
        if let Some((sep_key, right_child)) = self.insert_into_node(root, key, val)? {
            // Root split: allocate new root
            let new_root = self.allocate_page(PageType::Internal)?;
            let mut guard = self.bufpool.fetch_page_mut(new_root)?;
            guard.with_page_mut(|p| {
                let mut node = SlottedNode::from_page(p);
                node.set_rightmost_child(right_child);
                node.insert_internal_cell(0, root, &sep_key)?;
                node.page.update_crc();
                Ok::<(), Error>(())
            })?;
            self.set_root_page_id(new_root);
        }
        Ok(())
    }

    /// Delete a key from the leaf node (MVP deletes without merge/rebalance)
    pub fn delete(&self, key: &[u8]) -> Result<bool> {
        let root = self.root_page_id();
        let mut curr_id = root;

        loop {
            let ptype = {
                let guard = self.bufpool.fetch_page(curr_id)?;
                guard.with_page(|p| p.page_type())?
            };

            match ptype {
                PageType::Leaf => {
                    let mut guard = self.bufpool.fetch_page_mut(curr_id)?;
                    return guard.with_page_mut(|p| {
                        let mut node = SlottedNode::from_page(p);
                        let count = node.num_slots();
                        for i in 0..count {
                            let (k, _) = node.get_leaf_cell(i)?;
                            if k == key {
                                node.delete_slot(i)?;
                                node.page.update_crc();
                                return Ok(true);
                            }
                        }
                        Ok(false)
                    });
                }
                PageType::Internal => {
                    let guard = self.bufpool.fetch_page(curr_id)?;
                    let page_clone = guard.clone_page();
                    let mut p = page_clone;
                    let node = SlottedNode::from_page(&mut p);
                    let count = node.num_slots();
                    let mut next_id = node.rightmost_child();
                    for i in 0..count {
                        let (child, sep_key) = node.get_internal_cell(i)?;
                        if key < sep_key {
                            next_id = child;
                            break;
                        }
                    }
                    curr_id = next_id;
                }
                _ => return Err(Error::Corruption("Invalid node page type".into())),
            }
        }
    }

    /// Range scan through leaf sibling chain
    pub fn scan<R>(&self, range: R) -> Result<Vec<(Vec<u8>, Vec<u8>)>>
    where
        R: std::ops::RangeBounds<[u8]>,
    {
        let start_key: Option<&[u8]> = match range.start_bound() {
            std::ops::Bound::Included(b) => Some(b),
            std::ops::Bound::Excluded(b) => Some(b),
            std::ops::Bound::Unbounded => None,
        };

        // Find leaf page containing start_key (or first leaf)
        let root = self.root_page_id();
        let mut curr_id = root;

        loop {
            let ptype = {
                let guard = self.bufpool.fetch_page(curr_id)?;
                guard.with_page(|p| p.page_type())?
            };

            if ptype == PageType::Leaf {
                break;
            }

            let guard = self.bufpool.fetch_page(curr_id)?;
            let page_clone = guard.clone_page();
            let mut p = page_clone;
            let node = SlottedNode::from_page(&mut p);
            let count = node.num_slots();

            if let Some(target) = start_key {
                let mut next_id = node.rightmost_child();
                for i in 0..count {
                    let (child, sep_key) = node.get_internal_cell(i)?;
                    if target < sep_key {
                        next_id = child;
                        break;
                    }
                }
                curr_id = next_id;
            } else {
                // Unbounded start: follow leftmost child
                if count > 0 {
                    let (child, _) = node.get_internal_cell(0)?;
                    curr_id = child;
                } else {
                    curr_id = node.rightmost_child();
                }
            }
        }

        let mut results = Vec::new();
        let mut leaf_id = curr_id;

        while leaf_id != 0 {
            let guard = self.bufpool.fetch_page(leaf_id)?;
            let page_clone = guard.clone_page();
            let mut p = page_clone;
            let node = SlottedNode::from_page(&mut p);
            let count = node.num_slots();
            let next_sib = node.next_sibling();

            for i in 0..count {
                let (k, v) = node.get_leaf_cell(i)?;
                if !range.contains(k) {
                    // Check if we exceeded the upper bound
                    match range.end_bound() {
                        std::ops::Bound::Included(upper) if k > upper => return Ok(results),
                        std::ops::Bound::Excluded(upper) if k >= upper => return Ok(results),
                        _ => {}
                    }
                } else {
                    results.push((k.to_vec(), v.to_vec()));
                }
            }

            leaf_id = next_sib;
        }

        Ok(results)
    }

    /// Check tree invariants:
    /// - keys are sorted within each node
    /// - every child's keys lie within the parent's separator bounds
    /// - all leaves are at the same depth
    /// - the sibling chain equals the in-order leaf traversal
    /// - there are no cycles
    /// - every page's CRC is valid
    pub fn check_invariants(&self) -> Result<TreeStats> {
        let root = self.root_page_id();
        let mut visited = std::collections::HashSet::new();
        let mut leaf_depths = Vec::new();
        let mut in_order_leaves = Vec::new();
        let mut total_keys = 0;
        let mut leaf_pages = 0;
        let mut internal_pages = 0;
        let mut total_free_space = 0;

        self.validate_subtree(
            root,
            None,
            None,
            0,
            &mut visited,
            &mut leaf_depths,
            &mut in_order_leaves,
            &mut total_keys,
            &mut leaf_pages,
            &mut internal_pages,
            &mut total_free_space,
        )?;

        // All leaves must have the same depth
        if let Some(&first_depth) = leaf_depths.first() {
            for &depth in &leaf_depths {
                if depth != first_depth {
                    return Err(Error::Corruption(format!(
                        "Inconsistent leaf depths: {} vs {}",
                        first_depth, depth
                    )));
                }
            }
        }

        // Validate sibling chain against in-order leaf traversal
        let mut chain_leaves = Vec::new();
        if let Some(&first_leaf) = in_order_leaves.first() {
            let mut curr = first_leaf;
            let mut prev = 0;
            while curr != 0 {
                chain_leaves.push(curr);
                let guard = self.bufpool.fetch_page(curr)?;
                let page_clone = guard.clone_page();
                let mut p = page_clone;
                let node = SlottedNode::from_page(&mut p);
                if node.prev_sibling() != prev {
                    return Err(Error::Corruption(format!(
                        "Invalid prev sibling pointer on page {}: expected {}, got {}",
                        curr,
                        prev,
                        node.prev_sibling()
                    )));
                }
                prev = curr;
                curr = node.next_sibling();
            }
        }

        if in_order_leaves != chain_leaves {
            return Err(Error::Corruption(
                "Leaf sibling chain does not match in-order traversal".into(),
            ));
        }

        let total_nodes = leaf_pages + internal_pages;
        let height = leaf_depths.first().map(|d| d + 1).unwrap_or(1);
        let fill_factor = if total_nodes > 0 {
            1.0 - (total_free_space as f64 / ((total_nodes * crate::page::PAGE_SIZE) as f64))
        } else {
            1.0
        };

        Ok(TreeStats {
            height,
            leaf_pages,
            internal_pages,
            total_keys,
            fill_factor,
        })
    }

    fn validate_subtree(
        &self,
        page_id: PageId,
        lower: Option<&[u8]>,
        upper: Option<&[u8]>,
        depth: usize,
        visited: &mut std::collections::HashSet<PageId>,
        leaf_depths: &mut Vec<usize>,
        in_order_leaves: &mut Vec<PageId>,
        total_keys: &mut usize,
        leaf_pages: &mut usize,
        internal_pages: &mut usize,
        total_free_space: &mut usize,
    ) -> Result<()> {
        if !visited.insert(page_id) {
            return Err(Error::Corruption(format!("Cycle detected at page {}", page_id)));
        }

        let guard = self.bufpool.fetch_page(page_id)?;
        let page_clone = guard.clone_page();
        page_clone.verify_crc()?;
        let mut p = page_clone;
        let node = SlottedNode::from_page(&mut p);
        *total_free_space += node.available_free_space();

        match node.page.page_type()? {
            PageType::Leaf => {
                *leaf_pages += 1;
                leaf_depths.push(depth);
                in_order_leaves.push(page_id);
                let count = node.num_slots();
                *total_keys += count as usize;

                let mut prev_k: Option<Vec<u8>> = None;
                for i in 0..count {
                    let (k, _) = node.get_leaf_cell(i)?;
                    if let Some(ref prev) = prev_k {
                        if k <= prev.as_slice() {
                            return Err(Error::Corruption(format!(
                                "Leaf page {} keys out of order",
                                page_id
                            )));
                        }
                    }
                    if let Some(low) = lower {
                        if k < low {
                            return Err(Error::Corruption(format!(
                                "Leaf page {} key {:?} below lower bound {:?}",
                                page_id, k, low
                            )));
                        }
                    }
                    if let Some(up) = upper {
                        if k >= up {
                            return Err(Error::Corruption(format!(
                                "Leaf page {} key {:?} exceeds upper bound {:?}",
                                page_id, k, up
                            )));
                        }
                    }
                    prev_k = Some(k.to_vec());
                }
            }
            PageType::Internal => {
                *internal_pages += 1;
                let count = node.num_slots();
                let mut prev_sep: Option<Vec<u8>> = None;
                let mut children = Vec::new();

                for i in 0..count {
                    let (child, sep_key) = node.get_internal_cell(i)?;
                    if let Some(ref prev) = prev_sep {
                        if sep_key <= prev.as_slice() {
                            return Err(Error::Corruption(format!(
                                "Internal page {} separator keys out of order",
                                page_id
                            )));
                        }
                    }
                    children.push((child, sep_key.to_vec()));
                    prev_sep = Some(sep_key.to_vec());
                }

                let rightmost = node.rightmost_child();

                // Recurse into children
                for i in 0..children.len() {
                    let child_id = children[i].0;
                    let child_low = if i == 0 {
                        lower
                    } else {
                        Some(children[i - 1].1.as_slice())
                    };
                    let child_up = Some(children[i].1.as_slice());

                    self.validate_subtree(
                        child_id,
                        child_low,
                        child_up,
                        depth + 1,
                        visited,
                        leaf_depths,
                        in_order_leaves,
                        total_keys,
                        leaf_pages,
                        internal_pages,
                        total_free_space,
                    )?;
                }

                // Recurse into rightmost child
                let rightmost_low = if children.is_empty() {
                    lower
                } else {
                    Some(children.last().map(|c| c.1.as_slice()).unwrap_or(&[]))
                };
                self.validate_subtree(
                    rightmost,
                    rightmost_low,
                    upper,
                    depth + 1,
                    visited,
                    leaf_depths,
                    in_order_leaves,
                    total_keys,
                    leaf_pages,
                    internal_pages,
                    total_free_space,
                )?;
            }
            _ => return Err(Error::Corruption("Invalid tree node type".into())),
        }

        Ok(())
    }

    /// Insert helper returning (split_key, new_right_page_id) if node split
    fn insert_into_node(
        &self,
        page_id: PageId,
        key: &[u8],
        val: &[u8],
    ) -> Result<Option<(Vec<u8>, PageId)>> {
        let ptype = {
            let guard = self.bufpool.fetch_page(page_id)?;
            guard.with_page(|p| p.page_type())?
        };

        match ptype {
            PageType::Leaf => self.insert_into_leaf(page_id, key, val),
            PageType::Internal => {
                let (next_child, sep_slot) = {
                    let guard = self.bufpool.fetch_page(page_id)?;
                    let p = guard.clone_page();
                    let mut p_mut = p;
                    let node = SlottedNode::from_page(&mut p_mut);
                    let count = node.num_slots();
                    let mut child = node.rightmost_child();
                    let mut slot = count;
                    for i in 0..count {
                        let (c, sep) = node.get_internal_cell(i)?;
                        if key < sep {
                            child = c;
                            slot = i;
                            break;
                        }
                    }
                    (child, slot)
                };

                if let Some((sep_key, right_child)) = self.insert_into_node(next_child, key, val)?
                {
                    self.insert_into_internal(page_id, sep_slot, &sep_key, right_child)
                } else {
                    Ok(None)
                }
            }
            _ => Err(Error::Corruption("Invalid node type during insert".into())),
        }
    }

    fn insert_into_leaf(
        &self,
        page_id: PageId,
        key: &[u8],
        val: &[u8],
    ) -> Result<Option<(Vec<u8>, PageId)>> {
        let mut guard = self.bufpool.fetch_page_mut(page_id)?;
        let (_num_slots, slot_idx, update_existing) = guard.with_page_mut(|p| {
            let node = SlottedNode::from_page(p);
            let count = node.num_slots();
            for i in 0..count {
                let (k, _) = node.get_leaf_cell(i)?;
                match k.cmp(key) {
                    std::cmp::Ordering::Equal => return Ok::<_, Error>((count, i, true)),
                    std::cmp::Ordering::Greater => return Ok::<_, Error>((count, i, false)),
                    std::cmp::Ordering::Less => {}
                }
            }
            Ok::<_, Error>((count, count, false))
        })?;

        if update_existing {
            // Delete old slot, insert new
            guard.with_page_mut(|p| {
                let mut node = SlottedNode::from_page(p);
                node.delete_slot(slot_idx)?;
                node.insert_leaf_cell(slot_idx, key, val)?;
                node.page.update_crc();
                Ok::<(), Error>(())
            })?;
            return Ok(None);
        }

        // Try direct insert
        let insert_res = guard.with_page_mut(|p| {
            let mut node = SlottedNode::from_page(p);
            node.insert_leaf_cell(slot_idx, key, val)
        });

        match insert_res {
            Ok(_) => {
                guard.with_page_mut(|p| p.update_crc());
                Ok(None)
            }
            Err(Error::PageFull { .. }) => {
                // Must split leaf page
                drop(guard);
                self.split_leaf(page_id, slot_idx, key, val)
            }
            Err(e) => Err(e),
        }
    }

    fn split_leaf(
        &self,
        left_page_id: PageId,
        insert_slot: u16,
        key: &[u8],
        val: &[u8],
    ) -> Result<Option<(Vec<u8>, PageId)>> {
        // Collect all cells including new one
        let mut cells = Vec::new();
        {
            let guard = self.bufpool.fetch_page(left_page_id)?;
            let p = guard.clone_page();
            let mut p_mut = p;
            let node = SlottedNode::from_page(&mut p_mut);
            let count = node.num_slots();
            for i in 0..count {
                let (k, v) = node.get_leaf_cell(i)?;
                cells.push((k.to_vec(), v.to_vec()));
            }
        }

        cells.insert(insert_slot as usize, (key.to_vec(), val.to_vec()));

        let mid = cells.len() / 2;
        let right_page_id = self.allocate_page(PageType::Leaf)?;

        // Update sibling pointers and contents
        let (_old_prev_sib, old_next_sib) = {
            let mut left_guard = self.bufpool.fetch_page_mut(left_page_id)?;
            left_guard.with_page_mut(|p| {
                let old_prev = SlottedNode::from_page(p).prev_sibling();
                let old_next = SlottedNode::from_page(p).next_sibling();
                // Reset left page
                let mut node = SlottedNode::new(p, left_page_id, PageType::Leaf);
                node.set_prev_sibling(old_prev);
                node.set_next_sibling(right_page_id);
                for (i, (k, v)) in cells[..mid].iter().enumerate() {
                    node.insert_leaf_cell(i as u16, k, v)?;
                }
                node.page.update_crc();
                Ok::<(PageId, PageId), Error>((old_prev, old_next))
            })?
        };

        {
            let mut right_guard = self.bufpool.fetch_page_mut(right_page_id)?;
            right_guard.with_page_mut(|p| {
                let mut node = SlottedNode::new(p, right_page_id, PageType::Leaf);
                node.set_prev_sibling(left_page_id);
                node.set_next_sibling(old_next_sib);
                for (i, (k, v)) in cells[mid..].iter().enumerate() {
                    node.insert_leaf_cell(i as u16, k, v)?;
                }
                node.page.update_crc();
                Ok::<(), Error>(())
            })?;
        }

        if old_next_sib != 0 {
            let mut next_guard = self.bufpool.fetch_page_mut(old_next_sib)?;
            next_guard.with_page_mut(|p| {
                let mut node = SlottedNode::from_page(p);
                node.set_prev_sibling(right_page_id);
                node.page.update_crc();
            });
        }

        let split_key = cells[mid].0.clone();
        Ok(Some((split_key, right_page_id)))
    }

    fn insert_into_internal(
        &self,
        page_id: PageId,
        slot_idx: u16,
        sep_key: &[u8],
        right_child: PageId,
    ) -> Result<Option<(Vec<u8>, PageId)>> {
        let mut guard = self.bufpool.fetch_page_mut(page_id)?;

        let insert_res = guard.with_page_mut(|p| {
            let mut node = SlottedNode::from_page(p);
            // Replace the rightmost or child pointer appropriately
            if slot_idx == node.num_slots() {
                // The separator key separates the old rightmost child and right_child
                let old_right = node.rightmost_child();
                node.insert_internal_cell(slot_idx, old_right, sep_key)?;
                node.set_rightmost_child(right_child);
            } else {
                let (old_child, _) = node.get_internal_cell(slot_idx)?;
                node.insert_internal_cell(slot_idx, old_child, sep_key)?;
                // The cell at slot_idx + 1 now points to right_child
                let next_offset = node.get_slot(slot_idx + 1)?.0;
                node.page.data[next_offset as usize..next_offset as usize + 4]
                    .copy_from_slice(&right_child.to_le_bytes());
            }
            node.page.update_crc();
            Ok::<(), Error>(())
        });

        match insert_res {
            Ok(_) => Ok(None),
            Err(Error::PageFull { .. }) => {
                drop(guard);
                self.split_internal(page_id, slot_idx, sep_key, right_child)
            }
            Err(e) => Err(e),
        }
    }

    fn split_internal(
        &self,
        left_page_id: PageId,
        insert_slot: u16,
        sep_key: &[u8],
        right_child: PageId,
    ) -> Result<Option<(Vec<u8>, PageId)>> {
        // Collect all (child, key) entries and rightmost child
        let mut entries: Vec<(PageId, Vec<u8>)> = Vec::new();
        let rightmost;
        {
            let guard = self.bufpool.fetch_page(left_page_id)?;
            let p = guard.clone_page();
            let mut p_mut = p;
            let node = SlottedNode::from_page(&mut p_mut);
            let count = node.num_slots();
            for i in 0..count {
                let (c, k) = node.get_internal_cell(i)?;
                entries.push((c, k.to_vec()));
            }
            rightmost = node.rightmost_child();
        }

        if insert_slot == entries.len() as u16 {
            entries.push((rightmost, sep_key.to_vec()));
        } else {
            let old_child = entries[insert_slot as usize].0;
            entries.insert(insert_slot as usize, (old_child, sep_key.to_vec()));
            entries[insert_slot as usize + 1].0 = right_child;
        }

        let mid = entries.len() / 2;
        let promote_key = entries[mid].1.clone();
        let right_page_id = self.allocate_page(PageType::Internal)?;

        // Left gets 0..mid, its rightmost child is entries[mid].0
        {
            let mut left_guard = self.bufpool.fetch_page_mut(left_page_id)?;
            left_guard.with_page_mut(|p| {
                let mut node = SlottedNode::new(p, left_page_id, PageType::Internal);
                node.set_rightmost_child(entries[mid].0);
                for (i, (c, k)) in entries[..mid].iter().enumerate() {
                    node.insert_internal_cell(i as u16, *c, k)?;
                }
                node.page.update_crc();
                Ok::<(), Error>(())
            })?;
        }

        // Right gets (mid+1).., its rightmost child is rightmost (or right_child if at the end)
        let right_final_child = if insert_slot == (entries.len() - 1) as u16 {
            right_child
        } else {
            rightmost
        };

        {
            let mut right_guard = self.bufpool.fetch_page_mut(right_page_id)?;
            right_guard.with_page_mut(|p| {
                let mut node = SlottedNode::new(p, right_page_id, PageType::Internal);
                node.set_rightmost_child(right_final_child);
                for (i, (c, k)) in entries[mid + 1..].iter().enumerate() {
                    node.insert_internal_cell(i as u16, *c, k)?;
                }
                node.page.update_crc();
                Ok::<(), Error>(())
            })?;
        }

        Ok(Some((promote_key, right_page_id)))
    }
}
