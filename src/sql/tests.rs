use super::engine::SqlEngine;
use super::output::QueryOutput;
use super::plan::candidate_hints_for_where;
use crate::DocumentStore;
use crate::block::BlockType;
use crate::indexes::{IndexHint, TokenizerKind};
use rstest::rstest;
use sqlparser::ast::{SetExpr, Statement};
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser;

fn make_store() -> DocumentStore {
    let mut s = DocumentStore::new();
    s.add_str(
        "# Doc\n\n## Architecture\n\nDetails\n\n```rust\nfn main(){}\n```\n\n## Other\n\nOther\n",
    )
    .unwrap();
    s
}

// Doc B (no code, depth 1) sits between two rust/depth-3 docs.
fn make_multi_doc_store() -> DocumentStore {
    let mut s = DocumentStore::new();
    s.add_str("# A\n\n```rust\nfn a(){}\n```\n").unwrap();
    s.add_str("# B\n\nParagraph\n").unwrap();
    s.add_str("# C\n\n## C2\n\n### C3\n\n```rust\nfn c(){}\n```\n")
        .unwrap();
    s
}

#[test]
fn test_sql_documents_zone_map_columns() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute(
            "SELECT block_count, max_heading_depth, code_languages, frontmatter_keys \
             FROM documents",
        )
        .unwrap();
    assert_eq!(
        out.rows,
        vec![vec![
            "6".to_string(),
            "2".to_string(),
            "[\"rust\"]".to_string(),
            "[]".to_string(),
        ]]
    );
}

#[test]
fn test_sql_documents_properties_column() {
    let mut store = DocumentStore::new();
    store
        .add_str("---\ntitle: T\nowner: alice\nrank: 3\ntags: [a, b]\n---\n\n# H\n")
        .unwrap();
    store.add_str("# No frontmatter\n").unwrap();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute("SELECT properties FROM documents ORDER BY id")
        .unwrap();
    assert_eq!(
        out.rows,
        vec![
            vec![
                "{\"owner\":\"alice\",\"rank\":3,\"tags\":[\"a\",\"b\"],\"title\":\"T\"}"
                    .to_string()
            ],
            vec!["{}".to_string()],
        ]
    );
    let out = engine
        .execute("SELECT title FROM documents WHERE json_extract(properties, 'owner') = 'alice'")
        .unwrap();
    assert_eq!(out.rows, vec![vec!["T".to_string()]]);
}

#[test]
fn test_sql_links_table() {
    let mut store = DocumentStore::new();
    store
        .add_str(
            "# T\n\nSee [docs](https://a.com/x \"tt\") and ![img](p.png) and [ref][r].\n\n\
             - item [l2](other.md#sec)\n\n> quote [q](q.md)\n\n[r]: https://r.com\n",
        )
        .unwrap();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute("SELECT kind, url, text, title FROM links ORDER BY block_id, kind")
        .unwrap();
    let row = |k: &str, u: &str, t: &str, ti: &str| {
        vec![k.to_string(), u.to_string(), t.to_string(), ti.to_string()]
    };
    assert_eq!(
        out.rows,
        vec![
            row("link", "https://a.com/x", "docs", "tt"),
            row("image", "p.png", "img", ""),
            row("ref", "r", "ref", ""),
            row("link", "other.md#sec", "l2", ""),
            row("link", "q.md", "q", ""),
        ]
    );
}

#[test]
fn test_sql_links_join_blocks() {
    let mut store = DocumentStore::new();
    store.add_str("# A\n\n[x](x.md)\n").unwrap();
    store.add_str("# B\n\nplain\n").unwrap();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute(
            "SELECT b.content FROM links l JOIN blocks b \
             ON b.document_id = l.document_id AND b.id = l.block_id",
        )
        .unwrap();
    assert_eq!(out.rows.len(), 1);
}

#[test]
fn test_sql_select_all_blocks() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute("SELECT block_type, content FROM blocks ORDER BY pre")
        .unwrap();
    assert!(!out.rows.is_empty());
}

#[test]
fn test_sql_heading_filter() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute("SELECT content FROM blocks WHERE block_type = 'heading' ORDER BY pre")
        .unwrap();
    assert_eq!(out.rows.len(), 3);
}

#[test]
fn test_sql_under_function() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute(
            "SELECT b.content FROM blocks b
         WHERE under(b.pre, b.post,
           (SELECT pre FROM blocks WHERE block_type='heading' AND content='Architecture'),
           (SELECT post FROM blocks WHERE block_type='heading' AND content='Architecture')
         )",
        )
        .unwrap();
    assert_eq!(out.rows.len(), 2);
}

#[test]
fn test_query_output_table() {
    let out = QueryOutput {
        columns: vec!["id".to_string(), "type".to_string()],
        rows: vec![
            vec!["1".to_string(), "heading".to_string()],
            vec!["2".to_string(), "paragraph".to_string()],
        ],
    };
    let table = out.to_table();
    assert!(table.contains("heading"));
    assert!(table.contains("paragraph"));
    assert!(table.contains("2 rows"));
}

#[test]
fn test_sql_count_aggregate() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute("SELECT count(*) FROM blocks WHERE block_type = 'heading'")
        .unwrap();
    assert_eq!(out.rows.len(), 1);
    assert_eq!(out.rows[0][0], "3");
}

#[test]
fn test_sql_count_with_grouped_column() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute("SELECT block_type, count(*) FROM blocks GROUP BY block_type")
        .unwrap();
    assert!(!out.rows.is_empty());
}

#[test]
fn test_sql_count_with_ungrouped_column_errors() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let err = engine
        .execute("SELECT count(*), content FROM blocks")
        .unwrap_err();
    assert!(err.to_string().contains("GROUP BY"));
}

#[test]
fn test_sql_limit() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute("SELECT content FROM blocks LIMIT 2")
        .unwrap();
    assert_eq!(out.rows.len(), 2);
}

#[test]
fn test_sql_offset() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let all = engine
        .execute("SELECT content FROM blocks ORDER BY pre")
        .unwrap();
    let offset = engine
        .execute("SELECT content FROM blocks ORDER BY pre LIMIT 100 OFFSET 2")
        .unwrap();
    assert_eq!(offset.rows, all.rows[2..]);
}

#[test]
fn test_sql_offset_past_end_returns_empty() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute("SELECT content FROM blocks LIMIT 100 OFFSET 1000")
        .unwrap();
    assert!(out.rows.is_empty());
}

#[test]
fn test_sql_having() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute(
            "SELECT block_type, count(*) FROM blocks GROUP BY block_type \
             HAVING count(*) > 1 ORDER BY block_type",
        )
        .unwrap();
    assert_eq!(
        out.rows,
        vec![
            vec!["heading".to_string(), "3".to_string()],
            vec!["paragraph".to_string(), "2".to_string()],
        ]
    );
}

#[test]
fn test_sql_union() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute(
            "SELECT content FROM blocks WHERE block_type = 'heading' \
             UNION SELECT content FROM blocks WHERE block_type = 'heading'",
        )
        .unwrap();
    assert_eq!(out.rows.len(), 3);

    let out_all = engine
        .execute(
            "SELECT content FROM blocks WHERE block_type = 'heading' \
             UNION ALL SELECT content FROM blocks WHERE block_type = 'heading'",
        )
        .unwrap();
    assert_eq!(out_all.rows.len(), 6);
}

#[test]
fn test_sql_intersect_and_except() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let intersect = engine
        .execute(
            "SELECT content FROM blocks WHERE block_type = 'heading' \
             INTERSECT SELECT content FROM blocks WHERE content = 'Architecture'",
        )
        .unwrap();
    assert_eq!(intersect.rows, vec![vec!["Architecture".to_string()]]);

    let except = engine
        .execute(
            "SELECT content FROM blocks WHERE block_type = 'heading' \
             EXCEPT SELECT content FROM blocks WHERE content = 'Architecture'",
        )
        .unwrap();
    assert_eq!(except.rows.len(), 2);
    assert!(!except.rows.iter().any(|r| r[0] == "Architecture"));
}

#[test]
fn test_sql_union_column_count_mismatch_errors() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let err = engine
        .execute("SELECT content FROM blocks UNION SELECT content, block_type FROM blocks")
        .unwrap_err();
    assert!(err.to_string().contains("column"));
}

#[test]
fn test_sql_group_by_order_by_is_applied() {
    let store = make_multi_doc_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute(
            "SELECT document_id, COUNT(*) AS n FROM blocks \
             GROUP BY document_id ORDER BY document_id DESC",
        )
        .unwrap();
    let ids: Vec<i64> = out.rows.iter().map(|r| r[0].parse().unwrap()).collect();
    assert_eq!(ids, vec![2, 1, 0]);
}

// `n` names the `COUNT(*) AS n` alias (not an input column), so it must still
// resolve rather than sorting as if every group were equal.
#[test]
fn test_sql_group_by_order_by_select_alias() {
    let store = make_multi_doc_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute(
            "SELECT document_id, COUNT(*) AS n FROM blocks \
             GROUP BY document_id ORDER BY n DESC, document_id",
        )
        .unwrap();
    let ns: Vec<i64> = out.rows.iter().map(|r| r[1].parse().unwrap()).collect();
    let mut sorted = ns.clone();
    sorted.sort_by(|a, b| b.cmp(a));
    assert_eq!(ns, sorted);
}

#[test]
fn test_sql_in_subquery() {
    let store = make_multi_doc_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute(
            "SELECT id FROM documents \
             WHERE id IN (SELECT document_id FROM blocks WHERE lang = 'rust') \
             ORDER BY id",
        )
        .unwrap();
    // Docs A (0) and C (2) have a rust block; B (1) doesn't.
    assert_eq!(out.rows, vec![vec!["0".to_string()], vec!["2".to_string()]]);

    let out_not = engine
        .execute(
            "SELECT id FROM documents \
             WHERE id NOT IN (SELECT document_id FROM blocks WHERE lang = 'rust') \
             ORDER BY id",
        )
        .unwrap();
    assert_eq!(out_not.rows, vec![vec!["1".to_string()]]);
}

#[test]
fn test_sql_correlated_exists_and_not_exists() {
    let store = make_multi_doc_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute(
            "SELECT d.id FROM documents d \
             WHERE EXISTS (SELECT 1 FROM blocks b WHERE b.document_id = d.id AND b.lang = 'rust') \
             ORDER BY d.id",
        )
        .unwrap();
    assert_eq!(out.rows, vec![vec!["0".to_string()], vec!["2".to_string()]]);

    let out_not = engine
        .execute(
            "SELECT d.id FROM documents d \
             WHERE NOT EXISTS (SELECT 1 FROM blocks b WHERE b.document_id = d.id AND b.lang = 'rust') \
             ORDER BY d.id",
        )
        .unwrap();
    assert_eq!(out_not.rows, vec![vec!["1".to_string()]]);
}

#[test]
fn test_sql_left_join_null_extends_unmatched_rows() {
    let store = make_multi_doc_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute(
            "SELECT d.id, r.lang FROM documents d \
             LEFT JOIN blocks r ON r.document_id = d.id AND r.lang = 'rust' \
             ORDER BY d.id",
        )
        .unwrap();
    assert_eq!(
        out.rows,
        vec![
            vec!["0".to_string(), "rust".to_string()],
            vec!["1".to_string(), "NULL".to_string()],
            vec!["2".to_string(), "rust".to_string()],
        ]
    );

    // Anti-join: rows from the left side with no matching right row.
    let anti = engine
        .execute(
            "SELECT d.id FROM documents d \
             LEFT JOIN blocks r ON r.document_id = d.id AND r.lang = 'rust' \
             WHERE r.id IS NULL",
        )
        .unwrap();
    assert_eq!(anti.rows, vec![vec!["1".to_string()]]);
}

#[test]
fn test_sql_unknown_column_errors() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();

    let err = engine
        .execute("SELECT path, no_such_column FROM documents")
        .unwrap_err();
    assert!(err.to_string().contains("unknown column: no_such_column"));

    let err = engine
        .execute("SELECT path FROM documents WHERE no_such_column = 'x'")
        .unwrap_err();
    assert!(err.to_string().contains("unknown column: no_such_column"));

    let err = engine
        .execute("SELECT path FROM documents ORDER BY no_such_column")
        .unwrap_err();
    assert!(err.to_string().contains("unknown column: no_such_column"));

    // Real columns, aliases, and ordinal ORDER BY must be unaffected.
    engine
        .execute("SELECT path FROM documents WHERE title IS NOT NULL")
        .unwrap();
    engine
        .execute("SELECT path FROM documents ORDER BY 1")
        .unwrap();
}

#[test]
fn test_sql_date_functions() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute(
            "SELECT date_trunc('month', '2024-03-15'), \
             date_diff('day', '2024-01-01', '2024-03-15'), \
             date_diff('month', '2024-01-01', '2024-03-15'), \
             date_add('2024-01-31', 1, 'month'), \
             date_sub('2024-03-15', 10, 'day'), \
             strftime('%Y/%m/%d', '2024-03-15')",
        )
        .unwrap();
    assert_eq!(
        out.rows,
        vec![vec![
            "2024-03-01".to_string(),
            "74".to_string(),
            "2".to_string(),
            "2024-02-29".to_string(),
            "2024-03-05".to_string(),
            "2024/03/15".to_string(),
        ]]
    );
}

