//! Secondary indexes for fast block lookups in the SQL engine.
//!
//! Four index types, matching the characteristics of each column:
//!
//! | Index | Column(s) | Type | Why |
//! |---|---|---|---|
//! | [`BitmapIndex`] | `block_type` | Inverted list per type | 15 variants → very low cardinality |
//! | [`BTreeIndex`] | `pre`, `post` | Sorted Vec + binary search | Monotonically increasing integers, range queries |
//! | [`HashIndex`] | `content`, `lang`, `depth` | HashMap | Point/equality lookups |
//! | [`TermIndex`] | `content` (tokenized) | Inverted postings list | Full-text `match()`/`score()` |
//!
//! ## How it compares to DuckDB
//!
//! DuckDB uses an **ART (Adaptive Radix Tree)** for general indexes and
//! **RoaringBitmap** for low-cardinality columns. Here we use simpler
//! structures that achieve the same asymptotic complexity for the query
//! patterns in mq-db:
//!
//! - Bitmap lookup: `O(1)` key + `O(k)` iteration (k = matching blocks)
//! - B-Tree range: `O(log n)` to find start + `O(k)` iteration
//! - Hash lookup: `O(1)` average
//!
//! All three beat the `O(n)` full-scan baseline when `k << n`.
//!
//! ## Zone Maps (already implemented)
//!
//! [`crate::document::ZoneMaps`] provides document-level skipping (skip entire
//! files that cannot match). These indexes operate *within* a document once
//! Zone Maps have decided it is worth scanning.
//!
//! `SqlEngine` applies this automatically (see `zone_map_skip` in
//! `src/sql.rs`) for `lang =` / `depth =` / heading `content =` conjuncts,
//! but only for a single, non-`JOIN`ed `FROM blocks`.

use std::collections::BTreeMap;

use rustc_hash::FxHashMap;

use crate::{
    block::{Block, BlockType},
    error::MqdbError,
};

// BitmapIndex — block_type → sorted Vec of block positions

/// Bitmap-style inverted index on `block_type`.
///
/// Each entry maps a [`BlockType`] to a sorted list of block indices
/// (positions in `Document::blocks`). Equivalent to a RoaringBitmap but
/// using plain `Vec<u32>` since block counts per document are small.
///
/// Best for: `WHERE block_type = 'heading'`  
/// Complexity: build O(n), lookup O(1) key + O(k) iterate
#[derive(Debug, Default, Clone)]
pub struct BitmapIndex {
    map: FxHashMap<BlockType, Vec<u32>>,
}

impl BitmapIndex {
    pub fn build(blocks: &[Block]) -> Self {
        let mut map: FxHashMap<BlockType, Vec<u32>> = FxHashMap::default();
        for (idx, block) in blocks.iter().enumerate() {
            map.entry(block.block_type.clone())
                .or_default()
                .push(idx as u32);
        }
        Self { map }
    }

    /// Returns block indices for a single type. O(1).
    pub fn get(&self, block_type: &BlockType) -> &[u32] {
        self.map.get(block_type).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Returns block indices matching any of the given types (union). O(k).
    pub fn get_any(&self, types: &[BlockType]) -> Vec<u32> {
        let mut result: Vec<u32> = types
            .iter()
            .flat_map(|t| self.get(t).iter().copied())
            .collect();
        result.sort_unstable();
        result.dedup();
        result
    }

    /// Returns whether any block of the given type exists.
    pub fn contains_type(&self, block_type: &BlockType) -> bool {
        self.map.contains_key(block_type)
    }
}

// BTreeIndex — pre/post → block position

/// B-Tree index on `pre` (and a secondary one on `post`).
///
/// Since blocks produced by [`crate::index::build_blocks`] are already in
/// DFS pre-order, `blocks[i].pre` is *not* necessarily equal to `i` — the
/// pre counter increments for every tree slot, including heading scopes.
/// We need an explicit map for O(log n) lookup.
///
/// Best for: `WHERE pre = X`, `WHERE pre BETWEEN X AND Y`,
///           `JOIN … ON b.pre = h.post + 1` (next-sibling join)  
/// Complexity: build O(n log n), point O(log n), range O(log n + k)
#[derive(Debug, Default, Clone)]
pub struct BTreeIndex {
    /// pre value → index in `Document::blocks`
    by_pre: BTreeMap<u32, u32>,
    /// post value → index in `Document::blocks`
    by_post: BTreeMap<u32, u32>,
}

impl BTreeIndex {
    pub fn build(blocks: &[Block]) -> Self {
        let mut by_pre = BTreeMap::new();
        let mut by_post = BTreeMap::new();
        for (idx, block) in blocks.iter().enumerate() {
            by_pre.insert(block.pre, idx as u32);
            by_post.insert(block.post, idx as u32);
        }
        Self { by_pre, by_post }
    }

