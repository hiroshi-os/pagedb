use thiserror::Error;

pub type PageId = u32;
pub type Lsn = u64;

pub const PAGE_SIZE: usize = 8192;
pub const PAGE_HEADER_SIZE: usize = 24;
pub const META_PAGE_ID: PageId = 0;
pub const META_MAGIC: u32 = 0x50414745; // 'PAGE'
pub const META_VERSION: u32 = 1;

pub const MAX_KEY_SIZE: usize = 512;
pub const MAX_VALUE_SIZE: usize = PAGE_SIZE / 4; // 2048

#[derive(Error, Debug, PartialEq, Eq)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(String),

    #[error("Page CRC mismatch: expected {expected:#010x}, got {actual:#010x} on page {page_id}")]
    CrcMismatch {
        page_id: PageId,
        expected: u32,
        actual: u32,
    },

    #[error("Invalid page type: {0}")]
    InvalidPageType(u8),

    #[error("Value too large: size {size} exceeds max {max}")]
    ValueTooLarge { size: usize, max: usize },

    #[error("Key too large: size {size} exceeds max {max}")]
    KeyTooLarge { size: usize, max: usize },

    #[error("Page is full: required {required} bytes, available {available}")]
    PageFull { required: usize, available: usize },

    #[error("Slot index out of bounds: index {index}, count {count}")]
    SlotOutOfBounds { index: u16, count: u16 },

    #[error("Buffer pool full (all frames pinned or dirty)")]
    BufferPoolFull,

    #[error("Transaction conflict on key: {key:?}")]
    Conflict { key: Vec<u8> },

    #[error("Database corrupted: {0}")]
    Corruption(String),

    #[error("Invalid metadata page")]
    InvalidMetaPage,
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Error::Io(err.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageType {
    Meta = 0,
    Leaf = 1,
    Internal = 2,
    Free = 3,
}

impl TryFrom<u8> for PageType {
    type Error = Error;
    fn try_from(val: u8) -> Result<Self> {
        match val {
            0 => Ok(PageType::Meta),
            1 => Ok(PageType::Leaf),
            2 => Ok(PageType::Internal),
            3 => Ok(PageType::Free),
            other => Err(Error::InvalidPageType(other)),
        }
    }
}

/// In-memory representation of an 8 KiB page.
#[derive(Clone, PartialEq, Eq)]
pub struct Page {
    pub data: [u8; PAGE_SIZE],
}

impl Page {
    pub fn new() -> Self {
        Self {
            data: [0u8; PAGE_SIZE],
        }
    }

    pub fn from_bytes(bytes: [u8; PAGE_SIZE]) -> Self {
        Self { data: bytes }
    }

    // Common Header layout (24 bytes):
    // 0..4:   page_id (u32 LE)
    // 4..5:   page_type (u8)
    // 5..8:   reserved (3 bytes)
    // 8..16:  lsn (u64 LE)
    // 16..20: crc32c (u32 LE, calculated with this field zeroed)
    // 20..24: reserved / flags (4 bytes)

    pub fn page_id(&self) -> PageId {
        u32::from_le_bytes(self.data[0..4].try_into().unwrap_or([0; 4]))
    }

    pub fn set_page_id(&mut self, id: PageId) {
        self.data[0..4].copy_from_slice(&id.to_le_bytes());
    }

    pub fn page_type(&self) -> Result<PageType> {
        PageType::try_from(self.data[4])
    }

    pub fn set_page_type(&mut self, ptype: PageType) {
        self.data[4] = ptype as u8;
    }

    pub fn lsn(&self) -> Lsn {
        u64::from_le_bytes(self.data[8..16].try_into().unwrap_or([0; 8]))
    }

    pub fn set_lsn(&mut self, lsn: Lsn) {
        self.data[8..16].copy_from_slice(&lsn.to_le_bytes());
    }

    pub fn stored_crc(&self) -> u32 {
        u32::from_le_bytes(self.data[16..20].try_into().unwrap_or([0; 4]))
    }

    pub fn compute_crc(&self) -> u32 {
        let mut copy = self.data;
        copy[16..20].copy_from_slice(&[0u8; 4]);
        crc32c::crc32c(&copy)
    }

    pub fn update_crc(&mut self) {
        let crc = self.compute_crc();
        self.data[16..20].copy_from_slice(&crc.to_le_bytes());
    }

    pub fn verify_crc(&self) -> Result<()> {
        let expected = self.stored_crc();
        let actual = self.compute_crc();
        if expected != actual {
            return Err(Error::CrcMismatch {
                page_id: self.page_id(),
                expected,
                actual,
            });
        }
        Ok(())
    }
}

impl Default for Page {
    fn default() -> Self {
        Self::new()
    }
}

/// Metadata page (Page 0)
/// Layout:
/// 0..24:   Standard Page Header (page_id = 0, type = Meta)
/// 24..28:  magic (u32 LE, 0x50414745)
/// 28..32:  version (u32 LE, 1)
/// 32..36:  root_page_id (u32 LE)
/// 36..40:  next_page_id (u32 LE)
/// 40..48:  last_checkpoint_lsn (u64 LE)
/// 48..56:  last_checkpoint_commit_ts (u64 LE)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetaPage {
    pub root_page_id: PageId,
    pub next_page_id: PageId,
    pub last_checkpoint_lsn: Lsn,
    pub last_checkpoint_commit_ts: u64,
}