#[test]
fn test_sql_extract() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute("SELECT EXTRACT(YEAR FROM '2024-03-15T10:30:00'), EXTRACT(HOUR FROM '2024-03-15T10:30:00')")
        .unwrap();
    assert_eq!(out.rows, vec![vec!["2024".to_string(), "10".to_string()]]);
}

#[test]
fn test_sql_regexp_operator() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute("SELECT content FROM blocks WHERE content REGEXP '^Arch.*ure$'")
        .unwrap();
    assert_eq!(out.rows, vec![vec!["Architecture".to_string()]]);

    let negated = engine
        .execute("SELECT content FROM blocks WHERE content NOT REGEXP '^Arch'")
        .unwrap();
    assert!(!negated.rows.iter().any(|r| r[0] == "Architecture"));
}

#[test]
fn test_sql_regexp_functions() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute(
            "SELECT regexp_like('hello world', 'wor.d'), \
             regexp_replace('hello world', 'o', '0'), \
             regexp_extract('id-1234', '(\\d+)', 1)",
        )
        .unwrap();
    assert_eq!(
        out.rows,
        vec![vec![
            "true".to_string(),
            "hell0 w0rld".to_string(),
            "1234".to_string(),
        ]]
    );
}

#[test]
fn test_sql_like() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute("SELECT content FROM blocks WHERE content LIKE '%chitect%'")
        .unwrap();
    assert!(!out.rows.is_empty());
}

#[test]
fn test_sql_order_by_desc() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute("SELECT content FROM blocks ORDER BY pre DESC LIMIT 1")
        .unwrap();
    assert_eq!(out.rows.len(), 1);
}

#[test]
fn test_sql_engine_zero_copy() {
    let mut store = DocumentStore::new();
    for _ in 0..100 {
        store.add_str("# Heading\n\nParagraph text\n").unwrap();
    }
    let start = std::time::Instant::now();
    let _engine = SqlEngine::new(&store).unwrap();
    let elapsed = start.elapsed();
    // Bound loosened from 1ms to 5ms when `TermIndex` (a fourth
    // per-document index) was added; still catches anything
    // pathological (e.g. accidental file I/O or O(n^2) behaviour) while
    // tolerating cold-start allocator/thread warmup noise on the first
    // test invocation in a fresh process.
    assert!(
        elapsed.as_micros() < 5000,
        "SqlEngine::new took {}us — should be cheap",
        elapsed.as_micros()
    );
}

// make_store() produces:
//   "# Doc\n\n## Architecture\n\nDetails\n\n```rust\nfn main(){}\n```\n\n## Other\n\nOther\n"
// → heading×3, paragraph×2, code×1  (6 blocks total)

#[rstest]
#[case("SELECT content FROM blocks WHERE block_type = 'heading'", 3)]
#[case("SELECT content FROM blocks WHERE block_type = 'paragraph'", 2)]
#[case("SELECT content FROM blocks WHERE block_type = 'code'", 1)]
#[case("SELECT content FROM blocks WHERE block_type = 'list'", 0)]
fn test_sql_where_block_type_param(#[case] sql: &str, #[case] expected: usize) {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    assert_eq!(engine.execute(sql).unwrap().rows.len(), expected);
}

#[rstest]
#[case("SELECT content FROM blocks WHERE content LIKE '%Doc%'", 1)]
#[case("SELECT content FROM blocks WHERE content LIKE '%chitect%'", 1)]
#[case("SELECT content FROM blocks WHERE content LIKE '%Other%'", 2)]
#[case("SELECT content FROM blocks WHERE content LIKE '%Details%'", 1)]
#[case("SELECT content FROM blocks WHERE content LIKE '%nonexistent%'", 0)]
fn test_sql_like_pattern_param(#[case] sql: &str, #[case] expected: usize) {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    assert_eq!(engine.execute(sql).unwrap().rows.len(), expected);
}

#[rstest]
#[case("SELECT content FROM blocks LIMIT 1", 1)]
#[case("SELECT content FROM blocks LIMIT 3", 3)]
#[case("SELECT content FROM blocks LIMIT 5", 5)]
#[case("SELECT content FROM blocks LIMIT 1000", 6)]
fn test_sql_limit_row_count_param(#[case] sql: &str, #[case] expected: usize) {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    assert_eq!(engine.execute(sql).unwrap().rows.len(), expected);
}

#[rstest]
#[case("SELECT count(*) FROM blocks", "6")]
#[case("SELECT count(*) FROM blocks WHERE block_type = 'heading'", "3")]
#[case("SELECT count(*) FROM blocks WHERE block_type = 'code'", "1")]
fn test_sql_count_aggregate_param(#[case] sql: &str, #[case] expected: &str) {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine.execute(sql).unwrap();
    assert_eq!(out.rows.len(), 1);
    assert_eq!(out.rows[0][0], expected);
}

// depth = 0 should return all non-heading blocks (paragraphs + code), not 0 rows
#[test]
fn test_sql_depth_zero_returns_non_headings() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute("SELECT content FROM blocks WHERE depth = 0")
        .unwrap();
    // make_store has 2 paragraphs + 1 code block = 3 non-heading blocks
    assert_eq!(out.rows.len(), 3, "depth=0 must return non-heading blocks");
}

// lang = '' should return non-code blocks (paragraph, heading blocks have empty lang)
#[test]
fn test_sql_empty_lang_returns_non_code_blocks() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute("SELECT block_type FROM blocks WHERE lang = ''")
        .unwrap();
    // make_store: 3 headings + 2 paragraphs = 5 blocks with no lang
    assert_eq!(out.rows.len(), 5, "lang='' must return non-code blocks");
}

// to_table() must not let newlines inside cells break the table row structure
#[test]
fn test_to_table_newline_in_cell() {
    let out = QueryOutput {
        columns: vec!["content".to_string()],
        rows: vec![
            vec!["line one\nline two".to_string()],
            vec!["plain".to_string()],
        ],
    };
    let table = out.to_table();
    // Lines that start with '│' = header + 2 data rows = 3 (no extra split)
    let bar_lines: Vec<&str> = table.lines().filter(|l| l.starts_with('│')).collect();
    assert_eq!(
        bar_lines.len(),
        3,
        "newline in cell must not produce extra table rows"
    );
    // The first data row (index 1, after the header) must contain the normalised content
    assert!(bar_lines[1].contains("line one line two"));
}

// register_table / custom table query
#[test]
fn test_custom_table_query() {
    let mut store = DocumentStore::new();
    store.register_table(
        "kv",
        vec!["key".to_string(), "value".to_string()],
        vec![
            vec!["foo".to_string(), "bar".to_string()],
            vec!["hello".to_string(), "world".to_string()],
        ],
    );
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute("SELECT key, value FROM kv WHERE key = 'hello'")
        .unwrap();
    assert_eq!(out.rows.len(), 1);
    assert_eq!(out.rows[0][1], "world");
}

// CREATE TABLE (empty) then INSERT then SELECT
#[test]
fn test_ddl_create_insert_select() {
    let store = DocumentStore::new();
    let engine = SqlEngine::new(&store).unwrap();

    // create
    engine
        .execute("CREATE TABLE notes (id TEXT, body TEXT)")
        .unwrap();
    // insert two rows
    engine
        .execute("INSERT INTO notes VALUES ('1', 'hello')")
        .unwrap();
    engine
        .execute("INSERT INTO notes VALUES ('2', 'world')")
        .unwrap();
    // select with filter
    let out = engine
        .execute("SELECT body FROM notes WHERE id = '1'")
        .unwrap();
    assert_eq!(out.rows.len(), 1);
    assert_eq!(out.rows[0][0], "hello");
    // total rows
    let all = engine.execute("SELECT * FROM notes").unwrap();
    assert_eq!(all.rows.len(), 2);
}

#[test]
fn test_ddl_primary_key_rejects_null_and_duplicate() {
    let store = DocumentStore::new();
    let engine = SqlEngine::new(&store).unwrap();
    engine
        .execute("CREATE TABLE notes (id TEXT PRIMARY KEY, body TEXT)")
        .unwrap();
    engine
        .execute("INSERT INTO notes VALUES ('1', 'hello')")
        .unwrap();

    let dup = engine
        .execute("INSERT INTO notes VALUES ('1', 'again')")
        .unwrap_err();
    assert!(dup.to_string().contains("UNIQUE"));

    let null = engine
        .execute("INSERT INTO notes (body) VALUES ('no id')")
        .unwrap_err();
    assert!(null.to_string().contains("NOT NULL"));

    // Rejected batch must not leave partial rows committed.
    let all = engine.execute("SELECT * FROM notes").unwrap();
    assert_eq!(all.rows.len(), 1);
}

#[test]
fn test_ddl_table_level_unique_constraint() {
    let store = DocumentStore::new();
    let engine = SqlEngine::new(&store).unwrap();
    engine
        .execute("CREATE TABLE pairs (a TEXT, b TEXT, UNIQUE (a, b))")
        .unwrap();
    engine
        .execute("INSERT INTO pairs VALUES ('x', 'y')")
        .unwrap();
    // Same 'a' alone is fine; uniqueness is on the (a, b) pair.
    engine
        .execute("INSERT INTO pairs VALUES ('x', 'z')")
        .unwrap();
    let dup = engine
        .execute("INSERT INTO pairs VALUES ('x', 'y')")
        .unwrap_err();
    assert!(dup.to_string().contains("UNIQUE"));
}

// CREATE TABLE AS SELECT
#[test]
fn test_ddl_create_as_select() {
    let store = {
        let mut s = DocumentStore::new();
        s.add_str("# H1\n\n## H2\n\nParagraph\n").unwrap();
        s
    };
    let engine = SqlEngine::new(&store).unwrap();
    engine
        .execute(
            "CREATE TABLE headings AS \
             SELECT block_type, content FROM blocks WHERE block_type = 'heading'",
        )
        .unwrap();
    let out = engine.execute("SELECT content FROM headings").unwrap();
    assert_eq!(out.rows.len(), 2);
}

#[test]
fn test_ddl_create_as_select_rejects_existing_table() {
    let store = DocumentStore::new();
    let engine = SqlEngine::new(&store).unwrap();
    engine.execute("CREATE TABLE t AS SELECT 1 AS x").unwrap();
    let err = engine
        .execute("CREATE TABLE t AS SELECT 2 AS x")
        .unwrap_err();
    assert!(err.to_string().contains("already exists"));
    // The original table must survive the rejected statement.
    let out = engine.execute("SELECT x FROM t").unwrap();
    assert_eq!(out.rows, vec![vec!["1".to_string()]]);
}

#[test]
fn test_ddl_create_as_select_if_not_exists_keeps_existing() {
    let store = DocumentStore::new();
    let engine = SqlEngine::new(&store).unwrap();
    engine.execute("CREATE TABLE t AS SELECT 1 AS x").unwrap();
    let out = engine
        .execute("CREATE TABLE IF NOT EXISTS t AS SELECT 2 AS x")
        .unwrap();
    assert_eq!(out.rows[0][0], "already exists");
    let sel = engine.execute("SELECT x FROM t").unwrap();
    assert_eq!(sel.rows, vec![vec!["1".to_string()]]);
}

#[test]
fn test_ddl_create_as_select_or_replace_overwrites() {
    let store = DocumentStore::new();
    let engine = SqlEngine::new(&store).unwrap();
    engine.execute("CREATE TABLE t AS SELECT 1 AS x").unwrap();
    engine
        .execute("CREATE OR REPLACE TABLE t AS SELECT 2 AS x")
        .unwrap();
    let out = engine.execute("SELECT x FROM t").unwrap();
    assert_eq!(out.rows, vec![vec!["2".to_string()]]);
}

// DROP TABLE
#[test]
fn test_ddl_drop_table() {
    let store = DocumentStore::new();
    let engine = SqlEngine::new(&store).unwrap();
    engine.execute("CREATE TABLE tmp (x TEXT)").unwrap();
    engine.execute("DROP TABLE tmp").unwrap();
    let err = engine.execute("SELECT * FROM tmp").unwrap_err();
    assert!(err.to_string().contains("unknown table"));
}

// DROP TABLE IF EXISTS (must not error on missing table)
#[test]
fn test_ddl_drop_if_exists() {
    let store = DocumentStore::new();
    let engine = SqlEngine::new(&store).unwrap();
    engine
        .execute("DROP TABLE IF EXISTS no_such_table")
        .unwrap();
}

#[test]
fn test_ddl_drop_tables_multi_name_missing_is_atomic() {
    let store = DocumentStore::new();
    let engine = SqlEngine::new(&store).unwrap();
    engine.execute("CREATE TABLE a (x TEXT)").unwrap();
    let err = engine.execute("DROP TABLE a, missing").unwrap_err();
    assert!(err.to_string().contains("does not exist"));
    // 'a' must not have been dropped by the time 'missing' failed.
    engine.execute("SELECT * FROM a").unwrap();
}

