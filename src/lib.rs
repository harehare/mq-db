//! # mq-db – Markdown-specialised Embedded Database
//!
//! `mq-db` treats Markdown documents as **structured, hierarchical databases**
//! rather than plain text. It builds on [`mq-markdown`]'s AST parser and
//! adds:
//!
//! * **Flat block storage with row-polymorphic properties** – every heading,
//!   paragraph, code block, list, table, and front-matter entry becomes a
//!   typed [`Block`] row with its own property bag.
//!
//! * **Interval index (Nested Set / Pre-Post Order)** – a section hierarchy
//!   derived from heading depth is encoded as `(pre, post)` integer pairs so
//!   that ancestor/descendant checks are `O(1)` integer comparisons.
//!
//! * **Zone Maps** – per-document statistics (max heading depth, heading
//!   slugs, code languages, front-matter keys/tags) that let the query
//!   engine skip irrelevant documents before scanning their blocks.
//!
//! * **Chainable query API** – document-level and block-level predicates,
//!   `UNDER heading` section scoping, built-in linter helpers.
//!
//! ## Quick Start
//!
//! ```rust
//! use mq_db::{DocumentStore, block::BlockType};
//!
//! let mut store = DocumentStore::new();
//! store.add_str("# Hello\n\n## Architecture\n\nDetails\n\n```rust\ncode\n```\n").unwrap();
//!
//! // Extract all content under the "Architecture" H2
//! let chunks = store.query()
//!     .under_heading("Architecture", Some(2))
//!     .filter(|b| matches!(b.block_type, BlockType::Paragraph | BlockType::Code))
//!     .blocks();
//!
//! assert_eq!(chunks.len(), 2);
//! ```
//!
//! ## Structural Linting
//!
//! ```rust
//! use mq_db::{DocumentStore, block::BlockType};
//!
//! let mut store = DocumentStore::new();
//! store.add_str("## Section\n\n- item without intro paragraph\n").unwrap();
//!
//! let q = store.query();
//! let violations = q.lint_heading_followed_by(2, &[BlockType::List]);
//!
//! assert_eq!(violations.len(), 1);
//! ```

pub mod block;
pub mod discover;
pub mod document;
pub mod error;
pub mod index;
pub mod indexes;
pub mod mq_engine;
pub mod query;
pub mod sql;
pub mod storage;
pub mod store;
pub mod tui;

pub use document::Document;
pub use error::MqdbError;
pub use mq_engine::MqEngine;
pub use query::{LintViolation, Query, QueryResult};
pub use sql::{QueryOutput, SqlEngine};
pub use storage::Storage;
pub use storage::catalog::CatalogEntry;
pub use store::{DocumentStore, ReindexReport, StoreStats};
