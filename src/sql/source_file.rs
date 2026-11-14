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
                    l.replace("\r\n", "\n").replace('\n', "\r\n")
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
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        // Owner-only from creation; the original's mode is applied before the rename.
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(&tmp)?;
        // Open the directory first so a failure here happens before the rename.
        #[cfg(unix)]
        let dir_handle = std::fs::File::open(dir)?;
        file.write_all(contents.as_bytes())?;
        file.set_permissions(permissions)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, &target)?;
        // Make the rename itself durable.
        #[cfg(unix)]
        dir_handle.sync_all()?;
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
    fn crlf_in_replacement_text_is_not_doubled() {
        let mut parsed = SourceLines::parse("a\r\nb\r\n").unwrap();
        parsed.lines[0] = "x\r\ny".to_string();
        assert_eq!(parsed.join(), "x\r\ny\r\nb\r\n");
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

    #[rstest::rstest]
    #[case::lf("a\nb\n", "x\ny")]
    #[case::crlf("a\r\nb\r\n", "x\r\ny")]
    #[case::crlf_replacing_lf("a\r\nb", "x\ny")]
    #[case::lf_file_crlf_text("a\nb", "x\r\ny")]
    fn replacement_text_never_gets_doubled_carriage_returns(
        #[case] file: &str,
        #[case] replacement: &str,
    ) {
        let mut parsed = SourceLines::parse(file).unwrap();
        parsed.lines[0] = replacement.to_string();
        let joined = parsed.join();
        assert!(!joined.contains("\r\r"), "{joined:?}");
        if file.contains("\r\n") {
            assert_eq!(joined.matches('\n').count(), joined.matches("\r\n").count());
        }
    }

    proptest::proptest! {
        /// Any text with consistent line endings survives parse + join.
        #[test]
        fn consistent_texts_round_trip(
            lines in proptest::collection::vec("[a-z ]{0,6}", 0..8),
            crlf in proptest::bool::ANY,
            trailing in proptest::bool::ANY,
        ) {
            let eol = if crlf { "\r\n" } else { "\n" };
            let mut text = lines.join(eol);
            if trailing && !lines.is_empty() {
                text.push_str(eol);
            }
            let parsed = SourceLines::parse(&text).unwrap();
            proptest::prop_assert_eq!(parsed.join(), text);
        }

        /// Replacement text, whatever its endings, never yields `\r\r`, or a bare `\n` in a CRLF file.
        #[test]
        fn replacements_keep_the_files_line_endings(
            replacement in "[a-z]{0,3}(\n|\r\n)?[a-z]{0,3}(\n|\r\n)?[a-z]{0,3}",
            crlf in proptest::bool::ANY,
        ) {
            let file = if crlf { "a\r\nb\r\n" } else { "a\nb\n" };
            let mut parsed = SourceLines::parse(file).unwrap();
            parsed.lines[0] = replacement;
            let joined = parsed.join();
            proptest::prop_assert!(!joined.contains("\r\r"));
            if crlf {
                proptest::prop_assert_eq!(joined.matches('\n').count(), joined.matches("\r\n").count());
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_never_leaves_a_group_or_world_readable_temp_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.md");
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let watcher = {
            let (stop, dir) = (stop.clone(), dir.path().to_path_buf());
            std::thread::spawn(move || {
                let mut leaked = false;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    for e in std::fs::read_dir(&dir).unwrap().flatten() {
                        if e.file_name().to_string_lossy().ends_with(".tmp")
                            && let Ok(m) = e.metadata()
                        {
                            leaked |= m.permissions().mode() & 0o077 != 0;
                        }
                    }
                }
                leaked
            })
        };
        for i in 0..200 {
            write_atomic(&path, &format!("new {i}")).unwrap();
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(
            !watcher.join().unwrap(),
            "temp file was group/world accessible"
        );
    }
}