#[test]
fn test_ddl_drop_views_multi_name_missing_is_atomic() {
    let store = DocumentStore::new();
    let engine = SqlEngine::new(&store).unwrap();
    engine.execute("CREATE VIEW a AS SELECT 1").unwrap();
    let err = engine.execute("DROP VIEW a, missing").unwrap_err();
    assert!(err.to_string().contains("does not exist"));
    // 'a' must not have been dropped by the time 'missing' failed.
    engine.execute("SELECT * FROM a").unwrap();
}

#[test]
fn concurrent_create_table_statements_all_survive_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("store.mq-db");
    DocumentStore::new().save(&db_path).unwrap();
    let store = DocumentStore::open(&db_path).unwrap();

    const N: usize = 16;
    std::thread::scope(|scope| {
        for i in 0..N {
            let store = &store;
            scope.spawn(move || {
                let engine = SqlEngine::new(store).unwrap();
                engine
                    .execute(&format!("CREATE TABLE t{i} (x INT)"))
                    .unwrap();
            });
        }
    });
    drop(store);

    let reopened = DocumentStore::open(&db_path).unwrap();
    let engine = SqlEngine::new(&reopened).unwrap();
    let out = engine.execute("SHOW TABLES").unwrap();
    for i in 0..N {
        assert!(
            out.rows.iter().any(|r| r[0] == format!("t{i}")),
            "table t{i} missing after reopen — a concurrent flush lost it"
        );
    }
}

#[test]
fn concurrent_create_table_and_view_with_same_name_do_not_both_win() {
    let store = DocumentStore::new();

    let barrier = std::sync::Barrier::new(2);
    let (table_result, view_result) = std::thread::scope(|scope| {
        let table_handle = scope.spawn(|| {
            let engine = SqlEngine::new(&store).unwrap();
            barrier.wait();
            engine.execute("CREATE TABLE dup (x INT)")
        });
        let view_handle = scope.spawn(|| {
            let engine = SqlEngine::new(&store).unwrap();
            barrier.wait();
            engine.execute("CREATE VIEW dup AS SELECT 1 AS x")
        });
        (table_handle.join().unwrap(), view_handle.join().unwrap())
    });

    assert_ne!(
        table_result.is_ok(),
        view_result.is_ok(),
        "exactly one of the racing CREATE TABLE / CREATE VIEW must win"
    );

    let engine = SqlEngine::new(&store).unwrap();
    let out = engine.execute("SHOW TABLES").unwrap();
    assert_eq!(
        out.rows.iter().filter(|r| r[0] == "dup").count(),
        1,
        "the catalog must contain exactly one 'dup' entry"
    );
}

// DESC blocks (built-in)
#[test]
fn test_desc_builtin() {
    let store = DocumentStore::new();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine.execute("DESC blocks").unwrap();
    assert_eq!(out.columns, vec!["column", "type"]);
    assert!(out.rows.iter().any(|r| r[0] == "block_type"));
    assert!(out.rows.iter().any(|r| r[0] == "content"));
}

// DESC custom table
#[test]
fn test_desc_custom() {
    let store = DocumentStore::new();
    let engine = SqlEngine::new(&store).unwrap();
    engine
        .execute("CREATE TABLE meta (k TEXT, v TEXT)")
        .unwrap();
    let out = engine.execute("DESC meta").unwrap();
    assert_eq!(out.rows.len(), 2);
    assert_eq!(out.rows[0][0], "k");
    assert_eq!(out.rows[1][0], "v");
}

// SHOW TABLES
#[test]
fn test_show_tables() {
    let store = DocumentStore::new();
    let engine = SqlEngine::new(&store).unwrap();
    engine.execute("CREATE TABLE extra (a TEXT)").unwrap();
    let out = engine.execute("SHOW TABLES").unwrap();
    let names: Vec<&str> = out.rows.iter().map(|r| r[0].as_str()).collect();
    assert!(names.contains(&"blocks"));
    assert!(names.contains(&"documents"));
    assert!(names.contains(&"extra"));
}

// mq() scalar function applied to a literal markdown string
#[test]
fn test_mq_scalar_function() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute("SELECT mq('.h1 | to_text', '# Hello\n\nWorld\n') AS title FROM blocks LIMIT 1")
        .unwrap();
    assert_eq!(out.rows.len(), 1);
    assert_eq!(out.rows[0][0], "Hello");
}

// mq() returns NULL when program produces no output
#[test]
fn test_mq_scalar_null_on_no_match() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute("SELECT mq('.h1', '## No h1 here\n') FROM blocks LIMIT 1")
        .unwrap();
    assert_eq!(out.rows.len(), 1);
    assert_eq!(out.rows[0][0], "NULL");
}

#[test]
fn match_function_true_for_all_terms_present() {
    let store = DocumentStore::new();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute("SELECT match('The quick brown fox', 'quick fox')")
        .unwrap();
    assert_eq!(out.rows[0][0], "true");
}

#[test]
fn match_function_false_if_any_term_missing() {
    let store = DocumentStore::new();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute("SELECT match('The quick brown fox', 'quick zebra')")
        .unwrap();
    assert_eq!(out.rows[0][0], "false");
}

#[test]
fn match_function_case_insensitive() {
    let store = DocumentStore::new();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute("SELECT match('Rust Programming', 'rust')")
        .unwrap();
    assert_eq!(out.rows[0][0], "true");
}

#[test]
fn score_function_ranks_denser_matches_higher() {
    let mut store = DocumentStore::new();
    store
        .add_str("# Doc\n\nrust rust rust other words here\n\nrust is fine\n")
        .unwrap();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute(
            "SELECT content FROM blocks WHERE block_type = 'paragraph'
             ORDER BY score(content, 'rust') DESC",
        )
        .unwrap();
    assert_eq!(out.rows[0][0], "rust rust rust other words here");
}

#[test]
fn bm25_ranks_rarer_term_higher_than_common_term() {
    let mut store = DocumentStore::new();
    store
        .add_str("# Doc\n\nwidget\n\ncommon\n\ncommon\n\ncommon\n\ncommon\n")
        .unwrap();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute(
            "SELECT content FROM blocks WHERE block_type = 'paragraph'
             ORDER BY bm25(content, 'widget common') DESC LIMIT 1",
        )
        .unwrap();
    assert_eq!(out.rows[0][0], "widget");
}

#[test]
fn bm25_without_bm25_in_sql_text_is_unaffected() {
    let mut store = DocumentStore::new();
    store.add_str("# Doc\n\nfoo bar\n").unwrap();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute("SELECT content FROM blocks WHERE block_type = 'paragraph'")
        .unwrap();
    assert_eq!(out.rows[0][0], "foo bar");
}

#[test]
fn where_match_uses_term_match_index_hint() {
    let stmts = Parser::parse_sql(
        &GenericDialect {},
        "SELECT * FROM blocks WHERE match(content, 'foo bar')",
    )
    .unwrap();
    let Statement::Query(q) = stmts.into_iter().next().unwrap() else {
        panic!("expected query")
    };
    let SetExpr::Select(select) = q.body.as_ref() else {
        panic!("expected select")
    };
    let candidates = candidate_hints_for_where(select.selection.as_ref().unwrap());
    assert_eq!(
        candidates,
        vec![IndexHint::TermMatch(vec![
            "foo".to_string(),
            "bar".to_string()
        ])]
    );
}

#[test]
fn where_match_and_block_type_combines_hints() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute(
            "SELECT content FROM blocks
             WHERE match(content, 'architecture') AND block_type = 'heading'",
        )
        .unwrap();
    assert_eq!(out.rows, vec![vec!["Architecture".to_string()]]);
}

#[test]
fn where_bare_match_skips_recheck_but_result_is_still_correct() {
    // "architecture" only tokenizes out of the heading block, so if the
    // recheck-skip path (bare `match()` fully covering WHERE) somehow
    // returned a false positive, this would catch it.
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute("SELECT content FROM blocks WHERE match(content, 'architecture')")
        .unwrap();
    assert_eq!(out.rows, vec![vec!["Architecture".to_string()]]);
}

#[test]
fn trigram_store_matches_and_scores_japanese_content() {
    let mut store = DocumentStore::new();
    store.set_tokenizer(TokenizerKind::Trigram).unwrap();
    store
        .add_str("# 日記\n\n今日は良い天気です\n\n昨日は雨でした\n")
        .unwrap();
    let engine = SqlEngine::new(&store).unwrap();

    let out = engine
        .execute("SELECT content FROM blocks WHERE match(content, '良い天気')")
        .unwrap();
    assert_eq!(out.rows, vec![vec!["今日は良い天気です".to_string()]]);

    let out = engine
        .execute(
            "SELECT content FROM blocks
             WHERE block_type = 'paragraph'
             ORDER BY bm25(content, '良い天気') DESC LIMIT 1",
        )
        .unwrap();
    assert_eq!(out.rows[0][0], "今日は良い天気です");
}

#[test]
fn where_bare_match_still_rechecked_when_blocks_shadowed_by_cte() {
    // A CTE named `blocks` shadows the real table, so `table_rows_with_hint`
    // never consults the TermIndex for it; the recheck-skip path must
    // not fire here, or a CTE that fabricates non-matching content would
    // slip through uncaught.
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute(
            "WITH blocks AS (SELECT 'no match here' AS content)
             SELECT content FROM blocks WHERE match(content, 'architecture')",
        )
        .unwrap();
    assert!(out.rows.is_empty());
}

#[test]
fn cost_based_planner_picks_cheaper_of_two_candidates() {
    let mut store = DocumentStore::new();
    store
        .add_str("# Doc\n\nP1\n\nP2\n\nP3\n\nP4\n\n```rust\nfn main(){}\n```\n")
        .unwrap();
    let engine = SqlEngine::new(&store).unwrap();

    let stmts = Parser::parse_sql(
        &GenericDialect {},
        "SELECT * FROM blocks WHERE block_type = 'paragraph' AND lang = 'rust'",
    )
    .unwrap();
    let Statement::Query(q) = stmts.into_iter().next().unwrap() else {
        panic!("expected query")
    };
    let SetExpr::Select(select) = q.body.as_ref() else {
        panic!("expected select")
    };
    let candidates = candidate_hints_for_where(select.selection.as_ref().unwrap());
    assert_eq!(candidates.len(), 2);

    // 4 paragraphs vs. 1 rust code block; the lang lookup is cheaper.
    let chosen = engine.choose_best_hint(candidates);
    assert_eq!(chosen, IndexHint::LangExact("rust".to_string()));
}

#[test]
fn cost_based_planner_tie_breaks_deterministically() {
    let mut store = DocumentStore::new();
    store.add_str("# Title\n\nBody\n").unwrap();
    let engine = SqlEngine::new(&store).unwrap();

    let stmts = Parser::parse_sql(
        &GenericDialect {},
        "SELECT * FROM blocks WHERE block_type = 'heading' AND depth = 1",
    )
    .unwrap();
    let Statement::Query(q) = stmts.into_iter().next().unwrap() else {
        panic!("expected query")
    };
    let SetExpr::Select(select) = q.body.as_ref() else {
        panic!("expected select")
    };
    let candidates = candidate_hints_for_where(select.selection.as_ref().unwrap());
    assert_eq!(candidates.len(), 2);

    // Both candidates match exactly one block (the single H1) with equal
    // cost, so the first-encountered candidate (BlockType) must win,
    // consistently across repeated calls.
    let first = engine.choose_best_hint(candidates.clone());
    let second = engine.choose_best_hint(candidates);
    assert_eq!(first, second);
    assert_eq!(first, IndexHint::BlockType(vec![BlockType::Heading]));
}

#[test]
fn where_match_full_scan_fallback_when_query_not_literal() {
    let stmts = Parser::parse_sql(
        &GenericDialect {},
        "SELECT * FROM blocks WHERE match(content, lang)",
    )
    .unwrap();
    let Statement::Query(q) = stmts.into_iter().next().unwrap() else {
        panic!("expected query")
    };
    let SetExpr::Select(select) = q.body.as_ref() else {
        panic!("expected select")
    };
    let candidates = candidate_hints_for_where(select.selection.as_ref().unwrap());
    assert_eq!(candidates, Vec::<IndexHint>::new());
}

fn eval_one(sql: &str) -> String {
    let store = DocumentStore::new();
    let engine = SqlEngine::new(&store).unwrap();
    engine.execute(sql).unwrap().rows[0][0].clone()
}

