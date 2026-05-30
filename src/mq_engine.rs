//! mq query engine integration for mqdb.
//!
//! Provides [`MqEngine`] which runs [`mq-lang`] programs against Markdown
//! files on disk or against file-backed documents in a [`DocumentStore`].
//!
//! # Example
//!
//! ```rust,no_run
//! use mqdb::mq_engine::MqEngine;
//! use std::path::Path;
//!
//! // Run an mq program against a file
//! let results = MqEngine::eval_files(".h1", &[Path::new("README.md")]).unwrap();
//! for r in &results {
//!     println!("{}", r);
//! }
//! ```

use std::path::Path;

use mq_lang::{DefaultEngine, parse_markdown_input};

use crate::{DocumentStore, MqdbError};

/// Thin wrapper around [`mq_lang::DefaultEngine`] for evaluating mq programs
/// over Markdown content.
pub struct MqEngine;

impl MqEngine {
    /// Run an mq program against one or more files on disk.
    ///
    /// Each file is parsed independently; results from all files are
    /// concatenated in order.
    ///
    /// # Errors
    ///
    /// Returns [`MqdbError::Io`] if a file cannot be read,
    /// [`MqdbError::Mq`] if parsing or evaluation fails.
    pub fn eval_files(code: &str, paths: &[&Path]) -> Result<Vec<String>, MqdbError> {
        let mut engine = DefaultEngine::default();
        engine.load_builtin_module();

        let mut results = Vec::new();
        for path in paths {
            let content = std::fs::read_to_string(path)?;
            let input = parse_markdown_input(&content)
                .map_err(|e| MqdbError::Mq(e.to_string()))?;
            let output = engine
                .eval(code, input.into_iter())
                .map_err(|e| MqdbError::Mq(e.to_string()))?;
            for v in output.compact() {
                results.push(v.to_string());
            }
        }
        Ok(results)
    }

    /// Run an mq program against all file-backed documents in a store.
    ///
    /// Documents that were added via [`DocumentStore::add_str`] (no path)
    /// are silently skipped.
    pub fn eval_store(code: &str, store: &DocumentStore) -> Result<Vec<String>, MqdbError> {
        let paths: Vec<&Path> = store
            .documents()
            .iter()
            .filter_map(|d| d.path.as_deref())
            .collect();
        Self::eval_files(code, &paths)
    }
}