    /// O(log n) point lookup by `pre`.
    pub fn get_by_pre(&self, pre: u32) -> Option<u32> {
        self.by_pre.get(&pre).copied()
    }

    /// O(log n) point lookup by `post`.
    pub fn get_by_post(&self, post: u32) -> Option<u32> {
        self.by_post.get(&post).copied()
    }

    /// O(log n + k) range scan over `pre` values in `[lo, hi]`.
    pub fn range_by_pre(&self, lo: u32, hi: u32) -> impl Iterator<Item = u32> + '_ {
        self.by_pre.range(lo..=hi).map(|(_, &idx)| idx)
    }

    /// O(log n + k) range scan over `post` values in `[lo, hi]`.
    pub fn range_by_post(&self, lo: u32, hi: u32) -> impl Iterator<Item = u32> + '_ {
        self.by_post.range(lo..=hi).map(|(_, &idx)| idx)
    }
}

// HashIndex — content / lang / depth → block positions

/// Hash index for point-equality lookups on string/integer columns.
///
/// Covers `content` (exact match), `lang` (code language), and heading `depth`.
///
/// Best for: `WHERE content = 'Architecture'`, `WHERE lang = 'rust'`,
///           `WHERE depth = 2`  
/// Complexity: build O(n), lookup O(1) average
#[derive(Debug, Default, Clone)]
pub struct HashIndex {
    /// content (exact lowercase) → block indices
    pub by_content: FxHashMap<String, Vec<u32>>,
    /// lang tag → block indices (code blocks only)
    pub by_lang: FxHashMap<String, Vec<u32>>,
    /// heading depth → block indices
    pub by_depth: FxHashMap<u8, Vec<u32>>,
}

impl HashIndex {
    pub fn build(blocks: &[Block]) -> Self {
        let mut by_content: FxHashMap<String, Vec<u32>> = FxHashMap::default();
        let mut by_lang: FxHashMap<String, Vec<u32>> = FxHashMap::default();
        let mut by_depth: FxHashMap<u8, Vec<u32>> = FxHashMap::default();

        for (idx, block) in blocks.iter().enumerate() {
            let i = idx as u32;
            by_content
                .entry(block.content.to_lowercase())
                .or_default()
                .push(i);

            if let Some(lang) = block.code_lang() {
                by_lang.entry(lang.to_string()).or_default().push(i);
            }
            if let Some(depth) = block.heading_depth() {
                by_depth.entry(depth).or_default().push(i);
            }
        }

        Self {
            by_content,
            by_lang,
            by_depth,
        }
    }

