//! `grep` tool: recursive regex search over files honoring `.gitignore`
//! and `.kageignore`.
//!
//! Uses the `grep` and `ignore` crates from the ripgrep family. Does not
//! shell out to `rg`.

use std::path::Path;

use grep::regex::RegexMatcherBuilder;
use grep::searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkMatch};
use ignore::WalkBuilder;
use kage_core::{Risk, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::{Tool, ToolContext, ToolError, schema_for};

/// Characters of context kept before the match in a cut line.
const LEAD_CHARS: usize = 100;

/// Input shape for the `grep` tool.
#[derive(Debug, Deserialize, JsonSchema)]
struct GrepInput {
    /// Regex pattern (Rust regex syntax).
    pattern: String,
    /// Optional subdirectory under workdir to search. Defaults to workdir.
    #[serde(default, alias = "filePath", deserialize_with = "super::optional_path")]
    path: Option<String>,
    /// Match case-insensitively.
    #[serde(default)]
    ignore_case: bool,
    /// Cap on total matches across all files. Defaults to 1000.
    #[serde(default)]
    max_matches: Option<u64>,
}

/// Recursive regex search.
#[derive(Debug)]
pub struct GrepTool {
    /// Matches returned when the call sets no `max_matches`.
    max_matches: u64,
    /// Characters kept of one matching line.
    max_line_chars: usize,
    description: String,
}

impl Default for GrepTool {
    fn default() -> Self {
        Self::new(&kage_core::config::GrepToolConfig::default())
    }
}

impl GrepTool {
    /// A `grep` tool with the limits `[tools.grep]` sets.
    #[must_use]
    pub fn new(cfg: &kage_core::config::GrepToolConfig) -> Self {
        Self {
            max_matches: cfg.max_matches,
            max_line_chars: cfg.max_line_chars,
            description: format!(
                "Recursively search files for a regex pattern. Honors \
                 `.gitignore` and `.kageignore`; skips dotfiles and binary \
                 files (a file stops at its first NUL byte). Returns lines as \
                 `path:line:text`. Capped at {} matches by default.",
                cfg.max_matches
            ),
        }
    }
}

impl Tool for GrepTool {
    fn name(&self) -> &'static str {
        "grep"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> serde_json::Value {
        let mut schema = schema_for::<GrepInput>();
        crate::schema::set_property_doc(
            &mut schema,
            "max_matches",
            &format!(
                "Cap on total matches across all files. Defaults to {}.",
                self.max_matches
            ),
        );
        schema
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn execute(
        &self,
        input: serde_json::Value,
        cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let input: GrepInput = serde_json::from_value(input)?;
        let root = match &input.path {
            Some(p) => cx.resolve_path(Path::new(p))?,
            None => cx.workdir().to_path_buf(),
        };
        std::fs::metadata(&root).map_err(ToolError::io_at("search", &root))?;
        let max = input.max_matches.unwrap_or(self.max_matches).max(1);

        let matcher = RegexMatcherBuilder::new()
            .case_insensitive(input.ignore_case)
            .build(&input.pattern)
            .map_err(|e| ToolError::InvalidInput(format!("invalid regex: {e}")))?;

        let mut hits: Vec<Hit> = Vec::new();
        let mut total = 0u64;
        let mut hit_cap = false;

        let walker = WalkBuilder::new(&root)
            .add_custom_ignore_filename(".kageignore")
            .hidden(true)
            .build();

        'walk: for entry in walker {
            if cx.is_cancelled() {
                return Err(ToolError::Cancelled);
            }
            let Ok(entry) = entry else {
                continue;
            };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let path = entry.path().to_path_buf();
            let mut searcher = SearcherBuilder::new()
                .binary_detection(BinaryDetection::quit(b'\x00'))
                .build();
            let mut sink = MatchSink {
                matcher: &matcher,
                hits: &mut hits,
                total: &mut total,
                max,
                max_line_chars: self.max_line_chars,
                path: &path,
                root: &root,
                hit_cap: false,
            };
            let _ = searcher.search_path(&matcher, &path, &mut sink);
            if sink.hit_cap {
                hit_cap = true;
                break 'walk;
            }
        }

        let text = format_hits(&hits, hit_cap, max, total);
        Ok(ToolOutput {
            is_error: false,
            text,
            structured: Some(serde_json::json!({
                "pattern": input.pattern,
                "matches": total,
                "truncated": hit_cap,
            })),
            terminate: false,
            media: Vec::new(),
        })
    }
}

#[derive(Debug)]
struct Hit {
    rel_path: String,
    line: u64,
    text: String,
}

struct MatchSink<'a> {
    matcher: &'a grep::regex::RegexMatcher,
    hits: &'a mut Vec<Hit>,
    total: &'a mut u64,
    max: u64,
    max_line_chars: usize,
    path: &'a Path,
    root: &'a Path,
    hit_cap: bool,
}