impl MetaPage {
    pub fn init_new(page: &mut Page, root_page_id: PageId, next_page_id: PageId) {
        page.data.fill(0);
        page.set_page_id(META_PAGE_ID);
        page.set_page_type(PageType::Meta);
        page.data[24..28].copy_from_slice(&META_MAGIC.to_le_bytes());
        page.data[28..32].copy_from_slice(&META_VERSION.to_le_bytes());
        page.data[32..36].copy_from_slice(&root_page_id.to_le_bytes());
        page.data[36..40].copy_from_slice(&next_page_id.to_le_bytes());
        page.data[40..48].copy_from_slice(&0u64.to_le_bytes());
        page.data[48..56].copy_from_slice(&0u64.to_le_bytes());
        page.update_crc();
    }

    pub fn read(page: &Page) -> Result<Self> {
        page.verify_crc()?;
        if page.page_type()? != PageType::Meta {
            return Err(Error::InvalidMetaPage);
        }
        let magic = u32::from_le_bytes(page.data[24..28].try_into().unwrap_or([0; 4]));
        let version = u32::from_le_bytes(page.data[28..32].try_into().unwrap_or([0; 4]));
        if magic != META_MAGIC || version != META_VERSION {
            return Err(Error::InvalidMetaPage);
        }
        let root_page_id = u32::from_le_bytes(page.data[32..36].try_into().unwrap_or([0; 4]));
        let next_page_id = u32::from_le_bytes(page.data[36..40].try_into().unwrap_or([0; 4]));
        let last_checkpoint_lsn =
            u64::from_le_bytes(page.data[40..48].try_into().unwrap_or([0; 8]));
        let last_checkpoint_commit_ts =
            u64::from_le_bytes(page.data[48..56].try_into().unwrap_or([0; 8]));

        Ok(Self {
            root_page_id,
            next_page_id,
            last_checkpoint_lsn,
            last_checkpoint_commit_ts,
        })
    }

    pub fn write(&self, page: &mut Page, lsn: Lsn) {
        page.set_page_id(META_PAGE_ID);
        page.set_page_type(PageType::Meta);
        page.set_lsn(lsn);
        page.data[24..28].copy_from_slice(&META_MAGIC.to_le_bytes());
        page.data[28..32].copy_from_slice(&META_VERSION.to_le_bytes());
        page.data[32..36].copy_from_slice(&self.root_page_id.to_le_bytes());
        page.data[36..40].copy_from_slice(&self.next_page_id.to_le_bytes());
        page.data[40..48].copy_from_slice(&self.last_checkpoint_lsn.to_le_bytes());
        page.data[48..56].copy_from_slice(&self.last_checkpoint_commit_ts.to_le_bytes());
        page.update_crc();
    }
}

/// Slotted Node Page (Leaf or Internal)
///
/// Header (standard 24B) + Slotted Node Header (16B) = 40B total header:
/// 24..26: num_slots (u16 LE)
/// 26..28: free_space_offset (u16 LE) -> points to start of cell heap growing down from PAGE_SIZE
/// 28..32: next_sibling (u32 LE) -> PageId (for leaf chain), 0 if none
/// 32..36: prev_sibling (u32 LE) -> PageId (for leaf chain), 0 if none
/// 36..40: rightmost_child (u32 LE) -> PageId for Internal node (0 for Leaf)
///
/// Slots:
/// Slot directory starts at byte 40.
/// Each slot is 4 bytes:
/// - offset: u16 LE (offset into page where cell data starts)
/// - length: u16 LE (length of cell data)
///
/// Slot directory grows UPWARDS: 40 + i * 4
/// Cells grow DOWNWARDS: from PAGE_SIZE down towards free_space_offset
///
/// Leaf Cell layout:
/// - key_len: u16 LE
/// - val_len: u16 LE
/// - key: [u8; key_len]
/// - val: [u8; val_len]
///
/// Internal Cell layout:
/// - child_page_id: u32 LE (points to subtree where keys < cell.key)
/// - key_len: u16 LE
/// - key: [u8; key_len]
pub const SLOTTED_HEADER_SIZE: usize = 40;
pub const SLOT_SIZE: usize = 4;

