pub mod catalog;
pub mod codec;
pub mod page;

use std::{collections::HashSet, path::Path};

use crate::{
    block::Block,
    document::Document,
    error::MqdbError,
    storage::{
        catalog::{CatalogEntry, CustomTableEntry, read_catalog, write_catalog},
        codec::{decode_block, decode_table_rows, encode_block, encode_table_rows},
        page::{
            PAGE_BODY_SIZE, PAGE_HEADER_SIZE, PAGE_TYPE_BLOCK_DATA, PAGE_TYPE_CATALOG,
            PAGE_TYPE_INDEX, PAGE_TYPE_OVERFLOW, PAGE_TYPE_TABLE_DATA, PageFile, make_page,
            parse_page_header,
        },
    },
};

/// Usable bytes per table-row page, reserving 2 bytes for the page's real
/// (unpadded) chunk length. See [`Storage::write_table_row_chunks`].
const TABLE_ROW_PAGE_CAPACITY: usize = PAGE_BODY_SIZE - 2;

pub struct Storage {
    page_file: PageFile,
}

fn invalid_data(message: impl Into<String>) -> MqdbError {
    MqdbError::Storage(message.into())
}

impl Storage {
    /// Create a new empty database file. Writes file header + empty catalog.
    pub fn create(path: &Path) -> Result<Self, MqdbError> {
        let mut page_file = PageFile::create(path)?;
        let empty_catalog = 0u32.to_le_bytes();
        let page = make_page(PAGE_TYPE_CATALOG, 1, 0, &empty_catalog);
        let page_id = page_file.append_page(&page)?;
        if page_id != 1 {
            return Err(invalid_data(format!(
                "expected catalog page id 1, found {page_id}"
            )));
        }
        Ok(Self { page_file })
    }

    /// Open an existing database file. Validates magic + version.
    pub fn open(path: &Path) -> Result<Self, MqdbError> {
        Ok(Self {
            page_file: PageFile::open(path)?,
        })
    }

    /// Write one document's blocks to the page file. Returns the first_block_page.
    pub fn write_document(&mut self, doc: &Document) -> Result<u32, MqdbError> {
        let mut bytes = Vec::new();
        for block in &doc.blocks {
            bytes.extend_from_slice(&encode_block(block));
        }

        let chunks: Vec<&[u8]> = if bytes.is_empty() {
            vec![&[]]
        } else {
            bytes.chunks(PAGE_BODY_SIZE).collect()
        };

        let placeholder = make_page(PAGE_TYPE_BLOCK_DATA, 0, 0, &[]);
        let mut page_ids = Vec::with_capacity(chunks.len());
        for _ in 0..chunks.len() {
            page_ids.push(self.page_file.append_page(&placeholder)?);
        }

        for (index, chunk) in chunks.iter().enumerate() {
            let page_id = page_ids[index];
            let next_page = page_ids.get(index + 1).copied().unwrap_or(0);
            let page_type = if index == 0 {
                PAGE_TYPE_BLOCK_DATA
            } else {
                PAGE_TYPE_OVERFLOW
            };
            let page = make_page(page_type, page_id, next_page, chunk);
            self.page_file.write_page(page_id, &page)?;
        }

        page_ids
            .first()
            .copied()
            .ok_or_else(|| invalid_data("document page chain is empty"))
    }

