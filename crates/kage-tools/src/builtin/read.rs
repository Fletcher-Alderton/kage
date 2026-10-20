//! `read` tool: read a file from the workspace, optionally limited to a line range.

use std::fmt::Write;
use std::io::{BufRead, BufReader, Read, Seek};
use std::path::Path;

use base64::Engine as _;
use kage_core::{Risk, ToolMedia, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;

use kage_core::config::ReadToolConfig;

use crate::{Tool, ToolContext, ToolError, schema_for};

/// Largest image the tool attaches to its result, in raw bytes.
/// Anthropic accepts at most 5 MB per image before base64 encoding, so
/// staying under it keeps the attachment deliverable to every provider.
const MAX_IMAGE_BYTES: usize = 5_000_000;

/// Input shape for the `read` tool.
#[derive(Debug, Deserialize, JsonSchema)]
struct ReadInput {
    /// Path to read, relative to the workdir.
    #[serde(alias = "filePath")]
    path: String,
    /// Optional 1-indexed start line (inclusive). Line numbers are
    /// 1-based and 0 is rejected; sliced lines fold `\r\n` to `\n`.
    /// A start past the file's last line is an error.
    #[serde(default)]
    start_line: Option<u64>,
    /// Optional 1-indexed end line (inclusive). 0 is rejected.
    #[serde(default)]
    end_line: Option<u64>,
}

/// Read a file from disk.
#[derive(Debug)]
pub struct ReadTool {
    /// Bytes one read returns before it is cut, with a footer saying so.
    max_bytes: usize,
    description: String,
}

impl Default for ReadTool {
    fn default() -> Self {
        Self::new(&ReadToolConfig::default())
    }
}

impl ReadTool {
    /// A `read` tool with the limits `[tools.read]` sets.
    #[must_use]
    pub fn new(cfg: &ReadToolConfig) -> Self {
        Self {
            max_bytes: usize::try_from(cfg.max_bytes).unwrap_or(usize::MAX),
            description: format!(
                "Read a file. Optional `start_line` and `end_line` limit \
                 the returned range (1-indexed, inclusive). The returned range \
                 is capped at {}; a capped read says so. Images (png, jpeg, \
                 gif, webp) come back as an image attached to the result \
                 instead of text, so a model that sees images can view them.",
                super::human_bytes(cfg.max_bytes)
            ),
        }
    }
}

impl Tool for ReadTool {
    fn name(&self) -> &'static str {
        "read"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> serde_json::Value {
        schema_for::<ReadInput>()
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn execute(
        &self,
        input: serde_json::Value,
        cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let input: ReadInput = serde_json::from_value(input)?;
        let path = cx.resolve_path(Path::new(&input.path))?;
        if input.start_line == Some(0) || input.end_line == Some(0) {
            return Err(ToolError::InvalidInput(
                "line numbers are 1-based; line numbers must be at least 1".into(),
            ));
        }

        let file = std::fs::File::open(&path).map_err(ToolError::io_at("read", &path))?;
        let total_bytes = file
            .metadata()
            .map_err(ToolError::io_at("read", &path))?
            .len();
        // Sniff the head before treating the file as text: an image
        // comes back whole as attached media, everything else is
        // rewound and sliced line by line.
        let mut file = file;
        let mut head = Vec::with_capacity(16);
        (&mut file)
            .take(16)
            .read_to_end(&mut head)
            .map_err(ToolError::io_at("read", &path))?;
        if let Some(mime) = kage_core::image::sniff_mime(&head) {
            return read_image(&path, &input, file, total_bytes, mime);
        }
        file.rewind().map_err(ToolError::io_at("read", &path))?;
        // Slice before capping: lines are streamed, so memory stays
        // bounded by the cap rather than by the file, and a range past
        // the byte cap still returns its lines.
        let text = slice_file(&file, &path, &input, total_bytes, self.max_bytes)?;

        Ok(ToolOutput {
            is_error: false,
            text,
            structured: None,
            terminate: false,
            media: Vec::new(),
        })
    }
}

/// `path` holds image bytes the sniffer recognized: attach the whole
/// file to the result as base64 media instead of its text, so a model
/// that reads images sees it and hosts render it. An image past
/// [`MAX_IMAGE_BYTES`] is refused with its size, which the model can
/// act on by downscaling with `shell`, rather than silently dropped.
fn read_image(
    path: &Path,
    input: &ReadInput,
    mut file: std::fs::File,
    total_bytes: u64,
    mime: &'static str,
) -> Result<ToolOutput, ToolError> {
    let too_large = |bytes: usize| {
        ToolError::InvalidInput(format!(
            "{} is a {} {}; the largest image this tool attaches is {} ({} bytes). \
             Downscale or re-encode it first, then read it again",
            input.path,
            kage_core::image::human_bytes(bytes),
            mime,
            kage_core::image::human_bytes(MAX_IMAGE_BYTES),
            MAX_IMAGE_BYTES,
        ))
    };
    if usize::try_from(total_bytes).map_or(true, |n| n > MAX_IMAGE_BYTES) {
        return Err(too_large(
            usize::try_from(total_bytes).unwrap_or(usize::MAX),
        ));
    }
    // The metadata check bounds the common case; the take bounds a
    // file that grew between the two. The sniffer consumed the head,
    // so rewind first.
    file.rewind().map_err(ToolError::io_at("read", path))?;
    let mut bytes = Vec::with_capacity(usize::try_from(total_bytes).unwrap_or(0));
    (&mut file)
        .take(MAX_IMAGE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(ToolError::io_at("read", path))?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(too_large(bytes.len()));
    }
    let mut text = format!(
        "{} is an image ({}, {}). It comes with this result, and the user sees it.",
        input.path,
        mime,
        kage_core::image::human_bytes(bytes.len()),
    );
    if input.start_line.is_some() || input.end_line.is_some() {
        text.push_str(" Line ranges do not apply to images.");
    }
    Ok(ToolOutput {
        is_error: false,
        text,
        structured: None,
        terminate: false,
        media: vec![ToolMedia {
            mime: mime.to_owned(),
            data: base64::engine::general_purpose::STANDARD.encode(&bytes),
        }],
    })
}

/// Stream the line range out of the file behind `input`, appending the
/// truncation footer when the `max_bytes` cap cuts the range. Lines
/// before `start_line` are scanned in place and dropped, and a line is
/// gathered only up to the remaining budget, so memory stays bounded
/// by the cap, not by the file.
fn slice_file(
    file: &std::fs::File,
    path: &Path,
    input: &ReadInput,
    total_bytes: u64,
    max_bytes: usize,
) -> Result<String, ToolError> {
    let skip = input
        .start_line
        .map_or(0, |n| usize::try_from(n - 1).unwrap_or(usize::MAX));
    let mut reader = BufReader::new(file);

    if let Some((whole, partial)) = skip_lines(&mut reader, path, skip)? {
        let total_lines = whole + usize::from(partial > 0);
        let start = input.start_line.unwrap();
        return Err(ToolError::InvalidInput(format!(
            "{} has {total_lines} lines ({total_bytes} bytes); line {start} is past the end of the file",
            input.path
        )));
    }

    let mut raw: Vec<u8> = Vec::new();
    let mut lines: Vec<String> = Vec::new();
    let mut shown = 0usize;
    let mut cut = false;
    let mut line_no = skip;
    let mut reached_eof = false;
    let mut last_ended_newline = false;
    while !cut {
        if let Some(end) = input.end_line
            && (line_no as u64) >= end
        {
            break;
        }
        let budget = max_bytes - shown;
        if budget == 0 {
            // Nothing may be shown past the cap, but the footer is
            // honest only when something is actually left.
            cut = !reader
                .fill_buf()
                .map_err(ToolError::io_at("read", path))?
                .is_empty();
            if !cut {
                reached_eof = true;
            }
            break;
        }
        let gather = gather_line(&mut reader, path, &mut raw, budget + 1)?;
        if gather == Gather::Eof {
            reached_eof = true;
        }
        if raw.is_empty() {
            break;
        }
        line_no += 1;
        last_ended_newline = raw.last() == Some(&b'\n');
        if raw.ends_with(b"\r\n") {
            raw.truncate(raw.len() - 2);
        } else if last_ended_newline {
            raw.pop();
        }
        if show_line(&raw, budget, gather, input, &mut shown, &mut lines)? {
            cut = true;
            break;
        }
    }
    // The past-the-end error needs a genuine EOF: an inverted range
    // (end before start) stops early without one and stays an empty
    // success, as it always has.
    if let Some(start) = input.start_line
        && reached_eof
        && (line_no as u64) < start
    {
        return Err(ToolError::InvalidInput(format!(
            "{} has {line_no} lines ({total_bytes} bytes); line {start} is past the end of the file",
            input.path
        )));
    }
    let mut text = lines.join("\n");
    if !cut && reached_eof && last_ended_newline {
        text.push('\n');
    }
    if cut {
        let _ = write!(
            text,
            "\n\n[... truncated: showed {shown} bytes of this range; the file is {total_bytes} bytes ...]"
        );
    }
    Ok(text)
}

/// Decode one gathered line into `lines`, counting its bytes into
/// `shown`, and return whether the [`MAX_BYTES`] cap ended the read.
/// An oversized line is cut at a character boundary. An incomplete
/// sequence with a [`Gather::Capped`] line means the cap split the
/// line's final character: the whole characters that fit go in and
/// the read stops. The same sequence at [`Gather::Eof`] means the
/// file itself ends mid-character, so it stays a binary-file error.
/// Invalid UTF-8 away from the cap is a binary-file error either way.
fn show_line(
    raw: &[u8],
    budget: usize,
    gather: Gather,
    input: &ReadInput,
    shown: &mut usize,
    lines: &mut Vec<String>,
) -> Result<bool, ToolError> {
    let not_text = || {
        ToolError::InvalidInput(format!(
            "{} is not valid UTF-8 and is likely a binary file",
            input.path
        ))
    };
    let (head, cut) = match std::str::from_utf8(raw) {
        Ok(line) if line.len() > budget => {
            let mut at = budget;
            while !line.is_char_boundary(at) {
                at -= 1;
            }
            (&line[..at], true)
        }
        Ok(line) => (line, false),
        // A capped gather is at least budget + 1 bytes, so the line
        // is always over budget here.
        Err(e) if gather == Gather::Capped && e.error_len().is_none() => {
            let at = e.valid_up_to();
            let head = std::str::from_utf8(&raw[..at]).map_err(|_| not_text())?;
            (head, true)
        }
        Err(_) => return Err(not_text()),
    };
    *shown += head.len();
    lines.push(head.to_owned());
    Ok(cut)
}

/// Scan `skip` lines out of `reader` without keeping them. Returns
/// `Some((whole, partial))` when the file ended first, so the caller
/// can name the real line count: `whole` complete lines plus a
/// trailing line with no newline.
fn skip_lines<R: BufRead>(
    reader: &mut R,
    path: &Path,
    skip: usize,
) -> Result<Option<(usize, usize)>, ToolError> {
    let mut skip_left = skip;
    let mut whole = 0usize;
    let mut partial = 0usize;
    while skip_left > 0 {
        // A chunk without a newline is a partial line, not a whole
        // one: lines span refills, so only a newline ends a line here.
        let step = {
            let available = reader.fill_buf().map_err(ToolError::io_at("read", path))?;
            if available.is_empty() {
                None
            } else if let Some(i) = available.iter().position(|&b| b == b'\n') {
                whole += 1;
                partial = 0;
                Some((i + 1, true))
            } else {
                partial += available.len();
                Some((available.len(), false))
            }
        };
        match step {
            Some((consumed, found_newline)) => {
                reader.consume(consumed);
                if found_newline {
                    skip_left -= 1;
                }
            }
            None => return Ok(Some((whole, partial))),
        }
    }
    Ok(None)
}

/// How [`gather_line`]'s line ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gather {
    /// A newline was read into the buffer.
    Newline,
    /// The cap was reached without a newline, and more bytes remain.
    Capped,
    /// The file ended. The buffer holds a trailing line without a
    /// newline, or nothing at all.
    Eof,
}

/// Read one line out of `reader` into `raw`, which is cleared first
/// and never extended past `cap` bytes, so a newline-free line cannot
/// balloon the buffer.
fn gather_line<R: BufRead>(
    reader: &mut R,
    path: &Path,
    raw: &mut Vec<u8>,
    cap: usize,
) -> Result<Gather, ToolError> {
    raw.clear();
    raw.reserve(cap.min(4096));
    loop {
        let step = {
            let available = reader.fill_buf().map_err(ToolError::io_at("read", path))?;
            if available.is_empty() {
                None
            } else {
                let room = cap - raw.len();
                let window = &available[..room.min(available.len())];
                if let Some(i) = window.iter().position(|&b| b == b'\n') {
                    raw.extend_from_slice(&window[..=i]);
                    Some((i + 1, true))
                } else {
                    raw.extend_from_slice(window);
                    Some((window.len(), false))
                }
            }
        };
        match step {
            Some((consumed, found_newline)) => {
                reader.consume(consumed);
                if found_newline {
                    return Ok(Gather::Newline);
                }
            }
            None => return Ok(Gather::Eof),
        }
        if raw.len() >= cap {
            // Peek past the cap: bytes still waiting mean a genuine
            // cap, an empty buffer means the file ended exactly on
            // the cap, which is EOF, not a cut character.
            let more = !reader
                .fill_buf()
                .map_err(ToolError::io_at("read", path))?
                .is_empty();
            return Ok(if more { Gather::Capped } else { Gather::Eof });
        }
    }
}

#[cfg(test)]
mod tests {
    /// The default `[tools.read] max_bytes`.
    const MAX_BYTES: usize = 2_000_000;
    use std::fs;

    use kage_core::CancelFlag;

    use super::*;

    fn run(
        tool: &ReadTool,
        workdir: &Path,
        input: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let cancel = CancelFlag::new();
        let cx = ToolContext::new(workdir, &cancel);
        tool.execute(input, &cx)
    }

    #[test]
    fn reads_full_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("hello.txt"), "line1\nline2\nline3").unwrap();
        let out = run(
            &ReadTool::default(),
            dir.path(),
            serde_json::json!({"path":"hello.txt"}),
        )
        .unwrap();
        assert_eq!(out.text, "line1\nline2\nline3");
        assert!(!out.is_error);
    }

    #[test]
    fn slices_by_line_range() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("f.txt"), "a\nb\nc\nd\ne").unwrap();
        let out = run(
            &ReadTool::default(),
            dir.path(),
            serde_json::json!({"path":"f.txt","start_line":2,"end_line":4}),
        )
        .unwrap();
        assert_eq!(out.text, "b\nc\nd");
    }

    #[test]
    fn truncates_oversized_file() {
        let dir = tempfile::tempdir().unwrap();
        let big = "x".repeat(MAX_BYTES + 100);
        fs::write(dir.path().join("big.txt"), &big).unwrap();
        let out = run(
            &ReadTool::default(),
            dir.path(),
            serde_json::json!({"path":"big.txt"}),
        )
        .unwrap();
        assert!(out.text.contains("truncated"));
        assert!(out.text.len() > MAX_BYTES);
        // The cut lands exactly at the cap, so the buffer the tool
        // holds stays bounded by it.
        let body = out.text.split("\n\n[... truncated").next().unwrap();
        assert_eq!(body.len(), MAX_BYTES);
    }

    /// `[tools.read] max_bytes` replaces the default cap, and the
    /// description states the value in effect.
    #[test]
    fn a_configured_max_bytes_is_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = ReadToolConfig {
            max_bytes: 100,
            ..ReadToolConfig::default()
        };
        let tool = ReadTool::new(&cfg);
        assert!(
            tool.description().contains("100 bytes"),
            "{}",
            tool.description()
        );
        let big = "x".repeat(250);
        fs::write(dir.path().join("big.txt"), &big).unwrap();
        let out = run(&tool, dir.path(), serde_json::json!({"path":"big.txt"})).unwrap();
        assert!(out.text.contains("truncated"));
        let body = out.text.split("\n\n[... truncated").next().unwrap();
        assert_eq!(body.len(), 100);
    }

    /// A file whose size is exactly the cap is fully shown and says
    /// nothing about truncation.
    #[test]
    fn an_exact_cap_total_file_reads_without_a_false_footer() {
        let dir = tempfile::tempdir().unwrap();
        let big = "a\n".repeat(MAX_BYTES / 2);
        assert_eq!(big.len(), MAX_BYTES);
        fs::write(dir.path().join("big.txt"), &big).unwrap();
        let out = run(
            &ReadTool::default(),
            dir.path(),
            serde_json::json!({"path":"big.txt"}),
        )
        .unwrap();
        assert_eq!(out.text, big);
        assert!(!out.text.contains("truncated"), "{}", &out.text[..100]);
    }

    /// `end_line` before `start_line` has always been an empty
    /// success; it must not turn into a false line-count error.
    #[test]
    fn an_inverted_range_is_an_empty_success() {
        let dir = tempfile::tempdir().unwrap();
        let mut body = String::new();
        for n in 1..=1000 {
            let _ = writeln!(body, "L{n}");
        }
        fs::write(dir.path().join("f.txt"), &body).unwrap();
        let out = run(
            &ReadTool::default(),
            dir.path(),
            serde_json::json!({"path":"f.txt","start_line":50,"end_line":10}),
        )
        .unwrap();
        assert_eq!(out.text, "");
    }

    #[test]
    fn missing_file_error_names_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let err = run(
            &ReadTool::default(),
            dir.path(),
            serde_json::json!({"path":"missing.txt"}),
        )
        .unwrap_err()
        .to_string();
        let missing = dir.path().canonicalize().unwrap().join("missing.txt");
        let prefix = format!("cannot read {}: ", missing.display());
        assert!(err.starts_with(&prefix), "{err}");
    }

    #[test]
    fn invalid_input_returns_json_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = run(&ReadTool::default(), dir.path(), serde_json::json!({})).unwrap_err();
        assert!(matches!(err, ToolError::Json(_)), "got {err:?}");
    }

    #[test]
    fn end_line_beyond_eof_clamps() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("short.txt"), "a\nb").unwrap();
        let out = run(
            &ReadTool::default(),
            dir.path(),
            serde_json::json!({"path":"short.txt","start_line":1,"end_line":1000}),
        )
        .unwrap();
        assert_eq!(out.text, "a\nb");
    }

    #[test]
    fn zero_line_numbers_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("f.txt"), "a\nb\n").unwrap();
        for input in [
            serde_json::json!({"path":"f.txt","start_line":0}),
            serde_json::json!({"path":"f.txt","end_line":0}),
            serde_json::json!({"path":"f.txt","start_line":1,"end_line":0}),
        ] {
            let err = run(&ReadTool::default(), dir.path(), input).unwrap_err();
            assert!(matches!(err, ToolError::InvalidInput(_)), "{err:?}");
            assert!(err.to_string().contains("1-based"), "{err}");
        }
    }

    #[test]
    fn a_slice_reaching_eof_keeps_the_trailing_newline() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("f.txt"), "a\nb\nc\n").unwrap();
        let out = run(
            &ReadTool::default(),
            dir.path(),
            serde_json::json!({"path":"f.txt","start_line":2}),
        )
        .unwrap();
        assert_eq!(out.text, "b\nc\n");
        let out = run(
            &ReadTool::default(),
            dir.path(),
            serde_json::json!({"path":"f.txt"}),
        )
        .unwrap();
        assert_eq!(out.text, "a\nb\nc\n");
        let out = run(
            &ReadTool::default(),
            dir.path(),
            serde_json::json!({"path":"f.txt","end_line":2}),
        )
        .unwrap();
        assert_eq!(out.text, "a\nb");
    }

    #[test]
    fn sliced_crlf_lines_are_folded_to_lf() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("f.txt"), "a\r\nb\r\n").unwrap();
        let out = run(
            &ReadTool::default(),
            dir.path(),
            serde_json::json!({"path":"f.txt","start_line":1,"end_line":1}),
        )
        .unwrap();
        assert_eq!(out.text, "a");
    }

    /// The byte cap lands mid-character on one newline-free line of
    /// 4-byte characters: the tool keeps the whole characters that
    /// fit instead of rejecting the file as binary.
    #[test]
    fn truncates_single_long_line_of_four_byte_chars() {
        let dir = tempfile::tempdir().unwrap();
        let line = "\u{1F600}".repeat(MAX_BYTES / 4 + 10);
        fs::write(dir.path().join("emoji.txt"), &line).unwrap();
        let out = run(
            &ReadTool,
            dir.path(),
            serde_json::json!({"path":"emoji.txt"}),
        )
        .unwrap();
        assert!(out.text.contains("truncated"), "{}", &out.text[..100]);
        let body = out.text.split("\n\n[... truncated").next().unwrap();
        assert!(
            body.chars().all(|c| c == '\u{1F600}'),
            "the cut keeps whole characters, got {} bytes",
            body.len()
        );
        assert_eq!(body.len(), MAX_BYTES, "{}", body.len());
    }

    /// Earlier lines shrink the budget, moving the gather cap onto a
    /// 3-byte character: the same whole-character cut applies.
    #[test]
    fn truncates_multibyte_line_after_earlier_lines() {
        let dir = tempfile::tempdir().unwrap();
        let line = "\u{2022}".repeat(MAX_BYTES / 3 + 10);
        fs::write(dir.path().join("f.txt"), format!("a\n{line}")).unwrap();
        let out = run(&ReadTool, dir.path(), serde_json::json!({"path":"f.txt"})).unwrap();
        assert!(out.text.contains("truncated"), "{}", &out.text[..100]);
        let body = out.text.split("\n\n[... truncated").next().unwrap();
        assert!(body.starts_with("a\n"), "{}", &body[..20]);
        assert!(
            body[2..].chars().all(|c| c == '\u{2022}'),
            "the cut keeps whole characters, got {} bytes",
            body.len()
        );
        assert_eq!(body.len(), MAX_BYTES, "{}", body.len());
    }

    /// A valid line whose budget lands mid-character takes the `Ok`
    /// truncation branch: the cut walks back to the boundary itself,
    /// independently of the torn-tail `Err` branch.
    #[test]
    fn truncation_walks_back_to_a_boundary_inside_a_valid_line() {
        let dir = tempfile::tempdir().unwrap();
        let body = format!("a\n{}\u{e9}y\n", "x".repeat(MAX_BYTES - 2));
        fs::write(dir.path().join("f.txt"), &body).unwrap();
        let out = run(&ReadTool, dir.path(), serde_json::json!({"path":"f.txt"})).unwrap();
        assert!(out.text.contains("truncated"), "{}", &out.text[..100]);
        let shown = out.text.split("\n\n[... truncated").next().unwrap();
        assert_eq!(shown.len(), MAX_BYTES, "{}", shown.len());
        assert!(shown.ends_with('x'), "{}", &shown[shown.len() - 20..]);
    }

    /// Valid UTF-8 whose end lands exactly on the gather cap is
    /// legitimately truncated: EOF at the cap is not a torn file.
    #[test]
    fn valid_utf8_ending_exactly_at_the_cap_is_truncated() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("big.txt"), "a".repeat(MAX_BYTES + 1)).unwrap();
        let out = run(&ReadTool, dir.path(), serde_json::json!({"path":"big.txt"})).unwrap();
        assert!(out.text.contains("truncated"), "{}", &out.text[..100]);
        let body = out.text.split("\n\n[... truncated").next().unwrap();
        assert_eq!(body.len(), MAX_BYTES, "{}", body.len());
    }

    /// Invalid bytes before the cap are still a binary file: the
    /// oversized-line tolerance must not loosen the rejection.
    #[test]
    fn a_long_line_with_invalid_utf8_is_still_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut bytes = vec![b'x'; MAX_BYTES / 2];
        bytes.extend_from_slice(&[0xff, 0xfe]);
        bytes.resize(bytes.len() + MAX_BYTES, b'y');
        fs::write(dir.path().join("bin.txt"), &bytes).unwrap();
        let err = run(&ReadTool, dir.path(), serde_json::json!({"path":"bin.txt"})).unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)), "{err:?}");
        assert!(err.to_string().contains("UTF-8"), "{err}");
    }

    /// A file that merely ends with an incomplete sequence, with the
    /// cap nowhere in sight, is a torn file and stays rejected.
    #[test]
    fn an_incomplete_sequence_at_eof_is_still_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut bytes = b"ok text ".to_vec();
        bytes.extend_from_slice(&[0xf0, 0x9f, 0x98]);
        fs::write(dir.path().join("torn.txt"), &bytes).unwrap();
        let err = run(
            &ReadTool,
            dir.path(),
            serde_json::json!({"path":"torn.txt"}),
        )
        .unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)), "{err:?}");
        assert!(err.to_string().contains("UTF-8"), "{err}");
    }

    /// A file of exactly gather-cap bytes ending in an incomplete
    /// sequence is torn at EOF, not split by the cap: nothing is left
    /// to read, so no character was cut. It must stay a binary-file
    /// error, like any torn file.
    #[test]
    fn an_incomplete_sequence_at_eof_on_the_cap_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut bytes = vec![b'a'; MAX_BYTES - 2];
        bytes.extend_from_slice(&[0xf0, 0x9f, 0x98]);
        assert_eq!(bytes.len(), MAX_BYTES + 1);
        fs::write(dir.path().join("torn.txt"), &bytes).unwrap();
        let out = run(
            &ReadTool,
            dir.path(),
            serde_json::json!({"path":"torn.txt"}),
        );
        let err = match out {
            Ok(out) => panic!(
                "a file torn at EOF on the cap came back as truncated text, not an error: {:?}",
                &out.text[..out.text.len().min(80)]
            ),
            Err(err) => err,
        };
        assert!(matches!(err, ToolError::InvalidInput(_)), "{err:?}");
        assert!(err.to_string().contains("UTF-8"), "{err}");
    }

    #[test]
    fn a_binary_file_is_rejected_not_mojibake() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("bin.dat"), [0xffu8, 0xfe, 0x00]).unwrap();
        let err = run(
            &ReadTool::default(),
            dir.path(),
            serde_json::json!({"path":"bin.dat"}),
        )
        .unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)), "{err:?}");
        assert!(err.to_string().contains("bin.dat"), "{err}");
        assert!(err.to_string().contains("UTF-8"), "{err}");
    }

    /// One small file per sniffable format: the tool attaches it as
    /// base64 media instead of rejecting it as binary.
    #[test]
    fn image_files_come_back_as_attached_media() {
        use base64::Engine as _;
        let cases: [(&str, Vec<u8>, &str); 4] = [
            (
                "shot.png",
                [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 7, 8].into(),
                "image/png",
            ),
            (
                "photo.jpg",
                [0xFF, 0xD8, 0xFF, 0xE0, 1, 2].into(),
                "image/jpeg",
            ),
            ("anim.gif", b"GIF89a\x01\x00\x01\x00".to_vec(), "image/gif"),
            (
                "pic.webp",
                b"RIFF\x12\x00\x00\x00WEBPVP8 ".to_vec(),
                "image/webp",
            ),
        ];
        for (name, bytes, mime) in cases {
            let dir = tempfile::tempdir().unwrap();
            fs::write(dir.path().join(name), &bytes).unwrap();
            let out = run(
                &ReadTool::default(),
                dir.path(),
                serde_json::json!({"path": name}),
            )
            .unwrap();
            assert!(!out.is_error, "{name}");
            assert_eq!(out.media.len(), 1, "{name}");
            assert_eq!(out.media[0].mime, mime, "{name}");
            assert_eq!(
                base64::engine::general_purpose::STANDARD
                    .decode(&out.media[0].data)
                    .unwrap(),
                bytes,
                "{name}"
            );
            assert!(
                out.text.contains(name) && out.text.contains(mime),
                "{}",
                out.text
            );
        }
    }

    /// A text file named `.png` is read as text: the sniffer trusts
    /// bytes, never the declared extension.
    #[test]
    fn an_extension_alone_does_not_make_an_image() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("fake.png"), "just text").unwrap();
        let out = run(
            &ReadTool::default(),
            dir.path(),
            serde_json::json!({"path":"fake.png"}),
        )
        .unwrap();
        assert!(out.media.is_empty());
        assert_eq!(out.text, "just text");
    }

    #[test]
    fn an_oversized_image_is_refused_with_its_size() {
        let dir = tempfile::tempdir().unwrap();
        let mut bytes = vec![0x89u8, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        bytes.resize(MAX_IMAGE_BYTES + 1, 0);
        fs::write(dir.path().join("huge.png"), &bytes).unwrap();
        let err = run(
            &ReadTool::default(),
            dir.path(),
            serde_json::json!({"path":"huge.png"}),
        )
        .unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)), "{err:?}");
        let msg = err.to_string();
        assert!(msg.contains("huge.png"), "{msg}");
        assert!(msg.contains("Downscale"), "{msg}");
        assert!(msg.contains(&MAX_IMAGE_BYTES.to_string()), "{msg}");
    }

    #[test]
    fn line_ranges_are_ignored_for_images_with_a_note() {
        let dir = tempfile::tempdir().unwrap();
        let png = [0x89u8, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        fs::write(dir.path().join("shot.png"), png).unwrap();
        let out = run(
            &ReadTool::default(),
            dir.path(),
            serde_json::json!({"path":"shot.png","start_line":1,"end_line":2}),
        )
        .unwrap();
        assert_eq!(out.media.len(), 1);
        assert!(
            out.text.contains("Line ranges do not apply to images"),
            "{}",
            out.text
        );
    }

    #[test]
    fn confined_read_rejects_escape() {
        let (root, work) = nested_workdir();
        fs::write(root.path().join("outside-read.txt"), "secret").unwrap();
        let cancel = CancelFlag::new();
        let cx = ToolContext::new(&work, &cancel).with_confine();
        let err = ReadTool::default()
            .execute(serde_json::json!({"path":"../outside-read.txt"}), &cx)
            .unwrap_err();
        assert!(matches!(err, ToolError::Path { .. }), "got {err:?}");
    }

    #[test]
    fn unconfined_read_accepts_escape() {
        let (root, work) = nested_workdir();
        fs::write(root.path().join("outside-read.txt"), "secret").unwrap();
        let out = run(
            &ReadTool::default(),
            &work,
            serde_json::json!({"path":"../outside-read.txt"}),
        )
        .unwrap();
        assert_eq!(out.text, "secret");
    }

    /// A workdir inside its own temp root, so `..` stays private to
    /// the test.
    fn nested_workdir() -> (tempfile::TempDir, std::path::PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let work = root.path().join("work");
        fs::create_dir(&work).unwrap();
        (root, work)
    }

    #[test]
    fn filepath_is_accepted_as_an_alias_for_path() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "hello").unwrap();
        let out = run(
            &ReadTool::default(),
            dir.path(),
            serde_json::json!({"filePath":"a.txt"}),
        )
        .unwrap();
        assert_eq!(out.text, "hello");
        // The canonical key still works: the alias is additive.
        let out = run(
            &ReadTool::default(),
            dir.path(),
            serde_json::json!({"path":"a.txt"}),
        )
        .unwrap();
        assert_eq!(out.text, "hello");
    }

    /// ~2.1MB, 10,000 lines, each tagged with its 1-based number.
    fn big_numbered_file(dir: &Path) -> u64 {
        let filler = "x".repeat(199);
        let mut body = String::new();
        for n in 1..=10_000u64 {
            let _ = writeln!(body, "L{n:06} {filler}");
        }
        fs::write(dir.join("big.txt"), &body).unwrap();
        10_000
    }

    #[test]
    fn read_past_the_byte_cap_returns_the_requested_lines() {
        let dir = tempfile::tempdir().unwrap();
        let last = big_numbered_file(dir.path());
        let out = run(
            &ReadTool::default(),
            dir.path(),
            serde_json::json!({"path":"big.txt","start_line":last,"end_line":last}),
        )
        .unwrap();
        assert!(
            out.text.starts_with("L010000 "),
            "wanted the requested line, got: {}",
            &out.text[..60.min(out.text.len())]
        );
    }

    /// A requested line that straddles the reader's refill boundary
    /// must still land exactly: 40 lines of 208 bytes cross the
    /// default 8192-byte chunk edge mid-line.
    #[test]
    fn a_line_spanning_a_reader_refill_is_still_found() {
        let dir = tempfile::tempdir().unwrap();
        let filler = "x".repeat(199);
        let mut body = String::new();
        for n in 1..=41usize {
            let _ = writeln!(body, "L{n:06} {filler}");
        }
        fs::write(dir.path().join("f.txt"), &body).unwrap();
        let out = run(
            &ReadTool::default(),
            dir.path(),
            serde_json::json!({"path":"f.txt","start_line":41,"end_line":41}),
        )
        .unwrap();
        assert_eq!(out.text.lines().count(), 1, "{}", &out.text[..80]);
        assert!(
            out.text.starts_with("L000041 "),
            "wanted line 41, got: {}",
            &out.text[..60.min(out.text.len())]
        );
    }

    #[test]
    fn read_distinguishes_past_the_cap_from_past_the_end() {
        let dir = tempfile::tempdir().unwrap();
        let last = big_numbered_file(dir.path());
        let existing = run(
            &ReadTool::default(),
            dir.path(),
            serde_json::json!({"path":"big.txt","start_line":last,"end_line":last}),
        );
        let absent = run(
            &ReadTool::default(),
            dir.path(),
            serde_json::json!({"path":"big.txt","start_line":last + 1_000_000,"end_line":last + 1_000_000}),
        );
        assert!(existing.is_ok(), "an existing line past the cap must read");
        let err = absent.unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)), "{err:?}");
        assert!(err.to_string().contains("past the end"), "{err}");
        assert!(err.to_string().contains("10000 lines"), "{err}");
    }

    #[test]
    fn read_does_not_claim_truncation_for_an_in_range_slice() {
        let dir = tempfile::tempdir().unwrap();
        big_numbered_file(dir.path());
        let out = run(
            &ReadTool::default(),
            dir.path(),
            serde_json::json!({"path":"big.txt","start_line":1,"end_line":5}),
        )
        .unwrap();
        assert_eq!(out.text.lines().count(), 5, "{}", out.text);
        assert!(out.text.starts_with("L000001 "), "{}", out.text);
        assert!(!out.text.contains("truncated"), "{}", out.text);
    }

    #[test]
    fn read_past_the_end_of_the_file_is_an_error_naming_the_count() {
        let dir = tempfile::tempdir().unwrap();
        let last = big_numbered_file(dir.path());
        let err = run(
            &ReadTool::default(),
            dir.path(),
            serde_json::json!({"path":"big.txt","start_line":last,"end_line":last + 500}),
        )
        .unwrap();
        assert!(err.text.starts_with("L010000 "), "{}", err.text);
        // An `end_line` past EOF still clamps; only a `start_line` past
        // EOF is an error.
        let err = run(
            &ReadTool::default(),
            dir.path(),
            serde_json::json!({"path":"big.txt","start_line":last + 1}),
        )
        .unwrap_err();
        assert!(err.to_string().contains("past the end"), "{err}");
    }
}
