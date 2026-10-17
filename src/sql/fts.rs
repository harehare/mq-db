//! Full-text search state: tokenizer scopes and BM25 corpus stats.

use super::engine::SqlEngine;
use super::rows::ident_value;
use crate::indexes::{DocumentIndex, TokenizerKind, tokenize};
use crate::{DocumentStore, MqdbError};
use rustc_hash::FxHashMap;
use sqlparser::ast::TableFactor;

const BM25_K1: f64 = 1.2;

const BM25_B: f64 = 0.75;

// Corpus-wide stats for `bm25()`: the corpus for the FROM's driving (first)
// table, mirroring `QUERY_TOKENIZER` below. Rebuilt per `exec_select` (via
// `Bm25CorpusScope`), sourced from `BM25_CACHE` so repeated/correlated
// selects against the same store don't rebuild it.
thread_local! {
    pub(super) static BM25_CORPUS: std::cell::RefCell<Option<std::rc::Rc<Bm25Corpus>>> =
        const { std::cell::RefCell::new(None) };
}

// Per-alias override of `BM25_CORPUS`, for a `bm25()` call whose column
// belongs to a *non-driving* joined table (see `QUERY_TABLE_TOKENIZERS`).
thread_local! {
    pub(super) static QUERY_TABLE_BM25: std::cell::RefCell<FxHashMap<String, std::rc::Rc<Bm25Corpus>>> =
        std::cell::RefCell::new(FxHashMap::default());
}

// Corpus cache keyed by store identity (the `DocumentStore`'s address), not
// a schema name; a nested `SqlEngine` for an attached view/table sees its
// own store as local too, so a name-relative key like `None` would collide
// across engines. Shared across the whole top-level `execute()` call.
thread_local! {
    pub(super) static BM25_CACHE: std::cell::RefCell<FxHashMap<usize, std::rc::Rc<Bm25Corpus>>> =
        std::cell::RefCell::new(FxHashMap::default());
}