    /// Read all blocks for a document given its first_block_page and num_blocks.
    pub fn read_blocks(
        &mut self,
        first_page: u32,
        num_blocks: u32,
    ) -> Result<Vec<Block>, MqdbError> {
        if num_blocks == 0 {
            return Ok(Vec::new());
        }

        let mut bytes = Vec::new();
        let mut page_id = first_page;
        let mut visited = HashSet::new();
        let mut first = true;

        loop {
            if !visited.insert(page_id) {
                return Err(invalid_data("block page chain contains a cycle"));
            }

            let page = self.page_file.read_page(page_id)?;
            let (page_type, _, stored_page_id, next_page) = parse_page_header(&page);
            let expected_type = if first {
                PAGE_TYPE_BLOCK_DATA
            } else {
                PAGE_TYPE_OVERFLOW
            };
            if page_type != expected_type {
                return Err(invalid_data(format!(
                    "unexpected page type {page_type} in block chain; expected {expected_type}"
                )));
            }
            if stored_page_id != page_id {
                return Err(invalid_data(format!(
                    "block page header mismatch: expected {page_id}, found {stored_page_id}"
                )));
            }

            bytes.extend_from_slice(&page[PAGE_HEADER_SIZE..]);

            if next_page == 0 {
                break;
            }
            page_id = next_page;
            first = false;
        }

        let mut blocks = Vec::with_capacity(num_blocks as usize);
        let mut offset = 0usize;
        for _ in 0..num_blocks {
            let (block, consumed) = decode_block(&bytes[offset..])?;
            offset = offset
                .checked_add(consumed)
                .ok_or_else(|| invalid_data("block byte offset overflow"))?;
            blocks.push(block);
        }

        Ok(blocks)
    }

    /// Save catalog (call after all write_document calls).
    pub fn flush_catalog(
        &mut self,
        entries: &[CatalogEntry],
        custom_tables: &[CustomTableEntry],
    ) -> Result<(), MqdbError> {
        write_catalog(&mut self.page_file, entries, custom_tables)
    }

    /// Read the catalog.
    pub fn load_catalog(
        &mut self,
    ) -> Result<(Vec<CatalogEntry>, Vec<CustomTableEntry>), MqdbError> {
        read_catalog(&mut self.page_file)
    }

    /// Write raw index bytes as a chained page sequence. Returns the first page id.
    pub fn write_index(&mut self, bytes: &[u8]) -> Result<u32, MqdbError> {
        let chunks: Vec<&[u8]> = if bytes.is_empty() {
            vec![&[]]
        } else {
            bytes.chunks(PAGE_BODY_SIZE).collect()
        };

        let placeholder = make_page(PAGE_TYPE_INDEX, 0, 0, &[]);
        let mut page_ids = Vec::with_capacity(chunks.len());
        for _ in 0..chunks.len() {
            page_ids.push(self.page_file.append_page(&placeholder)?);
        }

        for (i, chunk) in chunks.iter().enumerate() {
            let page_id = page_ids[i];
            let next_page = page_ids.get(i + 1).copied().unwrap_or(0);
            let page_type = if i == 0 {
                PAGE_TYPE_INDEX
            } else {
                PAGE_TYPE_OVERFLOW
            };
            let page = make_page(page_type, page_id, next_page, chunk);
            self.page_file.write_page(page_id, &page)?;
        }

        page_ids
            .first()
            .copied()
            .ok_or_else(|| invalid_data("empty index page chain"))
    }

    /// Read all bytes from an index page chain starting at `first_page`.
    pub fn read_index_bytes(&mut self, first_page: u32) -> Result<Vec<u8>, MqdbError> {
        let mut bytes = Vec::new();
        let mut page_id = first_page;
        let mut visited = HashSet::new();
        let mut first = true;

        loop {
            if !visited.insert(page_id) {
                return Err(invalid_data("index page chain contains a cycle"));
            }

            let page = self.page_file.read_page(page_id)?;
            let (page_type, _, stored_page_id, next_page) = parse_page_header(&page);

            let expected = if first {
                PAGE_TYPE_INDEX
            } else {
                PAGE_TYPE_OVERFLOW
            };
            if page_type != expected {
                return Err(invalid_data(format!(
                    "unexpected page type {page_type} in index chain; expected {expected}"
                )));
            }
            if stored_page_id != page_id {
                return Err(invalid_data(format!(
                    "index page header mismatch: expected {page_id}, found {stored_page_id}"
                )));
            }

            bytes.extend_from_slice(&page[PAGE_HEADER_SIZE..]);

            if next_page == 0 {
                break;
            }
            page_id = next_page;
            first = false;
        }

        Ok(bytes)
    }

