//! Built-in scalar function dispatch.

use super::datetime::{
    current_datetime_utc, eval_date_add, eval_date_diff, eval_date_trunc, eval_strftime,
};
use super::fts::{BM25_CORPUS, query_tokenizer};
use super::pattern::compile_regex;
use super::value::Value;
use crate::indexes::tokenize;
use mq_lang::{DefaultEngine, parse_markdown_input};
use rustc_hash::FxHashMap;

pub(super) fn eval_scalar_function(name: &str, args: &[Value]) -> Value {
    match name.to_lowercase().as_str() {
        "under" => {
            if args.len() < 4 {
                return Value::Bool(false);
            }
            let (pre, post) = (args[0].as_i64().unwrap_or(0), args[1].as_i64().unwrap_or(0));
            let (ap, aq) = (args[2].as_i64().unwrap_or(0), args[3].as_i64().unwrap_or(0));
            Value::Bool(pre > ap && post < aq)
        }
        "json_extract" => {
            if args.len() < 2 {
                return Value::Null;
            }
            let json = args[0].as_str().unwrap_or("");
            let path = args[1].as_str().unwrap_or("");
            extract_json_path(json, path)
        }
        "cosine_similarity" | "dot_product" | "l2_distance" => {
            if args.len() < 2 {
                return Value::Null;
            }
            let (Some(a), Some(b)) = (parse_vector(&args[0]), parse_vector(&args[1])) else {
                return Value::Null;
            };
            if a.len() != b.len() {
                return Value::Null;
            }
            let dot: f64 = a.iter().zip(&b).map(|(x, y)| x * y).sum();
            match name.to_lowercase().as_str() {
                "dot_product" => Value::Float(dot),
                "l2_distance" => Value::Float(
                    a.iter()
                        .zip(&b)
                        .map(|(x, y)| (x - y).powi(2))
                        .sum::<f64>()
                        .sqrt(),
                ),
                _ => {
                    let norm = |v: &[f64]| v.iter().map(|x| x * x).sum::<f64>().sqrt();
                    let denom = norm(&a) * norm(&b);
                    if denom == 0.0 {
                        Value::Null
                    } else {
                        Value::Float(dot / denom)
                    }
                }
            }
        }
        "mq" => {
            if args.len() < 2 {
                return Value::Null;
            }
            let program = match args[0].as_str() {
                Some(s) => s.to_string(),
                None => return Value::Null,
            };
            let content = match args[1].as_str() {
                Some(s) => s.to_string(),
                None => return Value::Null,
            };
            eval_mq_scalar(&program, &content)
        }
        "match" => {
            let (Some(content), Some(query)) = (
                args.first().and_then(Value::as_str),
                args.get(1).and_then(Value::as_str),
            ) else {
                return Value::Bool(false);
            };
            let content_terms: std::collections::HashSet<String> =
                tokenize(content, query_tokenizer()).into_iter().collect();
            let query_terms = tokenize(query, query_tokenizer());
            Value::Bool(
                !query_terms.is_empty() && query_terms.iter().all(|t| content_terms.contains(t)),
            )
        }
        "score" => {
            let (Some(content), Some(query)) = (
                args.first().and_then(Value::as_str),
                args.get(1).and_then(Value::as_str),
            ) else {
                return Value::Float(0.0);
            };
            let content_terms = tokenize(content, query_tokenizer());
            let query_terms = tokenize(query, query_tokenizer());
            if content_terms.is_empty() || query_terms.is_empty() {
                return Value::Float(0.0);
            }
            // Simple term-frequency score, normalised by content length,
            // deliberately not BM25 (no IDF/corpus-wide stats): `eval_expr`
            // only ever sees one `Row` at a time with no back-reference to
            // the corpus, so a real IDF term would need a much larger
            // signature change (see `TermIndex`'s doc comment for the same
            // constraint on the index side). Good enough to rank matches
            // within a single query; a document that repeats a common word
            // many times can outrank one with a rarer, more specific match.
            let mut freq: FxHashMap<&str, u32> = FxHashMap::default();
            for t in &content_terms {
                *freq.entry(t.as_str()).or_default() += 1;
            }
            let hits: f64 = query_terms
                .iter()
                .map(|q| *freq.get(q.as_str()).unwrap_or(&0) as f64)
                .sum();
            Value::Float(hits / content_terms.len() as f64)
        }
        "bm25" => {
            let (Some(content), Some(query)) = (
                args.first().and_then(Value::as_str),
                args.get(1).and_then(Value::as_str),
            ) else {
                return Value::Float(0.0);
            };
            let score = BM25_CORPUS.with(|c| {
                c.borrow()
                    .as_ref()
                    .map(|corpus| corpus.score(content, query))
            });
            Value::Float(score.unwrap_or(0.0))
        }

        "lower" => str_fn(args, |s| s.to_lowercase()),
        "upper" => str_fn(args, |s| s.to_uppercase()),
        "length" | "len" | "char_length" | "character_length" => args
            .first()
            .and_then(|v| v.as_str())
            .map(|s| Value::Int(s.chars().count() as i64))
            .unwrap_or(Value::Null),
        "trim" => str_fn(args, |s| s.trim().to_string()),
        "ltrim" => {
            let chars = trim_char_set(args, 1);
            str_fn(args, |s| {
                s.trim_start_matches(|c| chars.contains(&c)).to_string()
            })
        }
        "rtrim" => {
            let chars = trim_char_set(args, 1);
            str_fn(args, |s| {
                s.trim_end_matches(|c| chars.contains(&c)).to_string()
            })
        }
        "concat" => Value::Str(
            args.iter()
                .map(|v| v.display())
                .collect::<Vec<_>>()
                .join(""),
        ),
        "concat_ws" => {
            let sep = match args.first().and_then(|v| v.as_str()) {
                Some(s) => s,
                None => return Value::Null,
            };
            Value::Str(
                args[1..]
                    .iter()
                    .filter(|v| !matches!(v, Value::Null))
                    .map(|v| v.display())
                    .collect::<Vec<_>>()
                    .join(sep),
            )
        }
        "replace" => {
            if args.len() < 3 {
                return Value::Null;
            }
            match (args[0].as_str(), args[1].as_str(), args[2].as_str()) {
                (Some(s), Some(from), Some(to)) => Value::Str(s.replace(from, to)),
                _ => Value::Null,
            }
        }
        "regexp_like" => {
            let (Some(s), Some(pat)) = (
                args.first().and_then(Value::as_str),
                args.get(1).and_then(Value::as_str),
            ) else {
                return Value::Bool(false);
            };
            compile_regex(pat)
                .map(|re| Value::Bool(re.is_match(s)))
                .unwrap_or(Value::Bool(false))
        }
        "regexp_replace" => {
            if args.len() < 3 {
                return Value::Null;
            }
            match (args[0].as_str(), args[1].as_str(), args[2].as_str()) {
                (Some(s), Some(pat), Some(rep)) => compile_regex(pat)
                    .map(|re| Value::Str(re.replace_all(s, rep).into_owned()))
                    .unwrap_or(Value::Null),
                _ => Value::Null,
            }
        }
        "regexp_extract" => {
            let (Some(s), Some(pat)) = (
                args.first().and_then(Value::as_str),
                args.get(1).and_then(Value::as_str),
            ) else {
                return Value::Null;
            };
            let group = args.get(2).and_then(Value::as_i64).unwrap_or(0).max(0) as usize;
            compile_regex(pat)
                .and_then(|re| re.captures(s))
                .and_then(|caps| caps.get(group))
                .map(|m| Value::Str(m.as_str().to_string()))
                .unwrap_or(Value::Null)
        }
        "left" => str_int_fn(args, |chars, n| {
            chars[..(n.max(0) as usize).min(chars.len())]
                .iter()
                .collect()
        }),
        "right" => str_int_fn(args, |chars, n| {
            let n = (n.max(0) as usize).min(chars.len());
            chars[chars.len() - n..].iter().collect()
        }),
        "lpad" => pad_fn(args, true),
        "rpad" => pad_fn(args, false),
        "reverse" => str_fn(args, |s| s.chars().rev().collect()),
        "repeat" => {
            if args.len() < 2 {
                return Value::Null;
            }
            match (args[0].as_str(), args[1].as_i64()) {
                (Some(s), Some(n)) => Value::Str(s.repeat(n.max(0) as usize)),
                _ => Value::Null,
            }
        }
        "initcap" => str_fn(args, |s| {
            s.split(' ')
                .map(|word| {
                    let mut c = word.chars();
                    match c.next() {
                        Some(first) => {
                            first.to_uppercase().collect::<String>() + &c.as_str().to_lowercase()
                        }
                        None => String::new(),
                    }
                })
                .collect::<Vec<_>>()
                .join(" ")
        }),
        "ascii" => args
            .first()
            .and_then(|v| v.as_str())
            .and_then(|s| s.chars().next())
            .map(|c| Value::Int(c as i64))
            .unwrap_or(Value::Null),
        "chr" => args
            .first()
            .and_then(|v| v.as_i64())
            .and_then(|n| u32::try_from(n).ok())
            .and_then(char::from_u32)
            .map(|c| Value::Str(c.to_string()))
            .unwrap_or(Value::Null),
        "instr" => {
            if args.len() < 2 {
                return Value::Null;
            }
            match (args[0].as_str(), args[1].as_str()) {
                (Some(haystack), Some(needle)) => {
                    let hay_chars: Vec<char> = haystack.chars().collect();
                    let needle_chars: Vec<char> = needle.chars().collect();
                    if needle_chars.is_empty() {
                        return Value::Int(0);
                    }
                    for i in 0..=hay_chars.len().saturating_sub(needle_chars.len()) {
                        if hay_chars[i..i + needle_chars.len()] == needle_chars[..] {
                            return Value::Int(i as i64 + 1);
                        }
                    }
                    Value::Int(0)
                }
                _ => Value::Null,
            }
        }
        "split_part" => {
            if args.len() < 3 {
                return Value::Null;
            }
            match (args[0].as_str(), args[1].as_str(), args[2].as_i64()) {
                (Some(s), Some(delim), Some(n)) if n > 0 => s
                    .split(delim)
                    .nth((n - 1) as usize)
                    .map(|p| Value::Str(p.to_string()))
                    .unwrap_or(Value::Null),
                _ => Value::Null,
            }
        }

        "abs" => num_fn(args, |n| n.abs(), |n| n.abs()),
        "round" => {
            let n = match args.first().and_then(|v| v.as_f64()) {
                Some(n) => n,
                None => return Value::Null,
            };
            let scale = args.get(1).and_then(|v| v.as_i64()).unwrap_or(0);
            let factor = 10f64.powi(scale as i32);
            let result = (n * factor).round() / factor;
            if scale <= 0 {
                Value::Int(result as i64)
            } else {
                Value::Float(result)
            }
        }
        "ceil" | "ceiling" => float_fn(args, |n| n.ceil()),
        "floor" => float_fn(args, |n| n.floor()),
        "trunc" | "truncate" => {
            let n = match args.first().and_then(|v| v.as_f64()) {
                Some(n) => n,
                None => return Value::Null,
            };
            let scale = args.get(1).and_then(|v| v.as_i64()).unwrap_or(0);
            let factor = 10f64.powi(scale as i32);
            let result = (n * factor).trunc() / factor;
            if scale <= 0 {
                Value::Int(result as i64)
            } else {
                Value::Float(result)
            }
        }
        "mod" => {
            if args.len() < 2 {
                return Value::Null;
            }
            match (&args[0], &args[1]) {
                (Value::Int(a), Value::Int(b)) if *b != 0 => Value::Int(a % b),
                _ => match (args[0].as_f64(), args[1].as_f64()) {
                    (Some(a), Some(b)) if b != 0.0 => Value::Float(a % b),
                    _ => Value::Null,
                },
            }
        }
        "power" | "pow" => {
            if args.len() < 2 {
                return Value::Null;
            }
            match (args[0].as_f64(), args[1].as_f64()) {
                (Some(a), Some(b)) => Value::Float(a.powf(b)),
                _ => Value::Null,
            }
        }
        "sqrt" => float_fn(args, |n| n.sqrt()),
        "sign" => float_fn(args, |n| {
            if n > 0.0 {
                1.0
            } else if n < 0.0 {
                -1.0
            } else {
                0.0
            }
        }),
        "exp" => float_fn(args, |n| n.exp()),
        "ln" => float_fn(args, |n| n.ln()),
        "log10" => float_fn(args, |n| n.log10()),
        "log2" => float_fn(args, |n| n.log2()),
        "log" => {
            let n = match args.first().and_then(|v| v.as_f64()) {
                Some(n) => n,
                None => return Value::Null,
            };
            match args.get(1).and_then(|v| v.as_f64()) {
                Some(base) => Value::Float(n.log(base)),
                None => Value::Float(n.log10()),
            }
        }
        "pi" => Value::Float(std::f64::consts::PI),
        "greatest" => args
            .iter()
            .filter(|v| !matches!(v, Value::Null))
            .cloned()
            .max_by(|a, b| a.cmp_val(b).unwrap_or(std::cmp::Ordering::Equal))
            .unwrap_or(Value::Null),
        "least" => args
            .iter()
            .filter(|v| !matches!(v, Value::Null))
            .cloned()
            .min_by(|a, b| a.cmp_val(b).unwrap_or(std::cmp::Ordering::Equal))
            .unwrap_or(Value::Null),

        "coalesce" | "ifnull" => args
            .iter()
            .find(|v| !matches!(v, Value::Null))
            .cloned()
            .unwrap_or(Value::Null),
        "nullif" => {
            if args.len() < 2 {
                return Value::Null;
            }
            if args[0] == args[1] {
                Value::Null
            } else {
                args[0].clone()
            }
        }

        "typeof" => Value::Str(
            match args.first() {
                Some(Value::Str(_)) => "text",
                Some(Value::Int(_)) => "integer",
                Some(Value::Float(_)) => "float",
                Some(Value::Bool(_)) => "boolean",
                Some(Value::Null) | None => "null",
            }
            .to_string(),
        ),
        "now" | "current_timestamp" => Value::Str(current_datetime_utc(true, true)),
        "current_date" => Value::Str(current_datetime_utc(true, false)),
        "current_time" => Value::Str(current_datetime_utc(false, true)),
        "date_trunc" => match (
            args.first().and_then(Value::as_str),
            args.get(1).and_then(Value::as_str),
        ) {
            (Some(unit), Some(date)) => eval_date_trunc(unit, date),
            _ => Value::Null,
        },
        "date_diff" => {
            match (
                args.first().and_then(Value::as_str),
                args.get(1).and_then(Value::as_str),
                args.get(2).and_then(Value::as_str),
            ) {
                (Some(unit), Some(d1), Some(d2)) => eval_date_diff(unit, d1, d2),
                _ => Value::Null,
            }
        }
        "date_add" => match (
            args.first().and_then(Value::as_str),
            args.get(1).and_then(Value::as_i64),
            args.get(2).and_then(Value::as_str),
        ) {
            (Some(date), Some(n), Some(unit)) => eval_date_add(date, n, unit),
            (Some(date), Some(n), None) => eval_date_add(date, n, "day"),
            _ => Value::Null,
        },
        "date_sub" => match (
            args.first().and_then(Value::as_str),
            args.get(1).and_then(Value::as_i64),
            args.get(2).and_then(Value::as_str),
        ) {
            (Some(date), Some(n), Some(unit)) => eval_date_add(date, -n, unit),
            (Some(date), Some(n), None) => eval_date_add(date, -n, "day"),
            _ => Value::Null,
        },
        "strftime" => match (
            args.first().and_then(Value::as_str),
            args.get(1).and_then(Value::as_str),
        ) {
            (Some(fmt), Some(date)) => eval_strftime(fmt, date),
            _ => Value::Null,
        },
        _ => Value::Null,
    }
}

