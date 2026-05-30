use crate::{
    block::{Block, BlockType},
    document::Document,
    store::DocumentStore,
};

// ─────────────────────────────────────────────────────────────────────────────
// Section anchor: how to locate the enclosing heading section
// ─────────────────────────────────────────────────────────────────────────────

enum SectionAnchor {
    /// Directly supply a known (pre, post) interval.
    Interval { pre: u32, post: u32 },
    /// Find the first heading matching the given content and optional depth.
    Heading { content: String, depth: Option<u8> },
}

// ─────────────────────────────────────────────────────────────────────────────
// QueryResult: a matched block with its owning document
// ─────────────────────────────────────────────────────────────────────────────

/// A query result pairing a matched [`Block`] with its parent [`Document`].
pub struct QueryResult<'a> {
    pub block: &'a Block,
    pub document: &'a Document,
}

// ─────────────────────────────────────────────────────────────────────────────
// Query builder
// ─────────────────────────────────────────────────────────────────────────────

/// Chainable, lazy query builder over a [`DocumentStore`].
///
/// Filters are applied in evaluation order (cheapest first):
/// 1. Document-level zone-map skip (applied per document before scanning blocks)
/// 2. Section `UNDER` constraint (interval check)
/// 3. Block-level predicates
///
/// # Example – RAG chunk extraction
///
/// ```rust
/// use mqdb::{DocumentStore, block::BlockType};
///
/// let mut store = DocumentStore::new();
/// store.add_str("# Doc\n\n## Architecture\n\nExplanation\n\n```rust\ncode\n```\n").unwrap();
///
/// let results = store.query()
///     .under_heading("Architecture", Some(2))
///     .filter(|b| matches!(b.block_type, BlockType::Paragraph | BlockType::Code))
///     .blocks();
///
/// assert_eq!(results.len(), 2);
/// ```
type DocPredicate<'store> = Box<dyn Fn(&Document) -> bool + 'store>;
type BlockPredicate<'store> = Box<dyn Fn(&Block) -> bool + 'store>;

pub struct Query<'store> {
    store: &'store DocumentStore,
    doc_predicate: Option<DocPredicate<'store>>,
    block_predicates: Vec<BlockPredicate<'store>>,
    anchor: Option<SectionAnchor>,
    limit: Option<usize>,
}

impl<'store> Query<'store> {
    pub(crate) fn new(store: &'store DocumentStore) -> Self {
        Self {
            store,
            doc_predicate: None,
            block_predicates: Vec::new(),
            anchor: None,
            limit: None,
        }
    }

    // ── Document-level filters ───────────────────────────────────────────────

    /// Skip documents for which `predicate` returns `false`.
    ///
    /// Use this to leverage zone-map statistics before scanning blocks:
    /// ```rust
    /// # use mqdb::DocumentStore;
    /// # let store = DocumentStore::new();
    /// store.query()
    ///     .documents(|doc| doc.zone_maps.code_languages.contains("python"));
    /// ```
    pub fn documents<F>(mut self, f: F) -> Self
    where
        F: Fn(&Document) -> bool + 'store,
    {
        self.doc_predicate = Some(Box::new(f));
        self
    }

    // ── Section scope (UNDER) ────────────────────────────────────────────────

    /// Restrict results to blocks that fall within the heading section
    /// identified by `content` and optional `depth`.
    ///
    /// Equivalent to the SQL `WHERE b UNDER (SELECT id FROM blocks WHERE ...)`.
    ///
    /// For best performance, chain a `.documents(|d| d.zone_maps.heading_contents.contains("..."))`
    /// filter before this to skip irrelevant documents via zone maps.
    pub fn under_heading(mut self, content: impl Into<String>, depth: Option<u8>) -> Self {
        self.anchor = Some(SectionAnchor::Heading {
            content: content.into(),
            depth,
        });
        self
    }

