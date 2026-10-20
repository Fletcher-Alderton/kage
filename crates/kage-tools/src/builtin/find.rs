//! `find` tool: locate files by glob, honoring ignore files.

use std::fmt::Write as _;
use std::path::Path;

use globset::GlobBuilder;
use ignore::WalkBuilder;
use kage_core::{Risk, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::{Tool, ToolContext, ToolError, schema_for};

/// Input shape for the `find` tool.
#[derive(Debug, Deserialize, JsonSchema)]
struct FindInput {
    /// Glob pattern, e.g. `**/*.rs` or `src/**/*.toml`. `*` and `?`
    /// stay within one path segment; `**` crosses them.
    pattern: String,
    /// Optional subdirectory under workdir. Defaults to workdir.
    #[serde(default, alias = "filePath", deserialize_with = "super::optional_path")]
    path: Option<String>,
    /// Filter by entry kind: `"f"` for files, `"d"` for directories. None = both.
    #[serde(default, rename = "type")]
    kind: Option<String>,
}

/// Locate files by glob pattern.
#[derive(Debug)]
pub struct FindTool {
    /// Entries listed before the output is cut.
    max_entries: usize,
    description: String,
}

impl Default for FindTool {
    fn default() -> Self {
        Self::new(&kage_core::config::ListingToolConfig::default())
    }
}

impl FindTool {
    /// A `find` tool with the limit `[tools.find]` sets.
    #[must_use]
    pub fn new(cfg: &kage_core::config::ListingToolConfig) -> Self {
        Self {
            max_entries: cfg.max_entries,
            description: format!(
                "Find paths matching a glob pattern. Honors `.gitignore` \
                 and `.kageignore`; skips dotfiles. `*` and `?` stay within one \
                 path segment, `**` crosses them. Optional `type` filters by `f` \
                 (files) or `d` (directories). Output is capped at {} entries.",
                cfg.max_entries
            ),
        }
    }
}

impl Tool for FindTool {
    fn name(&self) -> &'static str {
        "find"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> serde_json::Value {
        schema_for::<FindInput>()
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn execute(
        &self,
        input: serde_json::Value,
        cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let input: FindInput = serde_json::from_value(input)?;
        let root = match &input.path {
            Some(p) => cx.resolve_path(Path::new(p))?,
            None => cx.workdir().to_path_buf(),
        };
        std::fs::metadata(&root).map_err(ToolError::io_at("search", &root))?;
        let glob = GlobBuilder::new(&input.pattern)
            .literal_separator(true)
            .build()
            .map_err(|e| ToolError::InvalidInput(format!("invalid glob: {e}")))?
            .compile_matcher();

        let want_files = match input.kind.as_deref() {
            None => None,
            Some("f" | "file" | "files") => Some(true),
            Some("d" | "dir" | "directory" | "directories") => Some(false),
            Some(other) => {
                return Err(ToolError::InvalidInput(format!(
                    "invalid type `{other}`; expected `f` (files) or `d` (directories)"
                )));
            }
        };
        let want_dirs = want_files.map(|files| !files);

        let walker = WalkBuilder::new(&root)
            .add_custom_ignore_filename(".kageignore")
            .build();

        let mut matches: Vec<String> = Vec::new();
        let mut truncated = false;
        for entry in walker {
            if cx.is_cancelled() {
                return Err(ToolError::Cancelled);
            }
            let Ok(entry) = entry else { continue };
            let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
            let is_file = entry.file_type().is_some_and(|t| t.is_file());

            if let Some(true) = want_files
                && !is_file
            {
                continue;
            }
            if let Some(true) = want_dirs
                && !is_dir
            {
                continue;
            }
            let rel = entry.path().strip_prefix(&root).unwrap_or(entry.path());
            if rel.as_os_str().is_empty() {
                continue;
            }
            if glob.is_match(rel) {
                matches.push(kage_core::fsutil::slashed(rel));
                if matches.len() > self.max_entries {
                    truncated = true;
                    break;
                }
            }
        }
        if truncated {
            matches.truncate(self.max_entries);
        }
        matches.sort();

        let text = if matches.is_empty() {
            "(no matches)".to_owned()
        } else {
            let mut text = matches.join("\n");
            if truncated {
                let _ = write!(
                    text,
                    "\n[... truncated at {} entries ...]",
                    self.max_entries
                );
            }
            text
        };
        let count = matches.len();
        Ok(ToolOutput {
            is_error: false,
            text,
            structured: Some(serde_json::json!({
                "pattern": input.pattern,
                "matches": count,
                "truncated": truncated,
            })),
            terminate: false,
            media: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    /// The default `[tools.find] max_entries`.
    const MAX_ENTRIES: usize = 5_000;
    use std::fs;

    use kage_core::CancelFlag;

    use super::*;

    fn run(workdir: &Path, input: serde_json::Value) -> Result<ToolOutput, ToolError> {
        let cancel = CancelFlag::new();
        let cx = ToolContext::new(workdir, &cancel);
        FindTool::default().execute(input, &cx)
    }

    fn populate(dir: &Path) {
        fs::create_dir(dir.join("src")).unwrap();
        fs::write(dir.join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(dir.join("src/lib.rs"), "// lib").unwrap();
        fs::write(dir.join("Cargo.toml"), "[package]").unwrap();
        fs::write(dir.join("README.md"), "# kage").unwrap();
    }

    #[test]
    fn finds_rust_files() {
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        let out = run(dir.path(), serde_json::json!({"pattern":"**/*.rs"})).unwrap();
        assert!(out.text.contains("src/main.rs"));
        assert!(out.text.contains("src/lib.rs"));
        assert!(!out.text.contains("Cargo.toml"));
    }

    #[test]
    fn single_star_stays_within_one_path_segment() {
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        fs::create_dir_all(dir.path().join("src/nested")).unwrap();
        fs::write(dir.path().join("src/nested/deep.rs"), "// deep").unwrap();
        let out = run(dir.path(), serde_json::json!({"pattern":"src/*.rs"})).unwrap();
        assert!(out.text.contains("src/main.rs"), "{}", out.text);
        assert!(!out.text.contains("src/nested/deep.rs"), "{}", out.text);
    }

    #[test]
    fn double_star_still_crosses_path_segments() {
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        fs::create_dir_all(dir.path().join("src/nested")).unwrap();
        fs::write(dir.path().join("src/nested/deep.rs"), "// deep").unwrap();
        let out = run(dir.path(), serde_json::json!({"pattern":"**/*.rs"})).unwrap();
        assert!(out.text.contains("src/nested/deep.rs"), "{}", out.text);
    }

    #[test]
    fn question_mark_stays_within_one_path_segment() {
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        fs::create_dir_all(dir.path().join("src/nested")).unwrap();
        fs::write(dir.path().join("src/nested/deep.rs"), "// deep").unwrap();
        let out = run(dir.path(), serde_json::json!({"pattern":"src/mai?.rs"})).unwrap();
        assert!(out.text.contains("src/main.rs"), "{}", out.text);
        let out = run(dir.path(), serde_json::json!({"pattern":"src?nested/*.rs"})).unwrap();
        assert_eq!(out.text, "(no matches)");
    }

    #[test]
    fn type_dir_filters_to_directories() {
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        let out = run(dir.path(), serde_json::json!({"pattern":"*","type":"d"})).unwrap();
        assert_eq!(out.text, "src");
    }

    #[test]
    fn type_file_filters_to_files() {
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        let out = run(dir.path(), serde_json::json!({"pattern":"*","type":"f"})).unwrap();
        assert!(out.text.contains("Cargo.toml"), "{}", out.text);
        assert!(!out.text.lines().any(|line| line == "src"), "{}", out.text);
    }

    #[test]
    fn an_unknown_type_value_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        let err = run(dir.path(), serde_json::json!({"pattern":"*","type":"x"})).unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)), "{err:?}");
        assert!(err.to_string().contains("`x`"), "{err}");
        assert!(
            err.to_string().contains("`f`") && err.to_string().contains("`d`"),
            "{err}"
        );
    }

    #[test]
    fn invalid_glob_is_invalid_input() {
        let dir = tempfile::tempdir().unwrap();
        let err = run(dir.path(), serde_json::json!({"pattern":"["})).unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[test]
    fn missing_path_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let err = run(dir.path(), serde_json::json!({"pattern":"*","path":"nope"}))
            .unwrap_err()
            .to_string();
        let missing = dir.path().canonicalize().unwrap().join("nope");
        let prefix = format!("cannot search {}: ", missing.display());
        assert!(err.starts_with(&prefix), "{err}");
    }

    #[test]
    fn no_matches_returns_marker() {
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        let out = run(dir.path(), serde_json::json!({"pattern":"**/*.zzz"})).unwrap();
        assert_eq!(out.text, "(no matches)");
    }

    #[test]
    fn output_caps_at_max_entries() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..(MAX_ENTRIES + 10) {
            fs::write(dir.path().join(format!("f{i}.txt")), "x").unwrap();
        }
        let out = run(dir.path(), serde_json::json!({"pattern":"*.txt"})).unwrap();
        assert!(out.text.contains("[... truncated at 5000 entries ...]"));
        let structured = out.structured.unwrap();
        assert_eq!(structured["truncated"], true);
        assert_eq!(structured["matches"], MAX_ENTRIES);
    }

    #[test]
    fn structured_keeps_counts_but_not_the_path_list() {
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        let out = run(dir.path(), serde_json::json!({"pattern":"**/*.rs"})).unwrap();
        let structured = out.structured.unwrap();
        assert!(structured.get("paths").is_none(), "{structured}");
        assert_eq!(structured["matches"], 2);
        assert_eq!(structured["truncated"], false);
    }

    #[test]
    fn filepath_is_accepted_as_an_alias_for_path() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub").join("a.txt"), "x").unwrap();
        let out = run(
            dir.path(),
            serde_json::json!({"filePath":"sub","pattern":"**/*.txt"}),
        )
        .unwrap();
        assert!(out.text.contains("a.txt"), "got {}", out.text);
        // The canonical key still works: the alias is additive.
        let out = run(
            dir.path(),
            serde_json::json!({"path":"sub","pattern":"**/*.txt"}),
        )
        .unwrap();
        assert!(out.text.contains("a.txt"), "got {}", out.text);
    }
}