impl Sink for MatchSink<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        if *self.total >= self.max {
            self.hit_cap = true;
            return Ok(false);
        }
        let line = mat.line_number().unwrap_or(0);
        let at = grep::matcher::Matcher::find(self.matcher, mat.bytes())
            .ok()
            .flatten()
            .map_or(0, |found| found.start());
        let text = clip_line(mat.bytes(), at, self.max_line_chars);
        let rel =
            kage_core::fsutil::slashed(self.path.strip_prefix(self.root).unwrap_or(self.path));
        self.hits.push(Hit {
            rel_path: rel,
            line,
            text,
        });
        *self.total += 1;
        Ok(true)
    }
}

/// The matching line as text, cut to `max_chars` around the match
/// starting at byte `at` when it is longer, with markers saying how much
/// was cut on each side. Up to [`LEAD_CHARS`] (a third of a short cap)
/// of context stay before the match.
fn clip_line(bytes: &[u8], at: usize, max_chars: usize) -> String {
    let lead = LEAD_CHARS.min(max_chars / 3);
    let full = String::from_utf8_lossy(bytes);
    let full = full.trim_end();
    let count = full.chars().count();
    if count <= max_chars {
        return full.to_owned();
    }
    let at_char = full
        .char_indices()
        .take_while(|(byte, _)| *byte < at)
        .count();
    let start = at_char.saturating_sub(lead).min(count - max_chars);
    let end = start + max_chars;
    let window: String = full.chars().skip(start).take(max_chars).collect();
    let before = if start > 0 {
        format!("[+{start} chars] ")
    } else {
        String::new()
    };
    let after = if end < count {
        format!(" [+{} chars]", count - end)
    } else {
        String::new()
    };
    format!("{before}{window}{after}")
}

fn format_hits(hits: &[Hit], truncated: bool, max: u64, total: u64) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    if hits.is_empty() {
        return "(no matches)".into();
    }
    for h in hits {
        let _ = writeln!(out, "{}:{}:{}", h.rel_path, h.line, h.text);
    }
    if truncated {
        let _ = writeln!(out, "[... truncated at {max} matches; total {total}+ ...]");
    } else {
        let _ = writeln!(out, "({total} matches)");
    }
    out
}

#[cfg(test)]
mod tests {
    use std::fs;

    use kage_core::CancelFlag;

    use super::*;

    fn run(workdir: &Path, input: serde_json::Value) -> Result<ToolOutput, ToolError> {
        let cancel = CancelFlag::new();
        let cx = ToolContext::new(workdir, &cancel);
        GrepTool::default().execute(input, &cx)
    }

    fn populate(dir: &Path) {
        fs::write(dir.join("a.txt"), "alpha\nbravo\nCharlie\n").unwrap();
        fs::write(dir.join("b.txt"), "kage\nshadow\n").unwrap();
        fs::create_dir(dir.join("sub")).unwrap();
        fs::write(dir.join("sub/c.txt"), "alphabet").unwrap();
    }

    #[test]
    fn a_huge_line_keeps_a_window_around_the_match() {
        let dir = tempfile::tempdir().unwrap();
        let line = format!("{}needle{}", "x".repeat(50_000), "y".repeat(50_000));
        fs::write(dir.path().join("bundle.js"), line).unwrap();
        let out = run(dir.path(), serde_json::json!({"pattern":"needle"})).unwrap();
        assert!(out.text.len() < 600, "{}", out.text.len());
        assert!(out.text.contains("needle"));
        assert!(out.text.contains("[+49900 chars]"), "{}", out.text);
    }

