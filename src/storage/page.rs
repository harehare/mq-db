use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
};

use crate::error::MqdbError;

pub const PAGE_SIZE: usize = 8192;
pub const PAGE_HEADER_SIZE: usize = 16;
pub const PAGE_BODY_SIZE: usize = PAGE_SIZE - PAGE_HEADER_SIZE;

pub const PAGE_TYPE_FREE: u32 = 0;
pub(crate) const PAGE_TYPE_FILE_HEADER: u32 = 1;
pub(crate) const PAGE_TYPE_CATALOG: u32 = 2;
pub(crate) const PAGE_TYPE_BLOCK_DATA: u32 = 3;
pub(crate) const PAGE_TYPE_OVERFLOW: u32 = 4;
pub(crate) const PAGE_TYPE_INDEX: u32 = 5;
pub(crate) const PAGE_TYPE_TABLE_DATA: u32 = 6;

const FILE_MAGIC: u32 = 0x4D51_4442;
const FILE_VERSION: u32 = 4;
const CATALOG_START_PAGE: u32 = 1;

fn invalid_data(message: impl Into<String>) -> MqdbError {
    MqdbError::Storage(message.into())
}

fn file_header_body(num_pages: u32) -> [u8; PAGE_BODY_SIZE] {
    let mut body = [0u8; PAGE_BODY_SIZE];
    body[0..4].copy_from_slice(&FILE_MAGIC.to_le_bytes());
    body[4..8].copy_from_slice(&FILE_VERSION.to_le_bytes());
    body[8..12].copy_from_slice(&(PAGE_SIZE as u32).to_le_bytes());
    body[12..16].copy_from_slice(&num_pages.to_le_bytes());
    body[16..20].copy_from_slice(&CATALOG_START_PAGE.to_le_bytes());
    body
}

pub fn compute_checksum(page: &[u8; PAGE_SIZE]) -> u32 {
    let mut checksum = 0u32;
    for (index, byte) in page.iter().enumerate() {
        if (4..8).contains(&index) {
            continue;
        }
        checksum = checksum.wrapping_add(u32::from(*byte));
    }
    checksum
}

pub fn verify_checksum(page: &[u8; PAGE_SIZE]) -> bool {
    let (_, checksum, _, _) = parse_page_header(page);
    checksum == compute_checksum(page)
}

pub fn parse_page_header(page: &[u8; PAGE_SIZE]) -> (u32, u32, u32, u32) {
    let page_type = u32::from_le_bytes(page[0..4].try_into().expect("page type slice"));
    let checksum = u32::from_le_bytes(page[4..8].try_into().expect("checksum slice"));
    let page_id = u32::from_le_bytes(page[8..12].try_into().expect("page id slice"));
    let next_page = u32::from_le_bytes(page[12..16].try_into().expect("next page slice"));
    (page_type, checksum, page_id, next_page)
}

pub fn make_page(page_type: u32, page_id: u32, next_page: u32, body: &[u8]) -> [u8; PAGE_SIZE] {
    let mut page = [0u8; PAGE_SIZE];
    page[0..4].copy_from_slice(&page_type.to_le_bytes());
    page[8..12].copy_from_slice(&page_id.to_le_bytes());
    page[12..16].copy_from_slice(&next_page.to_le_bytes());

    let copy_len = body.len().min(PAGE_BODY_SIZE);
    page[PAGE_HEADER_SIZE..PAGE_HEADER_SIZE + copy_len].copy_from_slice(&body[..copy_len]);

    let checksum = compute_checksum(&page);
    page[4..8].copy_from_slice(&checksum.to_le_bytes());
    page
}

pub struct PageFile {
    file: File,
    pub num_pages: u32,
    /// `true` if `num_pages` has advanced since the header page was last
    /// written to disk; `append_page` no longer writes it eagerly.
    header_dirty: bool,
}

