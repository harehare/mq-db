use std::{collections::HashSet, path::PathBuf};

use crate::block::{Block, BlockType, DocumentId, PropertyValue};

/// Statistical metadata per document used for query pruning (Zone Maps).
///
/// Before scanning a document's blocks, the query engine checks these
/// statistics to decide whether the document can be skipped entirely.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ZoneMaps {
    /// Maximum heading depth (1–6) found in the document.
    pub max_heading_depth: u8,
    /// Set of heading slugs (lowercased, hyphenated) present in the document.
    /// Used to skip documents that don't contain a requested heading.
    pub heading_slugs: HashSet<String>,
    /// Set of heading content strings (plain text) for exact-match skipping.
    pub heading_contents: HashSet<String>,
    /// Set of code-block language tags present (e.g. `"rust"`, `"python"`).
    pub code_languages: HashSet<String>,
    /// Set of top-level front-matter keys.
    pub frontmatter_keys: HashSet<String>,
    /// Document title – from front-matter `title` field or first H1.
    pub title: Option<String>,
    /// Tags list from front-matter `tags` array.
    pub tags: Vec<String>,
}

/// A parsed Markdown document stored in the database.
#[derive(Debug, Clone, PartialEq)]
pub struct Document {
    pub id: DocumentId,
    /// Source file path, if the document was loaded from disk.
    pub path: Option<PathBuf>,
    /// Flattened, interval-indexed block list.
    pub blocks: Vec<Block>,
    /// Per-document statistics for query pruning.
    pub zone_maps: ZoneMaps,
}

impl Document {
    pub fn new(id: DocumentId, path: Option<PathBuf>, blocks: Vec<Block>) -> Self {
        let zone_maps = ZoneMaps::build(&blocks);
        Self::from_parts(id, path, blocks, zone_maps)
    }

    pub fn from_parts(
        id: DocumentId,
        path: Option<PathBuf>,
        blocks: Vec<Block>,
        zone_maps: ZoneMaps,
    ) -> Self {
        Self {
            id,
            path,
            blocks,
            zone_maps,
        }
    }

    /// Returns the block immediately following `block` in document order
    /// after its entire section (i.e. the next sibling section), or `None`.
    ///
    /// Uses the interval index: the next sibling has `pre == block.post + 1`.
    pub fn next_sibling<'a>(&'a self, block: &Block) -> Option<&'a Block> {
        let target_pre = block.post + 1;
        self.blocks.iter().find(|b| b.pre == target_pre)
    }

    /// Returns the first content block INSIDE `block`'s section, or `None`.
    ///
    /// Uses the interval index: the first child has `pre == block.pre + 1`.
    /// For leaf blocks (no children), this returns `None`.
    pub fn first_child<'a>(&'a self, block: &Block) -> Option<&'a Block> {
        let target_pre = block.pre + 1;
        self.blocks.iter().find(|b| b.pre == target_pre)
    }

    /// Returns all blocks that are direct or indirect descendants of
    /// `ancestor` in the section hierarchy.
    pub fn descendants_of<'a>(&'a self, ancestor: &Block) -> impl Iterator<Item = &'a Block> {
        let (anc_pre, anc_post) = (ancestor.pre, ancestor.post);
        self.blocks
            .iter()
            .filter(move |b| b.is_under_interval(anc_pre, anc_post))
    }
}

impl ZoneMaps {
    pub fn build(blocks: &[Block]) -> Self {
        let mut maps = ZoneMaps::default();

        for block in blocks {
            match &block.block_type {
                BlockType::Heading => {
                    if let Some(d) = block.heading_depth() {
                        maps.max_heading_depth = maps.max_heading_depth.max(d);
                    }
                    maps.heading_contents.insert(block.content.clone());
                    if let Some(PropertyValue::String(slug)) = block.properties.get("slug") {
                        maps.heading_slugs.insert(slug.clone());
                    }
                    // First H1 becomes the document title if not set via frontmatter
                    if block.heading_depth() == Some(1) && maps.title.is_none() {
                        maps.title = Some(block.content.clone());
                    }
                }
                BlockType::Code => {
                    if let Some(lang) = block.code_lang() {
                        maps.code_languages.insert(lang.to_string());
                    }
                }
                BlockType::Yaml | BlockType::Toml => {
                    for (k, v) in block.properties.iter() {
                        maps.frontmatter_keys.insert(k.clone());
                        if k == "title"
                            && let PropertyValue::String(s) = v {
                            maps.title = Some(s.clone());
                        }
                        if k == "tags"
                            && let PropertyValue::Array(arr) = v {
                            maps.tags = arr
                                .iter()
                                .filter_map(|pv| pv.as_str().map(|s| s.to_string()))
                                .collect();
                        }
                    }
                }
                _ => {}
            }
        }

        maps
    }
}