// Set once per top-level `execute()` call (cheap text scan) so `exec_select`
// knows whether it's worth resolving/building any `Bm25Corpus` at all.
thread_local! {
    pub(super) static QUERY_HAS_BM25: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

// Tokenizer for the free `match`/`score`/`bm25` functions below (no `&self`
// access). `exec_select` sets the default via `TokenizerScope`, the FROM's
// driving (first) table's tokenizer, for a plain, unqualified column
// reference. `QUERY_TABLE_TOKENIZERS` overrides it per-call when the
// referenced column is qualified by a *non-driving* joined table's alias,
// so `match(o.content, …)` against a later-joined, differently-tokenized
// attached table still tokenizes with `o`'s own tokenizer.
thread_local! {
    pub(super) static QUERY_TOKENIZER: std::cell::Cell<TokenizerKind> =
        const { std::cell::Cell::new(TokenizerKind::Word) };
}

thread_local! {
    pub(super) static QUERY_TABLE_TOKENIZERS: std::cell::RefCell<FxHashMap<String, TokenizerKind>> =
        std::cell::RefCell::new(FxHashMap::default());
}

pub(super) fn query_tokenizer() -> TokenizerKind {
    QUERY_TOKENIZER.with(std::cell::Cell::get)
}

/// RAII override of `QUERY_TOKENIZER`, restored on drop; nests correctly
/// across correlated subqueries.
pub(super) struct TokenizerScope(TokenizerKind);

impl TokenizerScope {
    pub(super) fn set(new: TokenizerKind) -> Self {
        let prev = query_tokenizer();
        QUERY_TOKENIZER.with(|c| c.set(new));
        Self(prev)
    }
}

impl Drop for TokenizerScope {
    fn drop(&mut self) {
        QUERY_TOKENIZER.with(|c| c.set(self.0));
    }
}

/// RAII override of `QUERY_TABLE_TOKENIZERS`, restored on drop.
pub(super) struct TableTokenizersScope(FxHashMap<String, TokenizerKind>);

impl TableTokenizersScope {
    pub(super) fn set(new: FxHashMap<String, TokenizerKind>) -> Self {
        let prev = QUERY_TABLE_TOKENIZERS.with(|c| c.replace(new));
        Self(prev)
    }
}

impl Drop for TableTokenizersScope {
    fn drop(&mut self) {
        QUERY_TABLE_TOKENIZERS.with(|c| *c.borrow_mut() = std::mem::take(&mut self.0));
    }
}

/// RAII override of `BM25_CORPUS`, restored on drop.
pub(super) struct Bm25CorpusScope(Option<std::rc::Rc<Bm25Corpus>>);

impl Bm25CorpusScope {
    pub(super) fn set(new: Option<std::rc::Rc<Bm25Corpus>>) -> Self {
        let prev = BM25_CORPUS.with(|c| c.replace(new));
        Self(prev)
    }
}

impl Drop for Bm25CorpusScope {
    fn drop(&mut self) {
        BM25_CORPUS.with(|c| *c.borrow_mut() = self.0.take());
    }
}

/// RAII override of `QUERY_TABLE_BM25`, restored on drop.
pub(super) struct TableBm25Scope(FxHashMap<String, std::rc::Rc<Bm25Corpus>>);

impl TableBm25Scope {
    pub(super) fn set(new: FxHashMap<String, std::rc::Rc<Bm25Corpus>>) -> Self {
        let prev = QUERY_TABLE_BM25.with(|c| c.replace(new));
        Self(prev)
    }
}

impl Drop for TableBm25Scope {
    fn drop(&mut self) {
        QUERY_TABLE_BM25.with(|c| *c.borrow_mut() = std::mem::take(&mut self.0));
    }
}

pub(super) struct Bm25Corpus {
    pub(super) n: u64,
    avgdl: f64,
    df: FxHashMap<String, u32>,
    pub(super) tokenizer: TokenizerKind,
}

impl Bm25Corpus {
    pub(super) fn build(indexes: &[DocumentIndex], tokenizer: TokenizerKind) -> Self {
        let mut n: u64 = 0;
        let mut total_tokens: u64 = 0;
        let mut df: FxHashMap<String, u32> = FxHashMap::default();
        for idx in indexes {
            n += u64::from(idx.term.block_count);
            total_tokens += idx.term.total_token_count;
            for (term, count) in idx.term.document_frequencies() {
                *df.entry(term.to_string()).or_default() += count;
            }
        }
        let avgdl = if n == 0 {
            0.0
        } else {
            total_tokens as f64 / n as f64
        };
        Self {
            n,
            avgdl,
            df,
            tokenizer,
        }
    }

    pub(super) fn score(&self, content: &str, query: &str) -> f64 {
        if self.n == 0 {
            return 0.0;
        }
        let content_terms = tokenize(content, self.tokenizer);
        let dl = content_terms.len() as f64;
        if dl == 0.0 {
            return 0.0;
        }
        let mut freq: FxHashMap<&str, u32> = FxHashMap::default();
        for t in &content_terms {
            *freq.entry(t.as_str()).or_default() += 1;
        }
        tokenize(query, self.tokenizer)
            .iter()
            .map(|qt| {
                let f = f64::from(*freq.get(qt.as_str()).unwrap_or(&0));
                if f == 0.0 {
                    return 0.0;
                }
                let df = f64::from(*self.df.get(qt.as_str()).unwrap_or(&0));
                let idf = ((self.n as f64 - df + 0.5) / (df + 0.5) + 1.0).ln();
                let numer = f * (BM25_K1 + 1.0);
                let denom = f + BM25_K1 * (1.0 - BM25_B + BM25_B * dl / self.avgdl);
                idf * numer / denom
            })
            .sum()
    }
}

impl<'a> SqlEngine<'a> {
    /// Tokenizer of `from`'s driving (first) table, an attached store's own
    /// tokenizer for `<alias>.<table>`, not this engine's.
    pub(super) fn tokenizer_for_from(
        &self,
        from: &[sqlparser::ast::TableWithJoins],
    ) -> TokenizerKind {
        if let Some(twj) = from.first()
            && let TableFactor::Table { name, .. } = &twj.relation
            && name.0.len() >= 2
        {
            let schema = ident_value(&name.0[name.0.len() - 2]).to_lowercase();
            if let Some(other) = self.store.attached.read().unwrap().get(schema.as_str()) {
                return other.tokenizer();
            }
        }
        self.store.tokenizer()
    }

    /// `alias -> TokenizerKind` for every table in `from`, driving table
    /// and every join, each resolved to its own attached store's tokenizer
    /// where applicable. Overrides `tokenizer_for_from`'s single default for
    /// a `match`/`score` call whose column is qualified by a non-driving
    /// table's alias.
    pub(super) fn tokenizer_map_for_from(
        &self,
        from: &[sqlparser::ast::TableWithJoins],
    ) -> FxHashMap<String, TokenizerKind> {
        let mut map = FxHashMap::default();
        let mut add = |factor: &TableFactor| {
            let TableFactor::Table { name, alias, .. } = factor else {
                return;
            };
            let key = match alias {
                Some(a) => a.name.value.to_lowercase(),
                None => name.0.last().map(ident_value).unwrap_or("").to_lowercase(),
            };
            let tokenizer = if name.0.len() >= 2 {
                let schema = ident_value(&name.0[name.0.len() - 2]).to_lowercase();
                self.store
                    .attached
                    .read()
                    .unwrap()
                    .get(schema.as_str())
                    .map(|other| other.tokenizer())
            } else {
                None
            }
            .unwrap_or_else(|| self.store.tokenizer());
            map.insert(key, tokenizer);
        };
        for twj in from {
            add(&twj.relation);
            for j in &twj.joins {
                add(&j.relation);
            }
        }
        map
    }

    /// The `Bm25Corpus` for `schema` (`None` = this store), cached in
    /// `BM25_CACHE` under the resolved store's address (see its comment).
    fn bm25_corpus_for_schema(
        &self,
        schema: Option<&str>,
    ) -> Result<std::rc::Rc<Bm25Corpus>, MqdbError> {
        let key = match schema {
            None => self.store as *const DocumentStore as usize,
            Some(schema) => {
                let guard = self.store.attached.read().unwrap();
                let other = guard
                    .get(schema)
                    .ok_or_else(|| MqdbError::SqlExec(format!("unknown database '{schema}'")))?;
                other as *const DocumentStore as usize
            }
        };
        if let Some(cached) = BM25_CACHE.with(|c| c.borrow().get(&key).cloned()) {
            return Ok(cached);
        }
        let corpus = match schema {
            None => std::rc::Rc::new(Bm25Corpus::build(&self.indexes, self.store.tokenizer())),
            Some(schema) => {
                let guard = self.store.attached.read().unwrap();
                let other = guard
                    .get(schema)
                    .ok_or_else(|| MqdbError::SqlExec(format!("unknown database '{schema}'")))?;
                let engine = SqlEngine::new(other)?;
                std::rc::Rc::new(Bm25Corpus::build(&engine.indexes, other.tokenizer()))
            }
        };
        BM25_CACHE.with(|c| c.borrow_mut().insert(key, corpus.clone()));
        Ok(corpus)
    }

    /// `Bm25Corpus` for `from`'s driving (first) table, the default used by
    /// an unqualified `bm25(content, …)` call, mirroring `tokenizer_for_from`.
    pub(super) fn bm25_corpus_for_from(
        &self,
        from: &[sqlparser::ast::TableWithJoins],
    ) -> Result<std::rc::Rc<Bm25Corpus>, MqdbError> {
        if let Some(twj) = from.first()
            && let TableFactor::Table { name, .. } = &twj.relation
            && name.0.len() >= 2
        {
            let schema = ident_value(&name.0[name.0.len() - 2]).to_lowercase();
            if self
                .store
                .attached
                .read()
                .unwrap()
                .contains_key(schema.as_str())
            {
                return self.bm25_corpus_for_schema(Some(&schema));
            }
        }
        self.bm25_corpus_for_schema(None)
    }

    /// `alias -> Bm25Corpus` for every table in `from`, driving table and
    /// every join, so `bm25(o.content, …)` against a non-driving joined
    /// table uses that table's own store's corpus (indexes, statistics,
    /// tokenizer) instead of the driving table's.
    pub(super) fn bm25_corpus_map_for_from(
        &self,
        from: &[sqlparser::ast::TableWithJoins],
    ) -> Result<FxHashMap<String, std::rc::Rc<Bm25Corpus>>, MqdbError> {
        let mut map = FxHashMap::default();
        let mut add = |factor: &TableFactor| -> Result<(), MqdbError> {
            let TableFactor::Table { name, alias, .. } = factor else {
                return Ok(());
            };
            let key = match alias {
                Some(a) => a.name.value.to_lowercase(),
                None => name.0.last().map(ident_value).unwrap_or("").to_lowercase(),
            };
            let schema = if name.0.len() >= 2 {
                let s = ident_value(&name.0[name.0.len() - 2]).to_lowercase();
                self.store
                    .attached
                    .read()
                    .unwrap()
                    .contains_key(s.as_str())
                    .then_some(s)
            } else {
                None
            };
            map.insert(key, self.bm25_corpus_for_schema(schema.as_deref())?);
            Ok(())
        };
        for twj in from {
            add(&twj.relation)?;
            for j in &twj.joins {
                add(&j.relation)?;
            }
        }
        Ok(map)
    }
}
