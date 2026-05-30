//! Terminal User Interface for mqdb using ratatui + crossterm.
//!
//! Run with `mqdb tui [--db store.mqdb]`.
//!
//! # Layout
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │  mqdb  [Mode: SQL]  [Tab: switch mode]  [Ctrl+C: quit]      │
//! ├──────────────────┬──────────────────────────────────────────┤
//! │ Documents        │ Query                                     │
//! │ ▶ 1  README.md   │ > SELECT * FROM blocks LIMIT 5           │
//! │   2  DESIGN.md   ├──────────────────────────────────────────┤
//! │   3  ...         │ Results                                   │
//! │                  │ ...                                       │
//! └──────────────────┴──────────────────────────────────────────┘
//!   [j/k: navigate]  [i: focus input]  [Esc: blur]  [Enter: run]
//! ```

use std::io;

use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};

use crate::{DocumentStore, MqdbError, MqEngine, SqlEngine};

// ─────────────────────────────────────────────────────────────────────────────
// State
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryMode {
    Mq,
    Sql,
}

impl QueryMode {
    fn label(&self) -> &'static str {
        match self {
            QueryMode::Mq => "mq",
            QueryMode::Sql => "SQL",
        }
    }

    fn toggle(self) -> Self {
        match self {
            QueryMode::Mq => QueryMode::Sql,
            QueryMode::Sql => QueryMode::Mq,
        }
    }
}

struct App {
    store: DocumentStore,
    mode: QueryMode,
    input: String,
    results: Vec<String>,
    doc_list_state: ListState,
    input_focused: bool,
    result_scroll: u16,
}

impl App {
    fn new(store: DocumentStore) -> Self {
        let mut doc_list_state = ListState::default();
        if !store.documents().is_empty() {
            doc_list_state.select(Some(0));
        }
        Self {
            store,
            mode: QueryMode::Sql,
            input: String::new(),
            results: Vec::new(),
            doc_list_state,
            input_focused: false,
            result_scroll: 0,
        }
    }

    fn run_query(&mut self) {
        let code = self.input.trim().to_string();
        if code.is_empty() {
            return;
        }
        self.result_scroll = 0;
        match self.mode {
            QueryMode::Sql => {
                match SqlEngine::new(&self.store) {
                    Ok(engine) => match engine.execute(&code) {
                        Ok(out) => {
                            self.results = out.to_table().lines().map(String::from).collect();
                        }
                        Err(e) => self.results = vec![format!("Error: {}", e)],
                    },
                    Err(e) => self.results = vec![format!("Engine error: {}", e)],
                }
            }
            QueryMode::Mq => {
                match MqEngine::eval_store(&code, &self.store) {
                    Ok(lines) => {
                        if lines.is_empty() {
                            self.results = vec!["(no results)".to_string()];
                        } else {
                            self.results = lines;
                        }
                    }
                    Err(e) => self.results = vec![format!("Error: {}", e)],
                }
            }
        }
    }