fn str_fn(args: &[Value], f: impl Fn(&str) -> String) -> Value {
    args.first()
        .and_then(|v| v.as_str())
        .map(|s| Value::Str(f(s)))
        .unwrap_or(Value::Null)
}

fn str_int_fn(args: &[Value], f: impl Fn(&[char], i64) -> String) -> Value {
    if args.len() < 2 {
        return Value::Null;
    }
    match (args[0].as_str(), args[1].as_i64()) {
        (Some(s), Some(n)) => {
            let chars: Vec<char> = s.chars().collect();
            Value::Str(f(&chars, n))
        }
        _ => Value::Null,
    }
}

fn num_fn(args: &[Value], int_f: impl Fn(i64) -> i64, flt_f: impl Fn(f64) -> f64) -> Value {
    match args.first() {
        Some(Value::Int(n)) => Value::Int(int_f(*n)),
        Some(v) => v
            .as_f64()
            .map(|n| Value::Float(flt_f(n)))
            .unwrap_or(Value::Null),
        None => Value::Null,
    }
}

fn float_fn(args: &[Value], f: impl Fn(f64) -> f64) -> Value {
    args.first()
        .and_then(|v| v.as_f64())
        .map(|n| Value::Float(f(n)))
        .unwrap_or(Value::Null)
}

