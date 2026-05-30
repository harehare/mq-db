//! mqdb CLI – Markdown-specialised embedded database command-line tool.

use std::{
    io::{BufRead, Write},
    path::{Path, PathBuf},
};

use clap::{Parser, Subcommand, ValueEnum};
use mqdb::{DocumentStore, MqEngine, SqlEngine, block::BlockType, sql::html_escape};

// ─────────────────────────────────────────────────────────────────────────────
// CLI structure
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "mqdb", about = "Markdown-specialised embedded database", version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Index Markdown files and save to a .mqdb store file
    Index {
        /// Markdown files or directories to index
        #[arg(required = true)]
        paths: Vec<PathBuf>,

        /// Output store file (default: store.mqdb)
        #[arg(short, long, default_value = "store.mqdb")]
        output: PathBuf,

        /// Recursively walk directories
        #[arg(short, long)]
        recursive: bool,

        /// Do not store source line/column spans (saves ~21 bytes per block)
        #[arg(long)]
        no_spans: bool,
    },

    /// List all indexed documents
    List {
        /// Path to .mqdb store file
        #[arg(short, long, default_value = "store.mqdb")]
        db: PathBuf,

        /// Output format
        #[arg(long, short = 'F', default_value = "table")]
        format: OutputFormat,
    },

    /// Run an mq query over the store
    Mq {
        /// mq program code (e.g. ".h1", "select(.code_lang == \"rust\")")
        code: String,

        /// Path to .mqdb store file
        #[arg(short, long, default_value = "store.mqdb")]
        db: PathBuf,

        /// Output format
        #[arg(long, short = 'F', default_value = "table")]
        format: OutputFormat,
    },

    /// Run a SQL query over the store
    Sql {
        /// SQL query string
        query: Option<String>,

        /// Path to .mqdb store file
        #[arg(short, long, default_value = "store.mqdb")]
        db: PathBuf,

        /// Read SQL from a file
        #[arg(short, long)]
        file: Option<PathBuf>,

        /// Output format
        #[arg(long, short = 'F', default_value = "table")]
        format: OutputFormat,
    },

    /// Interactive REPL (supports both mq and SQL)
    Repl {
        /// Path to .mqdb store file
        #[arg(short, long, default_value = "store.mqdb")]
        db: PathBuf,

        /// Initial query mode
        #[arg(short, long, default_value = "sql")]
        mode: ReplMode,
    },

    /// Run structural lint checks
    Lint {
        /// Path to .mqdb store file
        #[arg(short, long, default_value = "store.mqdb")]
        db: PathBuf,

        /// Heading depth to check (H1=1 .. H6=6)
        #[arg(long, default_value_t = 2)]
        depth: u8,
    },

    /// Show store statistics
    Stats {
        /// Path to .mqdb store file
        #[arg(short, long, default_value = "store.mqdb")]
        db: PathBuf,
    },

    /// Show all blocks in a document
    Show {
        /// Document ID to show
        doc_id: u32,

        /// Path to .mqdb store file
        #[arg(short, long, default_value = "store.mqdb")]
        db: PathBuf,
    },

    /// Launch the interactive TUI
    Tui {
        /// Path to .mqdb store file
        #[arg(short, long, default_value = "store.mqdb")]
        db: PathBuf,
    },
}

#[derive(Clone, ValueEnum, Debug, Default)]
enum OutputFormat {
    /// Unicode box-drawing table (default)
    #[default]
    Table,
    /// JSON array of objects
    Json,
    /// CSV with header row
    Csv,
    /// Tab-separated values with header row
    Tsv,
    /// GFM Markdown table / reconstructed Markdown (for mq)
    Markdown,
    /// HTML table / HTML blocks (for mq)
    Html,
}

#[derive(Clone, ValueEnum, Debug)]
enum ReplMode {
    Mq,
    Sql,
}

impl std::fmt::Display for ReplMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReplMode::Mq => write!(f, "mq"),
            ReplMode::Sql => write!(f, "sql"),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

fn collect_md_files(paths: &[PathBuf], recursive: bool) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for path in paths {
        if path.is_file() {
            if is_markdown(path) {
                files.push(path.clone());
            }
        } else if path.is_dir() {
            collect_dir(path, recursive, &mut files);
        } else {
            eprintln!("Warning: {} does not exist, skipping", path.display());
        }
    }
    files
}