impl PageFile {
    pub fn create(path: &Path) -> Result<Self, MqdbError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        let mut page_file = Self {
            file,
            num_pages: 1,
            header_dirty: false,
        };
        page_file.write_file_header()?;
        Ok(page_file)
    }

    pub fn open(path: &Path) -> Result<Self, MqdbError> {
        let mut file = OpenOptions::new().read(true).write(true).open(path)?;
        let mut page = [0u8; PAGE_SIZE];
        file.seek(SeekFrom::Start(0))?;
        file.read_exact(&mut page)?;

        if !verify_checksum(&page) {
            return Err(invalid_data("invalid file header checksum"));
        }

        let (page_type, _, page_id, _) = parse_page_header(&page);
        if page_type != PAGE_TYPE_FILE_HEADER || page_id != 0 {
            return Err(invalid_data("page 0 is not a valid file header page"));
        }

        let body = &page[PAGE_HEADER_SIZE..];
        let magic = u32::from_le_bytes(body[0..4].try_into().expect("magic slice"));
        let version = u32::from_le_bytes(body[4..8].try_into().expect("version slice"));
        let page_size = u32::from_le_bytes(body[8..12].try_into().expect("page size slice"));
        let num_pages = u32::from_le_bytes(body[12..16].try_into().expect("num pages slice"));
        let catalog_start = u32::from_le_bytes(body[16..20].try_into().expect("catalog slice"));

        if magic != FILE_MAGIC {
            return Err(invalid_data("invalid MQDB magic number"));
        }
        if version != FILE_VERSION {
            return Err(invalid_data(format!(
                "unsupported file version {version} (expected {FILE_VERSION}); run `mq-db index` to recreate the store"
            )));
        }
        if page_size != PAGE_SIZE as u32 {
            return Err(invalid_data(format!("unsupported page size: {page_size}")));
        }
        if catalog_start != CATALOG_START_PAGE {
            return Err(invalid_data(format!(
                "unexpected catalog start page: {catalog_start}"
            )));
        }
        if num_pages == 0 {
            return Err(invalid_data("invalid page count 0 in file header"));
        }

        let file_len = file.metadata()?.len();
        if file_len % PAGE_SIZE as u64 != 0 {
            return Err(invalid_data(
                "database file size is not aligned to page size",
            ));
        }
        if file_len < u64::from(num_pages) * PAGE_SIZE as u64 {
            return Err(invalid_data(
                "database file is shorter than file header page count",
            ));
        }

        Ok(Self {
            file,
            num_pages,
            header_dirty: false,
        })
    }

    pub fn read_page(&mut self, page_id: u32) -> Result<[u8; PAGE_SIZE], MqdbError> {
        if page_id >= self.num_pages {
            return Err(invalid_data(format!("page out of bounds: {page_id}")));
        }

        let mut page = [0u8; PAGE_SIZE];
        self.file
            .seek(SeekFrom::Start(u64::from(page_id) * PAGE_SIZE as u64))?;
        self.file.read_exact(&mut page)?;

        if !verify_checksum(&page) {
            return Err(invalid_data(format!(
                "checksum mismatch for page {page_id}"
            )));
        }

        let (_, _, stored_page_id, _) = parse_page_header(&page);
        if stored_page_id != page_id {
            return Err(invalid_data(format!(
                "page header id mismatch: expected {page_id}, found {stored_page_id}"
            )));
        }

        Ok(page)
    }

    pub fn write_page(&mut self, page_id: u32, data: &[u8; PAGE_SIZE]) -> Result<(), MqdbError> {
        if page_id >= self.num_pages {
            return Err(invalid_data(format!("page out of bounds: {page_id}")));
        }

        self.file
            .seek(SeekFrom::Start(u64::from(page_id) * PAGE_SIZE as u64))?;
        self.file.write_all(data)?;
        self.file.flush()?;
        Ok(())
    }

    pub fn append_page(&mut self, data: &[u8; PAGE_SIZE]) -> Result<u32, MqdbError> {
        let page_id = self.num_pages;
        self.file.seek(SeekFrom::End(0))?;
        self.file.write_all(data)?;
        self.num_pages = self
            .num_pages
            .checked_add(1)
            .ok_or_else(|| invalid_data("page count overflow"))?;
        self.header_dirty = true;
        Ok(page_id)
    }

    /// Persists `num_pages` if it changed since the last write. Must run
    /// before the file is reopened from disk (see `Storage::flush_catalog`).
    pub fn sync_header(&mut self) -> Result<(), MqdbError> {
        if self.header_dirty {
            self.write_file_header()?;
            self.header_dirty = false;
        }
        Ok(())
    }

    fn write_file_header(&mut self) -> Result<(), MqdbError> {
        let body = file_header_body(self.num_pages);
        let page = make_page(PAGE_TYPE_FILE_HEADER, 0, 0, &body);
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&page)?;
        self.file.flush()?;
        Ok(())
    }
}
