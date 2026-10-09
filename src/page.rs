use thiserror::Error;

pub type PageId = u32;
pub type Lsn = u64;

pub const PAGE_SIZE: usize = 8192;
pub const PAGE_HEADER_SIZE: usize = 24;
pub const MAX_KEY_SIZE: usize = 512;
pub const MAX_VALUE_SIZE: usize = PAGE_SIZE / 4; // 2048

#[derive(Error, Debug)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Page CRC mismatch: expected {expected:#x}, got {actual:#x} on page {page_id}")]
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

    #[error("Buffer pool full (all frames pinned or dirty)")]
    BufferPoolFull,

    #[error("Transaction conflict on key: {key:?}")]
    Conflict { key: Vec<u8> },

    #[error("Database corrupted: {0}")]
    Corruption(String),

    #[error("Invalid metadata page")]
    InvalidMetaPage,
}

pub type Result<T> = std::result::Result<T, Error>;