fn collect_dir(dir: &Path, recursive: bool, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_file() && is_markdown(&path) {
            out.push(path);
        } else if path.is_dir() && recursive {
            collect_dir(&path, recursive, out);
        }
    }
}

fn is_markdown(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("md") | Some("markdown")
    )
}

fn load_store(db: &Path) -> anyhow::Result<DocumentStore> {
    if !db.exists() {
        anyhow::bail!(
            "Store file not found: {}\nRun `mqdb index <files...>` to create it.",
            db.display()
        );
    }
    DocumentStore::load(db).map_err(|e| anyhow::anyhow!("Failed to load store: {}", e))
}

/// Load only catalog metadata (zone maps, paths, block counts) — no block data.
/// Use this for commands that don't need block content.
fn load_catalog_store(db: &Path) -> anyhow::Result<DocumentStore> {
    if !db.exists() {
        anyhow::bail!(
            "Store file not found: {}\nRun `mqdb index <files...>` to create it.",
            db.display()
        );
    }
    DocumentStore::load_catalog_only(db)
        .map_err(|e| anyhow::anyhow!("Failed to load store: {}", e))
}

fn bar(count: usize, max: usize, width: usize) -> String {
    if max == 0 {
        return " ".repeat(width);
    }
    let filled = (count * width / max).min(width);
    let empty = width - filled;
    format!("{}{}", "█".repeat(filled), "░".repeat(empty))
}