#[rstest]
// string functions
#[case("SELECT lower('Hello')", "hello")]
#[case("SELECT upper('Hello')", "HELLO")]
#[case("SELECT length('héllo')", "5")]
#[case("SELECT trim('  hi  ')", "hi")]
#[case("SELECT ltrim('  hi  ')", "hi  ")]
#[case("SELECT rtrim('  hi  ')", "  hi")]
#[case("SELECT trim(LEADING 'x' FROM 'xxhixx')", "hixx")]
#[case("SELECT trim(TRAILING 'x' FROM 'xxhixx')", "xxhi")]
#[case("SELECT trim('x' FROM 'xxhixx')", "hi")]
#[case("SELECT concat('a', 'b', 'c')", "abc")]
#[case("SELECT concat_ws('-', 'a', 'b', NULL, 'c')", "a-b-c")]
#[case("SELECT replace('foobar', 'o', '0')", "f00bar")]
#[case("SELECT left('hello', 3)", "hel")]
#[case("SELECT right('hello', 3)", "llo")]
#[case("SELECT lpad('7', 3, '0')", "007")]
#[case("SELECT rpad('7', 3, '0')", "700")]
#[case("SELECT reverse('hello')", "olleh")]
#[case("SELECT repeat('ab', 3)", "ababab")]
#[case("SELECT initcap('hello world')", "Hello World")]
#[case("SELECT ascii('A')", "65")]
#[case("SELECT chr(65)", "A")]
#[case("SELECT instr('hello world', 'world')", "7")]
#[case("SELECT position('world' in 'hello world')", "7")]
#[case("SELECT split_part('a,b,c', ',', 2)", "b")]
#[case("SELECT substring('hello world', 1, 5)", "hello")]
#[case("SELECT substring('hello world' from 7)", "world")]
#[case("SELECT substr('hello world', 7, 5)", "world")]
// numeric functions
#[case("SELECT abs(-5)", "5")]
#[case("SELECT abs(-5.5)", "5.5")]
#[case("SELECT round(3.456, 2)", "3.46")]
#[case("SELECT round(3.5)", "4")]
#[case("SELECT ceil(3.1)", "4")]
#[case("SELECT floor(3.9)", "3")]
#[case("SELECT trunc(3.789, 1)", "3.7")]
#[case("SELECT mod(10, 3)", "1")]
#[case("SELECT power(2, 10)", "1024")]
#[case("SELECT sqrt(16)", "4")]
#[case("SELECT sign(-3)", "-1")]
#[case("SELECT greatest(3, 7, 2)", "7")]
#[case("SELECT least(3, 7, 2)", "2")]
// null handling
#[case("SELECT coalesce(NULL, NULL, 'x')", "x")]
#[case("SELECT ifnull(NULL, 'y')", "y")]
#[case("SELECT nullif('a', 'a')", "NULL")]
#[case("SELECT nullif('a', 'b')", "a")]
// misc
#[case("SELECT typeof('x')", "text")]
#[case("SELECT typeof(1)", "integer")]
// CASE
#[case(
    "SELECT CASE WHEN 1 = 2 THEN 'a' WHEN 1 = 1 THEN 'b' ELSE 'c' END",
    "b"
)]
#[case("SELECT CASE 2 WHEN 1 THEN 'a' WHEN 2 THEN 'b' ELSE 'c' END", "b")]
#[case("SELECT CASE WHEN 1 = 2 THEN 'a' ELSE 'c' END", "c")]
fn test_sql_scalar_functions(#[case] sql: &str, #[case] expected: &str) {
    assert_eq!(eval_one(sql), expected);
}

#[test]
fn test_sql_group_concat() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute("SELECT group_concat(content) FROM blocks WHERE block_type = 'heading'")
        .unwrap();
    assert_eq!(out.rows[0][0], "Doc,Architecture,Other");
}

#[test]
fn test_sql_string_agg_custom_separator() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute("SELECT string_agg(content, ' | ') FROM blocks WHERE block_type = 'heading'")
        .unwrap();
    assert_eq!(out.rows[0][0], "Doc | Architecture | Other");
}

#[test]
fn test_sql_count_distinct() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute("SELECT count(DISTINCT block_type) FROM blocks")
        .unwrap();
    assert_eq!(out.rows[0][0], "3");
}

// doc B has no code at all; A and C's rust blocks must still come through.
#[test]
fn test_sql_zone_map_skip_by_lang() {
    let store = make_multi_doc_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute("SELECT content FROM blocks WHERE lang = 'rust' ORDER BY content")
        .unwrap();
    let contents: Vec<&str> = out.rows.iter().map(|r| r[0].as_str()).collect();
    assert_eq!(contents, vec!["fn a(){}", "fn c(){}"]);
}

// depth=3 only exists in doc C; A and B (max depth 1) must be skipped.
#[test]
fn test_sql_zone_map_skip_by_depth() {
    let store = make_multi_doc_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute("SELECT content FROM blocks WHERE depth = 3")
        .unwrap();
    assert_eq!(out.rows.len(), 1);
    assert_eq!(out.rows[0][0], "C3");
}

// Only doc B has a heading named "B"; requires block_type='heading' too.
#[test]
fn test_sql_zone_map_skip_by_heading_content() {
    let store = make_multi_doc_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute("SELECT content FROM blocks WHERE block_type = 'heading' AND content = 'B'")
        .unwrap();
    assert_eq!(out.rows.len(), 1);
    assert_eq!(out.rows[0][0], "B");
}

// `lang = ''` means "no lang"; must never trigger a code-language skip.
#[test]
fn test_sql_zone_map_no_skip_on_empty_lang() {
    let store = make_multi_doc_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute("SELECT content FROM blocks WHERE lang = ''")
        .unwrap();
    let contents: Vec<&str> = out.rows.iter().map(|r| r[0].as_str()).collect();
    assert!(contents.contains(&"B"), "doc B must not be skipped");
    assert!(contents.contains(&"Paragraph"));
}

// `id` must stay stable regardless of which documents get skipped.
#[test]
fn test_sql_zone_map_skip_preserves_block_ids() {
    let store = make_multi_doc_store();
    let engine = SqlEngine::new(&store).unwrap();
    let full = engine.execute("SELECT id, content FROM blocks").unwrap();
    let filtered = engine
        .execute("SELECT id, content FROM blocks WHERE lang = 'rust'")
        .unwrap();
    assert_eq!(filtered.rows.len(), 2);
    for row in &filtered.rows {
        let same_id = full.rows.iter().find(|r| r[0] == row[0]).unwrap();
        assert_eq!(
            same_id[1], row[1],
            "id {} must reference the same block content in both queries",
            row[0]
        );
    }
}

// Zone-map skip is disabled whenever FROM has a join (see `exec_query`).
// Just checks a join with a recognized conjunct still scans normally.
#[test]
fn test_sql_zone_map_skip_disabled_for_joins() {
    let store = make_multi_doc_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute(
            "SELECT h.content, c.content FROM blocks h
             JOIN blocks c ON c.document_id = h.document_id AND c.block_type = 'code'
             WHERE h.block_type = 'heading'",
        )
        .unwrap();
    let headings: Vec<&str> = out.rows.iter().map(|r| r[0].as_str()).collect();
    assert_eq!(headings, vec!["A", "C", "C2", "C3"]);
}

#[test]
fn cte_basic_select_from_named_cte() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute(
            "WITH headings AS (SELECT content FROM blocks WHERE block_type = 'heading')
             SELECT content FROM headings ORDER BY content",
        )
        .unwrap();
    let contents: Vec<&str> = out.rows.iter().map(|r| r[0].as_str()).collect();
    assert_eq!(contents, vec!["Architecture", "Doc", "Other"]);
}

#[test]
fn cte_later_cte_references_earlier_cte_in_same_with() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute(
            "WITH h AS (SELECT content FROM blocks WHERE block_type = 'heading'),
                  h2 AS (SELECT content FROM h WHERE content != 'Doc')
             SELECT content FROM h2 ORDER BY content",
        )
        .unwrap();
    let contents: Vec<&str> = out.rows.iter().map(|r| r[0].as_str()).collect();
    assert_eq!(contents, vec!["Architecture", "Other"]);
}

#[test]
fn cte_forward_reference_to_later_cte_errors_unknown_table() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let err = engine
        .execute(
            "WITH a AS (SELECT content FROM b),
                  b AS (SELECT content FROM blocks WHERE block_type = 'heading')
             SELECT content FROM a",
        )
        .unwrap_err();
    assert!(err.to_string().contains("unknown table"));
}

#[test]
fn cte_used_in_join_both_sides() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute(
            "WITH h AS (SELECT content, document_id FROM blocks WHERE block_type = 'heading')
             SELECT a.content, b.content FROM h a JOIN h b
               ON a.document_id = b.document_id AND a.content = 'Doc' AND b.content = 'Other'",
        )
        .unwrap();
    assert_eq!(out.rows, vec![vec!["Doc".to_string(), "Other".to_string()]]);
}

#[test]
fn cte_visible_inside_subquery_in_where_clause() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute(
            "WITH h AS (SELECT content FROM blocks WHERE block_type = 'heading')
             SELECT content FROM blocks
             WHERE content = (SELECT content FROM h WHERE content = 'Doc')",
        )
        .unwrap();
    assert_eq!(out.rows, vec![vec!["Doc".to_string()]]);
}

#[test]
fn with_recursive_generates_number_sequence() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute(
            "WITH RECURSIVE seq AS (
               SELECT 1 AS n
               UNION ALL
               SELECT n + 1 FROM seq WHERE n < 5
             )
             SELECT n FROM seq ORDER BY n",
        )
        .unwrap();
    assert_eq!(
        out.rows,
        vec![
            vec!["1".to_string()],
            vec!["2".to_string()],
            vec!["3".to_string()],
            vec!["4".to_string()],
            vec!["5".to_string()],
        ]
    );
}

#[test]
fn with_recursive_union_dedupes_across_iterations() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    // Without dedup this would cycle between 1 and 2 forever; plain
    // UNION must drop the repeat and terminate.
    let out = engine
        .execute(
            "WITH RECURSIVE cyc AS (
               SELECT 1 AS n
               UNION
               SELECT mod(n, 2) + 1 FROM cyc
             )
             SELECT n FROM cyc ORDER BY n",
        )
        .unwrap();
    assert_eq!(out.rows, vec![vec!["1".to_string()], vec!["2".to_string()]]);
}

#[test]
fn with_recursive_walks_heading_ancestors_via_interval_containment() {
    let mut store = DocumentStore::new();
    store.add_str("# A\n\n## B\n\n### C\n\nLeaf\n").unwrap();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute(
            "WITH RECURSIVE ancestors AS (
               SELECT pre, post, content FROM blocks
               WHERE block_type = 'heading' AND content = 'C'
               UNION
               SELECT b.pre, b.post, b.content
               FROM blocks b, ancestors
               WHERE b.pre < ancestors.pre AND ancestors.post < b.post
                 AND b.block_type = 'heading'
             )
             SELECT content FROM ancestors ORDER BY pre",
        )
        .unwrap();
    let contents: Vec<&str> = out.rows.iter().map(|r| r[0].as_str()).collect();
    assert_eq!(contents, vec!["A", "B", "C"]);
}

#[test]
fn with_recursive_rejects_anchor_self_reference() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let err = engine
        .execute(
            "WITH RECURSIVE r AS (
               SELECT n FROM r
               UNION ALL
               SELECT n FROM r
             )
             SELECT n FROM r",
        )
        .unwrap_err();
    assert!(err.to_string().contains("anchor"));
}

#[test]
fn with_recursive_rejects_mismatched_column_counts() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let err = engine
        .execute(
            "WITH RECURSIVE r AS (
               SELECT 1 AS n
               UNION ALL
               SELECT n, n FROM r WHERE n < 5
             )
             SELECT n FROM r",
        )
        .unwrap_err();
    assert!(err.to_string().contains("different numbers of columns"));
}

#[test]
fn with_recursive_hits_iteration_cap_with_clear_error() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let err = engine
        .execute(
            "WITH RECURSIVE r AS (
               SELECT 1 AS n
               UNION ALL
               SELECT n + 1 FROM r
             )
             SELECT n FROM r",
        )
        .unwrap_err();
    assert!(err.to_string().contains("iterations"));
}

#[test]
fn with_recursive_non_self_referencing_cte_still_works() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute("WITH RECURSIVE r AS (SELECT content FROM blocks) SELECT content FROM r")
        .unwrap();
    assert!(!out.rows.is_empty());
}

#[test]
fn cte_name_shadows_blocks_table() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute("WITH blocks AS (SELECT 'shadowed' AS content) SELECT content FROM blocks")
        .unwrap();
    assert_eq!(out.rows, vec![vec!["shadowed".to_string()]]);
}

#[test]
fn cte_name_collision_with_custom_table_prefers_cte() {
    let mut store = make_store();
    store
        .execute_sql_mut("CREATE TABLE notes (name TEXT)")
        .unwrap();
    store
        .execute_sql_mut("INSERT INTO notes (name) VALUES ('real')")
        .unwrap();

    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute("WITH notes AS (SELECT 'cte' AS name) SELECT name FROM notes")
        .unwrap();
    assert_eq!(out.rows, vec![vec!["cte".to_string()]]);
}

#[test]
fn cte_column_alias_list_rejected() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let err = engine
        .execute(
            "WITH h(a) AS (SELECT content FROM blocks WHERE block_type = 'heading')
             SELECT a FROM h",
        )
        .unwrap_err();
    assert!(err.to_string().contains("column aliases"));
}

#[test]
fn cte_shadowing_across_nested_subquery_with_same_name() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute(
            "WITH x AS (SELECT content FROM blocks WHERE content = 'Doc')
             SELECT content FROM blocks
             WHERE block_type = 'heading'
               AND (content = (WITH x AS (SELECT content FROM blocks WHERE content = 'Other') SELECT content FROM x)
                    OR content = (SELECT content FROM x))
             ORDER BY content",
        )
        .unwrap();
    let contents: Vec<&str> = out.rows.iter().map(|r| r[0].as_str()).collect();
    assert_eq!(contents, vec!["Doc", "Other"]);
}

// UPDATE/DELETE write-back

