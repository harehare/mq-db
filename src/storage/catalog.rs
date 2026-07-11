use std::collections::HashSet;

use crate::{
    error::MqdbError,
    storage::page::{
        PAGE_BODY_SIZE, PAGE_HEADER_SIZE, PAGE_TYPE_CATALOG, PageFile, make_page, parse_page_header,
    },
};

#[derive(Debug, Clone, PartialEq)]
pub struct CatalogEntry {
    pub document_id: u32,
    pub path: Option<String>,
    pub first_block_page: u32,
    pub num_blocks: u32,
    pub zone_map_bytes: Vec<u8>,
    /// First page of the persisted secondary index chain. 0 = not stored.
    pub index_start_page: u32,
}

/// A user-defined table entry stored in the catalog.
///
/// Row data is *not* stored inline — it lives in its own page chain
/// (see [`crate::storage::Storage::write_table_rows`]) so that appending
/// rows only requires writing the new pages plus this small fixed-size
/// entry, instead of rewriting every previously-inserted row.
#[derive(Debug, Clone, PartialEq)]
pub struct CustomTableEntry {
    pub name: String,
    pub columns: Vec<String>,
    /// First page of the row chain. 0 = no rows persisted yet.
    pub first_row_page: u32,
    /// Last page of the row chain — new rows are appended after this page.
    pub last_row_page: u32,
    pub num_rows: u32,
}

/// Catalog entries, custom tables, and content hashes read from a page file.
pub type CatalogData = (Vec<CatalogEntry>, Vec<CustomTableEntry>, Vec<(u32, u64)>);

fn invalid_data(message: impl Into<String>) -> MqdbError {
    MqdbError::Storage(message.into())
}

fn as_u16(value: usize, field: &str) -> u16 {
    u16::try_from(value).unwrap_or_else(|_| panic!("{field} exceeds u16 range"))
}

fn as_u32(value: usize, field: &str) -> u32 {
    u32::try_from(value).unwrap_or_else(|_| panic!("{field} exceeds u32 range"))
}

struct Decoder<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Decoder<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn read_exact(&mut self, len: usize) -> Result<&'a [u8], MqdbError> {
        let end = self
            .pos
            .checked_add(len)
            .ok_or_else(|| invalid_data("byte offset overflow"))?;
        if end > self.data.len() {
            return Err(invalid_data("unexpected end of catalog data"));
        }
        let bytes = &self.data[self.pos..end];
        self.pos = end;
        Ok(bytes)
    }

    fn read_u8(&mut self) -> Result<u8, MqdbError> {
        Ok(self.read_exact(1)?[0])
    }

    fn read_u16(&mut self) -> Result<u16, MqdbError> {
        let bytes: [u8; 2] = self
            .read_exact(2)?
            .try_into()
            .map_err(|_| invalid_data("failed to read u16"))?;
        Ok(u16::from_le_bytes(bytes))
    }

    fn read_u32(&mut self) -> Result<u32, MqdbError> {
        let bytes: [u8; 4] = self
            .read_exact(4)?
            .try_into()
            .map_err(|_| invalid_data("failed to read u32"))?;
        Ok(u32::from_le_bytes(bytes))
    }

    fn read_u64(&mut self) -> Result<u64, MqdbError> {
        let bytes: [u8; 8] = self
            .read_exact(8)?
            .try_into()
            .map_err(|_| invalid_data("failed to read u64"))?;
        Ok(u64::from_le_bytes(bytes))
    }

    fn read_string_u16(&mut self) -> Result<String, MqdbError> {
        let len = usize::from(self.read_u16()?);
        let bytes = self.read_exact(len)?;
        String::from_utf8(bytes.to_vec())
            .map_err(|e| invalid_data(format!("invalid catalog string UTF-8: {e}")))
    }

    fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }
}

fn serialize_catalog(
    entries: &[CatalogEntry],
    custom_tables: &[CustomTableEntry],
    content_hashes: &[(u32, u64)],
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&as_u32(entries.len(), "catalog entry count").to_le_bytes());

    for entry in entries {
        out.extend_from_slice(&entry.document_id.to_le_bytes());
        match &entry.path {
            Some(path) => {
                out.push(1);
                out.extend_from_slice(&as_u16(path.len(), "catalog path length").to_le_bytes());
                out.extend_from_slice(path.as_bytes());
            }
            None => out.push(0),
        }
        out.extend_from_slice(&entry.first_block_page.to_le_bytes());
        out.extend_from_slice(&entry.num_blocks.to_le_bytes());
        out.extend_from_slice(&as_u32(entry.zone_map_bytes.len(), "zone map length").to_le_bytes());
        out.extend_from_slice(&entry.zone_map_bytes);
        out.extend_from_slice(&entry.index_start_page.to_le_bytes());
    }

    out.extend_from_slice(&as_u32(custom_tables.len(), "custom table count").to_le_bytes());
    for ct in custom_tables {
        out.extend_from_slice(&as_u16(ct.name.len(), "table name length").to_le_bytes());
        out.extend_from_slice(ct.name.as_bytes());
        out.extend_from_slice(&as_u16(ct.columns.len(), "column count").to_le_bytes());
        for col in &ct.columns {
            out.extend_from_slice(&as_u16(col.len(), "column name length").to_le_bytes());
            out.extend_from_slice(col.as_bytes());
        }
        out.extend_from_slice(&ct.first_row_page.to_le_bytes());
        out.extend_from_slice(&ct.last_row_page.to_le_bytes());
        out.extend_from_slice(&ct.num_rows.to_le_bytes());
    }

    // Trailing optional section (added after custom tables so older catalog
    // blobs without it still parse — `read_catalog` only reads this if bytes
    // remain). Maps `document_id` -> content hash, used to skip re-parsing
    // unchanged files on `reindex_paths`.
    out.extend_from_slice(&as_u32(content_hashes.len(), "content hash count").to_le_bytes());
    for (document_id, hash) in content_hashes {
        out.extend_from_slice(&document_id.to_le_bytes());
        out.extend_from_slice(&hash.to_le_bytes());
    }

    out
}

