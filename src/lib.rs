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
//! ## Quick Start
//!
//! ```rust
//! use mq_db::{DocumentStore, SqlEngine};
//!
//! let mut store = DocumentStore::new();
//! store.add_str("# Hello\n\n## Architecture\n\nDetails\n").unwrap();
//!
//! let engine = SqlEngine::new(&store).unwrap();
//! let out = engine
//!     .execute("SELECT content FROM blocks WHERE block_type = 'heading' ORDER BY pre")
//!     .unwrap();
//!
//! assert_eq!(out.rows.len(), 2);
//! ```

pub mod block;
pub mod discover;
pub mod document;
pub mod error;
pub mod index;
pub mod indexes;
pub mod mq_engine;
pub mod search;
pub mod sql;
pub mod storage;
pub mod store;
pub mod tui;

pub use document::Document;
pub use error::MqdbError;
pub use mq_engine::MqEngine;
pub use sql::{QueryOutput, SqlEngine};
pub use storage::Storage;
pub use storage::catalog::CatalogEntry;
pub use store::{DatabaseAlias, DocumentStore, ReindexReport, StoreStats};