fn write_md(dir: &tempfile::TempDir, name: &str, content: &str) -> std::path::PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, content).unwrap();
    path
}

#[test]
fn rollback_does_not_delete_a_table_committed_by_another_session_mid_transaction() {
    let store = DocumentStore::new();
    let began = std::sync::Barrier::new(2);
    let committed = std::sync::Barrier::new(2);

    std::thread::scope(|scope| {
        let engine_a_handle = scope.spawn(|| {
            let engine_a = SqlEngine::new(&store).unwrap();
            engine_a.execute("BEGIN").unwrap();
            began.wait();
            // Wait for the other session's CREATE TABLE to land before
            // rolling back.
            committed.wait();
            engine_a.execute("ROLLBACK").unwrap_err()
        });
        let engine_b_handle = scope.spawn(|| {
            began.wait();
            let engine_b = SqlEngine::new(&store).unwrap();
            engine_b.execute("CREATE TABLE audit (id TEXT)").unwrap();
            committed.wait();
        });
        let err = engine_a_handle.join().unwrap();
        engine_b_handle.join().unwrap();
        assert!(
            err.to_string().contains("another session"),
            "unexpected error: {err}"
        );
    });

    // The concurrently committed table must survive the rollback.
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine.execute("SHOW TABLES").unwrap();
    assert!(
        out.rows.iter().any(|r| r[0] == "audit"),
        "table 'audit' was deleted by an unrelated transaction's rollback"
    );
}

#[test]
fn rollback_ignores_noop_ddl_from_another_session() {
    let store = DocumentStore::new();
    let engine_a = SqlEngine::new(&store).unwrap();
    let engine_b = SqlEngine::new(&store).unwrap();
    engine_a.execute("CREATE TABLE notes (id TEXT)").unwrap();

    engine_a.execute("BEGIN").unwrap();
    engine_a.execute("INSERT INTO notes VALUES ('1')").unwrap();
    engine_b.execute("DROP TABLE IF EXISTS missing").unwrap();
    engine_b.execute("DROP VIEW IF EXISTS missing").unwrap();
    engine_b
        .execute("CREATE TABLE IF NOT EXISTS notes (id TEXT)")
        .unwrap();
    engine_b
        .execute("INSERT INTO notes SELECT * FROM notes WHERE 1 = 0")
        .unwrap();
    engine_a.execute("ROLLBACK").unwrap();

    assert_eq!(
        engine_a.execute("SELECT * FROM notes").unwrap().rows.len(),
        0
    );
}

#[test]
fn rollback_does_not_delete_a_commit_from_another_engine_on_the_same_thread() {
    let store = DocumentStore::new();
    let engine_a = SqlEngine::new(&store).unwrap();
    let engine_b = SqlEngine::new(&store).unwrap();

    engine_a.execute("BEGIN").unwrap();
    engine_b.execute("CREATE TABLE audit (id TEXT)").unwrap();
    let err = engine_a.execute("ROLLBACK").unwrap_err();
    assert!(
        err.to_string().contains("another session"),
        "unexpected error: {err}"
    );

    let out = engine_a.execute("SHOW TABLES").unwrap();
    assert!(out.rows.iter().any(|r| r[0] == "audit"));
}

#[test]
fn rollback_works_across_execute_sql_mut_calls() {
    let mut store = DocumentStore::new();
    store.execute_sql_mut("BEGIN").unwrap();
    store.execute_sql_mut("CREATE TABLE t (x TEXT)").unwrap();
    store.execute_sql_mut("ROLLBACK").unwrap();

    let out = SqlEngine::new(&store)
        .unwrap()
        .execute("SHOW TABLES")
        .unwrap();
    assert!(
        out.rows.iter().all(|r| r[0] != "t"),
        "table 't' survived rollback"
    );
}

#[test]
fn rollback_works_across_engines_sharing_a_session() {
    let store = DocumentStore::new();
    let session = DocumentStore::new_session_id();
    let run = |sql: &str| {
        SqlEngine::with_session(&store, session)
            .unwrap()
            .execute(sql)
    };
    run("BEGIN").unwrap();
    run("CREATE TABLE t (x TEXT)").unwrap();
    run("ROLLBACK").unwrap();

    let out = SqlEngine::new(&store)
        .unwrap()
        .execute("SHOW TABLES")
        .unwrap();
    assert!(
        out.rows.iter().all(|r| r[0] != "t"),
        "table 't' survived rollback"
    );
}

#[test]
fn test_transaction_rolls_back_custom_table() {
    let store = DocumentStore::new();
    let engine = SqlEngine::new(&store).unwrap();
    engine.execute("CREATE TABLE notes (id TEXT)").unwrap();
    engine.execute("BEGIN").unwrap();
    engine.execute("INSERT INTO notes VALUES ('1')").unwrap();
    assert_eq!(engine.execute("SELECT * FROM notes").unwrap().rows.len(), 1);
    engine.execute("ROLLBACK").unwrap();
    assert_eq!(engine.execute("SELECT * FROM notes").unwrap().rows.len(), 0);
}

#[test]
fn test_transaction_commit_keeps_changes() {
    let store = DocumentStore::new();
    let engine = SqlEngine::new(&store).unwrap();
    engine.execute("CREATE TABLE notes (id TEXT)").unwrap();
    engine.execute("BEGIN").unwrap();
    engine.execute("INSERT INTO notes VALUES ('1')").unwrap();
    engine.execute("COMMIT").unwrap();
    assert_eq!(engine.execute("SELECT * FROM notes").unwrap().rows.len(), 1);
    let err = engine.execute("COMMIT").unwrap_err();
    assert!(err.to_string().contains("no transaction"));
}

#[test]
fn test_transaction_nested_begin_errors() {
    let store = DocumentStore::new();
    let engine = SqlEngine::new(&store).unwrap();
    engine.execute("BEGIN").unwrap();
    let err = engine.execute("BEGIN").unwrap_err();
    assert!(err.to_string().contains("already in progress"));
    engine.execute("ROLLBACK").unwrap();
}

#[test]
fn test_transaction_rollback_without_begin_errors() {
    let store = DocumentStore::new();
    let engine = SqlEngine::new(&store).unwrap();
    let err = engine.execute("ROLLBACK").unwrap_err();
    assert!(err.to_string().contains("no transaction"));
}

#[test]
fn write_back_transaction_rollback_reverts_in_memory_blocks_but_flags_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_md(&dir, "doc.md", "# Old Title\n\nBody text\n");

    let mut store = DocumentStore::new();
    store.add_file(&path).unwrap();

    store.execute_sql_mut("BEGIN").unwrap();
    store
        .execute_sql_mut("UPDATE blocks SET content = 'New Title' WHERE block_type = 'heading'")
        .unwrap();
    assert!(
        store.documents()[0]
            .blocks
            .iter()
            .any(|b| b.content == "New Title")
    );

    let out = store.execute_sql_mut("ROLLBACK").unwrap();
    assert!(out.rows[0][0].contains("cannot be reverted"));

    assert!(
        store.documents()[0]
            .blocks
            .iter()
            .any(|b| b.content == "Old Title")
    );
    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert_eq!(on_disk, "# New Title\n\nBody text\n");
}

#[test]
fn write_back_update_rewrites_heading_and_keeps_rest_of_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_md(&dir, "doc.md", "# Old Title\n\nBody text\n");

    let mut store = DocumentStore::new();
    let doc_id = store.add_file(&path).unwrap();

    let out = store
        .execute_sql_mut("UPDATE blocks SET content = 'New Title' WHERE block_type = 'heading'")
        .unwrap();
    assert_eq!(out.rows[0][0], "1");

    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert_eq!(on_disk, "# New Title\n\nBody text\n");

    assert_eq!(store.documents()[0].id, doc_id);
    assert!(
        store.documents()[0]
            .blocks
            .iter()
            .any(|b| b.content == "New Title")
    );
}

#[test]
fn write_back_update_rewrites_paragraph_only() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_md(&dir, "doc.md", "# Title\n\nOld body\n\nAnother paragraph\n");

    let mut store = DocumentStore::new();
    store.add_file(&path).unwrap();

    store
        .execute_sql_mut("UPDATE blocks SET content = 'New body' WHERE content = 'Old body'")
        .unwrap();

    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert_eq!(on_disk, "# Title\n\nNew body\n\nAnother paragraph\n");
}

#[test]
fn write_back_delete_removes_matched_block_and_blank_line() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_md(&dir, "doc.md", "# Title\n\nKeep me\n\nRemove me\n");

    let mut store = DocumentStore::new();
    store.add_file(&path).unwrap();

    let out = store
        .execute_sql_mut("DELETE FROM blocks WHERE content = 'Remove me'")
        .unwrap();
    assert_eq!(out.rows[0][0], "1");

    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert_eq!(on_disk, "# Title\n\nKeep me\n");
    assert!(
        !store.documents()[0]
            .blocks
            .iter()
            .any(|b| b.content == "Remove me")
    );
}

#[test]
fn write_back_update_rejects_non_heading_paragraph_block_type() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_md(&dir, "doc.md", "# Title\n\n```rust\nfn main() {}\n```\n");

    let mut store = DocumentStore::new();
    store.add_file(&path).unwrap();

    let err = store
        .execute_sql_mut("UPDATE blocks SET content = 'fn other() {}' WHERE block_type = 'code'")
        .unwrap_err();
    assert!(err.to_string().contains("heading/paragraph"));
}

#[test]
fn write_back_rejects_document_with_no_source_path() {
    let mut store = DocumentStore::new();
    store.add_str("# Title\n\nBody\n").unwrap();

    let err = store
        .execute_sql_mut("UPDATE blocks SET content = 'x' WHERE block_type = 'heading'")
        .unwrap_err();
    assert!(err.to_string().contains("no source file"));
}

#[test]
fn write_back_rejects_column_other_than_content() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_md(&dir, "doc.md", "# Title\n\nBody\n");

    let mut store = DocumentStore::new();
    store.add_file(&path).unwrap();

    let err = store
        .execute_sql_mut("UPDATE blocks SET pre = 5 WHERE block_type = 'heading'")
        .unwrap_err();
    assert!(err.to_string().contains("'content'"));
}

#[test]
fn write_back_rejects_joins() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_md(&dir, "doc.md", "# Title\n\nBody\n");

    let mut store = DocumentStore::new();
    store.add_file(&path).unwrap();

    let err = store
        .execute_sql_mut(
            "UPDATE blocks b JOIN blocks c ON c.document_id = b.document_id SET b.content = 'x'",
        )
        .unwrap_err();
    assert!(err.to_string().contains("joins"));
}

#[test]
fn write_back_read_only_statements_still_work_via_execute_sql_mut() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_md(&dir, "doc.md", "# Title\n\nBody\n");

    let mut store = DocumentStore::new();
    store.add_file(&path).unwrap();

    let out = store
        .execute_sql_mut("SELECT content FROM blocks WHERE block_type = 'heading'")
        .unwrap();
    assert_eq!(out.rows, vec![vec!["Title".to_string()]]);
}

fn title_pre(store: &DocumentStore) -> String {
    SqlEngine::new(store)
        .unwrap()
        .execute("SELECT pre FROM blocks WHERE content = 'Title'")
        .unwrap()
        .rows[0][0]
        .clone()
}

#[test]
fn write_back_insert_heading_after_pre_anchor() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_md(&dir, "doc.md", "# Title\n\nBody\n");

    let mut store = DocumentStore::new();
    store.add_file(&path).unwrap();
    let pre = title_pre(&store);

    let out = store
        .execute_sql_mut(&format!(
            "INSERT INTO blocks (document_id, block_type, content, depth, after_pre) VALUES (0, 'heading', 'Subsection', 2, {pre})"
        ))
        .unwrap();
    assert_eq!(out.rows[0][0], "1");

    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert_eq!(on_disk, "# Title\n\n## Subsection\n\nBody\n");
    assert!(
        store.documents()[0]
            .blocks
            .iter()
            .any(|b| b.content == "Subsection")
    );
}

#[test]
fn write_back_insert_paragraph_append_at_end_no_after_pre() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_md(&dir, "doc.md", "# Title\n\nBody\n");

    let mut store = DocumentStore::new();
    store.add_file(&path).unwrap();

    let out = store
        .execute_sql_mut(
            "INSERT INTO blocks (document_id, block_type, content) VALUES (0, 'paragraph', 'Appended')",
        )
        .unwrap();
    assert_eq!(out.rows[0][0], "1");

    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert_eq!(on_disk, "# Title\n\nBody\n\nAppended\n");
}

#[test]
fn write_back_insert_append_preserves_missing_trailing_newline() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("doc.md");
    std::fs::write(&path, "# Title\n\nBody").unwrap();

    let mut store = DocumentStore::new();
    store.add_file(&path).unwrap();

    store
        .execute_sql_mut(
            "INSERT INTO blocks (document_id, block_type, content) VALUES (0, 'paragraph', 'Appended')",
        )
        .unwrap();

    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert_eq!(on_disk, "# Title\n\nBody\n\nAppended");
}

