//! Text-level edits that write-back applies to Markdown source: locating
//! list-item text, replacing one table cell, and removing lines without
//! leaving the neighbours merged or double-spaced.

use crate::MqdbError;
use regex::Regex;
use std::sync::OnceLock;

fn err(msg: impl Into<String>) -> MqdbError {
    MqdbError::SqlExec(msg.into())
}

/// Byte offset in `line` of the 1-based column `col`, which counts UTF-16
/// code units (what the parser reports: one per BMP character, two for an
/// emoji). `None` if `col` is past the end of the line or splits a
/// surrogate pair.
pub(super) fn byte_offset(line: &str, col: usize) -> Option<usize> {
    let mut units = 1usize;
    for (i, ch) in line.char_indices() {
        if units == col {
            return Some(i);
        }
        units += ch.len_utf16();
        if units > col {
            return None;
        }
    }
    (units == col).then_some(line.len())
}

fn list_prefix_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^[ \t]*(?:[-*+]|\d{1,9}[.)])[ \t]+(?:\[[ xX]\][ \t]+)?$").unwrap()
    })
}

/// Whether `prefix` is exactly an indent, a list marker and an optional task
/// checkbox, i.e. everything before a list item's text.
pub(super) fn is_list_prefix(prefix: &str) -> bool {
    list_prefix_regex().is_match(prefix)
}

/// The prefix a new sibling of the item with `prefix` should start with:
/// the same indent and marker, without a task checkbox. `None` if `prefix`
/// is not a list-item prefix.
pub(super) fn sibling_prefix(prefix: &str) -> Option<String> {
    if !is_list_prefix(prefix) {
        return None;
    }
    static CHECKBOX: OnceLock<Regex> = OnceLock::new();
    let checkbox = CHECKBOX.get_or_init(|| Regex::new(r"\[[ xX]\][ \t]+$").unwrap());
    Some(checkbox.replace(prefix, "").into_owned())
}

