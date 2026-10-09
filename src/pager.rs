use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::page::{Error, Page, PageId, Result, PAGE_SIZE};

pub struct Pager {
    path: PathBuf,
    file: File,
}

impl Pager {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path_buf = path.as_ref().to_path_buf();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path_buf)?;

        Ok(Self {
            path: path_buf,
            file,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn file_len(&self) -> Result<u64> {
        Ok(self.file.metadata()?.len())
    }

    pub fn total_pages(&self) -> Result<u32> {
        let len = self.file_len()?;
        Ok((len / (PAGE_SIZE as u64)) as u32)
    }

    pub fn read_page(&mut self, page_id: PageId) -> Result<Page> {
        let offset = (page_id as u64) * (PAGE_SIZE as u64);
        let len = self.file_len()?;
        if offset + (PAGE_SIZE as u64) > len {
            return Err(Error::Corruption(format!(
                "Page {} beyond file length {}",
                page_id, len
            )));
        }

        self.file.seek(SeekFrom::Start(offset))?;
        let mut page = Page::new();
        self.file.read_exact(&mut page.data)?;
        page.verify_crc()?;
        Ok(page)
    }

    pub fn write_page(&mut self, page_id: PageId, page: &mut Page) -> Result<()> {
        page.set_page_id(page_id);
        page.update_crc();
        let offset = (page_id as u64) * (PAGE_SIZE as u64);
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(&page.data)?;
        Ok(())
    }

    pub fn sync_all(&mut self) -> Result<()> {
        self.file.sync_all()?;
        Ok(())
    }
}