    /// Exact content match (case-insensitive). O(1).
    pub fn by_content(&self, content: &str) -> &[u32] {
        self.by_content
            .get(&content.to_lowercase())
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Exact lang match. O(1).
    pub fn by_lang(&self, lang: &str) -> &[u32] {
        self.by_lang.get(lang).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Heading depth lookup. O(1).
    pub fn by_depth(&self, depth: u8) -> &[u32] {
        self.by_depth.get(&depth).map(Vec::as_slice).unwrap_or(&[])
    }
}

/// Lowercase + split on non-alphanumeric (Unicode-aware via
/// `char::is_alphanumeric`).
///
/// This is used both to build [`TermIndex`]'s postings at index time and to
/// tokenize `match()`/`score()`'s arguments at query time (see `src/sql.rs`)
/// — the two **must** use this same function. `WHERE match(...)` uses the
/// index purely as a pre-filter with no full-scan fallback to catch a
/// mismatch, so if the two tokenizers ever disagreed, the index would
/// silently *drop* true matches rather than just mis-rank them.
///
/// Known limitations (intentional, dependency-free, documented rather than
/// fixed): no stemming, no stopword removal, no sub-splitting of
/// `camelCase`/`snake_case` beyond punctuation, and no CJK word segmentation
/// (a run of CJK characters with no ASCII punctuation between them tokenizes
/// as a single "word").
pub fn tokenize(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Inverted index on tokenized `content`: term → sorted, deduped block
/// indices containing that term at least once.
///
/// Best for: `WHERE match(content, 'foo bar')` (AND intersection across
/// query terms).
/// Complexity: build `O(n * avg_tokens)`, intersect `O(k)` for the rarest
/// term's postings length.
#[derive(Debug, Default, Clone)]
pub struct TermIndex {
    postings: FxHashMap<String, Vec<u32>>,
}

impl TermIndex {
    pub fn build(blocks: &[Block]) -> Self {
        let mut postings: FxHashMap<String, Vec<u32>> = FxHashMap::default();
        for (idx, block) in blocks.iter().enumerate() {
            // Sort + dedup the token list itself rather than allocating a
            // side `HashSet` per block — cheaper for the small token counts
            // typical of one block, and avoids an allocation per block.
            let mut terms = tokenize(&block.content);
            terms.sort_unstable();
            terms.dedup();
            for term in terms {
                postings.entry(term).or_default().push(idx as u32);
            }
        }
        Self { postings }
    }

    /// AND-intersection of postings for `terms`. Empty `terms` → empty
    /// result (mirrors `match()`'s "no terms → no match" semantics).
    pub fn intersect(&self, terms: &[String]) -> Vec<u32> {
        let mut iter = terms.iter();
        let Some(first) = iter.next() else {
            return Vec::new();
        };
        let mut acc: std::collections::BTreeSet<u32> = self
            .postings
            .get(first)
            .into_iter()
            .flatten()
            .copied()
            .collect();
        for term in iter {
            if acc.is_empty() {
                break;
            }
            let set: std::collections::HashSet<u32> = self
                .postings
                .get(term)
                .into_iter()
                .flatten()
                .copied()
                .collect();
            acc.retain(|idx| set.contains(idx));
        }
        acc.into_iter().collect()
    }
}

// DocumentIndex — all four indexes bundled for one document

/// All secondary indexes for a single [`crate::document::Document`].
///
/// Built once when the document is added to the store (O(n) construction),
/// then consulted by the SQL engine's predicate pushdown to skip full scans.
#[derive(Debug, Default, Clone)]
pub struct DocumentIndex {
    pub bitmap: BitmapIndex,
    pub btree: BTreeIndex,
    pub hash: HashIndex,
    pub term: TermIndex,
}

impl DocumentIndex {
    pub fn build(blocks: &[Block]) -> Self {
        Self {
            bitmap: BitmapIndex::build(blocks),
            btree: BTreeIndex::build(blocks),
            hash: HashIndex::build(blocks),
            term: TermIndex::build(blocks),
        }
    }

    /// Serialize the index to bytes for persistent storage.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();

        // BitmapIndex
        let mut bitmap_entries: Vec<(&BlockType, &Vec<u32>)> = self.bitmap.map.iter().collect();
        bitmap_entries.sort_by_key(|(bt, _)| block_type_ord(bt));
        out.extend_from_slice(&(bitmap_entries.len() as u32).to_le_bytes());
        for (bt, indices) in &bitmap_entries {
            out.push(block_type_ord(bt));
            out.extend_from_slice(&(indices.len() as u32).to_le_bytes());
            for &idx in indices.iter() {
                out.extend_from_slice(&idx.to_le_bytes());
            }
        }

        // BTreeIndex by_pre
        out.extend_from_slice(&(self.btree.by_pre.len() as u32).to_le_bytes());
        for (&pre, &idx) in &self.btree.by_pre {
            out.extend_from_slice(&pre.to_le_bytes());
            out.extend_from_slice(&idx.to_le_bytes());
        }

        // BTreeIndex by_post
        out.extend_from_slice(&(self.btree.by_post.len() as u32).to_le_bytes());
        for (&post, &idx) in &self.btree.by_post {
            out.extend_from_slice(&post.to_le_bytes());
            out.extend_from_slice(&idx.to_le_bytes());
        }

        // HashIndex by_content
        let mut content_entries: Vec<(&String, &Vec<u32>)> = self.hash.by_content.iter().collect();
        content_entries.sort_by_key(|(k, _)| k.as_str());
        out.extend_from_slice(&(content_entries.len() as u32).to_le_bytes());
        for (key, indices) in &content_entries {
            let kb = key.as_bytes();
            // Block content is unbounded (e.g. a large code block or table cell can
            // exceed 64KB), so the length prefix must be u32 — a u16 here would
            // silently wrap and desync the rest of the index stream.
            out.extend_from_slice(&(kb.len() as u32).to_le_bytes());
            out.extend_from_slice(kb);
            out.extend_from_slice(&(indices.len() as u32).to_le_bytes());
            for &idx in indices.iter() {
                out.extend_from_slice(&idx.to_le_bytes());
            }
        }

        // HashIndex by_lang
        let mut lang_entries: Vec<(&String, &Vec<u32>)> = self.hash.by_lang.iter().collect();
        lang_entries.sort_by_key(|(k, _)| k.as_str());
        out.extend_from_slice(&(lang_entries.len() as u32).to_le_bytes());
        for (key, indices) in &lang_entries {
            let kb = key.as_bytes();
            out.extend_from_slice(&(kb.len() as u32).to_le_bytes());
            out.extend_from_slice(kb);
            out.extend_from_slice(&(indices.len() as u32).to_le_bytes());
            for &idx in indices.iter() {
                out.extend_from_slice(&idx.to_le_bytes());
            }
        }

        // HashIndex by_depth
        let mut depth_entries: Vec<(&u8, &Vec<u32>)> = self.hash.by_depth.iter().collect();
        depth_entries.sort_by_key(|&(&d, _)| d);
        out.extend_from_slice(&(depth_entries.len() as u32).to_le_bytes());
        for &(&depth, indices) in &depth_entries {
            out.push(depth);
            out.extend_from_slice(&(indices.len() as u32).to_le_bytes());
            for &idx in indices.iter() {
                out.extend_from_slice(&idx.to_le_bytes());
            }
        }

        // TermIndex postings — appended after the four pre-existing sections
        // above; each of those is self-length-prefixed, so this is purely
        // additive and doesn't disturb their encoding/decoding order.
        let mut term_entries: Vec<(&String, &Vec<u32>)> = self.term.postings.iter().collect();
        term_entries.sort_by_key(|(k, _)| k.as_str());
        out.extend_from_slice(&(term_entries.len() as u32).to_le_bytes());
        for (term, indices) in &term_entries {
            let tb = term.as_bytes();
            out.extend_from_slice(&(tb.len() as u32).to_le_bytes());
            out.extend_from_slice(tb);
            out.extend_from_slice(&(indices.len() as u32).to_le_bytes());
            for &idx in indices.iter() {
                out.extend_from_slice(&idx.to_le_bytes());
            }
        }

        out
    }

    /// Deserialize an index from bytes previously produced by [`to_bytes`].
    pub fn from_bytes(data: &[u8]) -> Result<Self, MqdbError> {
        let mut pos = 0usize;

        macro_rules! read_u8 {
            () => {{
                if pos >= data.len() {
                    return Err(MqdbError::Storage("unexpected end of index data".into()));
                }
                let v = data[pos];
                pos += 1;
                v
            }};
        }
        macro_rules! read_u32 {
            () => {{
                let end = pos + 4;
                if end > data.len() {
                    return Err(MqdbError::Storage("unexpected end of index data".into()));
                }
                let v = u32::from_le_bytes(data[pos..end].try_into().unwrap());
                pos = end;
                v
            }};
        }
        macro_rules! read_str {
            ($len:expr) => {{
                let end = pos + $len;
                if end > data.len() {
                    return Err(MqdbError::Storage("unexpected end of index data".into()));
                }
                let s = String::from_utf8(data[pos..end].to_vec())
                    .map_err(|_| MqdbError::Storage("invalid UTF-8 in index".into()))?;
                pos = end;
                s
            }};
        }

        // BitmapIndex
        let num_bitmap = read_u32!() as usize;
        let mut bitmap_map: FxHashMap<BlockType, Vec<u32>> = FxHashMap::default();
        for _ in 0..num_bitmap {
            let bt = block_type_from_ord(read_u8!())?;
            let count = read_u32!() as usize;
            let mut indices = Vec::with_capacity(count);
            for _ in 0..count {
                indices.push(read_u32!());
            }
            bitmap_map.insert(bt, indices);
        }

        // BTreeIndex by_pre
        let num_pre = read_u32!() as usize;
        let mut by_pre = BTreeMap::new();
        for _ in 0..num_pre {
            let pre = read_u32!();
            let idx = read_u32!();
            by_pre.insert(pre, idx);
        }

        // BTreeIndex by_post
        let num_post = read_u32!() as usize;
        let mut by_post = BTreeMap::new();
        for _ in 0..num_post {
            let post = read_u32!();
            let idx = read_u32!();
            by_post.insert(post, idx);
        }

        // HashIndex by_content
        let num_content = read_u32!() as usize;
        let mut by_content: FxHashMap<String, Vec<u32>> = FxHashMap::default();
        for _ in 0..num_content {
            let key_len = read_u32!() as usize;
            let key = read_str!(key_len);
            let count = read_u32!() as usize;
            let mut indices = Vec::with_capacity(count);
            for _ in 0..count {
                indices.push(read_u32!());
            }
            by_content.insert(key, indices);
        }

        // HashIndex by_lang
        let num_lang = read_u32!() as usize;
        let mut by_lang: FxHashMap<String, Vec<u32>> = FxHashMap::default();
        for _ in 0..num_lang {
            let key_len = read_u32!() as usize;
            let key = read_str!(key_len);
            let count = read_u32!() as usize;
            let mut indices = Vec::with_capacity(count);
            for _ in 0..count {
                indices.push(read_u32!());
            }
            by_lang.insert(key, indices);
        }

        // HashIndex by_depth
        let num_depth = read_u32!() as usize;
        let mut by_depth: FxHashMap<u8, Vec<u32>> = FxHashMap::default();
        for _ in 0..num_depth {
            let depth = read_u8!();
            let count = read_u32!() as usize;
            let mut indices = Vec::with_capacity(count);
            for _ in 0..count {
                indices.push(read_u32!());
            }
            by_depth.insert(depth, indices);
        }

        // TermIndex postings
        let num_terms = read_u32!() as usize;
        let mut postings: FxHashMap<String, Vec<u32>> = FxHashMap::default();
        for _ in 0..num_terms {
            let term_len = read_u32!() as usize;
            let term = read_str!(term_len);
            let count = read_u32!() as usize;
            let mut indices = Vec::with_capacity(count);
            for _ in 0..count {
                indices.push(read_u32!());
            }
            postings.insert(term, indices);
        }

        Ok(DocumentIndex {
            bitmap: BitmapIndex { map: bitmap_map },
            btree: BTreeIndex { by_pre, by_post },
            hash: HashIndex {
                by_content,
                by_lang,
                by_depth,
            },
            term: TermIndex { postings },
        })
    }
}

fn block_type_ord(bt: &BlockType) -> u8 {
    match bt {
        BlockType::Heading => 0,
        BlockType::Paragraph => 1,
        BlockType::Code => 2,
        BlockType::List => 3,
        BlockType::TableCell => 4,
        BlockType::TableRow => 5,
        BlockType::TableAlign => 6,
        BlockType::Blockquote => 7,
        BlockType::HorizontalRule => 8,
        BlockType::Html => 9,
        BlockType::Yaml => 10,
        BlockType::Toml => 11,
        BlockType::Math => 12,
        BlockType::Definition => 13,
        BlockType::Footnote => 14,
    }
}

fn block_type_from_ord(v: u8) -> Result<BlockType, MqdbError> {
    match v {
        0 => Ok(BlockType::Heading),
        1 => Ok(BlockType::Paragraph),
        2 => Ok(BlockType::Code),
        3 => Ok(BlockType::List),
        4 => Ok(BlockType::TableCell),
        5 => Ok(BlockType::TableRow),
        6 => Ok(BlockType::TableAlign),
        7 => Ok(BlockType::Blockquote),
        8 => Ok(BlockType::HorizontalRule),
        9 => Ok(BlockType::Html),
        10 => Ok(BlockType::Yaml),
        11 => Ok(BlockType::Toml),
        12 => Ok(BlockType::Math),
        13 => Ok(BlockType::Definition),
        14 => Ok(BlockType::Footnote),
        _ => Err(MqdbError::Storage(format!("unknown block type ord: {v}"))),
    }
}

// IndexHint — what the SQL planner decided to use

/// The access plan chosen by the simple predicate pushdown analyser.
#[derive(Debug, Clone, PartialEq)]
pub enum IndexHint {
    /// Use the bitmap index: `WHERE block_type = 'X'` or `IN (...)`.
    BlockType(Vec<BlockType>),
    /// Use the btree index: `WHERE pre = X`.
    PreExact(u32),
    /// Use the btree index: `WHERE pre BETWEEN lo AND hi`.
    PreRange(u32, u32),
    /// Use the hash index: `WHERE content = 'X'`.
    ContentExact(String),
    /// Use the hash index: `WHERE lang = 'X'`.
    LangExact(String),
    /// Use the hash index: `WHERE depth = N`.
    DepthExact(u8),
    /// Use the term index: `WHERE match(content, 'foo bar')` (AND of tokens).
    TermMatch(Vec<String>),
    /// No applicable index — fall back to full scan.
    FullScan,
}

impl IndexHint {
    /// Apply the hint against a `DocumentIndex` to get matching block indices.
    ///
    /// Returns `None` if the hint is `FullScan` (caller does the scan).
    pub fn resolve(&self, idx: &DocumentIndex) -> Option<Vec<u32>> {
        match self {
            IndexHint::BlockType(types) => Some(idx.bitmap.get_any(types)),
            IndexHint::PreExact(pre) => Some(idx.btree.get_by_pre(*pre).into_iter().collect()),
            IndexHint::PreRange(lo, hi) => Some(idx.btree.range_by_pre(*lo, *hi).collect()),
            IndexHint::ContentExact(c) => Some(idx.hash.by_content(c).to_vec()),
            IndexHint::LangExact(l) => Some(idx.hash.by_lang(l).to_vec()),
            IndexHint::DepthExact(d) => Some(idx.hash.by_depth(*d).to_vec()),
            IndexHint::TermMatch(terms) => Some(idx.term.intersect(terms)),
            IndexHint::FullScan => None,
        }
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use mq_markdown::Markdown;
    use rstest::rstest;

    use crate::index::build_blocks;

    fn blocks_from(md: &str) -> Vec<Block> {
        let doc = md.parse::<Markdown>().unwrap();
        build_blocks(0, &doc.nodes)
    }

    #[test]
    fn test_bitmap_heading_lookup() {
        let blocks = blocks_from("# H1\n\n## H2\n\nParagraph\n\n```rust\ncode\n```\n");
        let idx = DocumentIndex::build(&blocks);

        let headings = idx.bitmap.get(&BlockType::Heading);
        assert_eq!(headings.len(), 2);

        let codes = idx.bitmap.get(&BlockType::Code);
        assert_eq!(codes.len(), 1);

        let paras = idx.bitmap.get(&BlockType::Paragraph);
        assert_eq!(paras.len(), 1);
    }

    #[test]
    fn test_bitmap_get_any() {
        let blocks = blocks_from("# H1\n\nParagraph\n\n```rust\ncode\n```\n");
        let idx = DocumentIndex::build(&blocks);

        let result = idx.bitmap.get_any(&[BlockType::Heading, BlockType::Code]);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_btree_pre_lookup() {
        let blocks = blocks_from("# H1\n\nParagraph\n");
        let idx = DocumentIndex::build(&blocks);

        // Every block's pre must be findable
        for (i, block) in blocks.iter().enumerate() {
            let found = idx.btree.get_by_pre(block.pre);
            assert_eq!(
                found,
                Some(i as u32),
                "pre={} not found in btree",
                block.pre
            );
        }
    }

    #[test]
    fn test_btree_pre_range() {
        let blocks = blocks_from("# A\n\n## B\n\n### C\n\nParagraph\n");
        let idx = DocumentIndex::build(&blocks);

        let max_pre = blocks.iter().map(|b| b.pre).max().unwrap_or(0);
        let all: Vec<u32> = idx.btree.range_by_pre(0, max_pre).collect();
        assert_eq!(
            all.len(),
            blocks.len(),
            "range scan should cover all blocks"
        );
    }

    #[test]
    fn test_hash_content_lookup() {
        let blocks = blocks_from("## Architecture\n\nDetails\n");
        let idx = DocumentIndex::build(&blocks);

        let found = idx.hash.by_content("architecture");
        assert_eq!(found.len(), 1);
        assert_eq!(blocks[found[0] as usize].content, "Architecture");
    }

    #[test]
    fn test_hash_lang_lookup() {
        let blocks = blocks_from("```rust\nfn main(){}\n```\n\n```python\npass\n```\n");
        let idx = DocumentIndex::build(&blocks);

        assert_eq!(idx.hash.by_lang("rust").len(), 1);
        assert_eq!(idx.hash.by_lang("python").len(), 1);
        assert_eq!(idx.hash.by_lang("go").len(), 0);
    }

    #[test]
    fn test_hash_depth_lookup() {
        let blocks = blocks_from("# H1\n\n## H2\n\n## H2b\n\n### H3\n");
        let idx = DocumentIndex::build(&blocks);

        assert_eq!(idx.hash.by_depth(1).len(), 1);
        assert_eq!(idx.hash.by_depth(2).len(), 2);
        assert_eq!(idx.hash.by_depth(3).len(), 1);
    }

    #[test]
    fn test_index_hint_resolve_block_type() {
        let blocks = blocks_from("# H1\n\nPara\n\n```rust\ncode\n```\n");
        let idx = DocumentIndex::build(&blocks);

        let hint = IndexHint::BlockType(vec![BlockType::Heading]);
        let result = hint.resolve(&idx).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(blocks[result[0] as usize].block_type, BlockType::Heading);
    }

    #[test]
    fn test_index_hint_fullscan_returns_none() {
        let blocks = blocks_from("# H1\n");
        let idx = DocumentIndex::build(&blocks);
        assert!(IndexHint::FullScan.resolve(&idx).is_none());
    }

    #[rstest]
    #[case(BlockType::Heading, 2)]
    #[case(BlockType::Paragraph, 1)]
    #[case(BlockType::Code, 1)]
    #[case(BlockType::List, 1)]
    #[case(BlockType::Blockquote, 0)]
    fn test_bitmap_block_type_count_param(#[case] block_type: BlockType, #[case] expected: usize) {
        let blocks = blocks_from("# H1\n\n## H2\n\nParagraph\n\n```rust\ncode\n```\n\n- item\n");
        let idx = DocumentIndex::build(&blocks);
        assert_eq!(idx.bitmap.get(&block_type).len(), expected);
    }

    #[rstest]
    #[case(1, 1)]
    #[case(2, 2)]
    #[case(3, 1)]
    #[case(4, 0)]
    fn test_hash_depth_count_param(#[case] depth: u8, #[case] expected: usize) {
        let blocks = blocks_from("# H1\n\n## H2a\n\n## H2b\n\n### H3\n");
        let idx = DocumentIndex::build(&blocks);
        assert_eq!(idx.hash.by_depth(depth).len(), expected);
    }

    #[rstest]
    #[case("rust", 1)]
    #[case("python", 1)]
    #[case("go", 0)]
    fn test_hash_lang_count_param(#[case] lang: &str, #[case] expected: usize) {
        let blocks = blocks_from("```rust\nfn main(){}\n```\n\n```python\npass\n```\n");
        let idx = DocumentIndex::build(&blocks);
        assert_eq!(idx.hash.by_lang(lang).len(), expected);
    }

    #[rstest]
    #[case(vec![BlockType::Heading], 2)]
    #[case(vec![BlockType::Paragraph], 1)]
    #[case(vec![BlockType::Code], 1)]
    #[case(vec![BlockType::Heading, BlockType::Code], 3)]
    fn test_index_hint_block_type_count_param(
        #[case] types: Vec<BlockType>,
        #[case] expected: usize,
    ) {
        let blocks = blocks_from("# H1\n\n## H2\n\nParagraph\n\n```rust\ncode\n```\n");
        let idx = DocumentIndex::build(&blocks);
        let result = IndexHint::BlockType(types).resolve(&idx).unwrap();
        assert_eq!(result.len(), expected);
    }

    #[rstest]
    #[case("fn main() {}", vec!["fn", "main"])]
    #[case("v1.2.3", vec!["v1", "2", "3"])]
    #[case("", vec![])]
    #[case("CamelCase HTML_tag", vec!["camelcase", "html", "tag"])]
    fn test_tokenize_param(#[case] input: &str, #[case] expected: Vec<&str>) {
        let expected: Vec<String> = expected.into_iter().map(str::to_string).collect();
        assert_eq!(tokenize(input), expected);
    }

    #[test]
    fn test_term_index_build_and_postings() {
        let blocks = blocks_from("# Hello World\n\nSome prose about Rust\n");
        let idx = DocumentIndex::build(&blocks);
        let hits = idx.term.intersect(&["rust".to_string()]);
        assert_eq!(hits.len(), 1);
        assert!(blocks[hits[0] as usize].content.contains("Rust"));
    }

    #[test]
    fn test_term_index_intersect_and_semantics() {
        let blocks = blocks_from("# H1\n\nfoo bar baz\n\nfoo only\n");
        let idx = DocumentIndex::build(&blocks);

        let both = idx.term.intersect(&["foo".to_string(), "bar".to_string()]);
        assert_eq!(both.len(), 1);

        let missing = idx
            .term
            .intersect(&["foo".to_string(), "nonexistent".to_string()]);
        assert!(missing.is_empty());

        assert!(idx.term.intersect(&[]).is_empty());
    }

    #[test]
    fn test_document_index_to_bytes_from_bytes_roundtrip_includes_term_index() {
        let blocks =
            blocks_from("# Title\n\nSome prose here.\n\n```rust\nfn main() { let x = 1; }\n```\n");
        let idx = DocumentIndex::build(&blocks);
        let restored = DocumentIndex::from_bytes(&idx.to_bytes()).unwrap();

        let mut original: Vec<(String, Vec<u32>)> = idx
            .term
            .postings
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let mut round_tripped: Vec<(String, Vec<u32>)> = restored
            .term
            .postings
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        original.sort();
        round_tripped.sort();

        assert!(!original.is_empty());
        assert_eq!(original, round_tripped);
    }
}