#[test]
fn write_back_insert_two_rows_same_after_pre_preserves_order() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_md(&dir, "doc.md", "# Title\n\nBody\n");

    let mut store = DocumentStore::new();
    store.add_file(&path).unwrap();
    let pre = title_pre(&store);

    store
        .execute_sql_mut(&format!(
            "INSERT INTO blocks (document_id, block_type, content, after_pre) VALUES (0, 'paragraph', 'First', {pre}), (0, 'paragraph', 'Second', {pre})"
        ))
        .unwrap();

    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert_eq!(on_disk, "# Title\n\nFirst\n\nSecond\n\nBody\n");
}

#[test]
fn write_back_insert_mixed_anchors_same_document() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_md(&dir, "doc.md", "# Title\n\nBody\n");

    let mut store = DocumentStore::new();
    store.add_file(&path).unwrap();
    let pre = title_pre(&store);

    store
        .execute_sql_mut(&format!(
            "INSERT INTO blocks (document_id, block_type, content, after_pre) VALUES \
             (0, 'paragraph', 'AfterTitle', {pre}), \
             (0, 'paragraph', 'AtEnd', NULL)"
        ))
        .unwrap();

    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert_eq!(on_disk, "# Title\n\nAfterTitle\n\nBody\n\nAtEnd\n");
}

#[test]
fn write_back_insert_multi_row_different_documents() {
    let dir = tempfile::tempdir().unwrap();
    let path_a = write_md(&dir, "a.md", "# A\n\nBodyA\n");
    let path_b = write_md(&dir, "b.md", "# B\n\nBodyB\n");

    let mut store = DocumentStore::new();
    store.add_file(&path_a).unwrap();
    store.add_file(&path_b).unwrap();

    let out = store
        .execute_sql_mut(
            "INSERT INTO blocks (document_id, block_type, content) VALUES \
             (0, 'paragraph', 'ExtraA'), (1, 'paragraph', 'ExtraB')",
        )
        .unwrap();
    assert_eq!(out.rows[0][0], "2");

    assert_eq!(
        std::fs::read_to_string(&path_a).unwrap(),
        "# A\n\nBodyA\n\nExtraA\n"
    );
    assert_eq!(
        std::fs::read_to_string(&path_b).unwrap(),
        "# B\n\nBodyB\n\nExtraB\n"
    );
}

#[test]
fn write_back_insert_rejects_unsupported_block_type() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_md(&dir, "doc.md", "# Title\n\nBody\n");
    let mut store = DocumentStore::new();
    store.add_file(&path).unwrap();

    let err = store
        .execute_sql_mut(
            "INSERT INTO blocks (document_id, block_type, content) VALUES (0, 'code', 'fn f(){}')",
        )
        .unwrap_err();
    assert!(err.to_string().contains("heading/paragraph"));
}

#[test]
fn write_back_insert_rejects_missing_depth_for_heading() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_md(&dir, "doc.md", "# Title\n\nBody\n");
    let mut store = DocumentStore::new();
    store.add_file(&path).unwrap();

    let err = store
        .execute_sql_mut(
            "INSERT INTO blocks (document_id, block_type, content) VALUES (0, 'heading', 'New')",
        )
        .unwrap_err();
    assert!(err.to_string().contains("depth"));
}

#[test]
fn write_back_insert_rejects_depth_for_paragraph() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_md(&dir, "doc.md", "# Title\n\nBody\n");
    let mut store = DocumentStore::new();
    store.add_file(&path).unwrap();

    let err = store
        .execute_sql_mut(
            "INSERT INTO blocks (document_id, block_type, content, depth) VALUES (0, 'paragraph', 'New', 2)",
        )
        .unwrap_err();
    assert!(err.to_string().contains("depth"));
}

#[test]
fn write_back_insert_rejects_positional_values() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_md(&dir, "doc.md", "# Title\n\nBody\n");
    let mut store = DocumentStore::new();
    store.add_file(&path).unwrap();

    let err = store
        .execute_sql_mut("INSERT INTO blocks VALUES (0, 'paragraph', 'New', NULL, NULL)")
        .unwrap_err();
    assert!(err.to_string().contains("column list"));
}

#[test]
fn write_back_insert_rejects_unknown_after_pre() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_md(&dir, "doc.md", "# Title\n\nBody\n");
    let mut store = DocumentStore::new();
    store.add_file(&path).unwrap();

    let err = store
        .execute_sql_mut(
            "INSERT INTO blocks (document_id, block_type, content, after_pre) VALUES (0, 'paragraph', 'New', 999)",
        )
        .unwrap_err();
    assert!(err.to_string().contains("after_pre"));
}

#[test]
fn write_back_insert_rejects_unknown_document_id() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_md(&dir, "doc.md", "# Title\n\nBody\n");
    let mut store = DocumentStore::new();
    store.add_file(&path).unwrap();

    let err = store
        .execute_sql_mut(
            "INSERT INTO blocks (document_id, block_type, content) VALUES (99, 'paragraph', 'New')",
        )
        .unwrap_err();
    assert!(err.to_string().contains("no such document"));
}

#[test]
fn write_back_insert_rejects_document_with_no_source_path() {
    let mut store = DocumentStore::new();
    store.add_str("# Title\n\nBody\n").unwrap();

    let err = store
        .execute_sql_mut(
            "INSERT INTO blocks (document_id, block_type, content) VALUES (0, 'paragraph', 'New')",
        )
        .unwrap_err();
    assert!(err.to_string().contains("no source file"));
}

#[test]
fn write_back_read_only_insert_into_blocks_still_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_md(&dir, "doc.md", "# Title\n\nBody\n");
    let mut store = DocumentStore::new();
    store.add_file(&path).unwrap();

    let engine = SqlEngine::new(&store).unwrap();
    let err = engine
        .execute(
            "INSERT INTO blocks (document_id, block_type, content) VALUES (0, 'paragraph', 'New')",
        )
        .unwrap_err();
    assert!(err.to_string().contains("blocks"));
}

#[test]
fn write_back_insert_into_custom_table_still_works_via_execute_sql_mut() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_md(&dir, "doc.md", "# Title\n\nBody\n");
    let mut store = DocumentStore::new();
    store.add_file(&path).unwrap();

    store
        .execute_sql_mut("CREATE TABLE notes (name TEXT)")
        .unwrap();
    let out = store
        .execute_sql_mut("INSERT INTO notes (name) VALUES ('hello')")
        .unwrap();
    assert_eq!(out.rows[0][0], "1");
}

// EXPLAIN / EXPLAIN ANALYZE

fn explain_detail<'a>(out: &'a QueryOutput, step: &str) -> &'a str {
    out.rows
        .iter()
        .find(|r| r[0] == step)
        .unwrap_or_else(|| panic!("no EXPLAIN row for step '{step}' in {:?}", out.rows))[1]
        .as_str()
}

#[test]
fn explain_reports_bitmap_index_for_block_type_eq() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute("EXPLAIN SELECT content FROM blocks WHERE block_type = 'heading'")
        .unwrap();
    assert_eq!(out.columns, vec!["step", "detail"]);
    assert!(explain_detail(&out, "query:where").contains("BitmapIndex"));
    assert!(explain_detail(&out, "query:zone-map").contains("not eligible"));
}

#[test]
fn explain_reports_zone_map_skip_eligibility() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute("EXPLAIN SELECT content FROM blocks WHERE lang = 'rust'")
        .unwrap();
    assert!(explain_detail(&out, "query:zone-map").contains("eligible via lang"));
}

#[test]
fn explain_reports_full_scan_when_no_hint_applies() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute("EXPLAIN SELECT content FROM blocks WHERE content LIKE '%foo%'")
        .unwrap();
    assert!(explain_detail(&out, "query:where").contains("full scan"));
}

#[test]
fn explain_reports_multiple_candidates_with_costs() {
    let mut store = DocumentStore::new();
    store
        .add_str("# Doc\n\nP1\n\nP2\n\nP3\n\nP4\n\n```rust\nfn main(){}\n```\n")
        .unwrap();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute("EXPLAIN SELECT * FROM blocks WHERE block_type = 'paragraph' AND lang = 'rust'")
        .unwrap();
    let detail = explain_detail(&out, "query:where");
    assert!(detail.contains("est."));
    assert!(detail.contains("also considered"));
    assert!(detail.contains("HashIndex(lang = 'rust') used"));
}

#[test]
fn explain_reports_no_where_row_when_no_where_clause() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute("EXPLAIN SELECT content FROM blocks")
        .unwrap();
    assert!(explain_detail(&out, "query:where").contains("full scan"));
}

#[test]
fn explain_reports_hash_join_for_equi_join() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute(
            "EXPLAIN SELECT h.content FROM blocks h
             JOIN blocks n ON n.document_id = h.document_id",
        )
        .unwrap();
    assert!(explain_detail(&out, "query:join[0]").contains("hash join"));
}

#[test]
fn explain_reports_nested_loop_for_non_equi_join() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute(
            "EXPLAIN SELECT h.content FROM blocks h
             JOIN blocks n ON n.pre = h.pre + 1",
        )
        .unwrap();
    assert!(explain_detail(&out, "query:join[0]").contains("nested loop"));
}

#[test]
fn explain_reports_group_by_order_by_and_limit() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute(
            "EXPLAIN SELECT block_type, count(*) FROM blocks
             GROUP BY block_type ORDER BY block_type LIMIT 5",
        )
        .unwrap();
    assert!(explain_detail(&out, "query:group-by").contains("1 key"));
    assert!(explain_detail(&out, "query:order-by").contains("ASC"));
    assert_eq!(explain_detail(&out, "query:limit"), "5");
}

#[test]
fn explain_describes_cte_separately_from_outer_query() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute(
            "EXPLAIN WITH headings AS (SELECT content FROM blocks WHERE block_type = 'heading')
             SELECT content FROM headings",
        )
        .unwrap();
    assert!(explain_detail(&out, "cte:headings:where").contains("BitmapIndex"));
    assert!(explain_detail(&out, "query:from").contains("headings (cte)"));
}

#[test]
fn explain_analyze_runs_query_and_reports_row_count() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute("EXPLAIN ANALYZE SELECT content FROM blocks WHERE block_type = 'heading'")
        .unwrap();
    assert_eq!(explain_detail(&out, "actual:rows"), "3 row(s) returned");
    assert!(explain_detail(&out, "actual:elapsed").contains("ms"));
    assert!(
        out.rows
            .iter()
            .any(|r| r[1].contains("document(s) skipped by zone map"))
    );
}

#[test]
fn explain_analyze_skips_doc_stats_for_joins() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute(
            "EXPLAIN ANALYZE SELECT h.content FROM blocks h
             JOIN blocks n ON n.document_id = h.document_id",
        )
        .unwrap();
    assert!(
        !out.rows
            .iter()
            .any(|r| r[1].contains("document(s) skipped by zone map"))
    );
    assert!(explain_detail(&out, "actual:rows").contains("row(s) returned"));
}

#[test]
fn explain_rejects_non_select_statement() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let err = engine
        .execute("EXPLAIN CREATE TABLE notes (name TEXT)")
        .unwrap_err();
    assert!(err.to_string().contains("SELECT"));
}

#[test]
fn explain_under_write_back_delegates_correctly() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_md(&dir, "doc.md", "# Title\n\nBody\n");
    let mut store = DocumentStore::new();
    store.add_file(&path).unwrap();

    let out = store
        .execute_sql_mut("EXPLAIN SELECT content FROM blocks WHERE block_type = 'heading'")
        .unwrap();
    assert!(explain_detail(&out, "query:where").contains("BitmapIndex"));
}

// CREATE VIEW / DROP VIEW

#[test]
fn create_view_then_select_reflects_current_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_md(&dir, "doc.md", "# Title\n\nOld Content\n");
    let mut store = DocumentStore::new();
    store.add_file(&path).unwrap();

    store
        .execute_sql_mut(
            "CREATE VIEW paras AS SELECT content FROM blocks WHERE block_type = 'paragraph'",
        )
        .unwrap();

    let out = SqlEngine::new(&store)
        .unwrap()
        .execute("SELECT content FROM paras")
        .unwrap();
    assert_eq!(out.rows, vec![vec!["Old Content".to_string()]]);

    store
        .execute_sql_mut("UPDATE blocks SET content = 'New Content' WHERE content = 'Old Content'")
        .unwrap();

    let out = SqlEngine::new(&store)
        .unwrap()
        .execute("SELECT content FROM paras")
        .unwrap();
    assert_eq!(out.rows, vec![vec!["New Content".to_string()]]);
}

#[test]
fn create_view_rejects_builtin_name() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let err = engine
        .execute("CREATE VIEW blocks AS SELECT 1")
        .unwrap_err();
    assert!(err.to_string().contains("built-in"));
}

#[test]
fn create_view_rejects_duplicate_without_if_not_exists() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    engine.execute("CREATE VIEW v AS SELECT 1").unwrap();
    let err = engine.execute("CREATE VIEW v AS SELECT 2").unwrap_err();
    assert!(err.to_string().contains("already exists"));
}

#[test]
fn create_view_if_not_exists_is_a_noop() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    engine.execute("CREATE VIEW v AS SELECT 1").unwrap();
    let out = engine
        .execute("CREATE VIEW IF NOT EXISTS v AS SELECT 2")
        .unwrap();
    assert_eq!(out.rows[0][0], "already exists");

    let sel = engine.execute("SELECT * FROM v").unwrap();
    assert_eq!(sel.rows, vec![vec!["1".to_string()]]);
}