fn block_type_icon(bt: &BlockType) -> &'static str {
    match bt {
        BlockType::Heading => "#",
        BlockType::Paragraph => "¶",
        BlockType::Code => "{}",
        BlockType::List => "•",
        BlockType::TableCell | BlockType::TableRow | BlockType::TableAlign => "▦",
        BlockType::Blockquote => "❝",
        BlockType::HorizontalRule => "─",
        BlockType::Html => "<>",
        BlockType::Yaml | BlockType::Toml => "≡",
        BlockType::Math => "∑",
        BlockType::Definition => "§",
        BlockType::Footnote => "†",
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Main
// ─────────────────────────────────────────────────────────────────────────────

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        // ── index ────────────────────────────────────────────────────────────
        Commands::Index { paths, output, recursive, no_spans } => {
            let files = collect_md_files(&paths, recursive);
            if files.is_empty() {
                anyhow::bail!("No Markdown files found in the specified paths.");
            }

            let mut store = DocumentStore::new();
            if no_spans {
                store.set_store_spans(false);
            }
            let mut errors = 0usize;
            for path in &files {
                match store.add_file(path) {
                    Ok(_) => eprintln!("  ✓ {}", path.display()),
                    Err(e) => {
                        eprintln!("  ✗ {}: {}", path.display(), e);
                        errors += 1;
                    }
                }
            }

            store
                .save(&output)
                .map_err(|e| anyhow::anyhow!("Failed to save store: {}", e))?;

            let indexed = files.len() - errors;
            println!(
                "\nIndexed {} file{}{} → {}",
                indexed,
                if indexed == 1 { "" } else { "s" },
                if errors > 0 { format!("  ({} failed)", errors) } else { String::new() },
                output.display()
            );
        }

        // ── list ─────────────────────────────────────────────────────────────
        Commands::List { db, format } => {
            // Catalog-only: skip deserialising all block data for a listing.
            let store = load_catalog_store(&db)?;
            if store.is_empty() {
                println!("(no documents indexed)");
                return Ok(());
            }

            match format {
                OutputFormat::Json
                | OutputFormat::Csv
                | OutputFormat::Tsv
                | OutputFormat::Markdown
                | OutputFormat::Html => {
                    let engine = SqlEngine::new(&store).map_err(|e| anyhow::anyhow!("{}", e))?;
                    let out = engine
                        .execute("SELECT id, path, title, tags FROM documents")
                        .map_err(|e| anyhow::anyhow!("{}", e))?;
                    match format {
                        OutputFormat::Json => print!("{}", out.to_json()),
                        OutputFormat::Csv => print!("{}", out.to_csv()),
                        OutputFormat::Tsv => print!("{}", out.to_tsv()),
                        OutputFormat::Markdown => print!("{}", out.to_markdown_table()),
                        OutputFormat::Html => print!("{}", out.to_html_table()),
                        OutputFormat::Table => unreachable!(),
                    }
                }
                OutputFormat::Table => {
                    // Compute column widths
                    let path_width = store
                        .documents()
                        .iter()
                        .map(|d| {
                            d.path
                                .as_ref()
                                .map(|p| p.to_string_lossy().len())
                                .unwrap_or(10)
                                .min(52)
                        })
                        .max()
                        .unwrap_or(10)
                        .max(12); // "Path / Title" header
                    let tag_width = store
                        .documents()
                        .iter()
                        .map(|d| d.zone_maps.tags.join(", ").len())
                        .max()
                        .unwrap_or(0)
                        .max(4); // "Tags" header

                    let sep_id = "──────";
                    let sep_path = "─".repeat(path_width + 2);
                    let sep_blocks = "────────";
                    let sep_tags = "─".repeat(tag_width.max(4) + 2);

                    println!(
                        "┌{}┬{}┬{}┬{}┐",
                        sep_id, sep_path, sep_blocks, sep_tags
                    );
                    println!(
                        "│ {:<4} │ {:<path_width$} │ {:>6} │ {:<tag_w$} │",
                        "ID",
                        "Path / Title",
                        "Blocks",
                        "Tags",
                        path_width = path_width,
                        tag_w = tag_width.max(4),
                    );
                    println!(
                        "├{}┼{}┼{}┼{}┤",
                        sep_id, sep_path, sep_blocks, sep_tags
                    );

                    for doc in store.documents() {
                        let path_str = doc
                            .path
                            .as_ref()
                            .map(|p| p.to_string_lossy().to_string())
                            .unwrap_or_else(|| {
                                doc.zone_maps
                                    .title
                                    .clone()
                                    .unwrap_or_else(|| format!("<doc {}>", doc.id))
                            });
                        let path_display = if path_str.len() > path_width {
                            format!("…{}", &path_str[path_str.len() - path_width + 1..])
                        } else {
                            path_str.clone()
                        };
                        let tags = doc.zone_maps.tags.join(", ");
                        println!(
                            "│ {:>4} │ {:<path_width$} │ {:>6} │ {:<tag_w$} │",
                            doc.id,
                            path_display,
                            doc.block_count,
                            tags,
                            path_width = path_width,
                            tag_w = tag_width.max(4),
                        );
                    }

                    println!(
                        "└{}┴{}┴{}┴{}┘",
                        sep_id, sep_path, sep_blocks, sep_tags
                    );
                    println!(
                        "{} document{}",
                        store.len(),
                        if store.len() == 1 { "" } else { "s" }
                    );
                }
            }
        }

        // ── mq ───────────────────────────────────────────────────────────────
        Commands::Mq { code, db, format } => {
            let store = load_store(&db)?;
            let results = MqEngine::eval_store(&code, &store)
                .map_err(|e| anyhow::anyhow!("{}", e))?;
            if results.is_empty() {
                println!("(no results)");
            } else {
                match format {
                    OutputFormat::Json => {
                        let items: Vec<String> = results
                            .iter()
                            .map(|s| {
                                format!(
                                    "\"{}\"",
                                    s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
                                )
                            })
                            .collect();
                        println!("[{}]", items.join(","));
                    }
                    OutputFormat::Csv => {
                        println!("content");
                        for line in &results {
                            let cell = if line.contains(',')
                                || line.contains('"')
                                || line.contains('\n')
                            {
                                format!("\"{}\"", line.replace('"', "\"\""))
                            } else {
                                line.clone()
                            };
                            println!("{}", cell);
                        }
                    }
                    OutputFormat::Tsv => {
                        println!("content");
                        for line in &results {
                            println!("{}", line);
                        }
                    }
                    OutputFormat::Markdown => {
                        println!("{}", mq_to_markdown(&results));
                    }
                    OutputFormat::Html => {
                        print!("{}", mq_to_html(&results));
                    }
                    OutputFormat::Table => {
                        for line in &results {
                            println!("{}", line);
                        }
                    }
                }
            }
        }

        // ── sql ──────────────────────────────────────────────────────────────
        Commands::Sql { query, db, file, format } => {
            let sql = if let Some(f) = file {
                std::fs::read_to_string(&f)
                    .map_err(|e| anyhow::anyhow!("Cannot read file {}: {}", f.display(), e))?
            } else if let Some(q) = query {
                q
            } else {
                anyhow::bail!("Provide a query argument or --file <path>");
            };

            let store = load_store(&db)?;
            let engine = SqlEngine::new(&store).map_err(|e| anyhow::anyhow!("{}", e))?;
            let out = engine.execute(&sql).map_err(|e| anyhow::anyhow!("{}", e))?;
            match format {
                OutputFormat::Table => print!("{}", out.to_table()),
                OutputFormat::Json => print!("{}", out.to_json()),
                OutputFormat::Csv => print!("{}", out.to_csv()),
                OutputFormat::Tsv => print!("{}", out.to_tsv()),
                OutputFormat::Markdown => print!("{}", out.to_markdown_table()),
                OutputFormat::Html => print!("{}", out.to_html_table()),
            }
        }

        // ── repl ─────────────────────────────────────────────────────────────
        Commands::Repl { db, mode } => {
            let store = load_store(&db)?;
            run_repl(store, mode)?;
        }

        // ── lint ─────────────────────────────────────────────────────────────
        Commands::Lint { db, depth } => {
            let store = load_store(&db)?;
            let q = store.query();
            let violations = q.lint_heading_followed_by(depth, &[BlockType::List]);
            if violations.is_empty() {
                println!("✓  No violations  (H{} must not be immediately followed by a list)", depth);
            } else {
                let n = violations.len();
                println!(
                    "✗  {} violation{}  (H{} immediately followed by list)\n",
                    n,
                    if n == 1 { "" } else { "s" },
                    depth
                );
                println!("  {:<40}  heading", "file");
                println!("  {}  {}", "─".repeat(40), "─".repeat(30));
                for v in &violations {
                    let path = v
                        .document
                        .path
                        .as_ref()
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_else(|| format!("<doc {}>", v.document.id));
                    let path_display = if path.len() > 40 {
                        format!("…{}", &path[path.len() - 39..])
                    } else {
                        path
                    };
                    println!("  {:<40}  \"{}\"", path_display, v.heading.content);
                }
            }
        }

        // ── stats ─────────────────────────────────────────────────────────────
        Commands::Stats { db } => {
            let store = load_store(&db)?;
            let mut type_counts: std::collections::HashMap<BlockType, usize> =
                std::collections::HashMap::new();
            let mut lang_counts: std::collections::HashMap<String, usize> =
                std::collections::HashMap::new();
            let mut total_blocks = 0usize;

            for doc in store.documents() {
                total_blocks += doc.blocks.len();
                for block in &doc.blocks {
                    *type_counts.entry(block.block_type.clone()).or_insert(0) += 1;
                    if block.block_type == BlockType::Code
                        && let Some(lang) = block.code_lang()
                    {
                        *lang_counts.entry(lang.to_string()).or_insert(0) += 1;
                    }
                }
            }

            println!("  Documents  {}", store.len());
            println!("  Blocks     {}", total_blocks);

            let mut types: Vec<(BlockType, usize)> =
                type_counts.into_iter().collect();
            types.sort_by_key(|(_, v)| std::cmp::Reverse(*v));
            let max_type = types.first().map(|(_, v)| *v).unwrap_or(1);

            println!("\n  Block types");
            println!("  {}", "─".repeat(56));
            for (bt, count) in &types {
                let pct = count * 100 / total_blocks.max(1);
                let b = bar(*count, max_type, 20);
                let icon = block_type_icon(bt);
                println!(
                    "  {:>2}  {:<12}  {}  {:>5}  ({:>2}%)",
                    icon,
                    bt.as_str(),
                    b,
                    count,
                    pct,
                );
            }

            if !lang_counts.is_empty() {
                let mut langs: Vec<(String, usize)> =
                    lang_counts.into_iter().collect();
                langs.sort_by_key(|(_, v)| std::cmp::Reverse(*v));
                let max_lang = langs.first().map(|(_, v)| *v).unwrap_or(1);
                let total_code: usize = langs.iter().map(|(_, v)| v).sum();

                println!("\n  Code languages");
                println!("  {}", "─".repeat(56));
                for (lang, count) in &langs {
                    let pct = count * 100 / total_code.max(1);
                    let b = bar(*count, max_lang, 20);
                    println!(
                        "  {{}}  {:<12}  {}  {:>5}  ({:>2}%)",
                        lang, b, count, pct
                    );
                }
            }
        }

        // ── show ─────────────────────────────────────────────────────────────
        Commands::Show { doc_id, db } => {
            let store = load_store(&db)?;
            let doc = store
                .get_document(doc_id)
                .ok_or_else(|| anyhow::anyhow!("Document {} not found", doc_id))?;

            let path = doc
                .path
                .as_ref()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|| format!("<doc {}>", doc.id));

            println!("  {}", path);
            if let Some(title) = &doc.zone_maps.title {
                println!("  title   {}", title);
            }
            println!("  blocks  {}", doc.blocks.len());
            if !doc.zone_maps.tags.is_empty() {
                println!("  tags    {}", doc.zone_maps.tags.join(", "));
            }
            println!();

            // Column widths
            let pre_w = doc.blocks.iter().map(|b| digits(b.pre)).max().unwrap_or(3).max(3);
            let post_w = doc.blocks.iter().map(|b| digits(b.post)).max().unwrap_or(4).max(4);

            println!(
                "  {:<pre_w$}  {:<post_w$}  {:<16}  content",
                "pre", "post", "type",
                pre_w = pre_w, post_w = post_w,
            );
            println!(
                "  {}  {}  {}  {}",
                "─".repeat(pre_w),
                "─".repeat(post_w),
                "─".repeat(16),
                "─".repeat(40),
            );

            for block in &doc.blocks {
                let depth = block.heading_depth().unwrap_or(0) as usize;
                let indent = if depth > 1 {
                    format!("{}", "  ".repeat(depth - 1))
                } else {
                    String::new()
                };
                let type_label = match block.block_type {
                    BlockType::Heading => format!(
                        "heading H{}",
                        block.heading_depth().unwrap_or(0)
                    ),
                    ref bt => bt.as_str().to_string(),
                };
                let preview: String = block.content.chars().take(48).collect();
                let preview = if block.content.chars().count() > 48 {
                    format!("{}…", preview)
                } else {
                    preview
                };
                // Strip newlines for display
                let preview = preview.replace('\n', " ");
                println!(
                    "  {:<pre_w$}  {:<post_w$}  {:<16}  {}{}",
                    block.pre,
                    block.post,
                    type_label,
                    indent,
                    preview,
                    pre_w = pre_w,
                    post_w = post_w,
                );
            }
        }

        // ── tui ──────────────────────────────────────────────────────────────
        Commands::Tui { db } => {
            let store = if db.exists() {
                DocumentStore::load(&db).map_err(|e| anyhow::anyhow!("{}", e))?
            } else {
                eprintln!("No store found at {}. Starting with empty store.", db.display());
                DocumentStore::new()
            };
            mqdb::tui::run(store).map_err(|e| anyhow::anyhow!("{}", e))?;
        }
    }

    Ok(())
}