    /// Write a fresh chain of table-row pages, starting a brand-new table.
    /// Returns `(first_page, last_page)`, or `(0, 0)` if `rows` is empty
    /// (nothing written — 0 is never a valid page id).
    pub fn write_table_rows(&mut self, rows: &[Vec<String>]) -> Result<(u32, u32), MqdbError> {
        self.write_table_row_chunks(rows, PAGE_TYPE_TABLE_DATA)
    }

    /// Append `rows` after an existing table-row chain by writing new pages
    /// and relinking the current tail (`tail_page`) to point at them.
    /// Returns the new tail page id (unchanged if `rows` is empty).
    pub fn append_table_rows(
        &mut self,
        tail_page: u32,
        rows: &[Vec<String>],
    ) -> Result<u32, MqdbError> {
        if rows.is_empty() {
            return Ok(tail_page);
        }

        // The first page of an appended batch continues the existing chain,
        // so it must be tagged OVERFLOW like every other non-head page —
        // only the table's very first page is ever PAGE_TYPE_TABLE_DATA.
        let (first_new, last_new) = self.write_table_row_chunks(rows, PAGE_TYPE_OVERFLOW)?;
        self.relink_next(tail_page, first_new)?;
        Ok(last_new)
    }

    /// Table-row chains are built incrementally across many separate write
    /// calls (one per `INSERT`), so — unlike block/index chains, which are
    /// always written whole in one pass — a short trailing chunk can end up
    /// in the *middle* of the logical chain, not just at its very end.
    /// Padding it out to `PAGE_BODY_SIZE` would silently splice zero bytes
    /// between two batches' real data. So each table-data/overflow page
    /// reserves its first 2 bytes for the real length of the chunk it holds.
    fn write_table_row_chunks(
        &mut self,
        rows: &[Vec<String>],
        head_page_type: u32,
    ) -> Result<(u32, u32), MqdbError> {
        if rows.is_empty() {
            return Ok((0, 0));
        }

        let bytes = encode_table_rows(rows);
        let chunks: Vec<&[u8]> = bytes.chunks(TABLE_ROW_PAGE_CAPACITY).collect();

        let placeholder = make_page(PAGE_TYPE_OVERFLOW, 0, 0, &[]);
        let mut page_ids = Vec::with_capacity(chunks.len());
        for _ in 0..chunks.len() {
            page_ids.push(self.page_file.append_page(&placeholder)?);
        }

        for (index, chunk) in chunks.iter().enumerate() {
            let page_id = page_ids[index];
            let next_page = page_ids.get(index + 1).copied().unwrap_or(0);
            let page_type = if index == 0 {
                head_page_type
            } else {
                PAGE_TYPE_OVERFLOW
            };
            let mut body = Vec::with_capacity(2 + chunk.len());
            body.extend_from_slice(&(chunk.len() as u16).to_le_bytes());
            body.extend_from_slice(chunk);
            let page = make_page(page_type, page_id, next_page, &body);
            self.page_file.write_page(page_id, &page)?;
        }

        let first = *page_ids.first().expect("checked non-empty above");
        let last = *page_ids.last().expect("checked non-empty above");
        Ok((first, last))
    }

    /// Rewrite a single page's `next_page` pointer in place, preserving its
    /// type, id, and body. Used to extend a page chain without touching any
    /// other page.
    fn relink_next(&mut self, page_id: u32, next_page: u32) -> Result<(), MqdbError> {
        let page = self.page_file.read_page(page_id)?;
        let (page_type, _, stored_page_id, _) = parse_page_header(&page);
        let body = &page[PAGE_HEADER_SIZE..];
        let relinked = make_page(page_type, stored_page_id, next_page, body);
        self.page_file.write_page(page_id, &relinked)
    }