    fn show_selected_document(&mut self) {
        if let Some(idx) = self.doc_list_state.selected()
            && let Some(doc) = self.store.documents().get(idx)
        {
            let mut lines = Vec::new();
            let path = doc
                .path
                .as_ref()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|| format!("<inline doc {}>", doc.id));
            lines.push(format!("Document: {}", path));
            lines.push(format!("Blocks: {}", doc.blocks.len()));
            lines.push(String::new());
            for block in &doc.blocks {
                let indent = "  ".repeat((block.pre as usize).min(6));
                let type_str = block.block_type.as_str();
                let preview: String = block.content.chars().take(60).collect();
                let preview = if block.content.len() > 60 {
                    format!("{}…", preview)
                } else {
                    preview
                };
                lines.push(format!(
                    "{}[{}] pre={} post={}  {}",
                    indent, type_str, block.pre, block.post, preview
                ));
            }
            self.results = lines;
            self.result_scroll = 0;
        }
    }

    fn doc_count(&self) -> usize {
        self.store.documents().len()
    }

    fn select_next(&mut self) {
        let count = self.doc_count();
        if count == 0 {
            return;
        }
        let i = self.doc_list_state.selected().map_or(0, |i| (i + 1).min(count - 1));
        self.doc_list_state.select(Some(i));
        self.show_selected_document();
    }

    fn select_prev(&mut self) {
        let count = self.doc_count();
        if count == 0 {
            return;
        }
        let i = self.doc_list_state.selected().map_or(0, |i| i.saturating_sub(1));
        self.doc_list_state.select(Some(i));
        self.show_selected_document();
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Entry point
// ─────────────────────────────────────────────────────────────────────────────

/// Launch the TUI. Blocks until the user quits.
pub fn run(store: DocumentStore) -> Result<(), MqdbError> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(store);
    // Show first document on startup
    app.show_selected_document();

    loop {
        terminal.draw(|f| ui(f, &mut app))?;

        if event::poll(std::time::Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
            && handle_key(&mut app, key)
        {
            break;
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    Ok(())
}

/// Returns `true` if the app should quit.
fn handle_key(app: &mut App, key: KeyEvent) -> bool {
    // Ctrl+C always quits
    if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('c') {
        return true;
    }

    if app.input_focused {
        match key.code {
            KeyCode::Esc => app.input_focused = false,
            KeyCode::Enter => app.run_query(),
            KeyCode::Backspace => { app.input.pop(); }
            KeyCode::Tab => {
                app.mode = app.mode.toggle();
            }
            KeyCode::Char(c) => app.input.push(c),
            _ => {}
        }
    } else {
        match key.code {
            KeyCode::Char('q') => return true,
            KeyCode::Char('i') => app.input_focused = true,
            KeyCode::Tab => {
                app.mode = app.mode.toggle();
            }
            KeyCode::Char('j') | KeyCode::Down => app.select_next(),
            KeyCode::Char('k') | KeyCode::Up => app.select_prev(),
            KeyCode::PageDown => app.result_scroll = app.result_scroll.saturating_add(5),
            KeyCode::PageUp => app.result_scroll = app.result_scroll.saturating_sub(5),
            _ => {}
        }
    }
    false
}

// ─────────────────────────────────────────────────────────────────────────────
// Rendering
// ─────────────────────────────────────────────────────────────────────────────

fn ui(f: &mut Frame, app: &mut App) {
    let area = f.area();

    // Split vertically: title bar + main area
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(area);

    render_title_bar(f, app, vertical[0]);

    // Split main area: left (docs) + right (query + results)
    let main = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
        .split(vertical[1]);

    render_doc_list(f, app, main[0]);

    // Split right: input (3 lines) + results
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0)])
        .split(main[1]);

    render_input(f, app, right[0]);
    render_results(f, app, right[1]);
}

fn render_title_bar(f: &mut Frame, app: &App, area: Rect) {
    let help = if app.input_focused {
        format!(
            " mqdb  Mode: {}  [Tab: switch mode]  [Enter: run]  [Esc: blur]  [Ctrl+C: quit]",
            app.mode.label()
        )
    } else {
        format!(
            " mqdb  Mode: {}  [Tab: switch]  [i: input]  [j/k: nav]  [q: quit]",
            app.mode.label()
        )
    };
    let style = Style::default()
        .bg(Color::Blue)
        .fg(Color::White)
        .add_modifier(Modifier::BOLD);
    let para = Paragraph::new(help).style(style);
    f.render_widget(para, area);
}

fn render_doc_list(f: &mut Frame, app: &mut App, area: Rect) {
    let items: Vec<ListItem> = app
        .store
        .documents()
        .iter()
        .map(|doc| {
            let label = doc
                .path
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| format!("doc {}", doc.id));
            let count = doc.blocks.len();
            ListItem::new(format!("{} ({})", label, count))
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" Documents "))
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    f.render_stateful_widget(list, area, &mut app.doc_list_state);
}

fn render_input(f: &mut Frame, app: &App, area: Rect) {
    let border_style = if app.input_focused {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    };
    let title = format!(" {} Query ", app.mode.label());
    let content = format!("{}_", app.input); // cursor indicator
    let widget = Paragraph::new(content)
        .block(Block::default().borders(Borders::ALL).title(title).border_style(border_style))
        .wrap(Wrap { trim: false });
    f.render_widget(widget, area);
}

fn render_results(f: &mut Frame, app: &App, area: Rect) {
    let lines: Vec<Line> = app
        .results
        .iter()
        .map(|s| Line::from(Span::raw(s.clone())))
        .collect();
    let widget = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" Results "))
        .wrap(Wrap { trim: false })
        .scroll((app.result_scroll, 0));
    f.render_widget(widget, area);
}