#[test]
fn create_view_or_replace_overwrites() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    engine.execute("CREATE VIEW v AS SELECT 1").unwrap();
    engine
        .execute("CREATE OR REPLACE VIEW v AS SELECT 2")
        .unwrap();
    let out = engine.execute("SELECT * FROM v").unwrap();
    assert_eq!(out.rows, vec![vec!["2".to_string()]]);
}

#[test]
fn create_view_rejects_name_colliding_with_custom_table() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    engine.execute("CREATE TABLE t (x TEXT)").unwrap();
    let err = engine.execute("CREATE VIEW t AS SELECT 1").unwrap_err();
    assert!(err.to_string().contains("table"));
}

#[test]
fn create_table_rejects_name_colliding_with_view() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    engine.execute("CREATE VIEW v AS SELECT 1").unwrap();
    let err = engine.execute("CREATE TABLE v (x TEXT)").unwrap_err();
    assert!(err.to_string().contains("view"));
}

#[test]
fn view_detects_circular_reference() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    engine.execute("CREATE VIEW a AS SELECT 1").unwrap();
    engine.execute("CREATE VIEW b AS SELECT * FROM a").unwrap();
    // At validation time this only sees b's current (non-circular)
    // definition, so it succeeds; but now a -> b -> a is a real cycle.
    engine
        .execute("CREATE OR REPLACE VIEW a AS SELECT * FROM b")
        .unwrap();

    let err = engine.execute("SELECT * FROM a").unwrap_err();
    assert!(err.to_string().contains("circular"));
}

#[test]
fn drop_view_removes_it() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    engine.execute("CREATE VIEW v AS SELECT 1").unwrap();
    engine.execute("DROP VIEW v").unwrap();
    let err = engine.execute("SELECT * FROM v").unwrap_err();
    assert!(err.to_string().contains("unknown table"));
}

#[test]
fn drop_view_if_exists_on_missing_is_noop() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine.execute("DROP VIEW IF EXISTS missing").unwrap();
    assert!(out.rows[0][0].contains("0 view"));
}

#[test]
fn show_tables_lists_views() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    engine.execute("CREATE VIEW v AS SELECT 1").unwrap();
    let out = engine.execute("SHOW TABLES").unwrap();
    assert!(out.rows.iter().any(|r| r[0] == "v" && r[1] == "view"));
}

#[test]
fn desc_view_reports_query_columns() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    engine
        .execute("CREATE VIEW headings AS SELECT content, depth FROM blocks WHERE block_type = 'heading'")
        .unwrap();
    let out = engine.execute("DESC headings").unwrap();
    let cols: Vec<&str> = out.rows.iter().map(|r| r[0].as_str()).collect();
    assert_eq!(cols, vec!["content", "depth"]);
}

#[test]
fn view_persists_across_save_and_load() {
    let dir = tempfile::tempdir().unwrap();
    let md_path = write_md(&dir, "doc.md", "# Title\n\nHello\n");
    let path = dir.path().join("test.mq-db");

    let mut store = DocumentStore::new();
    store.add_file(&md_path).unwrap();
    store
        .execute_sql_mut(
            "CREATE VIEW paras AS SELECT content FROM blocks WHERE block_type = 'paragraph'",
        )
        .unwrap();
    store.save(&path).unwrap();

    let reloaded = DocumentStore::load(&path).unwrap();
    let engine = SqlEngine::new(&reloaded).unwrap();
    let out = engine.execute("SELECT content FROM paras").unwrap();
    assert_eq!(out.rows, vec![vec!["Hello".to_string()]]);

    // Still live after reload, not a frozen snapshot.
    let mut reloaded = reloaded;
    reloaded
        .execute_sql_mut("UPDATE blocks SET content = 'Updated' WHERE content = 'Hello'")
        .unwrap();
    let out = SqlEngine::new(&reloaded)
        .unwrap()
        .execute("SELECT content FROM paras")
        .unwrap();
    assert_eq!(out.rows, vec![vec!["Updated".to_string()]]);
}

#[test]
fn view_works_through_execute_sql_mut() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_md(&dir, "doc.md", "# Title\n\nBody\n");
    let mut store = DocumentStore::new();
    store.add_file(&path).unwrap();

    store
        .execute_sql_mut("CREATE VIEW v AS SELECT content FROM blocks WHERE block_type = 'heading'")
        .unwrap();
    let out = store.execute_sql_mut("SELECT content FROM v").unwrap();
    assert_eq!(out.rows, vec![vec!["Title".to_string()]]);
}

// read_csv() / read_json() table functions

#[test]
fn read_csv_selects_rows_with_header_as_columns() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_md(
        &dir,
        "people.csv",
        "name,age\n\"Ann, B\",30\n\"She said \"\"hi\"\"\",25\n",
    );
    let store = DocumentStore::new();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute(&format!(
            "SELECT name, age FROM read_csv('{}') ORDER BY age",
            path.display()
        ))
        .unwrap();
    assert_eq!(
        out.rows,
        vec![
            vec!["She said \"hi\"".to_string(), "25".to_string()],
            vec!["Ann, B".to_string(), "30".to_string()],
        ]
    );
}

#[test]
fn read_csv_supports_numeric_where_and_arithmetic() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_md(&dir, "people.csv", "name,age\nAnn,30\nCarl,25\n");
    let store = DocumentStore::new();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute(&format!(
            "SELECT name, age + 1 FROM read_csv('{}') WHERE age > 26",
            path.display()
        ))
        .unwrap();
    assert_eq!(out.rows, vec![vec!["Ann".to_string(), "31".to_string()]]);
}

#[test]
fn read_csv_pads_ragged_rows_with_null() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_md(&dir, "ragged.csv", "a,b,c\n1,2,3\n4\n");
    let store = DocumentStore::new();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute(&format!(
            "SELECT a, b, c FROM read_csv('{}') ORDER BY a",
            path.display()
        ))
        .unwrap();
    assert_eq!(
        out.rows,
        vec![
            vec!["1".to_string(), "2".to_string(), "3".to_string()],
            vec!["4".to_string(), "NULL".to_string(), "NULL".to_string()],
        ]
    );
}

#[test]
fn read_csv_rejects_missing_file_with_clear_error() {
    let store = DocumentStore::new();
    let engine = SqlEngine::new(&store).unwrap();
    let err = engine
        .execute("SELECT * FROM read_csv('/no/such/file.csv')")
        .unwrap_err();
    assert!(err.to_string().contains("read_csv"));
}

#[test]
fn read_json_selects_rows_from_jsonl() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_md(
        &dir,
        "people.jsonl",
        "{\"name\":\"Ann\",\"age\":30,\"active\":true}\n{\"name\":\"Carl\",\"age\":25,\"active\":false}\n",
    );
    let store = DocumentStore::new();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute(&format!(
            "SELECT name, age, active FROM read_json('{}') WHERE age > 26",
            path.display()
        ))
        .unwrap();
    assert_eq!(
        out.rows,
        vec![vec![
            "Ann".to_string(),
            "30".to_string(),
            "true".to_string()
        ]]
    );
}

#[test]
fn read_json_unions_columns_across_varying_objects() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_md(
        &dir,
        "mixed.jsonl",
        "{\"name\":\"Ann\",\"age\":30}\n{\"name\":\"Carl\",\"city\":\"NYC\"}\n",
    );
    let store = DocumentStore::new();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute(&format!(
            "SELECT name, age, city FROM read_json('{}') ORDER BY name",
            path.display()
        ))
        .unwrap();
    assert_eq!(
        out.rows,
        vec![
            vec!["Ann".to_string(), "30".to_string(), "NULL".to_string()],
            vec!["Carl".to_string(), "NULL".to_string(), "NYC".to_string()],
        ]
    );
}

#[test]
fn read_json_rejects_non_object_line_with_line_number() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_md(&dir, "bad.jsonl", "{\"a\":1}\n[1,2,3]\n");
    let store = DocumentStore::new();
    let engine = SqlEngine::new(&store).unwrap();
    let err = engine
        .execute(&format!("SELECT * FROM read_json('{}')", path.display()))
        .unwrap_err();
    assert!(err.to_string().contains("line 2"));
}

#[test]
fn read_csv_works_inside_create_table_as_select() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_md(&dir, "people.csv", "name,age\nAnn,30\n");
    let store = DocumentStore::new();
    let engine = SqlEngine::new(&store).unwrap();
    engine
        .execute(&format!(
            "CREATE TABLE people AS SELECT * FROM read_csv('{}')",
            path.display()
        ))
        .unwrap();
    let out = engine.execute("SELECT name FROM people").unwrap();
    assert_eq!(out.rows, vec![vec!["Ann".to_string()]]);
}

#[test]
fn read_table_function_rejects_unknown_name() {
    let store = DocumentStore::new();
    let engine = SqlEngine::new(&store).unwrap();
    let err = engine
        .execute("SELECT * FROM read_parquet('/tmp/x.parquet')")
        .unwrap_err();
    assert!(err.to_string().contains("unknown table function"));
}

#[test]
fn vacuum_statement_redirects_to_cli() {
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let err = engine.execute("VACUUM").unwrap_err();
    assert!(err.to_string().contains("mq-db vacuum"));

    let mut store = DocumentStore::new();
    store.add_str("# Title\n\nBody\n").unwrap();
    let err = store.execute_sql_mut("VACUUM").unwrap_err();
    assert!(err.to_string().contains("mq-db vacuum"));
}

// ATTACH / DETACH

fn saved_store(dir: &tempfile::TempDir, name: &str, md: &str) -> std::path::PathBuf {
    let mut s = DocumentStore::new();
    s.add_str(md).unwrap();
    let path = dir.path().join(name);
    s.save(&path).unwrap();
    path
}

#[test]
fn attach_selects_rows_from_other_store() {
    let dir = tempfile::tempdir().unwrap();
    let other_path = saved_store(&dir, "other.mq-db", "# Other Doc\n\nOther body\n");

    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    engine
        .execute(&format!(
            "ATTACH DATABASE '{}' AS other",
            other_path.display()
        ))
        .unwrap();

    let out = engine
        .execute("SELECT content FROM other.blocks WHERE block_type = 'heading'")
        .unwrap();
    assert_eq!(out.rows, vec![vec!["Other Doc".to_string()]]);
}

#[test]
fn attach_join_across_local_and_other_store() {
    let dir = tempfile::tempdir().unwrap();
    let other_path = saved_store(&dir, "other.mq-db", "# Other Doc\n\nOther body\n");

    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    engine
        .execute(&format!(
            "ATTACH DATABASE '{}' AS other",
            other_path.display()
        ))
        .unwrap();

    let out = engine
        .execute(
            "SELECT b.content, o.content FROM blocks b JOIN other.blocks o \
             ON b.block_type = o.block_type WHERE b.block_type = 'heading'",
        )
        .unwrap();
    assert!(!out.rows.is_empty());
}

#[test]
fn attach_match_uses_other_stores_tokenizer() {
    let dir = tempfile::tempdir().unwrap();
    let mut other = DocumentStore::new();
    other.set_tokenizer(TokenizerKind::Trigram).unwrap();
    other.add_str("# Doc\n\n今日は良い天気です\n").unwrap();
    let other_path = dir.path().join("other.mq-db");
    other.save(&other_path).unwrap();

    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    engine
        .execute(&format!(
            "ATTACH DATABASE '{}' AS other",
            other_path.display()
        ))
        .unwrap();

    let out = engine
        .execute("SELECT content FROM other.blocks WHERE match(content, '良い天気')")
        .unwrap();
    assert_eq!(out.rows, vec![vec!["今日は良い天気です".to_string()]]);
}

#[test]
fn attach_match_on_non_driving_joined_table_uses_its_own_tokenizer() {
    let dir = tempfile::tempdir().unwrap();
    let mut other = DocumentStore::new();
    other.set_tokenizer(TokenizerKind::Trigram).unwrap();
    other.add_str("# Doc\n\n今日は良い天気です\n").unwrap();
    let other_path = dir.path().join("other.mq-db");
    other.save(&other_path).unwrap();

    // Driving table `b` is the local (Word-tokenized) store; the
    // matched column belongs to the non-driving joined `other` table.
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    engine
        .execute(&format!(
            "ATTACH DATABASE '{}' AS other",
            other_path.display()
        ))
        .unwrap();

    let out = engine
        .execute(
            "SELECT o.content FROM blocks b JOIN other.blocks o \
             ON b.block_type = o.block_type \
             WHERE match(o.content, '良い天気')",
        )
        .unwrap();
    assert!(!out.rows.is_empty());
    assert!(
        out.rows
            .iter()
            .all(|r| r == &vec!["今日は良い天気です".to_string()])
    );
}

