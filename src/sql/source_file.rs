//! Safe handling of a Markdown source file for write-back: keeping its line
//! endings, refusing to edit it when it no longer matches the index, and
//! replacing it atomically.

use crate::{Document, MqdbError, index};
use mq_markdown::Markdown;
use std::io::Write;
use std::path::Path;

fn err(msg: impl Into<String>) -> MqdbError {
    MqdbError::SqlExec(msg.into())
}

/// A source file split into lines, remembering how to put it back together.
pub(super) struct SourceLines {
    pub(super) lines: Vec<String>,
    crlf: bool,
    trailing_newline: bool,
}

impl SourceLines {
    /// Splits `text`, remembering whether it uses `\n` or `\r\n` and whether
    /// it ends with a line break. A file that mixes the two is refused:
    /// writing it back would change the endings of lines nobody edited.
    pub(super) fn parse(text: &str) -> Result<Self, MqdbError> {
        let crlf_count = text.matches("\r\n").count();
        let lf_count = text.matches('\n').count();
        if crlf_count > 0 && crlf_count != lf_count {
            return Err(err(
                "the file mixes `\\n` and `\\r\\n` line endings; make them consistent before using write-back",
            ));
        }
        Ok(Self {
            lines: text.lines().map(str::to_string).collect(),
            crlf: crlf_count > 0,
            trailing_newline: text.ends_with('\n'),
        })
    }

    /// The file's text again, with its original line endings. Replacement text
    /// may contain `\n` (a rendered code block); it follows the file's style.
    pub(super) fn join(&self) -> String {
        let eol = if self.crlf { "\r\n" } else { "\n" };
        let mut out = self
            .lines
            .iter()
            .map(|l| {
                if self.crlf {
                    l.replace('\n', "\r\n")
                } else {
                    l.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(eol);
        // A lone blank line is still a line ("\n"); only no lines at all is empty.
        if self.trailing_newline && !self.lines.is_empty() {
            out.push_str(eol);
        }
        out
    }
}

/// Errors unless `current`, the file's text right now, parses to exactly the
/// blocks the store holds for `doc`. Write-back locates text by the positions
/// recorded when the file was indexed; if the file was edited since, those
/// positions point at the wrong text and the edit would hit the wrong place.
pub(super) fn ensure_fresh(doc: &Document, current: &str) -> Result<(), MqdbError> {
    let describe = || {
        doc.path.as_ref().map_or_else(
            || format!("document {}", doc.id),
            |p| p.display().to_string(),
        )
    };
    let md = Markdown::from_markdown_str(current)
        .map_err(|e| err(format!("cannot parse {}: {e}", describe())))?;
    let mut fresh = index::build_blocks(doc.id, &md.nodes);
    if doc.blocks.iter().all(|b| b.span.is_none()) {
        for block in &mut fresh {
            block.span = None;
        }
    }
    if fresh != doc.blocks {
        return Err(err(format!(
            "{} changed since it was indexed; run `mq-db index` again before writing back",
            describe()
        )));
    }
    Ok(())
}

/// Replaces the file at `path` with `contents` so that a crash cannot leave it
/// half-written: the new text goes to a temporary file in the same
/// directory, is flushed to disk, and then renamed over the original. The
/// original's permissions are kept, and a symlink is followed so the link
/// itself survives.
pub(super) fn write_atomic(path: &Path, contents: &str) -> Result<(), MqdbError> {
    let target = std::fs::canonicalize(path)?;
    let permissions = std::fs::metadata(&target)?.permissions();
    let dir = target
        .parent()
        .ok_or_else(|| err(format!("{} has no parent directory", target.display())))?;
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(".{name}.mq-db-{}.tmp", std::process::id()));

    let result = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(contents.as_bytes())?;
        file.set_permissions(permissions)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, &target)?;
        // Make the rename itself durable.
        #[cfg(unix)]
        std::fs::File::open(dir)?.sync_all()?;
        Ok(())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(err(format!("cannot write {}: {e}", target.display())));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DocumentStore;

    #[test]
    fn lines_round_trip_exactly() {
        for text in [
            "",
            "one",
            "one\n",
            "a\n\nb\n",
            "a\n\n\n",
            "a\r\nb\r\n",
            "a\r\n\r\nb",
            "\n",
            "\r\n",
        ] {
            let parsed = SourceLines::parse(text).unwrap();
            assert_eq!(parsed.join(), text, "{text:?}");
        }
    }

    #[test]
    fn crlf_files_keep_crlf_even_for_text_with_embedded_newlines() {
        let mut parsed = SourceLines::parse("a\r\nb\r\n").unwrap();
        parsed.lines.insert(1, "```\ncode\n```".to_string());
        assert_eq!(parsed.join(), "a\r\n```\r\ncode\r\n```\r\nb\r\n");
    }

    #[test]
    fn mixed_line_endings_are_refused() {
        assert!(SourceLines::parse("a\r\nb\nc\r\n").is_err());
        assert!(SourceLines::parse("a\nb\r\n").is_err());
        assert!(SourceLines::parse("a\nb\n").is_ok());
    }

    #[test]
    fn ensure_fresh_accepts_an_unchanged_file_and_rejects_any_edit() {
        let md = "# T\n\npara\n\n- a\n- b\n";
        let mut store = DocumentStore::new();
        store.add_str(md).unwrap();
        let doc = &store.documents()[0];
        assert!(ensure_fresh(doc, md).is_ok());
        for changed in [
            "NEW\n\n# T\n\npara\n\n- a\n- b\n",
            "# T\n\npara!\n\n- a\n- b\n",
            "# T\n\npara\n\n- a\n- b\n\nmore\n",
            "# T\n\n- a\n- b\n",
        ] {
            let err = ensure_fresh(doc, changed).unwrap_err().to_string();
            assert!(err.contains("changed since it was indexed"), "{err}");
        }
    }

    #[test]
    fn write_atomic_replaces_the_file_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.md");
        std::fs::write(&path, "old").unwrap();
        write_atomic(&path, "new text").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new text");
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["a.md"]);
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_keeps_permissions_and_follows_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.md");
        std::fs::write(&real, "old").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o640)).unwrap();
        let link = dir.path().join("link.md");
        symlink(&real, &link).unwrap();

        write_atomic(&link, "new").unwrap();
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the link is still a link"
        );
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "new");
        let mode = std::fs::metadata(&real).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_failure_leaves_the_original_untouched() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.md");
        std::fs::write(&path, "keep me").unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        let attempt = write_atomic(&path, "replacement");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        if attempt.is_ok() {
            // Running as root: directory permissions do not apply.
            return;
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "keep me");
        let leftovers = std::fs::read_dir(dir.path()).unwrap().count();
        assert_eq!(leftovers, 1, "no temp file is left behind");
    }
}