    /// Restrict results to blocks that fall within the interval `(pre, post)`.
    ///
    /// Use this when you already know the ancestor block's interval values.
    pub fn under_interval(mut self, pre: u32, post: u32) -> Self {
        self.anchor = Some(SectionAnchor::Interval { pre, post });
        self
    }

    // ── Block-level filters ──────────────────────────────────────────────────

    /// Keep only blocks for which `predicate` returns `true`.
    pub fn filter<F>(mut self, f: F) -> Self
    where
        F: Fn(&Block) -> bool + 'store,
    {
        self.block_predicates.push(Box::new(f));
        self
    }

    /// Keep only blocks with the given [`BlockType`].
    pub fn block_type(self, ty: BlockType) -> Self {
        self.filter(move |b| b.block_type == ty)
    }

    /// Keep only heading blocks at the given depth.
    pub fn heading_depth(self, depth: u8) -> Self {
        self.filter(move |b| b.block_type == BlockType::Heading && b.heading_depth() == Some(depth))
    }

    /// Keep only code blocks with the given language tag.
    pub fn code_lang(self, lang: impl Into<String>) -> Self {
        let lang = lang.into();
        self.filter(move |b| b.code_lang() == Some(lang.as_str()))
    }

    /// Keep only blocks whose content contains `substring` (case-sensitive).
    pub fn content_contains(self, substring: impl Into<String>) -> Self {
        let s = substring.into();
        self.filter(move |b| b.content.contains(s.as_str()))
    }

    /// Keep only blocks whose content matches `pattern` (case-insensitive).
    pub fn content_contains_ci(self, pattern: impl Into<String>) -> Self {
        let pat = pattern.into().to_lowercase();
        self.filter(move |b| b.content.to_lowercase().contains(pat.as_str()))
    }

    // ── Limiting ─────────────────────────────────────────────────────────────

    /// Stop collecting after `n` results.
    pub fn limit(mut self, n: usize) -> Self {
        self.limit = Some(n);
        self
    }

    // ── Execution ────────────────────────────────────────────────────────────

    /// Execute the query, returning matched (block, document) pairs in
    /// document order.
    pub fn collect(&self) -> Vec<QueryResult<'_>> {
        let mut results: Vec<QueryResult<'_>> = Vec::new();

        'doc: for doc in self.store.documents() {
            // Zone-map skip
            if let Some(dp) = &self.doc_predicate
                && !dp(doc)
            {
                continue 'doc;
            }

            // Resolve the section anchor for this document
            let interval: Option<(u32, u32)> = match &self.anchor {
                None => None,
                Some(SectionAnchor::Interval { pre, post }) => Some((*pre, *post)),
                Some(SectionAnchor::Heading { content, depth }) => doc
                    .blocks
                    .iter()
                    .find(|b| {
                        b.block_type == BlockType::Heading
                            && b.content == *content
                            && depth.is_none_or(|d| b.heading_depth() == Some(d))
                    })
                    .map(|h| (h.pre, h.post)),
            };

