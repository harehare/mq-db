//! Full-text search shared by the `find` CLI subcommand and the TUI's find mode.

use crate::{DocumentStore, MqdbError, SqlEngine};

#[derive(Debug, Clone)]
pub struct FindHit {
    pub path: String,
    pub block_type: String,
    pub content: String,
    pub score: f64,
}

/// Top `limit` hits for `query`, ranked by relevance.
///
/// Adds a substring fallback to `match()`/`score()` because
/// [`crate::indexes::tokenize`] treats a punctuation-free CJK run as one
/// token, so a query for part of that run would otherwise never match.
pub fn find_hits(
    store: &DocumentStore,
    query: &str,
    limit: usize,
) -> Result<Vec<FindHit>, MqdbError> {
    let q = query.replace('\'', "''");
    let sql = format!(
        "SELECT d.path AS path, b.block_type AS type, b.content AS content, \
         score(b.content, '{q}') AS score \
         FROM blocks b JOIN documents d ON d.id = b.document_id \
         WHERE match(b.content, '{q}') OR b.content LIKE '%{q}%' \
         ORDER BY score DESC, path ASC \
         LIMIT {limit}"
    );
    let out = SqlEngine::new(store)?.execute(&sql)?;
    Ok(out
        .rows
        .into_iter()
        .filter_map(|row| {
            let [path, block_type, content, score] = <[String; 4]>::try_from(row).ok()?;
            Some(FindHit {
                path,
                block_type,
                content,
                score: score.parse().unwrap_or(0.0),
            })
        })
        .collect())
}

/// A `window`-char snippet of `content` centred on the first match of a
/// `query` word, plus the snippet-local char ranges to highlight.
pub fn snippet(content: &str, query: &str, window: usize) -> (String, Vec<(usize, usize)>) {
    let terms: Vec<String> = {
        let words: Vec<String> = query
            .split_whitespace()
            .map(|w| w.to_lowercase())
            .filter(|w| !w.is_empty())
            .collect();
        if words.is_empty() {
            let q = query.trim().to_lowercase();
            if q.is_empty() { Vec::new() } else { vec![q] }
        } else {
            words
        }
    };

    let chars: Vec<char> = content.chars().collect();
    let lower: Vec<char> = content.to_lowercase().chars().collect();
    if chars.is_empty() || terms.is_empty() || lower.len() != chars.len() {
        let text: String = chars
            .iter()
            .take(window)
            .collect::<String>()
            .replace('\n', " ");
        let text = if chars.len() > window {
            format!("{text}…")
        } else {
            text
        };
        return (text, Vec::new());
    }

    let mut ranges: Vec<(usize, usize)> = terms
        .iter()
        .flat_map(|t| char_positions(&lower, &t.chars().collect::<Vec<_>>()))
        .collect();
    ranges.sort_unstable();
    merge_overlapping(&mut ranges);

    let center = ranges.first().map_or(0, |&(s, _)| s);
    let half = window / 2;
    let win_start = center
        .saturating_sub(half)
        .min(chars.len().saturating_sub(window.min(chars.len())));
    let win_end = (win_start + window).min(chars.len());

    let prefix = if win_start > 0 { "…" } else { "" };
    let suffix = if win_end < chars.len() { "…" } else { "" };
    let body: String = chars[win_start..win_end]
        .iter()
        .collect::<String>()
        .replace('\n', " ");
    let snippet_text = format!("{prefix}{body}{suffix}");

    let shift = prefix.chars().count();
    let local_ranges: Vec<(usize, usize)> = ranges
        .into_iter()
        .filter_map(|(s, e)| {
            if e <= win_start || s >= win_end {
                None
            } else {
                let cs = s.max(win_start) - win_start + shift;
                let ce = e.min(win_end) - win_start + shift;
                Some((cs, ce))
            }
        })
        .collect();

    (snippet_text, local_ranges)
}

/// All char-offset start positions where `needle` occurs in `haystack`.
fn char_positions(haystack: &[char], needle: &[char]) -> Vec<(usize, usize)> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return Vec::new();
    }
    (0..=haystack.len() - needle.len())
        .filter(|&start| haystack[start..start + needle.len()] == *needle)
        .map(|start| (start, start + needle.len()))
        .collect()
}

/// Merges overlapping/adjacent (start, end) ranges in place. Assumes `ranges`
/// is already sorted by start.
fn merge_overlapping(ranges: &mut Vec<(usize, usize)>) {
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for &(s, e) in ranges.iter() {
        if let Some(last) = merged.last_mut()
            && s <= last.1
        {
            last.1 = last.1.max(e);
        } else {
            merged.push((s, e));
        }
    }
    *ranges = merged;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippet_centers_on_match_in_a_long_block() {
        let content = format!("{}error handling{}", "x".repeat(200), "y".repeat(200));
        let (text, ranges) = snippet(&content, "error", 40);
        assert!(text.contains("error"));
        assert!(text.starts_with('…'));
        assert!(text.ends_with('…'));
        assert_eq!(ranges.len(), 1);
        let (s, e) = ranges[0];
        assert_eq!(
            &text.chars().collect::<Vec<_>>()[s..e]
                .iter()
                .collect::<String>(),
            "error"
        );
    }

    #[test]
    fn snippet_highlights_cjk_substring_match() {
        let content = "検索エンジンについて";
        let (text, ranges) = snippet(content, "検索", 40);
        assert_eq!(text, content);
        assert_eq!(ranges, vec![(0, 2)]);
    }

    #[test]
    fn snippet_falls_back_to_prefix_without_a_match() {
        let (text, ranges) = snippet("hello world", "zzz", 5);
        assert_eq!(text, "hello…");
        assert!(ranges.is_empty());
    }
}
