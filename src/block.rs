use rustc_hash::FxHashMap;

/// Unique identifier for a block within a document.
pub type BlockId = u32;

/// Unique identifier for a document within the store.
pub type DocumentId = u32;

/// Line/column span in the source document (1-based, matching mq-markdown).
#[derive(Debug, Clone, PartialEq)]
pub struct Span {
    pub start_line: usize,
    pub start_col: usize,
    pub end_line: usize,
    pub end_col: usize,
}

/// Strongly-typed block categories mapping directly to Markdown constructs.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum BlockType {
    /// Heading (H1–H6). The `depth` property carries the level (1–6).
    Heading,
    /// Regular paragraph / inline text content (Text, Emphasis, Strong, etc.).
    Paragraph,
    /// Fenced or indented code block. The `lang` property carries the language.
    Code,
    /// List item (ordered or unordered).
    List,
    /// Table cell.
    TableCell,
    /// Table row.
    TableRow,
    /// Table alignment row.
    TableAlign,
    /// Block quote.
    Blockquote,
    /// Horizontal rule (`---`).
    HorizontalRule,
    /// Raw HTML block.
    Html,
    /// YAML front-matter. Properties are populated from parsed YAML keys.
    Yaml,
    /// TOML front-matter.
    Toml,
    /// Math block (`$$...$$`).
    Math,
    /// Link definition (`[id]: url`).
    Definition,
    /// Footnote definition (`[^id]: ...`).
    Footnote,
}

impl BlockType {
    pub fn as_str(&self) -> &'static str {
        match self {
            BlockType::Heading => "heading",
            BlockType::Paragraph => "paragraph",
            BlockType::Code => "code",
            BlockType::List => "list",
            BlockType::TableCell => "table_cell",
            BlockType::TableRow => "table_row",
            BlockType::TableAlign => "table_align",
            BlockType::Blockquote => "blockquote",
            BlockType::HorizontalRule => "horizontal_rule",
            BlockType::Html => "html",
            BlockType::Yaml => "yaml",
            BlockType::Toml => "toml",
            BlockType::Math => "math",
            BlockType::Definition => "definition",
            BlockType::Footnote => "footnote",
        }
    }
}

impl std::fmt::Display for BlockType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// A row-polymorphic property value – any attribute that varies by block type.
///
/// Common keys per block type:
/// - `Heading`  → `"depth": Int`, `"slug": String`
/// - `Code`     → `"lang": String`, `"meta": String`, `"fence": Bool`
/// - `List`     → `"ordered": Bool`, `"level": Int`, `"checked": Bool`
/// - `TableCell`→ `"row": Int`, `"column": Int`
/// - `Yaml`     → parsed frontmatter keys (e.g. `"title"`, `"tags"`)
#[derive(Debug, Clone, PartialEq)]
pub enum PropertyValue {
    String(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Array(Vec<PropertyValue>),
    Null,
}

impl PropertyValue {
    pub fn as_str(&self) -> Option<&str> {
        if let Self::String(s) = self {
            Some(s)
        } else {
            None
        }
    }

    pub fn as_int(&self) -> Option<i64> {
        if let Self::Int(i) = self {
            Some(*i)
        } else {
            None
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        if let Self::Bool(b) = self {
            Some(*b)
        } else {
            None
        }
    }

    pub fn as_array(&self) -> Option<&[PropertyValue]> {
        if let Self::Array(a) = self {
            Some(a)
        } else {
            None
        }
    }
}

impl From<String> for PropertyValue {
    fn from(s: String) -> Self {
        Self::String(s)
    }
}

impl From<&str> for PropertyValue {
    fn from(s: &str) -> Self {
        Self::String(s.to_string())
    }
}

impl From<i64> for PropertyValue {
    fn from(i: i64) -> Self {
        Self::Int(i)
    }
}

impl From<u8> for PropertyValue {
    fn from(i: u8) -> Self {
        Self::Int(i as i64)
    }
}

impl From<u32> for PropertyValue {
    fn from(i: u32) -> Self {
        Self::Int(i as i64)
    }
}

impl From<usize> for PropertyValue {
    fn from(i: usize) -> Self {
        Self::Int(i as i64)
    }
}

impl From<bool> for PropertyValue {
    fn from(b: bool) -> Self {
        Self::Bool(b)
    }
}

/// Open-record property bag – acts like extra virtual columns in a
/// row-polymorphic schema. Keyed by string name, typed by [`PropertyValue`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Properties(FxHashMap<String, PropertyValue>);

impl Properties {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, key: &str) -> Option<&PropertyValue> {
        self.0.get(key)
    }

    pub fn set(&mut self, key: impl Into<String>, value: impl Into<PropertyValue>) {
        self.0.insert(key.into(), value.into());
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.0.contains_key(key)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &PropertyValue)> {
        self.0.iter()
    }
}

/// A single logical block extracted from a Markdown document.
///
/// # Interval Index
///
/// The `pre` and `post` fields implement the **Nested Set / Pre-Post Order**
/// interval index for the *section hierarchy* (defined by headings).
///
/// A block `A` is a descendant of block `B` iff:
/// ```text
/// B.pre < A.pre  &&  A.post < B.post
/// ```
/// This reduces ancestor/descendant checks to O(1) integer comparisons,
/// eliminating recursive tree traversal entirely.
#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    pub id: BlockId,
    pub document_id: DocumentId,
    pub block_type: BlockType,
    /// Plain-text content rendered from the block (without markup).
    pub content: String,
    /// Source location (line/column, 1-based). `None` for synthetic blocks.
    pub span: Option<Span>,
    /// Pre-order DFS number – left boundary of the section interval.
    pub pre: u32,
    /// Post-order DFS number – right boundary of the section interval.
    pub post: u32,
    /// Row-polymorphic extra attributes; keyed by property name.
    pub properties: Properties,
}

impl Block {
    /// Returns `true` if `other` is a descendant of `self` in the section
    /// hierarchy (i.e. `other` is "under" `self`).
    ///
    /// Uses the interval index for O(1) comparison.
    pub fn contains(&self, other: &Block) -> bool {
        self.pre < other.pre && other.post < self.post
    }

    /// Returns `true` if `self` is a descendant of `ancestor`.
    pub fn is_under(&self, ancestor: &Block) -> bool {
        ancestor.pre < self.pre && self.post < ancestor.post
    }

    /// Returns `true` if `self` falls within the given ancestor interval
    /// `(anc_pre, anc_post)`.
    pub fn is_under_interval(&self, anc_pre: u32, anc_post: u32) -> bool {
        anc_pre < self.pre && self.post < anc_post
    }

    /// Heading depth (1–6), or `None` for non-heading blocks.
    pub fn heading_depth(&self) -> Option<u8> {
        self.properties.get("depth")?.as_int().map(|d| d as u8)
    }

    /// Code language tag (e.g. `"rust"`), or `None` for non-code blocks.
    pub fn code_lang(&self) -> Option<&str> {
        self.properties.get("lang")?.as_str()
    }
}