pub struct SlottedNode<'a> {
    pub page: &'a mut Page,
}

impl<'a> SlottedNode<'a> {
    pub fn new(page: &'a mut Page, page_id: PageId, ptype: PageType) -> Self {
        page.data.fill(0);
        page.set_page_id(page_id);
        page.set_page_type(ptype);
        // num_slots = 0
        page.data[24..26].copy_from_slice(&0u16.to_le_bytes());
        // free_space_offset = PAGE_SIZE as u16
        page.data[26..28].copy_from_slice(&(PAGE_SIZE as u16).to_le_bytes());
        // next_sibling = 0
        page.data[28..32].copy_from_slice(&0u32.to_le_bytes());
        // prev_sibling = 0
        page.data[32..36].copy_from_slice(&0u32.to_le_bytes());
        // rightmost_child = 0
        page.data[36..40].copy_from_slice(&0u32.to_le_bytes());
        page.update_crc();
        Self { page }
    }

    pub fn from_page(page: &'a mut Page) -> Self {
        Self { page }
    }

    pub fn num_slots(&self) -> u16 {
        u16::from_le_bytes(self.page.data[24..26].try_into().unwrap_or([0; 2]))
    }

    fn set_num_slots(&mut self, count: u16) {
        self.page.data[24..26].copy_from_slice(&count.to_le_bytes());
    }

    pub fn free_space_offset(&self) -> u16 {
        u16::from_le_bytes(self.page.data[26..28].try_into().unwrap_or([0; 2]))
    }

    fn set_free_space_offset(&mut self, offset: u16) {
        self.page.data[26..28].copy_from_slice(&offset.to_le_bytes());
    }

    pub fn next_sibling(&self) -> PageId {
        u32::from_le_bytes(self.page.data[28..32].try_into().unwrap_or([0; 4]))
    }

    pub fn set_next_sibling(&mut self, id: PageId) {
        self.page.data[28..32].copy_from_slice(&id.to_le_bytes());
    }

    pub fn prev_sibling(&self) -> PageId {
        u32::from_le_bytes(self.page.data[32..36].try_into().unwrap_or([0; 4]))
    }

    pub fn set_prev_sibling(&mut self, id: PageId) {
        self.page.data[32..36].copy_from_slice(&id.to_le_bytes());
    }

    pub fn rightmost_child(&self) -> PageId {
        u32::from_le_bytes(self.page.data[36..40].try_into().unwrap_or([0; 4]))
    }

    pub fn set_rightmost_child(&mut self, id: PageId) {
        self.page.data[36..40].copy_from_slice(&id.to_le_bytes());
    }

    /// Available unfragmented contiguous free bytes between slot directory and cell heap
    pub fn available_free_space(&self) -> usize {
        let slots_end = SLOTTED_HEADER_SIZE + (self.num_slots() as usize) * SLOT_SIZE;
        let heap_start = self.free_space_offset() as usize;
        if heap_start >= slots_end {
            heap_start - slots_end
        } else {
            0
        }
    }

    pub fn get_slot(&self, index: u16) -> Result<(u16, u16)> {
        let count = self.num_slots();
        if index >= count {
            return Err(Error::SlotOutOfBounds { index, count });
        }
        let pos = SLOTTED_HEADER_SIZE + (index as usize) * SLOT_SIZE;
        let offset = u16::from_le_bytes(self.page.data[pos..pos + 2].try_into().unwrap_or([0; 2]));
        let len = u16::from_le_bytes(
            self.page.data[pos + 2..pos + 4]
                .try_into()
                .unwrap_or([0; 2]),
        );
        Ok((offset, len))
    }

    pub fn set_slot(&mut self, index: u16, offset: u16, len: u16) -> Result<()> {
        let count = self.num_slots();
        if index >= count {
            return Err(Error::SlotOutOfBounds { index, count });
        }
        let pos = SLOTTED_HEADER_SIZE + (index as usize) * SLOT_SIZE;
        self.page.data[pos..pos + 2].copy_from_slice(&offset.to_le_bytes());
        self.page.data[pos + 2..pos + 4].copy_from_slice(&len.to_le_bytes());
        Ok(())
    }