    /// Read all rows for a table given its chain head, row count, and column count.
    pub fn read_table_rows(
        &mut self,
        first_page: u32,
        num_rows: u32,
        num_cols: usize,
    ) -> Result<Vec<Vec<String>>, MqdbError> {
        if first_page == 0 || num_rows == 0 {
            return Ok(Vec::new());
        }

        let mut bytes = Vec::new();
        let mut page_id = first_page;
        let mut visited = HashSet::new();
        let mut first = true;

        loop {
            if !visited.insert(page_id) {
                return Err(invalid_data("table row page chain contains a cycle"));
            }

            let page = self.page_file.read_page(page_id)?;
            let (page_type, _, stored_page_id, next_page) = parse_page_header(&page);
            let expected_type = if first {
                PAGE_TYPE_TABLE_DATA
            } else {
                PAGE_TYPE_OVERFLOW
            };
            if page_type != expected_type {
                return Err(invalid_data(format!(
                    "unexpected page type {page_type} in table row chain; expected {expected_type}"
                )));
            }
            if stored_page_id != page_id {
                return Err(invalid_data(format!(
                    "table row page header mismatch: expected {page_id}, found {stored_page_id}"
                )));
            }

            let body = &page[PAGE_HEADER_SIZE..];
            let chunk_len = usize::from(u16::from_le_bytes([body[0], body[1]]));
            let chunk_end = chunk_len.checked_add(2).ok_or_else(|| {
                invalid_data("table row page chunk length overflow")
            })?;
            if chunk_end > body.len() {
                return Err(invalid_data("table row page chunk length out of bounds"));
            }
            bytes.extend_from_slice(&body[2..chunk_end]);

            if next_page == 0 {
                break;
            }
            page_id = next_page;
            first = false;
        }

        decode_table_rows(&bytes, num_rows as usize, num_cols)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;
    use rstest::rstest;

    use crate::{
        DocumentStore,
        block::{BlockType, Properties, PropertyValue, Span},
        document::{Document, ZoneMaps},
        storage::{
            catalog::CatalogEntry,
            codec::{decode_block, decode_zone_map, encode_block, encode_zone_map},
        },
    };

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn test_file_path(name: &str) -> PathBuf {
        let unique = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before UNIX_EPOCH")
            .as_nanos();
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("mq-db-storage-tests");
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(format!("{name}-{timestamp}-{unique}.mq-db"))
    }

    fn cleanup(path: &Path) {
        let _ = std::fs::remove_file(path);
        let tmp_path = PathBuf::from(format!("{}.tmp", path.to_string_lossy()));
        let _ = std::fs::remove_file(tmp_path);
    }

    fn sample_block(block_type: BlockType, id: u32) -> Block {
        let mut properties = Properties::new();
        properties.set("name", format!("block-{id}"));
        properties.set("count", i64::from(id));
        properties.set("score", PropertyValue::Float(1.5f64 + f64::from(id)));
        properties.set("flag", id.is_multiple_of(2));
        properties.set(
            "items",
            PropertyValue::Array(vec![
                PropertyValue::Null,
                PropertyValue::String("value".to_string()),
                PropertyValue::Int(-3),
                PropertyValue::Float(2.25),
                PropertyValue::Bool(true),
                PropertyValue::Array(vec![PropertyValue::String("nested".to_string())]),
            ]),
        );

        Block {
            id,
            document_id: 7,
            block_type,
            content: format!("content-{id}"),
            span: Some(Span {
                start_line: 1,
                start_col: 2,
                end_line: 3,
                end_col: 4,
            }),
            pre: id * 2,
            post: id * 2 + 1,
            properties,
        }
    }

    #[test]
    fn block_codec_round_trip_all_block_types() {
        let block_types = [
            BlockType::Heading,
            BlockType::Paragraph,
            BlockType::Code,
            BlockType::List,
            BlockType::TableCell,
            BlockType::TableRow,
            BlockType::TableAlign,
            BlockType::Blockquote,
            BlockType::HorizontalRule,
            BlockType::Html,
            BlockType::Yaml,
            BlockType::Toml,
            BlockType::Math,
            BlockType::Definition,
            BlockType::Footnote,
        ];

        for (index, block_type) in block_types.into_iter().enumerate() {
            let block = sample_block(block_type, index as u32 + 1);
            let encoded = encode_block(&block);
            let (decoded, consumed) = decode_block(&encoded).unwrap();
            assert_eq!(consumed, encoded.len());
            assert_eq!(decoded, block);
        }
    }

    #[test]
    fn zone_map_codec_round_trip() {
        let mut zone_maps = ZoneMaps {
            max_heading_depth: 4,
            heading_slugs: ["intro".to_string(), "usage".to_string()]
                .into_iter()
                .collect(),
            heading_contents: ["Intro".to_string(), "Usage".to_string()]
                .into_iter()
                .collect(),
            code_languages: ["rust".to_string(), "python".to_string()]
                .into_iter()
                .collect(),
            frontmatter_keys: ["title".to_string(), "tags".to_string()]
                .into_iter()
                .collect(),
            title: Some("Storage Spec".to_string()),
            tags: vec!["db".to_string(), "markdown".to_string()],
        };
        let encoded = encode_zone_map(&zone_maps);
        let decoded = decode_zone_map(&encoded).unwrap();
        assert_eq!(decoded, zone_maps);

        zone_maps.title = None;
        let encoded_without_title = encode_zone_map(&zone_maps);
        let decoded_without_title = decode_zone_map(&encoded_without_title).unwrap();
        assert_eq!(decoded_without_title, zone_maps);
    }

    #[test]
    fn storage_round_trip_multi_page_document() {
        let path = test_file_path("multi-page");
        cleanup(&path);

        let blocks: Vec<Block> = (0..32)
            .map(|id| {
                let mut block = sample_block(BlockType::Paragraph, id + 1);
                block.content = "x".repeat(PAGE_BODY_SIZE / 2);
                block.pre = id * 2;
                block.post = id * 2 + 1;
                block
            })
            .collect();
        let document = Document::new(1, None, blocks.clone());

        let mut storage = Storage::create(&path).unwrap();
        let first_page = storage.write_document(&document).unwrap();
        let catalog_entry = CatalogEntry {
            document_id: document.id,
            path: None,
            first_block_page: first_page,
            num_blocks: document.blocks.len() as u32,
            zone_map_bytes: encode_zone_map(&document.zone_maps),
            index_start_page: 0,
        };
        storage.flush_catalog(&[catalog_entry], &[]).unwrap();
        drop(storage);

        let mut reopened = Storage::open(&path).unwrap();
        let (catalog, _) = reopened.load_catalog().unwrap();
        assert_eq!(catalog.len(), 1);
        assert_eq!(
            decode_zone_map(&catalog[0].zone_map_bytes).unwrap(),
            document.zone_maps
        );
        let decoded_blocks = reopened
            .read_blocks(first_page, document.blocks.len() as u32)
            .unwrap();
        assert_eq!(decoded_blocks, blocks);

        cleanup(&path);
    }

    #[test]
    fn document_store_save_load_round_trip() {
        let path = test_file_path("store-save-load");
        cleanup(&path);

        let mut store = DocumentStore::new();
        store
            .add_str(
                "---\ntitle: Demo\ntags: [db, rust]\n---\n# Intro\n\nParagraph\n\n```rust\nfn main() {}\n```\n",
            )
            .unwrap();
        store
            .add_str("## Usage\n\n- item one\n- item two\n")
            .unwrap();

        store.save(&path).unwrap();
        let loaded = DocumentStore::load(&path).unwrap();

        assert_eq!(loaded.len(), store.len());
        // Compare blocks and zone_maps only; first_block_page / index_start_page
        // are storage-layer fields set after writing to disk.
        for (l, s) in loaded.documents().iter().zip(store.documents().iter()) {
            assert_eq!(l.id, s.id);
            assert_eq!(l.blocks, s.blocks);
            assert_eq!(l.zone_maps, s.zone_maps);
        }

        cleanup(&path);
    }

    #[test]
    fn persisted_index_round_trip() {
        use crate::{SqlEngine, indexes::DocumentIndex};

        let path = test_file_path("index-round-trip");
        cleanup(&path);

        let mut store = DocumentStore::new();
        store
            .add_str("# Hello\n\n## Arch\n\nDetails\n\n```rust\ncode\n```\n")
            .unwrap();
        store.add_str("## Usage\n\n- item\n").unwrap();
        store.save(&path).unwrap();

        // Open lazily: catalog + indexes only
        let mut opened = DocumentStore::open(&path).unwrap();
        assert!(
            opened.documents()[0].blocks.is_empty(),
            "blocks not loaded yet"
        );

        // Load blocks and indexes from file
        opened.load_all_blocks().unwrap();
        opened.load_all_indexes().unwrap();

        assert!(!opened.documents()[0].blocks.is_empty(), "blocks loaded");
        assert!(opened.get_doc_index(0).is_some(), "index cached");

        // Index round-trip: verify the loaded index matches a freshly built one
        for (i, doc) in opened.documents().iter().enumerate() {
            let from_file = opened.get_doc_index(i).unwrap().clone();
            let from_blocks = DocumentIndex::build(&doc.blocks);
            assert_eq!(
                from_file.to_bytes(),
                from_blocks.to_bytes(),
                "index mismatch for doc {i}"
            );
        }

        // SqlEngine should use cached indexes (no rebuild cost)
        let engine = SqlEngine::new(&opened).unwrap();
        let out = engine.execute("SELECT count(*) FROM blocks").unwrap();
        assert!(!out.rows.is_empty());

        cleanup(&path);
    }

    #[rstest]
    #[case(BlockType::Heading)]
    #[case(BlockType::Paragraph)]
    #[case(BlockType::Code)]
    #[case(BlockType::List)]
    #[case(BlockType::TableCell)]
    #[case(BlockType::TableRow)]
    #[case(BlockType::TableAlign)]
    #[case(BlockType::Blockquote)]
    #[case(BlockType::HorizontalRule)]
    #[case(BlockType::Html)]
    #[case(BlockType::Yaml)]
    #[case(BlockType::Toml)]
    #[case(BlockType::Math)]
    #[case(BlockType::Definition)]
    #[case(BlockType::Footnote)]
    fn block_codec_round_trip_param(#[case] block_type: BlockType) {
        let block = sample_block(block_type, 42);
        let encoded = encode_block(&block);
        let (decoded, consumed) = decode_block(&encoded).unwrap();
        assert_eq!(consumed, encoded.len());
        assert_eq!(decoded, block);
    }

    #[test]
    fn table_row_chain_round_trip_across_multiple_appends() {
        // Regression test for the incremental INSERT path: each batch is
        // written with `append_table_rows` (mirroring multiple separate SQL
        // INSERTs), and only the very first page of the whole chain should
        // be tagged PAGE_TYPE_TABLE_DATA — every later page, including the
        // head of each appended batch, must be PAGE_TYPE_OVERFLOW or the
        // chain reader rejects it.
        let path = test_file_path("table-row-chain-append");
        cleanup(&path);

        let mut storage = Storage::create(&path).unwrap();
        storage.flush_catalog(&[], &[]).unwrap();

        let batch1 = vec![
            vec!["1".to_string(), "a".to_string()],
            vec!["2".to_string(), "b".to_string()],
        ];
        let batch2 = vec![vec!["3".to_string(), "c".to_string()]];
        let batch3 = vec![
            vec!["4".to_string(), "d".to_string()],
            vec!["5".to_string(), "e".to_string()],
        ];

        let (first_page, last_page) = storage.write_table_rows(&batch1).unwrap();
        let last_page = storage.append_table_rows(last_page, &batch2).unwrap();
        let last_page = storage.append_table_rows(last_page, &batch3).unwrap();
        assert_ne!(last_page, 0);

        let all_rows = storage.read_table_rows(first_page, 5, 2).unwrap();
        let expected: Vec<Vec<String>> = batch1
            .into_iter()
            .chain(batch2)
            .chain(batch3)
            .collect();
        assert_eq!(all_rows, expected);

        // A batch large enough to span multiple pages, appended after the
        // small single-page batches above, must not corrupt either side.
        let big_batch: Vec<Vec<String>> = (0..10)
            .map(|i| vec![i.to_string(), "x".repeat(PAGE_BODY_SIZE)])
            .collect();
        let last_page = storage.append_table_rows(last_page, &big_batch).unwrap();
        assert_ne!(last_page, 0);

        let all_rows = storage.read_table_rows(first_page, 15, 2).unwrap();
        let expected: Vec<Vec<String>> = expected.into_iter().chain(big_batch).collect();
        assert_eq!(all_rows, expected);

        cleanup(&path);
    }

    #[test]
    fn custom_table_round_trip() {
        let path = test_file_path("custom-table-round-trip");
        cleanup(&path);

        let mut store = DocumentStore::new();
        store.add_str("# Hello\n\nWorld\n").unwrap();
        store.save(&path).unwrap();

        // Open and CREATE TABLE + INSERT
        let mut opened = DocumentStore::open(&path).unwrap();
        opened.load_all_blocks().unwrap();
        opened.load_all_indexes().unwrap();
        let engine = crate::SqlEngine::new(&opened).unwrap();
        engine
            .execute("CREATE TABLE notes (id TEXT, body TEXT)")
            .unwrap();
        engine
            .execute("INSERT INTO notes VALUES ('1', 'hello')")
            .unwrap();
        engine
            .execute("INSERT INTO notes VALUES ('2', 'world')")
            .unwrap();
        drop(engine);
        drop(opened);

        // Re-open and verify tables persisted
        let mut reopened = DocumentStore::open(&path).unwrap();
        reopened.load_all_blocks().unwrap();
        reopened.load_all_indexes().unwrap();
        let engine2 = crate::SqlEngine::new(&reopened).unwrap();
        let out = engine2
            .execute("SELECT body FROM notes WHERE id = '1'")
            .unwrap();
        assert_eq!(out.rows.len(), 1);
        assert_eq!(out.rows[0][0], "hello");
        let all = engine2.execute("SELECT * FROM notes").unwrap();
        assert_eq!(all.rows.len(), 2);

        cleanup(&path);
    }

    #[rstest]
    #[case(Some("My Title"))]
    #[case(None)]
    #[case(Some("title with spaces and unicode: こんにちは"))]
    fn zone_map_title_round_trip_param(#[case] title: Option<&str>) {
        let zone_maps = ZoneMaps {
            max_heading_depth: 3,
            heading_slugs: ["intro", "usage"].iter().map(|s| s.to_string()).collect(),
            heading_contents: ["Intro", "Usage"].iter().map(|s| s.to_string()).collect(),
            code_languages: ["rust", "python"].iter().map(|s| s.to_string()).collect(),
            frontmatter_keys: ["title", "tags"].iter().map(|s| s.to_string()).collect(),
            title: title.map(|s| s.to_string()),
            tags: vec!["db".to_string(), "markdown".to_string()],
        };
        let encoded = encode_zone_map(&zone_maps);
        let decoded = decode_zone_map(&encoded).unwrap();
        assert_eq!(decoded, zone_maps);
    }
}
