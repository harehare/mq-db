//! Terminal User Interface for mqdb using ratatui + crossterm.

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

use crate::{DocumentStore, MqdbError, MqEngine, SqlEngine, block::BlockType};

// ─────────────────────────────────────────────────────────────────────────────
// State
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryMode {
    Mq,
    Sql,
}

impl QueryMode {
    fn label(self) -> &'static str {
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

// A single displayable line in the results pane, optionally styled.
#[derive(Clone)]
struct ResultLine {
    text: String,
    style: Style,
}

impl ResultLine {
    fn plain(text: impl Into<String>) -> Self {
        Self { text: text.into(), style: Style::default() }
    }

    fn styled(text: impl Into<String>, style: Style) -> Self {
        Self { text: text.into(), style }
    }
}

struct App {
    store: DocumentStore,
    mode: QueryMode,
    input: String,
    cursor_pos: usize,
    result_lines: Vec<ResultLine>,
    doc_list_state: ListState,
    input_focused: bool,
    result_scroll: u16,
    status_msg: Option<String>,
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
            cursor_pos: 0,
            result_lines: Vec::new(),
            doc_list_state,
            input_focused: false,
            result_scroll: 0,
            status_msg: None,
        }
    }

    fn run_query(&mut self) {
        let code = self.input.trim().to_string();
        if code.is_empty() {
            return;
        }
        self.result_scroll = 0;
        self.status_msg = None;

        match self.mode {
            QueryMode::Sql => match SqlEngine::new(&self.store) {
                Ok(engine) => match engine.execute(&code) {
                    Ok(out) => {
                        self.result_lines =
                            out.to_table().lines().map(|l| ResultLine::plain(l)).collect();
                        self.status_msg =
                            Some(format!("{} row{}", out.rows.len(), if out.rows.len() == 1 { "" } else { "s" }));
                    }
                    Err(e) => {
                        self.result_lines = vec![ResultLine::styled(
                            format!("error: {}", e),
                            Style::default().fg(Color::Red),
                        )];
                    }
                },
                Err(e) => {
                    self.result_lines = vec![ResultLine::styled(
                        format!("engine error: {}", e),
                        Style::default().fg(Color::Red),
                    )];
                }
            },
            QueryMode::Mq => match MqEngine::eval_store(&code, &self.store) {
                Ok(lines) => {
                    if lines.is_empty() {
                        self.result_lines = vec![ResultLine::styled(
                            "(no results)".to_string(),
                            Style::default().fg(Color::DarkGray),
                        )];
                    } else {
                        self.result_lines = lines.iter().map(|l| ResultLine::plain(l)).collect();
                        self.status_msg = Some(format!("{} result{}", lines.len(), if lines.len() == 1 { "" } else { "s" }));
                    }
                }
                Err(e) => {
                    self.result_lines = vec![ResultLine::styled(
                        format!("error: {}", e),
                        Style::default().fg(Color::Red),
                    )];
                }
            },
        }
    }

    fn show_selected_document(&mut self) {
        let Some(idx) = self.doc_list_state.selected() else { return };
        let Some(doc) = self.store.documents().get(idx) else { return };

        let mut lines: Vec<ResultLine> = Vec::new();

        let path = doc
            .path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| format!("<inline doc {}>", doc.id));

        lines.push(ResultLine::styled(
            path,
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ));
        if let Some(title) = &doc.zone_maps.title {
            lines.push(ResultLine::styled(
                format!("  title   {}", title),
                Style::default().fg(Color::White),
            ));
        }
        lines.push(ResultLine::styled(
            format!("  blocks  {}", doc.blocks.len()),
            Style::default().fg(Color::DarkGray),
        ));
        if !doc.zone_maps.tags.is_empty() {
            lines.push(ResultLine::styled(
                format!("  tags    {}", doc.zone_maps.tags.join(", ")),
                Style::default().fg(Color::Yellow),
            ));
        }
        lines.push(ResultLine::plain(String::new()));

        // Header
        lines.push(ResultLine::styled(
            format!("  {:<4}  {:<4}  {:<14}  content", "pre", "post", "type"),
            Style::default().fg(Color::DarkGray),
        ));
        lines.push(ResultLine::styled(
            format!("  {}  {}  {}  {}", "────", "────", "──────────────", "─".repeat(40)),
            Style::default().fg(Color::DarkGray),
        ));

        for block in &doc.blocks {
            let (icon, type_label, color) = block_display(&block.block_type, block.heading_depth());
            let depth = block.heading_depth().unwrap_or(0) as usize;
            let indent = "  ".repeat(depth.saturating_sub(1));
            let preview: String = block.content.chars().take(48).collect();
            let preview = if block.content.chars().count() > 48 {
                format!("{}…", preview)
            } else {
                preview
            };
            let preview = preview.replace('\n', " ");

            lines.push(ResultLine::styled(
                format!(
                    "  {:>4}  {:>4}  {:<2} {:<12}  {}{}",
                    block.pre, block.post, icon, type_label, indent, preview,
                ),
                Style::default().fg(color),
            ));
        }

        self.result_lines = lines;
        self.result_scroll = 0;
        self.status_msg = None;
    }

    fn doc_count(&self) -> usize {
        self.store.documents().len()
    }

    fn select_next(&mut self) {
        let count = self.doc_count();
        if count == 0 { return; }
        let i = self.doc_list_state.selected().map_or(0, |i| (i + 1).min(count - 1));
        self.doc_list_state.select(Some(i));
        self.show_selected_document();
    }

    fn select_prev(&mut self) {
        let count = self.doc_count();
        if count == 0 { return; }
        let i = self.doc_list_state.selected().map_or(0, |i| i.saturating_sub(1));
        self.doc_list_state.select(Some(i));
        self.show_selected_document();
    }

    fn insert_char(&mut self, c: char) {
        let byte_pos = self.input.char_indices().nth(self.cursor_pos).map_or(self.input.len(), |(i, _)| i);
        self.input.insert(byte_pos, c);
        self.cursor_pos += 1;
    }

    fn delete_char_before(&mut self) {
        if self.cursor_pos == 0 { return; }
        self.cursor_pos -= 1;
        let byte_pos = self.input.char_indices().nth(self.cursor_pos).map(|(i, _)| i).unwrap_or(self.input.len());
        self.input.remove(byte_pos);
    }

    fn move_cursor_left(&mut self) {
        self.cursor_pos = self.cursor_pos.saturating_sub(1);
    }

    fn move_cursor_right(&mut self) {
        if self.cursor_pos < self.input.chars().count() {
            self.cursor_pos += 1;
        }
    }
}