            for block in &doc.blocks {
                // UNDER interval check
                if let Some((anc_pre, anc_post)) = interval
                    && !block.is_under_interval(anc_pre, anc_post)
                {
                    continue;
                }

                // Block predicates
                if self.block_predicates.iter().all(|f| f(block)) {
                    results.push(QueryResult {
                        block,
                        document: doc,
                    });

                    if let Some(limit) = self.limit
                        && results.len() >= limit
                    {
                        return results;
                    }
                }
            }
        }

        results
    }

    /// Like [`collect`], but returns cloned blocks (discarding document context).
    ///
    /// Returns owned `Vec<Block>` so it can be used on temporaries:
    /// ```rust
    /// # use mqdb::{DocumentStore, block::BlockType};
    /// # let mut store = DocumentStore::new();
    /// # store.add_str("# Hello\n").unwrap();
    /// let blocks = store.query().heading_depth(1).blocks();
    /// assert_eq!(blocks.len(), 1);
    /// ```
    pub fn blocks(&self) -> Vec<Block> {
        self.collect()
            .into_iter()
            .map(|r| r.block.clone())
            .collect()
    }

    /// Returns the number of matching blocks without materialising them.
    pub fn count(&self) -> usize {
        self.collect().len()
    }

    // ── Linter helpers ───────────────────────────────────────────────────────

    /// Find all (heading, next_sibling) pairs where the heading matches the
    /// given depth and the immediately following sibling has one of the
    /// `forbidden_types`.
    ///
    /// This is the foundation for structural lint rules such as:
    /// > "An H2 heading must not be immediately followed by a list."
    ///
    /// # Example
    ///
    /// ```rust
    /// use mqdb::{DocumentStore, block::BlockType};
    ///
    /// let mut store = DocumentStore::new();
    /// store.add_str("## Section\n\n- item\n").unwrap();
    ///
    /// let q = store.query();
    /// let violations = q.lint_heading_followed_by(2, &[BlockType::List]);
    ///
    /// assert_eq!(violations.len(), 1);
    /// ```
    pub fn lint_heading_followed_by(
        &self,
        heading_depth: u8,
        forbidden_types: &[BlockType],
    ) -> Vec<LintViolation<'_>> {
        let mut violations = Vec::new();

        for doc in self.store.documents() {
            if let Some(dp) = &self.doc_predicate
                && !dp(doc)
            {
                continue;
            }

            for block in &doc.blocks {
                if block.block_type != BlockType::Heading {
                    continue;
                }
                if block.heading_depth() != Some(heading_depth) {
                    continue;
                }

                if let Some(next) = doc.first_child(block)
                    && forbidden_types.contains(&next.block_type)
                {
                    violations.push(LintViolation {
                        heading: block,
                        offending: next,
                        document: doc,
                    });
                }
            }
        }

        violations
    }
}

