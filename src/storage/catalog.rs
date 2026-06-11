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
#[derive(Debug, Clone, PartialEq)]
pub struct CustomTableEntry {
    pub name: String,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

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

    fn read_string_u16(&mut self) -> Result<String, MqdbError> {
        let len = usize::from(self.read_u16()?);
        let bytes = self.read_exact(len)?;
        String::from_utf8(bytes.to_vec())
            .map_err(|e| invalid_data(format!("invalid catalog string UTF-8: {e}")))
    }

    fn read_string_u32(&mut self) -> Result<String, MqdbError> {
        let len = usize::try_from(self.read_u32()?)
            .map_err(|_| invalid_data("string length exceeds usize range"))?;
        let bytes = self.read_exact(len)?;
        String::from_utf8(bytes.to_vec())
            .map_err(|e| invalid_data(format!("invalid catalog string UTF-8: {e}")))
    }

    fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }
}

fn serialize_catalog(entries: &[CatalogEntry], custom_tables: &[CustomTableEntry]) -> Vec<u8> {
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

    // Custom tables section (appended for backward compatibility — old readers see trailing zeros
    // and stop; new readers detect the count field).
    out.extend_from_slice(&as_u32(custom_tables.len(), "custom table count").to_le_bytes());
    for ct in custom_tables {
        out.extend_from_slice(&as_u16(ct.name.len(), "table name length").to_le_bytes());
        out.extend_from_slice(ct.name.as_bytes());
        out.extend_from_slice(&as_u16(ct.columns.len(), "column count").to_le_bytes());
        for col in &ct.columns {
            out.extend_from_slice(&as_u16(col.len(), "column name length").to_le_bytes());
            out.extend_from_slice(col.as_bytes());
        }
        out.extend_from_slice(&as_u32(ct.rows.len(), "row count").to_le_bytes());
        for row in &ct.rows {
            for cell in row {
                out.extend_from_slice(&as_u32(cell.len(), "cell length").to_le_bytes());
                out.extend_from_slice(cell.as_bytes());
            }
        }
    }

    out
}

pub fn write_catalog(
    pf: &mut PageFile,
    entries: &[CatalogEntry],
    custom_tables: &[CustomTableEntry],
) -> Result<(), MqdbError> {
    if pf.num_pages < 2 {
        return Err(invalid_data("catalog start page is missing"));
    }

    let bytes = serialize_catalog(entries, custom_tables);
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

pub fn read_catalog(
    pf: &mut PageFile,
) -> Result<(Vec<CatalogEntry>, Vec<CustomTableEntry>), MqdbError> {
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

    // Custom tables section — present in new-format files; old files have trailing zeros here
    // which decode as count=0 (backward compatible).
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
            let num_rows = usize::try_from(decoder.read_u32()?)
                .map_err(|_| invalid_data("row count exceeds usize range"))?;
            let mut rows = Vec::with_capacity(num_rows);
            for _ in 0..num_rows {
                let mut row = Vec::with_capacity(num_cols);
                for _ in 0..num_cols {
                    row.push(decoder.read_string_u32()?);
                }
                rows.push(row);
            }
            tables.push(CustomTableEntry {
                name,
                columns,
                rows,
            });
        }
        tables
    } else {
        vec![]
    };

    Ok((entries, custom_tables))
}