#[test]
fn attach_bm25_on_non_driving_joined_table_uses_its_own_corpus() {
    let dir = tempfile::tempdir().unwrap();
    let mut other = DocumentStore::new();
    other.set_tokenizer(TokenizerKind::Trigram).unwrap();
    other.add_str("# Doc\n\n今日は良い天気です\n").unwrap();
    let other_path = dir.path().join("other.mq-db");
    other.save(&other_path).unwrap();

    // Driving table `b` is the local (Word-tokenized) store; `bm25()`
    // scores content from the non-driving joined `other` table, so it
    // must use `other`'s own corpus stats/tokenizer, not the local one.
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    engine
        .execute(&format!(
            "ATTACH DATABASE '{}' AS other",
            other_path.display()
        ))
        .unwrap();

    let out = engine
        .execute(
            "SELECT bm25(o.content, '良い天気') FROM blocks b \
             JOIN other.blocks o ON b.block_type = o.block_type \
             WHERE o.block_type = 'paragraph'",
        )
        .unwrap();
    assert!(!out.rows.is_empty());
    for row in &out.rows {
        let score: f64 = row[0].parse().unwrap();
        assert!(score > 0.0, "expected positive bm25 score, got {score}");
    }
}

#[test]
fn attach_match_on_wrapped_joined_column_uses_its_own_tokenizer() {
    let dir = tempfile::tempdir().unwrap();
    let mut other = DocumentStore::new();
    other.set_tokenizer(TokenizerKind::Trigram).unwrap();
    other.add_str("# Doc\n\n今日は良い天気です\n").unwrap();
    let other_path = dir.path().join("other.mq-db");
    other.save(&other_path).unwrap();

    // Same table setup as `attach_match_on_non_driving_joined_table_uses_its_own_tokenizer`,
    // but the reference to `o.content` is wrapped in `lower(...)` instead
    // of appearing bare; it must still resolve to `other`'s tokenizer.
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    engine
        .execute(&format!(
            "ATTACH DATABASE '{}' AS other",
            other_path.display()
        ))
        .unwrap();

    let out = engine
        .execute(
            "SELECT o.content FROM blocks b JOIN other.blocks o \
             ON b.block_type = o.block_type \
             WHERE match(lower(o.content), '良い天気')",
        )
        .unwrap();
    assert!(!out.rows.is_empty());
    assert!(
        out.rows
            .iter()
            .all(|r| r == &vec!["今日は良い天気です".to_string()])
    );
}

#[test]
fn attach_view_bm25_does_not_reuse_local_corpus() {
    let dir = tempfile::tempdir().unwrap();
    let mut other = DocumentStore::new();
    other.set_tokenizer(TokenizerKind::Trigram).unwrap();
    other.add_str("# Doc\n\n今日は良い天気です\n").unwrap();
    {
        let view_engine = SqlEngine::new(&other).unwrap();
        view_engine
            .execute(
                "CREATE VIEW ranked AS SELECT block_type, bm25(content, '良い天気') AS score FROM blocks",
            )
            .unwrap();
    }
    let other_path = dir.path().join("other.mq-db");
    other.save(&other_path).unwrap();

    // The outer, local (Word-tokenized) query's own `bm25()` call caches
    // its corpus under the top-level engine's key first; `other.ranked`'s
    // own unqualified `bm25()` then runs in a nested `SqlEngine` and must
    // not receive that cached local corpus.
    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    engine
        .execute(&format!(
            "ATTACH DATABASE '{}' AS other",
            other_path.display()
        ))
        .unwrap();

    let out = engine
        .execute(
            "SELECT r.score FROM blocks b JOIN other.ranked r \
             ON b.block_type = r.block_type \
             WHERE bm25(b.content, 'Details') >= 0 AND r.block_type = 'paragraph'",
        )
        .unwrap();
    assert!(!out.rows.is_empty());
    for row in &out.rows {
        let score: f64 = row[0].parse().unwrap();
        assert!(score > 0.0, "expected positive bm25 score, got {score}");
    }
}

#[test]
fn detach_makes_alias_unknown_again() {
    let dir = tempfile::tempdir().unwrap();
    let other_path = saved_store(&dir, "other.mq-db", "# Other Doc\n\nOther body\n");

    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    engine
        .execute(&format!(
            "ATTACH DATABASE '{}' AS other",
            other_path.display()
        ))
        .unwrap();
    engine.execute("DETACH other").unwrap();

    let err = engine.execute("SELECT * FROM other.blocks").unwrap_err();
    assert!(err.to_string().contains("unknown database"));

    let err = engine.execute("DETACH other").unwrap_err();
    assert!(err.to_string().contains("not attached"));
}

#[test]
fn attach_rejects_duplicate_alias() {
    let dir = tempfile::tempdir().unwrap();
    let other_path = saved_store(&dir, "other.mq-db", "# Other Doc\n\nOther body\n");

    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    let attach_sql = format!("ATTACH DATABASE '{}' AS other", other_path.display());
    engine.execute(&attach_sql).unwrap();

    let err = engine.execute(&attach_sql).unwrap_err();
    assert!(err.to_string().contains("already attached"));
}

#[test]
fn attach_does_not_block_a_concurrent_writer_on_the_same_file() {
    let dir = tempfile::tempdir().unwrap();
    let other_path = saved_store(&dir, "other.mq-db", "# Other Doc\n\nOther body\n");

    let _writer = DocumentStore::open(&other_path).unwrap();

    let store = make_store();
    let engine = SqlEngine::new(&store).unwrap();
    engine
        .execute(&format!(
            "ATTACH DATABASE '{}' AS other",
            other_path.display()
        ))
        .unwrap();

    let result = engine.execute("SELECT content FROM other.blocks").unwrap();
    assert!(!result.rows.is_empty());
}

#[test]
fn qualified_writes_to_attached_store_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let other_path = saved_store(&dir, "other.mq-db", "# Other Doc\n\nOther body\n");

    let mut store = make_store();
    {
        let engine = SqlEngine::new(&store).unwrap();
        engine
            .execute(&format!(
                "ATTACH DATABASE '{}' AS other",
                other_path.display()
            ))
            .unwrap();
    }

    let err = SqlEngine::new(&store)
        .unwrap()
        .execute("CREATE TABLE other.t (a TEXT)")
        .unwrap_err();
    assert!(err.to_string().contains("not supported"));

    let err = store
        .execute_sql_mut("UPDATE other.blocks SET content = 'x' WHERE block_type = 'heading'")
        .unwrap_err();
    assert!(err.to_string().contains("not supported"));

    let err = store
        .execute_sql_mut("INSERT INTO other.blocks (block_type, content) VALUES ('paragraph', 'x')")
        .unwrap_err();
    assert!(err.to_string().contains("not supported"));
}

#[rstest::rstest]
#[case(r#"{"a":"x","n":3,"f":1.5,"b":true}"#, "a", "x")]
#[case(r#"{"a":"x","n":3,"f":1.5,"b":true}"#, "$.n", "3")]
#[case(r#"{"a":"x","n":3,"f":1.5,"b":true}"#, "f", "1.5")]
#[case(r#"{"a":"x","n":3,"f":1.5,"b":true}"#, "b", "true")]
#[case(r#"{"a":{"b":{"c":"deep"}}}"#, "$.a.b.c", "deep")]
#[case(r#"{"tags":["x","y"]}"#, "tags[1]", "y")]
#[case(r#"{"tags":["x","y"]}"#, "$.tags", r#"["x","y"]"#)]
#[case(r#"{"a":"say \"hi\", ok"}"#, "a", r#"say "hi", ok"#)]
#[case(r#"{"a":"x"}"#, "b", "NULL")]
#[case(r#"{"a":"x"}"#, "a.b", "NULL")]
#[case(r#"{"tags":["x"]}"#, "tags[5]", "NULL")]
#[case("not json", "a", "NULL")]
fn test_sql_json_extract(#[case] json: &str, #[case] path: &str, #[case] expected: &str) {
    let store = DocumentStore::new();
    let engine = SqlEngine::new(&store).unwrap();
    let json = json.replace('\'', "\'\'");
    let out = engine
        .execute(&format!("SELECT json_extract('{json}', '{path}')"))
        .unwrap();
    assert_eq!(out.rows[0][0], expected);
}

#[test]
fn test_sql_properties_json_escapes_control_characters() {
    let mut store = DocumentStore::new();
    store
        .add_str("---\ntitle: \"a\\nb\\tc \\\"q\\\"\"\n---\n\n# H\n")
        .unwrap();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute("SELECT json_extract(properties, 'title') FROM documents")
        .unwrap();
    assert_eq!(out.rows[0][0], "a\nb\tc \"q\"");
}

#[test]
fn test_sql_json_output_escapes_control_characters() {
    let mut store = DocumentStore::new();
    store.add_str("# H\n\n```\na\tb\n```\n").unwrap();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute("SELECT content FROM blocks WHERE block_type = 'code'")
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&out.to_json()).unwrap();
    assert_eq!(parsed[0]["content"], "a\tb");
}

#[rstest::rstest]
#[case("l.url LIKE d.title || '%'", 1)]
#[case("l.url NOT LIKE d.title || 'z%'", 1)]
#[case("l.url NOT LIKE d.title", 1)]
#[case("l.url LIKE '%' || d.title", 0)]
#[case("l.url REGEXP d.title", 1)]
#[case("CASE WHEN d.title = 'a' THEN l.url = 'a.md' ELSE false END", 1)]
#[case("l.url = CAST(d.title AS TEXT) || '.md'", 1)]
fn test_sql_correlated_subquery_expression_forms(#[case] cond: &str, #[case] expected: usize) {
    let mut store = DocumentStore::new();
    store.add_str("# a\n\n[x](a.md)\n").unwrap();
    store.add_str("# b\n\nplain\n").unwrap();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine
        .execute(&format!(
            "SELECT d.title FROM documents d \
             WHERE EXISTS (SELECT 1 FROM links l WHERE l.document_id = d.id AND {cond})"
        ))
        .unwrap();
    assert_eq!(out.rows.len(), expected, "{cond}");
}

#[rstest::rstest]
#[case("cosine_similarity('[1,0]', '[1,0]')", "1")]
#[case("cosine_similarity('[1,0]', '[0,1]')", "0")]
#[case("cosine_similarity('[3,4]', '[3,4]')", "1")]
#[case("dot_product('[1,2,3]', '[4,5,6]')", "32")]
#[case("l2_distance('[0,0]', '[3,4]')", "5")]
#[case("COSINE_SIMILARITY('[1,0]', '[1,0]')", "1")]
#[case("cosine_similarity('[1,0]', '[1,0,0]')", "NULL")]
#[case("cosine_similarity('[0,0]', '[1,0]')", "NULL")]
#[case("cosine_similarity('[]', '[]')", "NULL")]
#[case("cosine_similarity('[1,\"a\"]', '[1,0]')", "NULL")]
#[case("cosine_similarity('not json', '[1,0]')", "NULL")]
#[case("cosine_similarity(NULL, '[1,0]')", "NULL")]
fn test_sql_vector_functions(#[case] expr: &str, #[case] expected: &str) {
    let store = DocumentStore::new();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine.execute(&format!("SELECT {expr}")).unwrap();
    assert_eq!(out.rows[0][0], expected, "{expr}");
}

#[test]
fn test_sql_nearest_by_embedding() {
    let store = DocumentStore::new();
    let engine = SqlEngine::new(&store).unwrap();
    engine
        .execute("CREATE TABLE emb (name TEXT, embedding TEXT)")
        .unwrap();
    engine
        .execute("INSERT INTO emb VALUES ('x', '[1,0]'), ('y', '[0,1]'), ('xy', '[0.7,0.7]')")
        .unwrap();
    let out = engine
        .execute(
            "SELECT name FROM emb \
             ORDER BY cosine_similarity(embedding, '[0.1,1]') DESC LIMIT 2",
        )
        .unwrap();
    assert_eq!(
        out.rows,
        vec![vec!["y".to_string()], vec!["xy".to_string()]]
    );
}

#[rstest::rstest]
#[case("1 = '1'", "true")]
#[case("'1' = 1", "true")]
#[case("1 <> '1'", "false")]
#[case("1 = '01'", "false")]
#[case("1 = ' 1'", "false")]
#[case("1 = 'x'", "false")]
#[case("2 > '1'", "true")]
#[case("1 < '10'", "true")]
fn test_sql_integer_string_comparison(#[case] expr: &str, #[case] expected: &str) {
    let store = DocumentStore::new();
    let engine = SqlEngine::new(&store).unwrap();
    let out = engine.execute(&format!("SELECT {expr}")).unwrap();
    assert_eq!(out.rows[0][0], expected, "{expr}");
}

#[test]
fn test_sql_join_custom_table_text_key_to_block_id() {
    let mut store = DocumentStore::new();
    store.add_str("# One\n\nalpha\n\n# Two\n\nbeta\n").unwrap();
    let engine = SqlEngine::new(&store).unwrap();
    engine
        .execute("CREATE TABLE emb (block_id INTEGER, embedding TEXT)")
        .unwrap();
    engine
        .execute("INSERT INTO emb VALUES (1, '[1,0]'), (3, '[0,1]'), (0, '[0.7,0.7]')")
        .unwrap();
    // blocks: 0=One 1=alpha 2=Two 3=beta
    let out = engine
        .execute(
            "SELECT b.content FROM emb e JOIN blocks b ON b.id = e.block_id \
             ORDER BY cosine_similarity(e.embedding, '[0.1,1]') DESC LIMIT 2",
        )
        .unwrap();
    assert_eq!(
        out.rows,
        vec![vec!["beta".to_string()], vec!["One".to_string()]]
    );
}