pub fn write_catalog(
    pf: &mut PageFile,
    entries: &[CatalogEntry],
    custom_tables: &[CustomTableEntry],
    content_hashes: &[(u32, u64)],
) -> Result<(), MqdbError> {
    if pf.num_pages < 2 {
        return Err(invalid_data("catalog start page is missing"));
    }

    let bytes = serialize_catalog(entries, custom_tables, content_hashes);
    let chunks: Vec<&[u8]> = if bytes.is_empty() {
        vec![&[]]
    } else {
        bytes.chunks(PAGE_BODY_SIZE).collect()
    };

    let mut page_ids = Vec::with_capacity(chunks.len());
    page_ids.push(1);

    for _ in 1..chunks.len() {
        let placeholder = make_page(PAGE_TYPE_CATALOG, 0, 0, &[]);
        let page_id = pf.append_page(&placeholder)?;
        page_ids.push(page_id);
    }

    for (index, chunk) in chunks.iter().enumerate() {
        let page_id = page_ids[index];
        let next_page = page_ids.get(index + 1).copied().unwrap_or(0);
        let page = make_page(PAGE_TYPE_CATALOG, page_id, next_page, chunk);
        pf.write_page(page_id, &page)?;
    }

    Ok(())
}

pub fn read_catalog(pf: &mut PageFile) -> Result<CatalogData, MqdbError> {
    if pf.num_pages < 2 {
        return Err(invalid_data("catalog start page is missing"));
    }

    let mut bytes = Vec::new();
    let mut page_id = 1u32;
    let mut visited = HashSet::new();

    loop {
        if !visited.insert(page_id) {
            return Err(invalid_data("catalog page chain contains a cycle"));
        }

        let page = pf.read_page(page_id)?;
        let (page_type, _, stored_page_id, next_page) = parse_page_header(&page);
        if page_type != PAGE_TYPE_CATALOG {
            return Err(invalid_data(format!(
                "page {page_id} is not a catalog page"
            )));
        }
        if stored_page_id != page_id {
            return Err(invalid_data(format!(
                "catalog page header mismatch: expected {page_id}, found {stored_page_id}"
            )));
        }

        bytes.extend_from_slice(&page[PAGE_HEADER_SIZE..]);

        if next_page == 0 {
            break;
        }
        page_id = next_page;
    }

    let mut decoder = Decoder::new(&bytes);
    let entry_count = usize::try_from(decoder.read_u32()?)
        .map_err(|_| invalid_data("catalog entry count exceeds usize range"))?;
    let mut entries = Vec::with_capacity(entry_count);

    for _ in 0..entry_count {
        let document_id = decoder.read_u32()?;
        let path = match decoder.read_u8()? {
            0 => None,
            1 => Some(decoder.read_string_u16()?),
            value => return Err(invalid_data(format!("invalid path presence tag: {value}"))),
        };
        let first_block_page = decoder.read_u32()?;
        let num_blocks = decoder.read_u32()?;
        let zone_map_len = usize::try_from(decoder.read_u32()?)
            .map_err(|_| invalid_data("zone map length exceeds usize range"))?;
        let zone_map_bytes = decoder.read_exact(zone_map_len)?.to_vec();

        let index_start_page = decoder.read_u32()?;
        entries.push(CatalogEntry {
            document_id,
            path,
            first_block_page,
            num_blocks,
            zone_map_bytes,
            index_start_page,
        });
    }

    let custom_tables = if decoder.remaining() >= 4 {
        let count = usize::try_from(decoder.read_u32()?)
            .map_err(|_| invalid_data("custom table count exceeds usize range"))?;
        let mut tables = Vec::with_capacity(count);
        for _ in 0..count {
            let name = decoder.read_string_u16()?;
            let num_cols = usize::from(decoder.read_u16()?);
            let mut columns = Vec::with_capacity(num_cols);
            for _ in 0..num_cols {
                columns.push(decoder.read_string_u16()?);
            }
            let first_row_page = decoder.read_u32()?;
            let last_row_page = decoder.read_u32()?;
            let num_rows = decoder.read_u32()?;
            tables.push(CustomTableEntry {
                name,
                columns,
                first_row_page,
                last_row_page,
                num_rows,
            });
        }
        tables
    } else {
        vec![]
    };

    let content_hashes = if decoder.remaining() >= 4 {
        let count = usize::try_from(decoder.read_u32()?)
            .map_err(|_| invalid_data("content hash count exceeds usize range"))?;
        let mut hashes = Vec::with_capacity(count);
        for _ in 0..count {
            let document_id = decoder.read_u32()?;
            let hash = decoder.read_u64()?;
            hashes.push((document_id, hash));
        }
        hashes
    } else {
        vec![]
    };

    Ok((entries, custom_tables, content_hashes))
}