fn digits(n: u32) -> usize {
    if n == 0 { 1 } else { n.ilog10() as usize + 1 }
}

/// Join mq results back into reconstructed Markdown, separated by blank lines.
/// Adjacent list items are kept together without a blank line between them.
fn mq_to_markdown(results: &[String]) -> String {
    let mut out = String::new();
    for (i, block) in results.iter().enumerate() {
        if i > 0 {
            let prev = &results[i - 1];
            let prev_is_list = prev.trim_start().starts_with("- ") || prev.trim_start().starts_with("* ") || prev.trim_start().chars().next().is_some_and(|c| c.is_ascii_digit());
            let curr_is_list = block.trim_start().starts_with("- ") || block.trim_start().starts_with("* ") || block.trim_start().chars().next().is_some_and(|c| c.is_ascii_digit());
            if prev_is_list && curr_is_list {
                out.push('\n');
            } else {
                out.push_str("\n\n");
            }
        }
        out.push_str(block);
    }
    out
}

/// Convert mq results (markdown block strings) to HTML.
fn mq_to_html(results: &[String]) -> String {
    let mut out = String::new();
    for block in results {
        out.push_str(&md_block_to_html(block));
        out.push('\n');
    }
    out
}

fn md_block_to_html(s: &str) -> String {
    let trimmed = s.trim();

    // Headings: # … ######
    for depth in (1u8..=6).rev() {
        let prefix = "#".repeat(depth as usize);
        if let Some(rest) = trimmed.strip_prefix(&prefix) {
            if rest.starts_with(' ') || rest.is_empty() {
                let text = html_escape(rest.trim());
                return format!("<h{depth}>{text}</h{depth}>");
            }
        }
    }

    // Fenced code block
    if trimmed.starts_with("```") {
        let first_line = trimmed.lines().next().unwrap_or("");
        let lang = first_line.trim_start_matches('`').trim();
        let code: String = trimmed
            .lines()
            .skip(1)
            .take_while(|l| !l.trim_start().starts_with("```"))
            .collect::<Vec<_>>()
            .join("\n");
        let escaped = html_escape(&code);
        return if lang.is_empty() {
            format!("<pre><code>{escaped}</code></pre>")
        } else {
            format!("<pre><code class=\"language-{lang}\">{escaped}</code></pre>")
        };
    }

    // Blockquote
    if trimmed.starts_with("> ") {
        let inner = trimmed
            .lines()
            .map(|l| l.strip_prefix("> ").unwrap_or(l))
            .collect::<Vec<_>>()
            .join("\n");
        return format!("<blockquote><p>{}</p></blockquote>", html_escape(&inner));
    }

    // Horizontal rule
    if matches!(trimmed, "---" | "***" | "___") {
        return "<hr>".to_string();
    }

    // Unordered list
    if trimmed.lines().all(|l| {
        let l = l.trim();
        l.is_empty() || l.starts_with("- ") || l.starts_with("* ")
    }) && trimmed.lines().any(|l| l.trim().starts_with("- ") || l.trim().starts_with("* ")) {
        let items: String = trimmed
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                let text = l.trim().trim_start_matches("- ").trim_start_matches("* ");
                format!("<li>{}</li>", html_escape(text))
            })
            .collect::<Vec<_>>()
            .join("\n");
        return format!("<ul>\n{items}\n</ul>");
    }

    // Ordered list
    if trimmed.lines().all(|l| {
        let l = l.trim();
        l.is_empty() || l.chars().next().is_some_and(|c| c.is_ascii_digit())
    }) && trimmed.lines().any(|l| l.trim().chars().next().is_some_and(|c| c.is_ascii_digit())) {
        let items: String = trimmed
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                let text = l.trim().splitn(2, ". ").nth(1).unwrap_or(l.trim());
                format!("<li>{}</li>", html_escape(text))
            })
            .collect::<Vec<_>>()
            .join("\n");
        return format!("<ol>\n{items}\n</ol>");
    }

    // Paragraph (default)
    format!("<p>{}</p>", html_escape(trimmed))
}

