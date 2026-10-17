//! Query output and its text/JSON/CSV/HTML renderers.

fn json_value_str(s: &str) -> String {
    if let Ok(n) = s.parse::<i64>() {
        return n.to_string();
    }
    if let Ok(f) = s.parse::<f64>() {
        return f.to_string();
    }
    if s == "true" || s == "false" || s == "null" || s == "NULL" {
        return s.to_lowercase();
    }
    // Treat as JSON string: escape quotes and backslashes
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn csv_cell(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn csv_row(fields: &[String]) -> String {
    let mut row = fields
        .iter()
        .map(|f| csv_cell(f))
        .collect::<Vec<_>>()
        .join(",");
    row.push('\n');
    row
}

pub fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The tabular output of a SQL query.
#[derive(Debug)]
pub struct QueryOutput {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

impl QueryOutput {
    /// Render as a JSON array of objects, one object per row.
    pub fn to_json(&self) -> String {
        if self.rows.is_empty() {
            return "[]\n".to_string();
        }
        let objects: Vec<String> = self
            .rows
            .iter()
            .map(|row| {
                let pairs: Vec<String> = self
                    .columns
                    .iter()
                    .zip(row.iter())
                    .map(|(col, val)| {
                        format!(
                            "\"{}\":{}",
                            col.replace('\\', "\\\\").replace('"', "\\\""),
                            json_value_str(val)
                        )
                    })
                    .collect();
                format!("{{{}}}", pairs.join(","))
            })
            .collect();
        format!("[{}]\n", objects.join(","))
    }

    /// Render as RFC 4180 CSV with a header row.
    pub fn to_csv(&self) -> String {
        let mut out = String::new();
        if !self.columns.is_empty() {
            out.push_str(&csv_row(&self.columns));
        }
        for row in &self.rows {
            out.push_str(&csv_row(row));
        }
        out
    }

    /// Render as tab-separated values with a header row.
    pub fn to_tsv(&self) -> String {
        let mut out = String::new();
        if !self.columns.is_empty() {
            out.push_str(&self.columns.join("\t"));
            out.push('\n');
        }
        for row in &self.rows {
            out.push_str(&row.join("\t"));
            out.push('\n');
        }
        out
    }

    /// Render as a GFM Markdown table.
    pub fn to_markdown_table(&self) -> String {
        if self.columns.is_empty() {
            return String::new();
        }
        let mut widths: Vec<usize> = self.columns.iter().map(|h| h.len().max(3)).collect();
        for row in &self.rows {
            for (i, cell) in row.iter().enumerate() {
                if i < widths.len() {
                    widths[i] = widths[i].max(cell.len());
                }
            }
        }

        let mut out = String::new();
        out.push('|');
        for (i, h) in self.columns.iter().enumerate() {
            out.push_str(&format!(" {:<w$} |", h, w = widths[i]));
        }
        out.push('\n');

        out.push('|');
        for &w in &widths {
            out.push_str(&format!(" {} |", "-".repeat(w)));
        }
        out.push('\n');

        for row in &self.rows {
            out.push('|');
            for (i, &w) in widths.iter().enumerate() {
                let cell = row.get(i).map(String::as_str).unwrap_or("");
                let escaped = cell
                    .replace('|', "\\|")
                    .replace('\n', " ")
                    .replace('\r', "");
                out.push_str(&format!(" {:<w$} |", escaped, w = w));
            }
            out.push('\n');
        }
        out
    }

    /// Render as an HTML `<table>`.
    pub fn to_html_table(&self) -> String {
        let mut out = String::from("<table>\n");
        if !self.columns.is_empty() {
            out.push_str("<thead><tr>");
            for h in &self.columns {
                out.push_str(&format!("<th>{}</th>", html_escape(h)));
            }
            out.push_str("</tr></thead>\n");
        }
        out.push_str("<tbody>\n");
        for row in &self.rows {
            out.push_str("<tr>");
            for (i, _) in self.columns.iter().enumerate() {
                let cell = row.get(i).map(String::as_str).unwrap_or("");
                out.push_str(&format!("<td>{}</td>", html_escape(cell)));
            }
            out.push_str("</tr>\n");
        }
        out.push_str("</tbody>\n</table>\n");
        out
    }

    /// Render as a Unicode box-drawing table. Cells > 60 chars are truncated.
    pub fn to_table(&self) -> String {
        const MAX_CELL: usize = 60;

        if self.columns.is_empty() {
            return "(no columns)\n".to_string();
        }
        if self.rows.is_empty() {
            return "(0 rows)\n".to_string();
        }

        let mut widths: Vec<usize> = self.columns.iter().map(|h| h.len()).collect();
        for row in &self.rows {
            for (i, cell) in row.iter().enumerate() {
                if i < widths.len() {
                    let display_len = cell.replace('\r', "").replace('\n', " ").chars().count();
                    widths[i] = widths[i].max(display_len.min(MAX_CELL));
                }
            }
        }

        let col_count = self.columns.len();
        let mut out = String::new();

        out.push('┌');
        for (i, &w) in widths.iter().enumerate() {
            out.push_str(&"─".repeat(w + 2));
            out.push(if i + 1 < col_count { '┬' } else { '┐' });
        }
        out.push('\n');

        out.push('│');
        for (i, h) in self.columns.iter().enumerate() {
            out.push_str(&format!(" {:<width$} │", h, width = widths[i]));
        }
        out.push('\n');

        out.push('├');
        for (i, &w) in widths.iter().enumerate() {
            out.push_str(&"─".repeat(w + 2));
            out.push(if i + 1 < col_count { '┼' } else { '┤' });
        }
        out.push('\n');

        for row in &self.rows {
            out.push('│');
            for (i, &w) in widths.iter().enumerate() {
                let cell = row.get(i).map(String::as_str).unwrap_or("");
                let cell = cell.replace('\r', "").replace('\n', " ");
                let truncated: String = if cell.chars().count() > MAX_CELL {
                    let mut s: String = cell.chars().take(MAX_CELL - 1).collect();
                    s.push('…');
                    s
                } else {
                    cell
                };
                out.push_str(&format!(" {:<width$} │", truncated, width = w));
            }
            out.push('\n');
        }

        out.push('└');
        for (i, &w) in widths.iter().enumerate() {
            out.push_str(&"─".repeat(w + 2));
            out.push(if i + 1 < col_count { '┴' } else { '┘' });
        }
        out.push('\n');
        out.push_str(&format!(
            "({} row{})\n",
            self.rows.len(),
            if self.rows.len() == 1 { "" } else { "s" }
        ));
        out
    }
}
