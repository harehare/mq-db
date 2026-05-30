//! mqdb CLI – Markdown-specialised embedded database command-line tool.
//!
//! # Commands
//!
//! ```text
//! mqdb index <paths...> [--output store.mqdb] [--recursive]
//! mqdb list  [--db store.mqdb]
//! mqdb mq    <code>  [--db store.mqdb]
//! mqdb sql   <query> [--db store.mqdb]
//! mqdb repl  [--db store.mqdb] [--mode mq|sql]
//! mqdb lint  [--db store.mqdb] [--depth <n>]
//! mqdb stats [--db store.mqdb]
//! mqdb show  <doc_id> [--db store.mqdb]
//! mqdb tui   [--db store.mqdb]
//! ```

use std::{
    io::{BufRead, Write},
    path::{Path, PathBuf},
};

use clap::{Parser, Subcommand, ValueEnum};
use mqdb::{DocumentStore, MqEngine, SqlEngine, block::BlockType};

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
    },

    /// List all indexed documents
    List {
        /// Path to .mqdb store file
        #[arg(short, long, default_value = "store.mqdb")]
        db: PathBuf,
    },

    /// Run an mq query over the store
    Mq {
        /// mq program code (e.g. ".h1", "select(.code_lang == \"rust\")")
        code: String,

        /// Path to .mqdb store file
        #[arg(short, long, default_value = "store.mqdb")]
        db: PathBuf,
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

/// Collect all Markdown file paths from a list of paths/directories.
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

/// Load a DocumentStore from disk, or return a helpful error.
fn load_store(db: &Path) -> anyhow::Result<DocumentStore> {
    if !db.exists() {
        anyhow::bail!(
            "Store file not found: {}\nRun `mqdb index <files...>` to create it.",
            db.display()
        );
    }
    DocumentStore::load(db).map_err(|e| anyhow::anyhow!("Failed to load store: {}", e))
}

// ─────────────────────────────────────────────────────────────────────────────
// Main
// ─────────────────────────────────────────────────────────────────────────────

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        // ── index ────────────────────────────────────────────────────────────
        Commands::Index { paths, output, recursive } => {
            let files = collect_md_files(&paths, recursive);
            if files.is_empty() {
                anyhow::bail!("No Markdown files found in the specified paths.");
            }

            let mut store = DocumentStore::new();
            let mut errors = 0usize;
            for path in &files {
                match store.add_file(path) {
                    Ok(_) => {}
                    Err(e) => {
                        eprintln!("  ✗ {}: {}", path.display(), e);
                        errors += 1;
                    }
                }
            }

            store
                .save(&output)
                .map_err(|e| anyhow::anyhow!("Failed to save store: {}", e))?;

            println!(
                "✓ Indexed {} file{} ({} error{}) → {}",
                files.len() - errors,
                if files.len() - errors == 1 { "" } else { "s" },
                errors,
                if errors == 1 { "" } else { "s" },
                output.display()
            );
        }

        // ── list ─────────────────────────────────────────────────────────────
        Commands::List { db } => {
            let store = load_store(&db)?;
            if store.is_empty() {
                println!("(no documents)");
                return Ok(());
            }
            println!("{:<6}  {:<50}  {:<8}  Tags", "ID", "Path / Title", "Blocks");
            println!("{}", "─".repeat(80));
            for doc in store.documents() {
                let path = doc
                    .path
                    .as_ref()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|| {
                        doc.zone_maps
                            .title
                            .clone()
                            .unwrap_or_else(|| format!("<doc {}>", doc.id))
                    });
                let tags = doc.zone_maps.tags.join(", ");
                println!(
                    "{:<6}  {:<50}  {:<8}  {}",
                    doc.id,
                    &path[..path.len().min(50)],
                    doc.blocks.len(),
                    tags
                );
            }
            println!("\n{} document{}", store.len(), if store.len() == 1 { "" } else { "s" });
        }

        // ── mq ───────────────────────────────────────────────────────────────
        Commands::Mq { code, db } => {
            let store = load_store(&db)?;
            let results = MqEngine::eval_store(&code, &store)
                .map_err(|e| anyhow::anyhow!("{}", e))?;
            for line in &results {
                println!("{}", line);
            }
            if results.is_empty() {
                println!("(no results)");
            }
        }

        // ── sql ──────────────────────────────────────────────────────────────
        Commands::Sql { query, db, file } => {
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
            print!("{}", out.to_table());
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
                println!("✓ No lint violations found (H{} → List rule).", depth);
            } else {
                println!(
                    "Found {} violation{}:\n",
                    violations.len(),
                    if violations.len() == 1 { "" } else { "s" }
                );
                for v in &violations {
                    let path = v
                        .document
                        .path
                        .as_ref()
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_else(|| format!("<doc {}>", v.document.id));
                    println!(
                        "  {}  H{} \"{}\" immediately followed by {}",
                        path,
                        depth,
                        v.heading.content,
                        v.offending.block_type.as_str()
                    );
                }
            }
        }

        // ── stats ─────────────────────────────────────────────────────────────
        Commands::Stats { db } => {
            let store = load_store(&db)?;
            let mut type_counts = std::collections::HashMap::new();
            let mut lang_counts = std::collections::HashMap::new();
            let mut total_blocks = 0usize;

            for doc in store.documents() {
                total_blocks += doc.blocks.len();
                for block in &doc.blocks {
                    *type_counts.entry(block.block_type.clone()).or_insert(0usize) += 1;
                    if block.block_type == BlockType::Code
                        && let Some(lang) = block.code_lang()
                    {
                        *lang_counts.entry(lang.to_string()).or_insert(0usize) += 1;
                    }
                }
            }

            println!("Documents : {}", store.len());
            println!("Blocks    : {}", total_blocks);
            println!("\nBlock type breakdown:");
            let mut types: Vec<_> = type_counts.iter().collect();
            types.sort_by_key(|(_, v)| std::cmp::Reverse(**v));
            for (bt, count) in &types {
                println!("  {:<20} {}", bt.as_str(), count);
            }

            if !lang_counts.is_empty() {
                println!("\nCode block languages:");
                let mut langs: Vec<_> = lang_counts.iter().collect();
                langs.sort_by_key(|(_, v)| std::cmp::Reverse(**v));
                for (lang, count) in &langs {
                    println!("  {:<20} {}", lang, count);
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
            println!("Document: {}  (id={})", path, doc.id);
            if let Some(title) = &doc.zone_maps.title {
                println!("Title: {}", title);
            }
            println!("Blocks: {}", doc.blocks.len());
            println!();
            println!(
                "{:<6}  {:<6}  {:<6}  {:<16}  content",
                "pre", "post", "depth", "type"
            );
            println!("{}", "─".repeat(70));

            for block in &doc.blocks {
                let depth_indent = "  ".repeat(
                    (block.pre as usize / 2).min(8),
                );
                let preview: String = block.content.chars().take(50).collect();
                let preview = if block.content.len() > 50 {
                    format!("{}…", preview)
                } else {
                    preview
                };
                println!(
                    "{:<6}  {:<6}  {:<6}  {:<16}  {}{}",
                    block.pre,
                    block.post,
                    block.heading_depth().map_or(String::new(), |d| format!("H{}", d)),
                    block.block_type.as_str(),
                    depth_indent,
                    preview
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

// ─────────────────────────────────────────────────────────────────────────────
// REPL
// ─────────────────────────────────────────────────────────────────────────────

fn run_repl(store: DocumentStore, initial_mode: ReplMode) -> anyhow::Result<()> {
    let stdin = std::io::stdin();
    let mut mode = initial_mode;

    println!("mqdb REPL  (type .help for commands, .quit to exit)");
    println!("Mode: {}  (.mode mq | .mode sql to switch)\n", mode);

    loop {
        print!("{}> ", mode);
        std::io::stdout().flush()?;

        let mut line = String::new();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => break, // EOF (Ctrl+D)
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
                println!("Switched to mq mode.");
            }
            ".mode sql" => {
                mode = ReplMode::Sql;
                println!("Switched to SQL mode.");
            }
            _ => {
                match mode {
                    ReplMode::Sql => {
                        match SqlEngine::new(&store) {
                            Ok(engine) => match engine.execute(input) {
                                Ok(out) => print!("{}", out.to_table()),
                                Err(e) => eprintln!("Error: {}", e),
                            },
                            Err(e) => eprintln!("Engine error: {}", e),
                        }
                    }
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
                        Err(e) => eprintln!("Error: {}", e),
                    },
                }
            }
        }
    }

    println!("Bye!");
    Ok(())
}

fn print_repl_help() {
    println!(
        r#"
mqdb REPL commands:
  .mode sql        Switch to SQL mode
  .mode mq         Switch to mq mode
  .quit / .exit    Exit the REPL
  .help            Show this help

SQL mode examples:
  SELECT block_type, count(*) FROM blocks GROUP BY block_type;
  SELECT content FROM blocks WHERE block_type = 'heading' ORDER BY pre;
  SELECT b.content FROM blocks b
    WHERE under(b.pre, b.post,
      (SELECT pre FROM blocks WHERE content = 'Architecture'),
      (SELECT post FROM blocks WHERE content = 'Architecture'));

mq mode examples:
  .h1              Extract all H1 headings
  .code            Extract all code blocks
  select(.block_type == "heading")
"#
    );
}