#[cfg(test)]
mod tests {
    use std::{
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;
    use crate::storage::page::{PAGE_TYPE_CATALOG, PageFile, make_page};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn test_file_path(name: &str) -> PathBuf {
        let unique = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("mq-db-catalog-tests");
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(format!("{name}-{unique}.mq-db"))
    }

    /// Encodes catalog bytes in the format that predates content-hash
    /// tracking (entries + custom tables, no trailing hash section) — this
    /// mirrors what `.mq-db` files written by older `mq-db` versions look
    /// like on disk, to confirm `read_catalog` still parses them.
    fn serialize_catalog_pre_hash_format(entries: &[CatalogEntry]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&as_u32(entries.len(), "entry count").to_le_bytes());
        for entry in entries {
            out.extend_from_slice(&entry.document_id.to_le_bytes());
            match &entry.path {
                Some(path) => {
                    out.push(1);
                    out.extend_from_slice(&as_u16(path.len(), "path len").to_le_bytes());
                    out.extend_from_slice(path.as_bytes());
                }
                None => out.push(0),
            }
            out.extend_from_slice(&entry.first_block_page.to_le_bytes());
            out.extend_from_slice(&entry.num_blocks.to_le_bytes());
            out.extend_from_slice(
                &as_u32(entry.zone_map_bytes.len(), "zone map len").to_le_bytes(),
            );
            out.extend_from_slice(&entry.zone_map_bytes);
            out.extend_from_slice(&entry.index_start_page.to_le_bytes());
        }
        // Pre-migration files always had a (possibly empty) custom-table
        // count here, but no bytes at all after it.
        out.extend_from_slice(&as_u32(0, "table count").to_le_bytes());
        out
    }

    #[test]
    fn read_catalog_parses_pre_content_hash_format() {
        let path = test_file_path("pre-hash-format");
        let _ = std::fs::remove_file(&path);

        let entry = CatalogEntry {
            document_id: 7,
            path: Some("doc.md".to_string()),
            first_block_page: 3,
            num_blocks: 5,
            zone_map_bytes: vec![1, 2, 3],
            index_start_page: 9,
        };
        let bytes = serialize_catalog_pre_hash_format(std::slice::from_ref(&entry));

        let mut pf = PageFile::create(&path).unwrap();
        let page = make_page(PAGE_TYPE_CATALOG, 1, 0, &bytes);
        pf.append_page(&page).unwrap();
        pf.sync_header().unwrap();
        drop(pf);

        let mut reopened = PageFile::open(&path).unwrap();
        let (entries, custom_tables, content_hashes) = read_catalog(&mut reopened).unwrap();

        assert_eq!(entries, vec![entry]);
        assert!(custom_tables.is_empty());
        assert!(content_hashes.is_empty());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn write_then_read_catalog_round_trips_content_hashes() {
        let path = test_file_path("hash-round-trip");
        let _ = std::fs::remove_file(&path);

        let entry = CatalogEntry {
            document_id: 1,
            path: Some("a.md".to_string()),
            first_block_page: 1,
            num_blocks: 2,
            zone_map_bytes: vec![],
            index_start_page: 0,
        };
        let hashes = vec![(1u32, 0xDEAD_BEEFu64), (2u32, 42u64)];

        let mut pf = PageFile::create(&path).unwrap();
        // Reserve page 1 the same way `Storage::create` does.
        let placeholder = make_page(PAGE_TYPE_CATALOG, 1, 0, &0u32.to_le_bytes());
        pf.append_page(&placeholder).unwrap();
        write_catalog(&mut pf, std::slice::from_ref(&entry), &[], &hashes).unwrap();
        pf.sync_header().unwrap();
        drop(pf);

        let mut reopened = PageFile::open(&path).unwrap();
        let (entries, custom_tables, read_hashes) = read_catalog(&mut reopened).unwrap();

        assert_eq!(entries, vec![entry]);
        assert!(custom_tables.is_empty());
        let mut read_hashes = read_hashes;
        read_hashes.sort();
        let mut expected = hashes;
        expected.sort();
        assert_eq!(read_hashes, expected);

        let _ = std::fs::remove_file(&path);
    }
}