    pub fn cell_bytes(&self, index: u16) -> Result<&[u8]> {
        let (offset, len) = self.get_slot(index)?;
        let start = offset as usize;
        let end = start + (len as usize);
        if end > PAGE_SIZE || start < SLOTTED_HEADER_SIZE {
            return Err(Error::Corruption(format!(
                "Cell bounds invalid: {}..{} in page {}",
                start,
                end,
                self.page.page_id()
            )));
        }
        Ok(&self.page.data[start..end])
    }

    /// Defragment / compact cell heap when needed
    pub fn defragment(&mut self) -> Result<()> {
        let count = self.num_slots() as usize;
        let mut cells: Vec<(u16, Vec<u8>)> = Vec::with_capacity(count);
        for i in 0..count {
            let (offset, len) = self.get_slot(i as u16)?;
            let slice = &self.page.data[offset as usize..offset as usize + len as usize];
            cells.push((i as u16, slice.to_vec()));
        }

        let mut current_offset = PAGE_SIZE;
        for (i, cell_data) in cells.iter() {
            current_offset -= cell_data.len();
            self.page.data[current_offset..current_offset + cell_data.len()]
                .copy_from_slice(cell_data);
            self.set_slot(*i, current_offset as u16, cell_data.len() as u16)?;
        }

        self.set_free_space_offset(current_offset as u16);
        Ok(())
    }

    // Leaf node cell access: (key, value)
    pub fn get_leaf_cell(&self, index: u16) -> Result<(&[u8], &[u8])> {
        let cell = self.cell_bytes(index)?;
        if cell.len() < 4 {
            return Err(Error::Corruption("Leaf cell too short".into()));
        }
        let key_len = u16::from_le_bytes(cell[0..2].try_into().unwrap_or([0; 2])) as usize;
        let val_len = u16::from_le_bytes(cell[2..4].try_into().unwrap_or([0; 2])) as usize;
        if cell.len() < 4 + key_len + val_len {
            return Err(Error::Corruption("Leaf cell truncated".into()));
        }
        let key = &cell[4..4 + key_len];
        let val = &cell[4 + key_len..4 + key_len + val_len];
        Ok((key, val))
    }

    // Internal node cell access: (child_page_id, key)
    pub fn get_internal_cell(&self, index: u16) -> Result<(PageId, &[u8])> {
        let cell = self.cell_bytes(index)?;
        if cell.len() < 6 {
            return Err(Error::Corruption("Internal cell too short".into()));
        }
        let child = u32::from_le_bytes(cell[0..4].try_into().unwrap_or([0; 4]));
        let key_len = u16::from_le_bytes(cell[4..6].try_into().unwrap_or([0; 2])) as usize;
        if cell.len() < 6 + key_len {
            return Err(Error::Corruption("Internal cell truncated".into()));
        }
        let key = &cell[6..6 + key_len];
        Ok((child, key))
    }

    /// Insert a leaf cell at slot `slot_idx`. Shifts higher slots up.
    pub fn insert_leaf_cell(&mut self, slot_idx: u16, key: &[u8], val: &[u8]) -> Result<()> {
        if key.len() > MAX_KEY_SIZE {
            return Err(Error::KeyTooLarge {
                size: key.len(),
                max: MAX_KEY_SIZE,
            });
        }
        if val.len() > MAX_VALUE_SIZE {
            return Err(Error::ValueTooLarge {
                size: val.len(),
                max: MAX_VALUE_SIZE,
            });
        }
        let cell_len = 4 + key.len() + val.len();
        let total_needed = cell_len + SLOT_SIZE;

        if self.available_free_space() < total_needed {
            self.defragment()?;
            if self.available_free_space() < total_needed {
                return Err(Error::PageFull {
                    required: total_needed,
                    available: self.available_free_space(),
                });
            }
        }

        let num_slots = self.num_slots();
        if slot_idx > num_slots {
            return Err(Error::SlotOutOfBounds {
                index: slot_idx,
                count: num_slots,
            });
        }

        // Allocate in heap
        let new_offset = self.free_space_offset() - (cell_len as u16);
        let cell_start = new_offset as usize;
        self.page.data[cell_start..cell_start + 2]
            .copy_from_slice(&(key.len() as u16).to_le_bytes());
        self.page.data[cell_start + 2..cell_start + 4]
            .copy_from_slice(&(val.len() as u16).to_le_bytes());
        self.page.data[cell_start + 4..cell_start + 4 + key.len()].copy_from_slice(key);
        self.page.data[cell_start + 4 + key.len()..cell_start + cell_len].copy_from_slice(val);
        self.set_free_space_offset(new_offset);

        // Shift slots right
        let slots_start = SLOTTED_HEADER_SIZE;
        for i in (slot_idx..num_slots).rev() {
            let src = slots_start + (i as usize) * SLOT_SIZE;
            let dst = slots_start + ((i + 1) as usize) * SLOT_SIZE;
            let mut buf = [0u8; SLOT_SIZE];
            buf.copy_from_slice(&self.page.data[src..src + SLOT_SIZE]);
            self.page.data[dst..dst + SLOT_SIZE].copy_from_slice(&buf);
        }

        // Write new slot
        let new_slot_pos = slots_start + (slot_idx as usize) * SLOT_SIZE;
        self.page.data[new_slot_pos..new_slot_pos + 2].copy_from_slice(&new_offset.to_le_bytes());
        self.page.data[new_slot_pos + 2..new_slot_pos + 4]
            .copy_from_slice(&(cell_len as u16).to_le_bytes());
        self.set_num_slots(num_slots + 1);

        Ok(())
    }