/// A structural lint violation: a heading followed by a forbidden block type.
pub struct LintViolation<'a> {
    pub heading: &'a Block,
    pub offending: &'a Block,
    pub document: &'a Document,
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DocumentStore;
    use rstest::rstest;

    fn store_with(sources: &[&str]) -> DocumentStore {
        let mut s = DocumentStore::new();
        for src in sources {
            s.add_str(src).unwrap();
        }
        s
    }

    #[test]
    fn test_block_type_filter() {
        let store = store_with(&["# H1\n\n## H2\n\nParagraph\n\n```rust\ncode\n```\n"]);
        let q = store.query().block_type(BlockType::Heading);
        let headings = q.blocks();
        assert_eq!(headings.len(), 2);
    }

    #[test]
    fn test_heading_depth_filter() {
        let store = store_with(&["# H1\n\n## H2\n\n### H3\n"]);
        let q = store.query().heading_depth(2);
        let h2s = q.blocks();
        assert_eq!(h2s.len(), 1);
        assert_eq!(h2s[0].content, "H2");
    }

    #[test]
    fn test_under_heading_filter() {
        let store = store_with(&[
            "# Doc\n\n## Architecture\n\nExplanation\n\n```rust\ncode\n```\n\n## Other\n\nOther para\n",
        ]);

        let q = store
            .query()
            .under_heading("Architecture", Some(2))
            .filter(|b| matches!(b.block_type, BlockType::Paragraph | BlockType::Code));
        let results = q.blocks();

        // Should contain "Explanation" and the code block, but NOT "Other para"
        assert_eq!(results.len(), 2);
        assert!(results.iter().any(|b| b.content.contains("Explanation")));
        assert!(results.iter().any(|b| b.block_type == BlockType::Code));
        assert!(!results.iter().any(|b| b.content.contains("Other para")));
    }

    #[test]
    fn test_limit() {
        let store = store_with(&["# A\n\n# B\n\n# C\n\n# D\n"]);
        let q = store.query().heading_depth(1).limit(2);
        let results = q.blocks();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_content_contains() {
        let store = store_with(&["# Hello World\n\n## Goodbye\n"]);
        let q = store.query().content_contains("World");
        let results = q.blocks();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].content, "Hello World");
    }

    #[test]
    fn test_code_lang_filter() {
        let store = store_with(&["```rust\nfn x(){}\n```\n\n```python\nx=1\n```\n"]);
        let q = store.query().code_lang("rust");
        let results = q.blocks();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].code_lang(), Some("rust"));
    }

    #[test]
    fn test_zone_map_document_skip() {
        let store = store_with(&["```python\nx=1\n```\n", "```rust\nfn x(){}\n```\n"]);

        let q = store
            .query()
            .documents(|doc| doc.zone_maps.code_languages.contains("rust"))
            .block_type(BlockType::Code);
        let results = q.blocks();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].code_lang(), Some("rust"));
    }

    #[test]
    fn test_lint_heading_followed_by_list() {
        let store = store_with(&[
            "## Good\n\nParagraph intro\n\n- item\n",
            "## Bad\n\n- item without intro\n",
        ]);

        let q = store.query();
        let violations = q.lint_heading_followed_by(2, &[BlockType::List]);
        // Only the second doc has a violation
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].heading.content, "Bad");
        assert_eq!(violations[0].offending.block_type, BlockType::List);
    }

    #[test]
    fn test_multi_document_query() {
        let store = store_with(&[
            "# Doc1\n\n```rust\nfn a(){}\n```\n",
            "# Doc2\n\n```python\nx=1\n```\n",
            "# Doc3\n\n```rust\nfn b(){}\n```\n",
        ]);

        let q = store.query().code_lang("rust");
        let results = q.blocks();
        assert_eq!(results.len(), 2);
    }

    #[rstest]
    #[case(BlockType::Heading, 3)]
    #[case(BlockType::Paragraph, 1)]
    #[case(BlockType::Code, 1)]
    #[case(BlockType::List, 1)]
    fn test_block_type_filter_count_param(#[case] block_type: BlockType, #[case] expected: usize) {
        let store = store_with(&[
            "# H1\n\n## H2\n\n### H3\n\nParagraph\n\n```rust\ncode\n```\n\n- item\n",
        ]);
        assert_eq!(store.query().block_type(block_type).blocks().len(), expected);
    }

    #[rstest]
    #[case(1, 1, "H1")]
    #[case(2, 1, "H2")]
    #[case(3, 1, "H3")]
    fn test_heading_depth_count_param(
        #[case] depth: u8,
        #[case] expected: usize,
        #[case] content: &str,
    ) {
        let store = store_with(&["# H1\n\n## H2\n\n### H3\n"]);
        let results = store.query().heading_depth(depth).blocks();
        assert_eq!(results.len(), expected);
        assert_eq!(results[0].content, content);
    }

    #[rstest]
    #[case("Hello", 1)]
    #[case("World", 1)]
    #[case("Goodbye", 1)]
    #[case("nonexistent_xyz", 0)]
    fn test_content_contains_count_param(#[case] needle: &str, #[case] expected: usize) {
        let store = store_with(&["# Hello World\n\n## Goodbye\n"]);
        assert_eq!(store.query().content_contains(needle).blocks().len(), expected);
    }

    #[rstest]
    #[case("rust", 1)]
    #[case("python", 1)]
    #[case("go", 0)]
    fn test_code_lang_count_param(#[case] lang: &str, #[case] expected: usize) {
        let store = store_with(&["```rust\nfn x(){}\n```\n\n```python\nx=1\n```\n"]);
        assert_eq!(store.query().code_lang(lang).blocks().len(), expected);
    }

    #[rstest]
    #[case(1, 1)]
    #[case(2, 2)]
    #[case(3, 3)]
    #[case(100, 4)]
    fn test_limit_count_param(#[case] limit: usize, #[case] expected: usize) {
        let store = store_with(&["# A\n\n# B\n\n# C\n\n# D\n"]);
        let results = store.query().heading_depth(1).limit(limit).blocks();
        assert_eq!(results.len(), expected);
    }
}