/// Builds the set of characters TRIM/LTRIM/RTRIM should strip, defaulting to
/// whitespace when no explicit character argument is given.
fn trim_char_set(args: &[Value], chars_idx: usize) -> Vec<char> {
    args.get(chars_idx)
        .and_then(|v| v.as_str())
        .map(|s| s.chars().collect())
        .unwrap_or_else(|| vec![' ', '\t', '\n', '\r'])
}

fn pad_fn(args: &[Value], left: bool) -> Value {
    if args.len() < 2 {
        return Value::Null;
    }
    let s = match args[0].as_str() {
        Some(s) => s,
        None => return Value::Null,
    };
    let target_len = match args[1].as_i64() {
        Some(n) => n.max(0) as usize,
        None => return Value::Null,
    };
    let pad_str = args.get(2).and_then(|v| v.as_str()).unwrap_or(" ");
    let mut chars: Vec<char> = s.chars().collect();
    if chars.len() >= target_len {
        chars.truncate(target_len);
        return Value::Str(chars.into_iter().collect());
    }
    if pad_str.is_empty() {
        return Value::Str(s.to_string());
    }
    let pad_chars: Vec<char> = pad_str.chars().collect();
    let needed = target_len - chars.len();
    let padding: Vec<char> = pad_chars.iter().cycle().take(needed).copied().collect();
    if left {
        Value::Str(padding.into_iter().chain(chars).collect())
    } else {
        chars.extend(padding);
        Value::Str(chars.into_iter().collect())
    }
}

