//! Computes embeddings with an OpenAI-compatible `/embeddings` endpoint and
//! keeps them in a custom table keyed by `blocks.content_hash`.
//!
//! Works with anything that speaks that API: OpenAI, Ollama
//! (`http://localhost:11434/v1`), llama.cpp's server, vLLM, ...

use crate::{DocumentStore, MqdbError, SqlEngine};
use std::time::Duration;

/// Where and how to compute embeddings.
#[derive(Debug, Clone)]
pub struct EmbedConfig {
    /// Base URL; `/embeddings` is appended.
    pub endpoint: String,
    pub model: String,
    pub api_key: Option<String>,
    /// Texts sent per request.
    pub batch_size: usize,
    /// Forwarded as `dimensions` to models that support shortening.
    pub dimensions: Option<usize>,
    /// Drop every stored embedding first instead of refusing a model change.
    pub rebuild: bool,
}

impl Default for EmbedConfig {
    fn default() -> Self {
        Self {
            endpoint: "https://api.openai.com/v1".into(),
            model: "text-embedding-3-small".into(),
            api_key: None,
            batch_size: 64,
            dimensions: None,
            rebuild: false,
        }
    }
}

/// What [`sync_embeddings`] did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct EmbedReport {
    /// Embeddings computed and stored by this run.
    pub embedded: usize,
    /// Blocks that already had an embedding.
    pub up_to_date: usize,
    /// Stored embeddings dropped because no block has that hash any more.
    pub removed: usize,
}

const MAX_ATTEMPTS: u32 = 3;

/// Embeds `inputs` in one request; the result is in `inputs` order.
pub fn embed_batch(cfg: &EmbedConfig, inputs: &[String]) -> Result<Vec<Vec<f32>>, MqdbError> {
    if inputs.is_empty() {
        return Ok(Vec::new());
    }
    let url = format!("{}/embeddings", cfg.endpoint.trim_end_matches('/'));
    let mut body = serde_json::json!({ "model": cfg.model, "input": inputs });
    if let Some(d) = cfg.dimensions {
        body["dimensions"] = d.into();
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(120)))
        .build()
        .into();

    let mut last_err = String::new();
    for attempt in 0..MAX_ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(Duration::from_secs(1 << (attempt - 1)));
        }
        let mut req = agent.post(&url);
        if let Some(key) = &cfg.api_key {
            req = req.header("Authorization", &format!("Bearer {key}"));
        }
        match req.send_json(&body) {
            Ok(mut resp) => {
                let value: serde_json::Value = resp
                    .body_mut()
                    .read_json()
                    .map_err(|e| MqdbError::Embed(format!("invalid response from {url}: {e}")))?;
                return parse_embeddings(&value, inputs.len());
            }
            Err(e) => last_err = e.to_string(),
        }
    }
    Err(MqdbError::Embed(format!(
        "request to {url} failed after {MAX_ATTEMPTS} attempts: {last_err}"
    )))
}

/// Extracts `data[].embedding`, ordered by `data[].index`.
fn parse_embeddings(
    value: &serde_json::Value,
    expected: usize,
) -> Result<Vec<Vec<f32>>, MqdbError> {
    let data = value
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or_else(|| MqdbError::Embed("response has no `data` array".into()))?;
    if data.len() != expected {
        return Err(MqdbError::Embed(format!(
            "sent {expected} texts but got {} embeddings",
            data.len()
        )));
    }
    let mut indexed: Vec<(usize, Vec<f32>)> = Vec::with_capacity(data.len());
    for (pos, item) in data.iter().enumerate() {
        let index = item
            .get("index")
            .and_then(|i| i.as_u64())
            .map_or(pos, |i| i as usize);
        let vec = item
            .get("embedding")
            .and_then(|e| e.as_array())
            .ok_or_else(|| MqdbError::Embed("embedding item has no `embedding` array".into()))?
            .iter()
            .map(|n| {
                n.as_f64()
                    .map(|f| f as f32)
                    .ok_or_else(|| MqdbError::Embed("embedding contains a non-number".into()))
            })
            .collect::<Result<Vec<f32>, _>>()?;
        indexed.push((index, vec));
    }
    indexed.sort_by_key(|(i, _)| *i);
    Ok(indexed.into_iter().map(|(_, v)| v).collect())
}

fn valid_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn sql_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn vector_json(v: &[f32]) -> String {
    let parts: Vec<String> = v.iter().map(f32::to_string).collect();
    format!("[{}]", parts.join(","))
}