// ─────────────────────────────────────────────────────────────────────────────
// REPL
// ─────────────────────────────────────────────────────────────────────────────

fn run_repl(store: DocumentStore, initial_mode: ReplMode) -> anyhow::Result<()> {
    let stdin = std::io::stdin();
    let mut mode = initial_mode;

    println!("mqdb  (.help for commands  .quit to exit)");
    println!("mode: {}  (.mode mq | .mode sql)\n", mode);

    loop {
        print!("{}> ", mode);
        std::io::stdout().flush()?;

        let mut line = String::new();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => anyhow::bail!("Read error: {}", e),
        }
        let input = line.trim();

        if input.is_empty() {
            continue;
        }

        match input {
            ".quit" | ".exit" | "\\q" => break,
            ".help" => print_repl_help(),
            ".mode mq" => {
                mode = ReplMode::Mq;
                println!("→ mq mode");
            }
            ".mode sql" => {
                mode = ReplMode::Sql;
                println!("→ sql mode");
            }
            _ => match mode {
                ReplMode::Sql => match SqlEngine::new(&store) {
                    Ok(engine) => match engine.execute(input) {
                        Ok(out) => print!("{}", out.to_table()),
                        Err(e) => eprintln!("error: {}", e),
                    },
                    Err(e) => eprintln!("error: {}", e),
                },
                ReplMode::Mq => match MqEngine::eval_store(input, &store) {
                    Ok(results) => {
                        if results.is_empty() {
                            println!("(no results)");
                        } else {
                            for r in results {
                                println!("{}", r);
                            }
                        }
                    }
                    Err(e) => eprintln!("error: {}", e),
                },
            },
        }
    }

    println!("bye");
    Ok(())
}

fn print_repl_help() {
    println!(
        r#"
  .mode sql    switch to SQL mode
  .mode mq     switch to mq mode
  .quit        exit

  SQL examples
    SELECT block_type, count(*) FROM blocks GROUP BY block_type;
    SELECT content FROM blocks WHERE block_type = 'heading' ORDER BY pre;
    SELECT b.content FROM blocks b
      WHERE under(b.pre, b.post,
        (SELECT pre FROM blocks WHERE content = 'Architecture'),
        (SELECT post FROM blocks WHERE content = 'Architecture'));

  mq examples
    .h1
    .code
    select(.block_type == "heading")
"#
    );
}