    /// Insert an internal cell at slot `slot_idx`.
    pub fn insert_internal_cell(
        &mut self,
        slot_idx: u16,
        child_page_id: PageId,
        key: &[u8],
    ) -> Result<()> {
        if key.len() > MAX_KEY_SIZE {
            return Err(Error::KeyTooLarge {
                size: key.len(),
                max: MAX_KEY_SIZE,
            });
        }
        let cell_len = 6 + key.len();
        let total_needed = cell_len + SLOT_SIZE;

        if self.available_free_space() < total_needed {
            self.defragment()?;
            if self.available_free_space() < total_needed {
                return Err(Error::PageFull {
                    required: total_needed,
                    available: self.available_free_space(),
                });
            }
        }

        let num_slots = self.num_slots();
        if slot_idx > num_slots {
            return Err(Error::SlotOutOfBounds {
                index: slot_idx,
                count: num_slots,
            });
        }

        let new_offset = self.free_space_offset() - (cell_len as u16);
        let cell_start = new_offset as usize;
        self.page.data[cell_start..cell_start + 4].copy_from_slice(&child_page_id.to_le_bytes());
        self.page.data[cell_start + 4..cell_start + 6]
            .copy_from_slice(&(key.len() as u16).to_le_bytes());
        self.page.data[cell_start + 6..cell_start + cell_len].copy_from_slice(key);
        self.set_free_space_offset(new_offset);

        // Shift slots right
        let slots_start = SLOTTED_HEADER_SIZE;
        for i in (slot_idx..num_slots).rev() {
            let src = slots_start + (i as usize) * SLOT_SIZE;
            let dst = slots_start + ((i + 1) as usize) * SLOT_SIZE;
            let mut buf = [0u8; SLOT_SIZE];
            buf.copy_from_slice(&self.page.data[src..src + SLOT_SIZE]);
            self.page.data[dst..dst + SLOT_SIZE].copy_from_slice(&buf);
        }

        let new_slot_pos = slots_start + (slot_idx as usize) * SLOT_SIZE;
        self.page.data[new_slot_pos..new_slot_pos + 2].copy_from_slice(&new_offset.to_le_bytes());
        self.page.data[new_slot_pos + 2..new_slot_pos + 4]
            .copy_from_slice(&(cell_len as u16).to_le_bytes());
        self.set_num_slots(num_slots + 1);

        Ok(())
    }

    /// Delete slot at index, shifting subsequent slots down.
    pub fn delete_slot(&mut self, slot_idx: u16) -> Result<()> {
        let num_slots = self.num_slots();
        if slot_idx >= num_slots {
            return Err(Error::SlotOutOfBounds {
                index: slot_idx,
                count: num_slots,
            });
        }
        let slots_start = SLOTTED_HEADER_SIZE;
        for i in slot_idx..(num_slots - 1) {
            let src = slots_start + ((i + 1) as usize) * SLOT_SIZE;
            let dst = slots_start + (i as usize) * SLOT_SIZE;
            let mut buf = [0u8; SLOT_SIZE];
            buf.copy_from_slice(&self.page.data[src..src + SLOT_SIZE]);
            self.page.data[dst..dst + SLOT_SIZE].copy_from_slice(&buf);
        }
        self.set_num_slots(num_slots - 1);
        Ok(())
    }
}