/// Escapes unescaped `|` so `text` stays inside one table cell. Cells cannot
/// hold a line break.
pub(super) fn escape_cell(text: &str) -> Result<String, MqdbError> {
    if text.contains(['\n', '\r']) {
        return Err(err("a table cell cannot contain a line break"));
    }
    let mut out = String::with_capacity(text.len());
    let mut escaped = false;
    for ch in text.chars() {
        if escaped {
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '|' {
            out.push('\\');
        }
        out.push(ch);
    }
    Ok(out)
}

/// Splits a table row on unescaped pipes, like GFM (a pipe inside a code
/// span still separates cells unless it is escaped).
fn split_row(line: &str) -> Vec<String> {
    let mut segments = vec![String::new()];
    let mut escaped = false;
    for ch in line.chars() {
        let cur = segments.last_mut().unwrap();
        if escaped {
            cur.push(ch);
            escaped = false;
        } else if ch == '\\' {
            cur.push(ch);
            escaped = true;
        } else if ch == '|' {
            segments.push(String::new());
        } else {
            cur.push(ch);
        }
    }
    segments
}

/// Replaces cell number `column` (0-based) of the table row `line` with
/// `text`, leaving every other character of the row as it was.
pub(super) fn set_table_cell(line: &str, column: usize, text: &str) -> Result<String, MqdbError> {
    let escaped = escape_cell(text)?;
    let mut segments = split_row(line);
    if segments.len() < 2 {
        return Err(err("expected a table row containing `|`"));
    }
    // A leading/trailing pipe leaves an empty (blank) segment at that end.
    let first = usize::from(segments[0].trim().is_empty());
    let last = segments.len() - usize::from(segments.last().is_some_and(|s| s.trim().is_empty()));
    let cells = first..last.max(first);
    let index = first + column;
    if !cells.contains(&index) {
        return Err(err(format!(
            "cannot find cell {column} in the table row `{}`",
            line.trim()
        )));
    }
    segments[index] = format!(" {escaped} ");
    Ok(segments.join("|"))
}

/// Removes lines `start..end` (0-based, end exclusive) and tidies the seam:
/// two blank lines are never left side by side, and a blank line is not left
/// at the start or end of the file. Neighbouring non-blank lines are never
/// joined or separated, so a list or paragraph that followed keeps its place.
pub(super) fn remove_lines(lines: &mut Vec<String>, start: usize, end: usize) {
    lines.splice(start..end, std::iter::empty());
    let blank = |l: &String| l.trim().is_empty();
    if start < lines.len() && blank(&lines[start]) && (start == 0 || blank(&lines[start - 1])) {
        lines.remove(start);
    }
    if start >= lines.len() {
        while lines.last().is_some_and(blank) {
            lines.pop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_offset_counts_utf16_units() {
        assert_eq!(byte_offset("- abc", 3), Some(2));
        assert_eq!(byte_offset("- abc", 6), Some(5), "one past the end");
        assert_eq!(byte_offset("- abc", 7), None);
        // Japanese is one unit per character (three bytes each).
        assert_eq!(byte_offset("- 日本語", 4), Some(5));
        // An emoji takes two units and four bytes; its middle is not a boundary.
        assert_eq!(byte_offset("- a😀b", 4), Some(3));
        assert_eq!(byte_offset("- a😀b", 5), None);
        assert_eq!(byte_offset("- a😀b", 6), Some(7));
    }

    #[test]
    fn list_prefixes() {
        for ok in [
            "- ",
            "  * ",
            "+ ",
            "1. ",
            "12) ",
            "- [ ] ",
            "2. [x] ",
            "\t- [X]  ",
        ] {
            assert!(is_list_prefix(ok), "{ok:?}");
        }
        for bad in ["", "-", "-x ", "a. ", "- [y] ", "text - ", "- text "] {
            assert!(!is_list_prefix(bad), "{bad:?}");
        }
        assert_eq!(sibling_prefix("  - [x] ").as_deref(), Some("  - "));
        assert_eq!(sibling_prefix("2. ").as_deref(), Some("2. "));
        assert_eq!(sibling_prefix("nope"), None);
    }

    #[test]
    fn escape_cell_escapes_only_bare_pipes() {
        assert_eq!(escape_cell("a|b").unwrap(), "a\\|b");
        assert_eq!(escape_cell("a\\|b").unwrap(), "a\\|b", "already escaped");
        assert_eq!(escape_cell("plain").unwrap(), "plain");
        assert!(escape_cell("two\nlines").is_err());
    }

    #[test]
    fn set_table_cell_replaces_one_cell_and_keeps_the_rest() {
        let row = "| a | b |";
        assert_eq!(set_table_cell(row, 0, "x").unwrap(), "| x | b |");
        assert_eq!(set_table_cell(row, 1, "x").unwrap(), "| a | x |");
        assert!(set_table_cell(row, 2, "x").is_err());
        // Spacing of the untouched cells survives.
        assert_eq!(
            set_table_cell("|  a   |   b|", 0, "new").unwrap(),
            "| new |   b|"
        );
        // No leading or no trailing pipe.
        assert_eq!(set_table_cell("a | b", 1, "x").unwrap(), "a | x ");
        assert_eq!(set_table_cell("| a | b", 1, "x").unwrap(), "| a | x ");
        assert_eq!(set_table_cell("a | b |", 0, "x").unwrap(), " x | b |");
        // Indented rows keep their indent.
        assert_eq!(
            set_table_cell("  | a | b |", 1, "x").unwrap(),
            "  | a | x |"
        );
        // An escaped pipe is not a separator, and new pipes are escaped.
        assert_eq!(
            set_table_cell("| 1 \\| x | y |", 1, "p|q").unwrap(),
            "| 1 \\| x | p\\|q |"
        );
        assert_eq!(
            set_table_cell("| 1 \\| x | y |", 0, "z").unwrap(),
            "| z | y |"
        );
        assert!(set_table_cell("no pipes here", 0, "x").is_err());
        assert!(set_table_cell("| a |", 0, "two\nlines").is_err());
    }

    fn lines(s: &str) -> Vec<String> {
        s.lines().map(str::to_string).collect()
    }

    #[test]
    fn remove_lines_tidies_blank_lines_without_joining_neighbours() {
        // Interior block: the doubled blank line collapses.
        let mut l = lines("A\n\nB\n\nC");
        remove_lines(&mut l, 2, 3);
        assert_eq!(l, lines("A\n\nC"));
        // Last block: the trailing blank goes.
        let mut l = lines("# T\n\nKeep\n\nDrop");
        remove_lines(&mut l, 4, 5);
        assert_eq!(l, lines("# T\n\nKeep"));
        // First block: no leading blank is left.
        let mut l = lines("# H\n\npara");
        remove_lines(&mut l, 0, 1);
        assert_eq!(l, lines("para"));
        // A block directly followed by text: the blank before it stays, so the
        // previous paragraph is not glued to the text.
        let mut l = lines("para\n\n# H\ntext");
        remove_lines(&mut l, 2, 3);
        assert_eq!(l, lines("para\n\ntext"));
        // A list item between two others leaves no gap.
        let mut l = lines("- a\n- b\n- c");
        remove_lines(&mut l, 1, 2);
        assert_eq!(l, lines("- a\n- c"));
        // The first item: the blank line before the list is kept.
        let mut l = lines("para\n\n- a\n- b");
        remove_lines(&mut l, 2, 3);
        assert_eq!(l, lines("para\n\n- b"));
        // The only item removes the list and one of its separating blanks.
        let mut l = lines("para\n\n- a\n\nnext");
        remove_lines(&mut l, 2, 3);
        assert_eq!(l, lines("para\n\nnext"));
        // Removing everything is fine.
        let mut l = lines("only");
        remove_lines(&mut l, 0, 1);
        assert!(l.is_empty());
    }
}