fn eval_mq_scalar(program: &str, content: &str) -> Value {
    let mut engine = DefaultEngine::default();
    engine.load_builtin_module();
    let input = match parse_markdown_input(content) {
        Ok(i) => i,
        Err(_) => return Value::Null,
    };
    match engine.eval(program, input.into_iter()) {
        Ok(output) => {
            let parts: Vec<String> = output
                .compact()
                .into_iter()
                .map(|v| v.to_string())
                .collect();
            if parts.is_empty() {
                Value::Null
            } else {
                Value::Str(parts.join("\n"))
            }
        }
        Err(_) => Value::Null,
    }
}

/// Parses a non-empty JSON array of numbers; `None` for anything else.
fn parse_vector(value: &Value) -> Option<Vec<f64>> {
    let serde_json::Value::Array(items) = serde_json::from_str(value.as_str()?).ok()? else {
        return None;
    };
    if items.is_empty() {
        return None;
    }
    items.iter().map(serde_json::Value::as_f64).collect()
}

/// Looks up `path` (`key`, `$.a.b`, `a[0].b`) in `json`.
///
/// Scalars map to the matching SQL value; objects and arrays come back as
/// compact JSON text. Invalid JSON or a missing path gives `NULL`.
fn extract_json_path(json: &str, path: &str) -> Value {
    let Ok(mut cur) = serde_json::from_str::<serde_json::Value>(json) else {
        return Value::Null;
    };
    let path = path.trim().trim_start_matches('$').trim_start_matches('.');
    for part in path.split('.').filter(|p| !p.is_empty()) {
        let (key, mut rest) = match part.find('[') {
            Some(i) => (&part[..i], &part[i..]),
            None => (part, ""),
        };
        let key = key.trim_matches('"');
        if !key.is_empty() {
            let Some(next) = cur.get(key) else {
                return Value::Null;
            };
            cur = next.clone();
        }
        while let Some(inner) = rest.strip_prefix('[') {
            let Some((idx, tail)) = inner.split_once(']') else {
                return Value::Null;
            };
            let Some(next) = idx.trim().parse::<usize>().ok().and_then(|i| cur.get(i)) else {
                return Value::Null;
            };
            cur = next.clone();
            rest = tail;
        }
        if !rest.is_empty() {
            return Value::Null;
        }
    }
    match cur {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(b),
        serde_json::Value::Number(n) => match n.as_i64() {
            Some(i) => Value::Int(i),
            None => n.as_f64().map_or(Value::Null, Value::Float),
        },
        serde_json::Value::String(s) => Value::Str(s),
        other => Value::Str(other.to_string()),
    }
}
