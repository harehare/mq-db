//! Secondary indexes for fast block lookups in the SQL engine.
//!
//! Three index types, matching the characteristics of each column:
//!
//! | Index | Column(s) | Type | Why |
//! |---|---|---|---|
//! | [`BitmapIndex`] | `block_type` | Inverted list per type | 15 variants → very low cardinality |
//! | [`BTreeIndex`] | `pre`, `post` | Sorted Vec + binary search | Monotonically increasing integers, range queries |
//! | [`HashIndex`] | `content`, `lang`, `depth` | HashMap | Point/equality lookups |
//!
//! ## How it compares to DuckDB
//!
//! DuckDB uses an **ART (Adaptive Radix Tree)** for general indexes and
//! **RoaringBitmap** for low-cardinality columns. Here we use simpler
//! structures that achieve the same asymptotic complexity for the query
//! patterns in mqdb:
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

use std::collections::{BTreeMap, HashMap};

use crate::block::{Block, BlockType};

// ─────────────────────────────────────────────────────────────────────────────
// BitmapIndex — block_type → sorted Vec of block positions
// ─────────────────────────────────────────────────────────────────────────────

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
    map: HashMap<BlockType, Vec<u32>>,
}

impl BitmapIndex {
    pub fn build(blocks: &[Block]) -> Self {
        let mut map: HashMap<BlockType, Vec<u32>> = HashMap::new();
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

// ─────────────────────────────────────────────────────────────────────────────
// BTreeIndex — pre/post → block position
// ─────────────────────────────────────────────────────────────────────────────

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

// ─────────────────────────────────────────────────────────────────────────────
// HashIndex — content / lang / depth → block positions
// ─────────────────────────────────────────────────────────────────────────────

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
    pub by_content: HashMap<String, Vec<u32>>,
    /// lang tag → block indices (code blocks only)
    pub by_lang: HashMap<String, Vec<u32>>,
    /// heading depth → block indices
    pub by_depth: HashMap<u8, Vec<u32>>,
}

impl HashIndex {
    pub fn build(blocks: &[Block]) -> Self {
        let mut by_content: HashMap<String, Vec<u32>> = HashMap::new();
        let mut by_lang: HashMap<String, Vec<u32>> = HashMap::new();
        let mut by_depth: HashMap<u8, Vec<u32>> = HashMap::new();

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

        Self { by_content, by_lang, by_depth }
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

// ─────────────────────────────────────────────────────────────────────────────
// DocumentIndex — all three indexes bundled for one document
// ─────────────────────────────────────────────────────────────────────────────

/// All secondary indexes for a single [`crate::document::Document`].
///
/// Built once when the document is added to the store (O(n) construction),
/// then consulted by the SQL engine's predicate pushdown to skip full scans.
#[derive(Debug, Default, Clone)]
pub struct DocumentIndex {
    pub bitmap: BitmapIndex,
    pub btree: BTreeIndex,
    pub hash: HashIndex,
}

impl DocumentIndex {
    pub fn build(blocks: &[Block]) -> Self {
        Self {
            bitmap: BitmapIndex::build(blocks),
            btree: BTreeIndex::build(blocks),
            hash: HashIndex::build(blocks),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// IndexHint — what the SQL planner decided to use
// ─────────────────────────────────────────────────────────────────────────────

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
            IndexHint::PreExact(pre) => {
                Some(idx.btree.get_by_pre(*pre).into_iter().collect())
            }
            IndexHint::PreRange(lo, hi) => {
                Some(idx.btree.range_by_pre(*lo, *hi).collect())
            }
            IndexHint::ContentExact(c) => Some(idx.hash.by_content(c).to_vec()),
            IndexHint::LangExact(l) => Some(idx.hash.by_lang(l).to_vec()),
            IndexHint::DepthExact(d) => Some(idx.hash.by_depth(*d).to_vec()),
            IndexHint::FullScan => None,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

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
            assert_eq!(found, Some(i as u32), "pre={} not found in btree", block.pre);
        }
    }

    #[test]
    fn test_btree_pre_range() {
        let blocks = blocks_from("# A\n\n## B\n\n### C\n\nParagraph\n");
        let idx = DocumentIndex::build(&blocks);

        let max_pre = blocks.iter().map(|b| b.pre).max().unwrap_or(0);
        let all: Vec<u32> = idx.btree.range_by_pre(0, max_pre).collect();
        assert_eq!(all.len(), blocks.len(), "range scan should cover all blocks");
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
        let blocks =
            blocks_from("# H1\n\n## H2\n\nParagraph\n\n```rust\ncode\n```\n\n- item\n");
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
}