/// Brings `table` (`content_hash`, `embedding`, `model`) in line with the
/// store's blocks: embeds blocks that have no row yet and drops rows whose
/// block is gone. `progress` is called with `(done, total)` after each batch.
///
/// Blocks with identical type and content share one embedding. Front matter
/// and empty blocks are skipped.
pub fn sync_embeddings(
    store: &DocumentStore,
    cfg: &EmbedConfig,
    table: &str,
    mut progress: impl FnMut(usize, usize),
) -> Result<EmbedReport, MqdbError> {
    if !valid_identifier(table) {
        return Err(MqdbError::Embed(format!("invalid table name '{table}'")));
    }
    if cfg.batch_size == 0 {
        return Err(MqdbError::Embed("batch size must be at least 1".into()));
    }
    let engine = SqlEngine::new(store)?;
    engine.execute(&format!(
        "CREATE TABLE IF NOT EXISTS {table} (content_hash TEXT, embedding TEXT, model TEXT)"
    ))?;

    if cfg.rebuild {
        engine.execute(&format!("DELETE FROM {table}"))?;
    } else {
        let models = engine.execute(&format!("SELECT DISTINCT model FROM {table}"))?;
        if let Some(other) = models.rows.iter().find(|r| r[0] != cfg.model) {
            return Err(MqdbError::Embed(format!(
                "table '{table}' holds embeddings from model '{}', not '{}'; \
                 use --rebuild to replace them or a different --table",
                other[0], cfg.model
            )));
        }
    }

    let removed = engine
        .execute(&format!(
            "DELETE FROM {table} WHERE content_hash NOT IN (SELECT content_hash FROM blocks)"
        ))?
        .rows
        .first()
        .and_then(|r| r.first())
        .and_then(|n| n.parse::<usize>().ok())
        .unwrap_or(0);

    let total_blocks = engine
        .execute(
            "SELECT count(DISTINCT content_hash) FROM blocks \
             WHERE block_type NOT IN ('yaml', 'toml') AND content <> ''",
        )?
        .rows[0][0]
        .parse::<usize>()
        .unwrap_or(0);
    let pending = engine.execute(&format!(
        "SELECT DISTINCT content_hash, content FROM blocks \
         WHERE block_type NOT IN ('yaml', 'toml') AND content <> '' \
         AND content_hash NOT IN (SELECT content_hash FROM {table})"
    ))?;
    let todo = pending.rows;
    let mut report = EmbedReport {
        embedded: 0,
        up_to_date: total_blocks.saturating_sub(todo.len()),
        removed,
    };

    for chunk in todo.chunks(cfg.batch_size) {
        let texts: Vec<String> = chunk.iter().map(|r| r[1].clone()).collect();
        let vectors = embed_batch(cfg, &texts)?;
        let values: Vec<String> = chunk
            .iter()
            .zip(&vectors)
            .map(|(row, v)| {
                format!(
                    "({}, {}, {})",
                    sql_quote(&row[0]),
                    sql_quote(&vector_json(v)),
                    sql_quote(&cfg.model)
                )
            })
            .collect();
        engine.execute(&format!(
            "INSERT INTO {table} (content_hash, embedding, model) VALUES {}",
            values.join(", ")
        ))?;
        report.embedded += chunk.len();
        progress(report.embedded, todo.len());
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    /// Serves one `/embeddings` response per request: a 3-dim vector whose
    /// first component is the text's length, so tests can tell inputs apart.
    fn mock_server() -> (String, Arc<Mutex<Vec<serde_json::Value>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let body = loop {
                    let n = stream.read(&mut chunk).unwrap_or(0);
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some(split) = text.find("\r\n\r\n") {
                        let len = text
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if buf.len() >= split + 4 + len || n == 0 {
                            break text[split + 4..].to_string();
                        }
                    } else if n == 0 {
                        break String::new();
                    }
                };
                let req: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
                let inputs = req["input"].as_array().cloned().unwrap_or_default();
                // Reply in reverse order, with `index`, to exercise re-ordering.
                let data: Vec<serde_json::Value> = inputs
                    .iter()
                    .enumerate()
                    .rev()
                    .map(|(i, t)| {
                        serde_json::json!({
                            "index": i,
                            "embedding": [t.as_str().unwrap().len() as f64, 0.5, -1.0]
                        })
                    })
                    .collect();
                seen.lock().unwrap().push(req);
                let payload = serde_json::json!({ "data": data }).to_string();
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    payload.len(),
                    payload
                );
            }
        });
        (format!("http://{addr}/v1"), requests)
    }

    fn cfg(endpoint: String) -> EmbedConfig {
        EmbedConfig {
            endpoint,
            model: "test-model".into(),
            batch_size: 2,
            ..EmbedConfig::default()
        }
    }

    fn store_with(docs: &[&str]) -> DocumentStore {
        let mut store = DocumentStore::new();
        for d in docs {
            store.add_str(d).unwrap();
        }
        store
    }

    #[test]
    fn parse_orders_by_index_and_validates() {
        let v = serde_json::json!({"data": [
            {"index": 1, "embedding": [2.0]},
            {"index": 0, "embedding": [1.0]},
        ]});
        assert_eq!(parse_embeddings(&v, 2).unwrap(), vec![vec![1.0], vec![2.0]]);
        assert!(parse_embeddings(&v, 3).is_err());
        assert!(parse_embeddings(&serde_json::json!({}), 1).is_err());
        assert!(parse_embeddings(&serde_json::json!({"data": [{"embedding": ["x"]}]}), 1).is_err());
    }

    #[test]
    fn quoting_and_identifiers() {
        assert_eq!(sql_quote("it's"), "'it''s'");
        assert!(valid_identifier("emb_2"));
        assert!(!valid_identifier("emb; DROP"));
        assert!(!valid_identifier("1emb"));
        assert!(!valid_identifier(""));
    }

    #[test]
    fn embed_batch_sends_model_dimensions_and_auth() {
        let (endpoint, requests) = mock_server();
        let mut c = cfg(endpoint);
        c.dimensions = Some(3);
        c.api_key = Some("k".into());
        let out = embed_batch(&c, &["ab".into(), "abcd".into()]).unwrap();
        assert_eq!(out[0][0], 2.0);
        assert_eq!(out[1][0], 4.0);
        let req = requests.lock().unwrap()[0].clone();
        assert_eq!(req["model"], "test-model");
        assert_eq!(req["dimensions"], 3);
    }

    #[test]
    fn sync_embeds_only_missing_blocks_and_drops_orphans() {
        let (endpoint, requests) = mock_server();
        let store = store_with(&["# Title\n\nHello\n\nWorld\n"]);
        let c = cfg(endpoint);

        let first = sync_embeddings(&store, &c, "emb", |_, _| {}).unwrap();
        assert_eq!(first.embedded, 3); // heading + 2 paragraphs
        assert_eq!(first.up_to_date, 0);
        assert_eq!(requests.lock().unwrap().len(), 2); // batch size 2

        let again = sync_embeddings(&store, &c, "emb", |_, _| {}).unwrap();
        assert_eq!(
            again,
            EmbedReport {
                embedded: 0,
                up_to_date: 3,
                removed: 0
            }
        );
        assert_eq!(requests.lock().unwrap().len(), 2, "no new requests");

        // Same content under a different document: shares the embedding.
        let engine = SqlEngine::new(&store).unwrap();
        let rows = engine.execute("SELECT count(*) FROM emb").unwrap();
        assert_eq!(rows.rows[0][0], "3");
        let sample = engine.execute("SELECT embedding FROM emb LIMIT 1").unwrap();
        assert!(sample.rows[0][0].starts_with('['));

        // A stale row for a vanished block is removed.
        engine
            .execute("INSERT INTO emb VALUES ('deadbeef', '[1]', 'test-model')")
            .unwrap();
        let cleaned = sync_embeddings(&store, &c, "emb", |_, _| {}).unwrap();
        assert_eq!(cleaned.removed, 1);
    }

    #[test]
    fn sync_refuses_model_change_unless_rebuild() {
        let (endpoint, _) = mock_server();
        let store = store_with(&["# T\n\nbody\n"]);
        let c = cfg(endpoint);
        sync_embeddings(&store, &c, "emb", |_, _| {}).unwrap();

        let mut other = c.clone();
        other.model = "other-model".into();
        let err = sync_embeddings(&store, &other, "emb", |_, _| {}).unwrap_err();
        assert!(err.to_string().contains("--rebuild"));

        other.rebuild = true;
        let report = sync_embeddings(&store, &other, "emb", |_, _| {}).unwrap();
        assert_eq!(report.embedded, 2);
    }

    #[test]
    fn sync_rejects_bad_table_name_and_batch_size() {
        let store = store_with(&["# T\n"]);
        let c = EmbedConfig::default();
        assert!(sync_embeddings(&store, &c, "a b", |_, _| {}).is_err());
        let zero = EmbedConfig { batch_size: 0, ..c };
        assert!(sync_embeddings(&store, &zero, "emb", |_, _| {}).is_err());
    }
}