    #[test]
    fn finds_matches_recursively() {
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        let out = run(dir.path(), serde_json::json!({"pattern":"alpha"})).unwrap();
        assert!(out.text.contains("a.txt:1:alpha"));
        assert!(out.text.contains("sub/c.txt:1:alphabet"));
    }

    #[test]
    fn case_insensitive_match() {
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        let out = run(
            dir.path(),
            serde_json::json!({"pattern":"charlie","ignore_case":true}),
        )
        .unwrap();
        assert!(out.text.contains("a.txt:3:Charlie"));
    }

    #[test]
    fn no_matches_returns_empty_marker() {
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        let out = run(dir.path(), serde_json::json!({"pattern":"zzz"})).unwrap();
        assert_eq!(out.text, "(no matches)");
    }

    #[test]
    fn missing_path_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let err = run(dir.path(), serde_json::json!({"pattern":"x","path":"nope"}))
            .unwrap_err()
            .to_string();
        let missing = dir.path().canonicalize().unwrap().join("nope");
        let prefix = format!("cannot search {}: ", missing.display());
        assert!(err.starts_with(&prefix), "{err}");
    }

    #[test]
    fn invalid_regex_is_invalid_input() {
        let dir = tempfile::tempdir().unwrap();
        let err = run(dir.path(), serde_json::json!({"pattern":"["})).unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[test]
    fn honors_kageignore() {
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        fs::write(dir.path().join(".kageignore"), "b.txt\n").unwrap();
        let out = run(dir.path(), serde_json::json!({"pattern":"shadow"})).unwrap();
        assert_eq!(out.text, "(no matches)");
    }

    #[test]
    fn a_file_quitting_at_a_nul_byte_is_not_searched() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("bin.dat"), b"x\0y\nneedle\n").unwrap();
        let out = run(dir.path(), serde_json::json!({"pattern":"needle"})).unwrap();
        assert_eq!(out.text, "(no matches)");
    }

    #[test]
    fn dotfiles_are_not_searched() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".env"), "needle\n").unwrap();
        fs::write(dir.path().join("a.txt"), "needle\n").unwrap();
        let out = run(dir.path(), serde_json::json!({"pattern":"needle"})).unwrap();
        assert!(out.text.contains("a.txt:1:needle"), "{}", out.text);
        assert!(!out.text.contains(".env"), "{}", out.text);
    }

    #[test]
    fn max_matches_cap_truncates() {
        let dir = tempfile::tempdir().unwrap();
        let many: String = (0..50).map(|_| "x\n").collect();
        fs::write(dir.path().join("f.txt"), &many).unwrap();
        let out = run(
            dir.path(),
            serde_json::json!({"pattern":"x","max_matches":5}),
        )
        .unwrap();
        assert!(out.text.contains("truncated"));
        assert_eq!(out.structured.unwrap()["truncated"], true);
    }

    /// A zero cap is clamped to one, so the text and the structured
    /// output agree that a match was found and truncated.
    #[test]
    fn zero_max_matches_clamps_to_one() {
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        let out = run(
            dir.path(),
            serde_json::json!({"pattern":"alpha","max_matches":0}),
        )
        .unwrap();
        let lines: Vec<&str> = out.text.lines().collect();
        assert_eq!(lines.len(), 2, "one hit plus the truncation marker");
        assert!(lines[1].contains("truncated at 1 matches"), "{}", out.text);
        let structured = out.structured.unwrap();
        assert_eq!(structured["matches"], 1);
        assert_eq!(structured["truncated"], true);
    }

    #[test]
    fn filepath_is_accepted_as_an_alias_for_path() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub").join("a.txt"), "needle").unwrap();
        let out = run(
            dir.path(),
            serde_json::json!({"filePath":"sub","pattern":"needle"}),
        )
        .unwrap();
        assert!(out.text.contains("a.txt"), "got {}", out.text);
        // The canonical key still works: the alias is additive.
        let out = run(
            dir.path(),
            serde_json::json!({"path":"sub","pattern":"needle"}),
        )
        .unwrap();
        assert!(out.text.contains("a.txt"), "got {}", out.text);
    }
}
