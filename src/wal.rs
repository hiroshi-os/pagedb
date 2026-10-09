use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::page::{Error, Lsn, Result};

pub const WAL_RECORD_HEADER_SIZE: usize = 28;
pub const WAL_RECORD_MAGIC: u32 = 0x57414C52; // 'WALR'

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalOpType {
    Put = 1,
    Delete = 2,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalWriteOp {
    pub op_type: WalOpType,
    pub key: Vec<u8>,
    pub val: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalCommitRecord {
    pub lsn: Lsn,
    pub commit_ts: u64,
    pub writes: Vec<WalWriteOp>,
}

impl WalCommitRecord {
    pub fn encode(&self) -> Vec<u8> {
        let mut body = Vec::new();
        // Number of operations (u32 LE)
        body.extend_from_slice(&(self.writes.len() as u32).to_le_bytes());
        for op in &self.writes {
            body.push(op.op_type as u8);
            body.extend_from_slice(&(op.key.len() as u16).to_le_bytes());
            body.extend_from_slice(&op.key);
            body.extend_from_slice(&(op.val.len() as u16).to_le_bytes());
            body.extend_from_slice(&op.val);
        }

        let body_crc = crc32c::crc32c(&body);
        let body_len = body.len() as u32;

        let mut record = Vec::with_capacity(WAL_RECORD_HEADER_SIZE + body.len());
        // Header:
        // 0..4: magic (0x57414C52)
        // 4..12: lsn (u64 LE)
        // 12..20: commit_ts (u64 LE)
        // 20..24: body_len (u32 LE)
        // 24..28: body_crc (u32 LE)
        record.extend_from_slice(&WAL_RECORD_MAGIC.to_le_bytes());
        record.extend_from_slice(&self.lsn.to_le_bytes());
        record.extend_from_slice(&self.commit_ts.to_le_bytes());
        record.extend_from_slice(&body_len.to_le_bytes());
        record.extend_from_slice(&body_crc.to_le_bytes());
        record.extend_from_slice(&body);

        record
    }

    pub fn decode(bytes: &[u8]) -> Result<(Self, usize)> {
        if bytes.len() < WAL_RECORD_HEADER_SIZE {
            return Err(Error::Corruption("WAL record header truncated".into()));
        }

        let magic = u32::from_le_bytes(bytes[0..4].try_into().unwrap_or([0; 4]));
        if magic != WAL_RECORD_MAGIC {
            return Err(Error::Corruption("Invalid WAL record magic".into()));
        }

        let lsn = u64::from_le_bytes(bytes[4..12].try_into().unwrap_or([0; 8]));
        let commit_ts = u64::from_le_bytes(bytes[12..20].try_into().unwrap_or([0; 8]));
        let body_len = u32::from_le_bytes(bytes[20..24].try_into().unwrap_or([0; 4])) as usize;
        let expected_crc = u32::from_le_bytes(bytes[24..28].try_into().unwrap_or([0; 4]));

        let total_size = WAL_RECORD_HEADER_SIZE + body_len;
        if bytes.len() < total_size {
            return Err(Error::Corruption("WAL record body truncated".into()));
        }

        let body = &bytes[WAL_RECORD_HEADER_SIZE..total_size];
        let actual_crc = crc32c::crc32c(body);
        if expected_crc != actual_crc {
            return Err(Error::Corruption(format!(
                "WAL record CRC mismatch: expected {expected_crc:#x}, got {actual_crc:#x}"
            )));
        }

        let mut offset = 0;
        if body.len() < 4 {
            return Err(Error::Corruption("WAL record empty body".into()));
        }
        let num_writes = u32::from_le_bytes(body[0..4].try_into().unwrap_or([0; 4])) as usize;
        offset += 4;

        let mut writes = Vec::with_capacity(num_writes);
        for _ in 0..num_writes {
            if offset + 1 > body.len() {
                return Err(Error::Corruption("Truncated op type".into()));
            }
            let op_type_byte = body[offset];
            offset += 1;
            let op_type = match op_type_byte {
                1 => WalOpType::Put,
                2 => WalOpType::Delete,
                other => return Err(Error::Corruption(format!("Unknown WAL op type {other}"))),
            };

            if offset + 2 > body.len() {
                return Err(Error::Corruption("Truncated key len".into()));
            }
            let klen = u16::from_le_bytes(body[offset..offset + 2].try_into().unwrap_or([0; 2]))
                as usize;
            offset += 2;

            if offset + klen > body.len() {
                return Err(Error::Corruption("Truncated key data".into()));
            }
            let key = body[offset..offset + klen].to_vec();
            offset += klen;

            if offset + 2 > body.len() {
                return Err(Error::Corruption("Truncated val len".into()));
            }
            let vlen = u16::from_le_bytes(body[offset..offset + 2].try_into().unwrap_or([0; 2]))
                as usize;
            offset += 2;

            if offset + vlen > body.len() {
                return Err(Error::Corruption("Truncated val data".into()));
            }
            let val = body[offset..offset + vlen].to_vec();
            offset += vlen;

            writes.push(WalWriteOp { op_type, key, val });
        }

        Ok((
            Self {
                lsn,
                commit_ts,
                writes,
            },
            total_size,
        ))
    }
}

pub struct WalManager {
    dir: PathBuf,
    segment_size: usize,
    current_segment_idx: u64,
    current_file: File,
    current_file_size: usize,
    next_lsn: Lsn,
}

impl WalManager {
    pub fn open<P: AsRef<Path>>(dir: P, segment_size: usize) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;

        // Find existing segments
        let mut segments = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if let Some(ext) = path.extension() {
                if ext == "wal" {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        if let Ok(idx) = stem.parse::<u64>() {
                            segments.push(idx);
                        }
                    }
                }
            }
        }
        segments.sort_unstable();

        let (current_segment_idx, current_file, current_file_size) = if let Some(&last_idx) =
            segments.last()
        {
            let seg_path = dir.join(format!("{:016}.wal", last_idx));
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&seg_path)?;
            let size = file.metadata()?.len() as usize;
            (last_idx, file, size)
        } else {
            let seg_path = dir.join(format!("{:016}.wal", 0));
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&seg_path)?;
            (0, file, 0)
        };

        Ok(Self {
            dir,
            segment_size,
            current_segment_idx,
            current_file,
            current_file_size,
            next_lsn: 1,
        })
    }

    pub fn next_lsn(&self) -> Lsn {
        self.next_lsn
    }

    pub fn set_next_lsn(&mut self, lsn: Lsn) {
        self.next_lsn = lsn;
    }

    pub fn append_record(&mut self, record: &WalCommitRecord) -> Result<Lsn> {
        let encoded = record.encode();

        if self.current_file_size + encoded.len() > self.segment_size {
            // Roll over to next segment
            self.current_file.sync_all()?;
            self.current_segment_idx += 1;
            let seg_path = self
                .dir
                .join(format!("{:016}.wal", self.current_segment_idx));
            self.current_file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&seg_path)?;
            self.current_file_size = 0;
        }

        self.current_file.seek(SeekFrom::End(0))?;
        self.current_file.write_all(&encoded)?;
        self.current_file_size += encoded.len();
        self.next_lsn = record.lsn + (encoded.len() as u64);

        Ok(self.next_lsn)
    }

    pub fn sync(&mut self) -> Result<()> {
        self.current_file.sync_all()?;
        Ok(())
    }

    /// Read all valid records across segments starting from `start_lsn`.
    /// On encounter of bad/corrupt/partial record at the tail, truncate the file there and fsync.
    pub fn recover_from(
        &mut self,
        start_lsn: Lsn,
    ) -> Result<(Vec<WalCommitRecord>, Option<Lsn>, Option<u64>)> {
        let mut segments = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if let Some(ext) = path.extension() {
                if ext == "wal" {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        if let Ok(idx) = stem.parse::<u64>() {
                            segments.push((idx, path));
                        }
                    }
                }
            }
        }
        segments.sort_by_key(|s| s.0);

        let mut valid_records = Vec::new();
        let mut max_lsn: Option<Lsn> = None;
        let mut max_commit_ts: Option<u64> = None;

        for (_idx, path) in segments {
            let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;

            let mut offset = 0;
            let mut need_truncate = false;
            let mut valid_bytes_len = 0;

            while offset < bytes.len() {
                match WalCommitRecord::decode(&bytes[offset..]) {
                    Ok((rec, rec_len)) => {
                        if rec.lsn >= start_lsn {
                            max_lsn = Some(max_lsn.unwrap_or(0).max(rec.lsn));
                            max_commit_ts =
                                Some(max_commit_ts.unwrap_or(0).max(rec.commit_ts));
                            valid_records.push(rec);
                        }
                        offset += rec_len;
                        valid_bytes_len = offset;
                    }
                    Err(_) => {
                        // Tail corruption or torn record! Stop here and truncate file
                        need_truncate = true;
                        break;
                    }
                }
            }

            if need_truncate {
                file.set_len(valid_bytes_len as u64)?;
                file.sync_all()?;
            }
        }

        Ok((valid_records, max_lsn, max_commit_ts))
    }

    /// Delete WAL segments older than `min_segment_idx`
    pub fn delete_segments_before(&self, min_segment_idx: u64) -> Result<()> {
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if let Some(ext) = path.extension() {
                if ext == "wal" {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        if let Ok(idx) = stem.parse::<u64>() {
                            if idx < min_segment_idx {
                                fs::remove_file(&path)?;
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }
}
