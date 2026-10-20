//! `ls` tool: list directory contents, optionally recursive.

use std::fmt::Write as _;
use std::path::Path;

use ignore::WalkBuilder;
use kage_core::{Risk, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::{Tool, ToolContext, ToolError, schema_for};

/// Input shape for the `ls` tool.
#[derive(Debug, Deserialize, JsonSchema)]
struct LsInput {
    /// Optional subdirectory under workdir. Defaults to workdir.
    #[serde(default, alias = "filePath", deserialize_with = "super::optional_path")]
    path: Option<String>,
    /// Recurse into subdirectories. Both modes honor `.gitignore` and
    /// `.kageignore` and skip dotfiles.
    #[serde(default)]
    recursive: bool,
}

/// List files and directories.
#[derive(Debug)]
pub struct LsTool {
    /// Entries listed before the output is cut.
    max_entries: usize,
    description: String,
}

impl Default for LsTool {
    fn default() -> Self {
        Self::new(&kage_core::config::ListingToolConfig::default())
    }
}

impl LsTool {
    /// A `ls` tool with the limit `[tools.ls]` sets.
    #[must_use]
    pub fn new(cfg: &kage_core::config::ListingToolConfig) -> Self {
        Self {
            max_entries: cfg.max_entries,
            description: format!(
                "List directory contents. With `recursive: true`, walks subdirectories. \
                 Both modes honor `.gitignore` and `.kageignore` and skip dotfiles. \
                 Entries are prefixed with `f` (file), `d` (directory), or `l` (symlink). \
                 Output is capped at {} entries.",
                cfg.max_entries
            ),
        }
    }
}

impl Tool for LsTool {
    fn name(&self) -> &'static str {
        "ls"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> serde_json::Value {
        schema_for::<LsInput>()
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn execute(
        &self,
        input: serde_json::Value,
        cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let input: LsInput = serde_json::from_value(input)?;
        let target = match &input.path {
            Some(p) => cx.resolve_path(Path::new(p))?,
            None => cx.workdir().to_path_buf(),
        };
        std::fs::metadata(&target).map_err(ToolError::io_at("list", &target))?;

        let mut entries: Vec<String> = Vec::new();
        let mut truncated = false;
        let mut walker = WalkBuilder::new(&target);
        walker.add_custom_ignore_filename(".kageignore");
        if !input.recursive {
            walker.max_depth(Some(1));
        }
        for entry in walker.build() {
            if cx.is_cancelled() {
                return Err(ToolError::Cancelled);
            }
            let Ok(entry) = entry else { continue };
            if entry.depth() == 0 {
                continue;
            }
            let rel = entry.path().strip_prefix(&target).unwrap_or(entry.path());
            let prefix = entry_prefix(entry.file_type());
            entries.push(format!("{prefix} {}", kage_core::fsutil::slashed(rel)));
            if entries.len() > self.max_entries {
                truncated = true;
                break;
            }
        }
        if truncated {
            entries.truncate(self.max_entries);
        }
        entries.sort();

        let text = if entries.is_empty() {
            "(empty)".to_owned()
        } else {
            let mut text = entries.join("\n");
            if truncated {
                let _ = write!(
                    text,
                    "\n[... truncated at {} entries ...]",
                    self.max_entries
                );
            }
            text
        };
        let count = entries.len();
        Ok(ToolOutput {
            is_error: false,
            text,
            structured: Some(serde_json::json!({
                "count": count,
                "truncated": truncated,
            })),
            terminate: false,
            media: Vec::new(),
        })
    }
}

fn entry_prefix(file_type: Option<std::fs::FileType>) -> char {
    match file_type {
        Some(t) if t.is_dir() => 'd',
        Some(t) if t.is_symlink() => 'l',
        Some(_) => 'f',
        None => '?',
    }
}

#[cfg(test)]
mod tests {
    /// The default `[tools.ls] max_entries`.
    const MAX_ENTRIES: usize = 5_000;
    use std::fs;

    use kage_core::CancelFlag;

    use super::*;

    fn run(workdir: &Path, input: serde_json::Value) -> Result<ToolOutput, ToolError> {
        let cancel = CancelFlag::new();
        let cx = ToolContext::new(workdir, &cancel);
        LsTool::default().execute(input, &cx)
    }

    #[test]
    fn lists_workdir_root_by_default() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "x").unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        let out = run(dir.path(), serde_json::json!({})).unwrap();
        assert!(out.text.contains("f a.txt"));
        assert!(out.text.contains("d sub"));
    }

    #[test]
    fn missing_path_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let missing = root.join("nope");
        for recursive in [false, true] {
            let input = serde_json::json!({"path":"nope","recursive":recursive});
            let err = run(dir.path(), input).unwrap_err().to_string();
            let prefix = format!("cannot list {}: ", missing.display());
            assert!(err.starts_with(&prefix), "{err}");
        }

        let gone = root.join("gone");
        let err = run(&gone, serde_json::json!({"path":null})).unwrap_err();
        let prefix = format!("cannot list {}: ", gone.display());
        assert!(err.to_string().starts_with(&prefix), "{err}");
    }

    #[test]
    fn recursive_walks_subdirs() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/inner.txt"), "x").unwrap();
        let out = run(dir.path(), serde_json::json!({"recursive":true})).unwrap();
        assert!(out.text.contains("sub"));
        assert!(out.text.contains("sub/inner.txt"));
    }

    #[test]
    fn empty_dir_returns_marker() {
        let dir = tempfile::tempdir().unwrap();
        let out = run(dir.path(), serde_json::json!({})).unwrap();
        assert_eq!(out.text, "(empty)");
    }

    #[test]
    fn lists_named_subdir() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/x.txt"), "x").unwrap();
        let out = run(dir.path(), serde_json::json!({"path":"sub"})).unwrap();
        assert!(out.text.contains("f x.txt"));
    }

    #[test]
    fn recursive_honors_kageignore() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "x").unwrap();
        fs::write(dir.path().join("b.txt"), "x").unwrap();
        fs::write(dir.path().join(".kageignore"), "b.txt\n").unwrap();
        let out = run(dir.path(), serde_json::json!({"recursive":true})).unwrap();
        assert!(out.text.contains("a.txt"));
        assert!(!out.text.contains("b.txt"));
    }

    #[test]
    fn non_recursive_honors_kageignore() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "x").unwrap();
        fs::write(dir.path().join("b.txt"), "x").unwrap();
        fs::write(dir.path().join(".kageignore"), "b.txt\n").unwrap();
        let out = run(dir.path(), serde_json::json!({"recursive":false})).unwrap();
        assert!(out.text.contains("a.txt"), "{}", out.text);
        assert!(!out.text.contains("b.txt"), "{}", out.text);
    }

    #[test]
    fn non_recursive_lists_each_direct_child_once() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "x").unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/inner.txt"), "x").unwrap();
        let out = run(dir.path(), serde_json::json!({"recursive":false})).unwrap();
        assert!(out.text.contains("f a.txt"), "{}", out.text);
        assert!(out.text.contains("d sub"), "{}", out.text);
        assert!(!out.text.contains("inner.txt"), "{}", out.text);
        assert_eq!(out.structured.unwrap()["count"], 2);
    }

    #[test]
    fn a_directory_empty_after_filtering_is_the_empty_marker() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("gone.txt"), "x").unwrap();
        fs::write(dir.path().join(".kageignore"), "gone.txt\n").unwrap();
        let out = run(dir.path(), serde_json::json!({"recursive":false})).unwrap();
        assert_eq!(out.text, "(empty)");
    }

    #[test]
    fn both_modes_skip_dotfiles() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".env"), "SECRET=1").unwrap();
        fs::write(dir.path().join("a.txt"), "x").unwrap();
        for recursive in [false, true] {
            let out = run(dir.path(), serde_json::json!({"recursive":recursive})).unwrap();
            assert!(out.text.contains("a.txt"), "{}", out.text);
            assert!(!out.text.contains(".env"), "{}", out.text);
        }
    }

    #[test]
    fn output_caps_at_max_entries() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..(MAX_ENTRIES + 10) {
            fs::write(dir.path().join(format!("f{i}.txt")), "x").unwrap();
        }
        let out = run(dir.path(), serde_json::json!({})).unwrap();
        assert!(out.text.contains("[... truncated at 5000 entries ...]"));
        let structured = out.structured.unwrap();
        assert_eq!(structured["truncated"], true);
        assert_eq!(structured["count"], MAX_ENTRIES);
    }

    #[test]
    fn structured_keeps_counts_but_not_the_entry_list() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "x").unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        let out = run(dir.path(), serde_json::json!({})).unwrap();
        let structured = out.structured.unwrap();
        assert!(structured.get("entries").is_none(), "{structured}");
        assert_eq!(structured["count"], 2);
        assert_eq!(structured["truncated"], false);
    }

    #[test]
    fn a_null_string_path_lists_the_workdir() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("marker.txt"), "x").unwrap();
        let out = run(dir.path(), serde_json::json!({"path":"null"})).unwrap();
        assert!(out.text.contains("marker.txt"), "{}", out.text);
    }

    #[test]
    fn filepath_is_accepted_as_an_alias_for_path() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub").join("a.txt"), "x").unwrap();
        let out = run(dir.path(), serde_json::json!({"filePath":"sub"})).unwrap();
        assert!(out.text.contains("a.txt"), "got {}", out.text);
        // The canonical key still works: the alias is additive.
        let out = run(dir.path(), serde_json::json!({"path":"sub"})).unwrap();
        assert!(out.text.contains("a.txt"), "got {}", out.text);
    }
}