fn block_display(bt: &BlockType, depth: Option<u8>) -> (&'static str, String, Color) {
    match bt {
        BlockType::Heading => {
            let icon = "#";
            let label = format!("H{}", depth.unwrap_or(1));
            (icon, label, Color::Cyan)
        }
        BlockType::Paragraph => ("¶", "paragraph".to_string(), Color::White),
        BlockType::Code => ("{}", "code".to_string(), Color::Yellow),
        BlockType::List => ("•", "list".to_string(), Color::Green),
        BlockType::Blockquote => ("❝", "blockquote".to_string(), Color::Magenta),
        BlockType::TableCell | BlockType::TableRow | BlockType::TableAlign => {
            ("▦", "table".to_string(), Color::Blue)
        }
        BlockType::Yaml | BlockType::Toml => ("≡", "frontmatter".to_string(), Color::LightBlue),
        BlockType::Html => ("<>", "html".to_string(), Color::DarkGray),
        BlockType::HorizontalRule => ("─", "hr".to_string(), Color::DarkGray),
        BlockType::Math => ("∑", "math".to_string(), Color::LightMagenta),
        BlockType::Definition => ("§", "definition".to_string(), Color::DarkGray),
        BlockType::Footnote => ("†", "footnote".to_string(), Color::DarkGray),
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
    app.show_selected_document();

    loop {
        terminal.draw(|f| ui(f, &mut app))?;

        if event::poll(std::time::Duration::from_millis(50))?
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
    if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('c') {
        return true;
    }

    if app.input_focused {
        match key.code {
            KeyCode::Esc => app.input_focused = false,
            KeyCode::Enter => app.run_query(),
            KeyCode::Backspace => app.delete_char_before(),
            KeyCode::Left => app.move_cursor_left(),
            KeyCode::Right => app.move_cursor_right(),
            KeyCode::Home => app.cursor_pos = 0,
            KeyCode::End => app.cursor_pos = app.input.chars().count(),
            KeyCode::Tab => app.mode = app.mode.toggle(),
            KeyCode::Char(c) => app.insert_char(c),
            _ => {}
        }
    } else {
        match key.code {
            KeyCode::Char('q') => return true,
            KeyCode::Char('i') => app.input_focused = true,
            KeyCode::Tab => app.mode = app.mode.toggle(),
            KeyCode::Char('j') | KeyCode::Down => app.select_next(),
            KeyCode::Char('k') | KeyCode::Up => app.select_prev(),
            KeyCode::Char('g') => {
                app.result_scroll = 0;
            }
            KeyCode::Char('G') => {
                app.result_scroll = app.result_lines.len().saturating_sub(1) as u16;
            }
            KeyCode::PageDown | KeyCode::Char('d') => {
                app.result_scroll = app.result_scroll.saturating_add(10);
            }
            KeyCode::PageUp | KeyCode::Char('u') => {
                app.result_scroll = app.result_scroll.saturating_sub(10);
            }
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

    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);

    render_title_bar(f, app, vertical[0]);

    let main = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(28), Constraint::Percentage(72)])
        .split(vertical[1]);

    render_doc_list(f, app, main[0]);

    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0)])
        .split(main[1]);

    render_input(f, app, right[0]);
    render_results(f, app, right[1]);

    render_status_bar(f, app, vertical[2]);
}

fn render_title_bar(f: &mut Frame, app: &App, area: Rect) {
    let mode_indicator = match app.mode {
        QueryMode::Sql => "SQL",
        QueryMode::Mq => " mq",
    };
    let text = format!(
        " mqdb  {}  {}",
        mode_indicator,
        if app.input_focused {
            "Tab:switch  Enter:run  Esc:blur  Ctrl+C:quit"
        } else {
            "Tab:switch  i:input  j/k:nav  d/u:scroll  q:quit"
        }
    );
    f.render_widget(
        Paragraph::new(text).style(
            Style::default()
                .bg(Color::Rgb(43, 87, 115))
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        area,
    );
}

fn render_status_bar(f: &mut Frame, app: &App, area: Rect) {
    let msg = app.status_msg.as_deref().unwrap_or("");
    let total = format!(
        " {} doc{}  {} block{}  {}",
        app.store.len(),
        if app.store.len() == 1 { "" } else { "s" },
        app.store.documents().iter().map(|d| d.blocks.len()).sum::<usize>(),
        if app.store.documents().iter().map(|d| d.blocks.len()).sum::<usize>() == 1 { "" } else { "s" },
        msg,
    );
    f.render_widget(
        Paragraph::new(total).style(
            Style::default()
                .bg(Color::Rgb(30, 30, 30))
                .fg(Color::DarkGray),
        ),
        area,
    );
}

fn render_doc_list(f: &mut Frame, app: &mut App, area: Rect) {
    let items: Vec<ListItem> = app
        .store
        .documents()
        .iter()
        .map(|doc| {
            let filename = doc
                .path
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| format!("doc {}", doc.id));
            let title = doc.zone_maps.title.as_deref().unwrap_or("");
            let count = doc.blocks.len();

            let name_line = Line::from(vec![
                Span::styled(filename, Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
            ]);
            let meta_line = Line::from(vec![
                Span::styled(
                    format!("  {} blocks{}", count, if title.is_empty() { String::new() } else { format!("  {}", &title[..title.len().min(18)]) }),
                    Style::default().fg(Color::DarkGray),
                ),
            ]);

            ListItem::new(vec![name_line, meta_line])
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(Span::styled(" Documents ", Style::default().fg(Color::Cyan))),
        )
        .highlight_style(
            Style::default()
                .bg(Color::Rgb(50, 70, 90))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    f.render_stateful_widget(list, area, &mut app.doc_list_state);
}

fn render_input(f: &mut Frame, app: &App, area: Rect) {
    let border_style = if app.input_focused {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let title_style = if app.input_focused {
        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let title = format!(" {} ", app.mode.label());

    // Build input content with cursor indicator
    let before_cursor: String = app.input.chars().take(app.cursor_pos).collect();
    let at_cursor: String = app.input.chars().nth(app.cursor_pos).map_or(" ".to_string(), |c| c.to_string());
    let after_cursor: String = app.input.chars().skip(app.cursor_pos + 1).collect();

    let spans = if app.input_focused {
        vec![
            Span::raw(before_cursor),
            Span::styled(at_cursor, Style::default().bg(Color::Yellow).fg(Color::Black)),
            Span::raw(after_cursor),
        ]
    } else {
        vec![Span::styled(app.input.clone(), Style::default().fg(Color::DarkGray))]
    };

    let widget = Paragraph::new(Line::from(spans))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(Span::styled(title, title_style))
                .border_style(border_style),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(widget, area);
}

fn render_results(f: &mut Frame, app: &App, area: Rect) {
    let lines: Vec<Line> = app
        .result_lines
        .iter()
        .map(|rl| Line::from(Span::styled(rl.text.clone(), rl.style)))
        .collect();

    let widget = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(Span::styled(" Results ", Style::default().fg(Color::Cyan))),
        )
        .wrap(Wrap { trim: false })
        .scroll((app.result_scroll, 0));
    f.render_widget(widget, area);
}
