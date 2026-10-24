//! Tests for view rendering helpers.

use std::time::{Duration, Instant};

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use serde_json::json;

use super::*;
use crate::buffer::Buffer;

// --- Word-wrap helpers ---

#[test]
fn wrap_breaks_at_word_boundary_when_word_fits() {
    // Width 10 fits "hello" (5) + space + "world" (5) = 11 chars,
    // so "world" must wrap to a new row instead of splitting.
    let rows = wrap_input_rows("hello world", 10);
    let rendered: Vec<&str> = rows.iter().map(|(s, e)| &"hello world"[*s..*e]).collect();
    assert_eq!(rendered, vec!["hello", "world"]);
}

#[test]
fn wrap_falls_back_to_char_break_for_oversize_word() {
    // 15-char word in width 10 should char-break at 10 chars.
    let rows = wrap_input_rows("aaaaaaaaaaaaaaa", 10);
    let rendered: Vec<&str> = rows
        .iter()
        .map(|(s, e)| &"aaaaaaaaaaaaaaa"[*s..*e])
        .collect();
    assert_eq!(rendered, vec!["aaaaaaaaaa", "aaaaa"]);
}

#[test]
fn wrap_preserves_logical_newlines_as_row_breaks() {
    let rows = wrap_input_rows("ab\ncd", 10);
    assert_eq!(rows, vec![(0, 2), (3, 5)]);
}

#[test]
fn wrap_empty_logical_line_emits_one_zero_length_row() {
    let rows = wrap_input_rows("\n", 10);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].1 - rows[0].0, 0);
}

#[test]
fn visual_cursor_word_wrapped_matches_paint() {
    // "hello world" at width 10 wraps to ["hello", "world"].
    // Cursor at byte 6 (start of "world") should be at (row 1, col 0).
    let (row, col) = input_visual_cursor("hello world", 6, 10);
    assert_eq!((row, col), (1, 0));
}

#[test]
fn visual_cursor_at_end_of_first_row_after_word_break() {
    // Cursor at byte 5 (the space) should be at end of row 0.
    let (row, col) = input_visual_cursor("hello world", 5, 10);
    assert_eq!((row, col), (0, 5));
}

#[test]
fn wrap_packs_rows_by_display_width_not_char_count() {
    // Four CJK glyphs are 8 display columns; a 4-col budget must
    // yield two rows of two glyphs, not one row of four (8 cols).
    let text = "\u{4f60}\u{597d}\u{4e16}\u{754c}";
    let rows = wrap_input_rows(text, 4);
    let rendered: Vec<&str> = rows.iter().map(|(s, e)| &text[*s..*e]).collect();
    assert_eq!(rendered, vec!["\u{4f60}\u{597d}", "\u{4e16}\u{754c}"]);
}

#[test]
fn visual_cursor_col_counts_display_width() {
    // Cursor after the first glyph sits at column 2 (its display
    // width), not column 1 (its char count), so the terminal cursor
    // lands beside the glyph instead of inside it.
    let (row, col) = input_visual_cursor("\u{4f60}\u{597d}", 3, 10);
    assert_eq!((row, col), (0, 2));
}

#[test]
fn truncate_to_width_never_exceeds_cell_budget() {
    let out = truncate_to_width("\u{4f60}\u{597d}\u{4e16}\u{754c}", 5, "\u{2026}");
    assert_eq!(out, "\u{4f60}\u{597d}\u{2026}");
    assert!(out.width() <= 5, "got {out:?} at {} cols", out.width());
}

#[test]
fn bubble_row_pad_fills_exact_card_width_for_wide_chars() {
    // A CJK prompt paints 4 display columns from 2 chars. Padding
    // that counted chars would leave the card two cells short and
    // shift the rule/pad chrome; every row must sum to exactly the
    // card width.
    let lines = user_block_lines("\u{4f60}\u{597d}", 20, Emphasis::None);
    for line in &lines {
        let cells: usize = line.spans.iter().map(|s| s.content.width()).sum();
        assert_eq!(cells, 20, "row {line:?} is {cells} cols, card is 20");
    }
}

#[test]
fn row_count_matches_actual_painted_rows() {
    // Three short words separated by spaces should be one row,
    // since they total 9 + 2 spaces = 11 > 10? No: "a b c" = 5
    // chars in width 10 = 1 row.
    assert_eq!(input_visual_row_count("a b c", 10), 1);
    // "alpha beta" = 10 chars, alpha+space+beta = 4+1+4 = 9 fits.
    assert_eq!(input_visual_row_count("alpha beta", 10), 1);
    // "alpha beta gamma" = 16 chars, wraps to 2 rows.
    assert_eq!(input_visual_row_count("alpha beta gamma", 10), 2);
}

// --- split_line_into_rows (block widgets) ---

fn row_text(row: &[Span<'_>]) -> String {
    row.iter().map(|s| s.content.as_ref()).collect()
}

#[test]
fn block_wrap_breaks_at_word_boundary_when_word_fits() {
    let line = Line::from(Span::raw("hello world"));
    let rows = split_line_into_rows(line, 10);
    let texts: Vec<String> = rows.iter().map(|r| row_text(r)).collect();
    assert_eq!(texts, vec!["hello", "world"]);
}

#[test]
fn block_wrap_falls_back_to_char_break_for_oversize_word() {
    let line = Line::from(Span::raw("aaaaaaaaaaaaaaa"));
    let rows = split_line_into_rows(line, 10);
    let texts: Vec<String> = rows.iter().map(|r| row_text(r)).collect();
    assert_eq!(texts, vec!["aaaaaaaaaa", "aaaaa"]);
}

#[test]
fn block_wrap_preserves_span_styles_across_break() {
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let line = Line::from(vec![Span::styled("hello", bold), Span::raw(" world tail")]);
    let rows = split_line_into_rows(line, 11);
    // Row 0 fits "hello world" (5+1+5=11); the bold style on
    // "hello" must survive.
    assert!(rows.len() >= 2, "expected at least 2 rows, got {rows:?}");
    let first_bold = rows[0]
        .iter()
        .any(|s| s.content == "hello" && s.style.add_modifier.contains(Modifier::BOLD));
    assert!(first_bold, "bold style should survive the wrap");
}

#[test]
fn block_wrap_uses_display_width_not_char_count() {
    // Each CJK ideograph is two display columns. With a 6-col
    // budget a row holds at most three; counting `char`s would
    // pack six (12 cols) and the outer `Paragraph::wrap` would
    // then fold the overflow onto a gutter-less continuation,
    // which is the "rule skips wrapped text" symptom.
    let line = Line::from(Span::raw(
        "\u{4e00}\u{4e8c}\u{4e09}\u{56db}\u{4e94}\u{516d}",
    ));
    let rows = split_line_into_rows(line, 6);
    for r in &rows {
        let cells: usize = row_text(r)
            .chars()
            .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
            .sum();
        assert!(
            cells <= 6,
            "row {:?} is {cells} cols, over the 6-col budget",
            row_text(r)
        );
    }
    assert_eq!(rows.len(), 2, "6 wide glyphs at 6 cols is 2 rows of 3");
}

#[test]
fn block_wrap_empty_line_yields_single_empty_row() {
    let line = Line::from(Span::raw(""));
    let rows = split_line_into_rows(line, 10);
    assert_eq!(rows.len(), 1);
    assert!(rows[0].is_empty() || row_text(&rows[0]).is_empty());
}

fn snapshot_lines(buffer: &mut Buffer, input: &InputState, area: Rect) -> Vec<String> {
    snapshot_frame(buffer, input, None, &StatusCtx::default(), None, area)
}

/// Paint one full frame with the chrome sized as the App sizes it and
/// return its rows, right-trimmed.
fn snapshot_frame(
    buffer: &mut Buffer,
    input: &InputState,
    cmdline: Option<&CommandLine>,
    status: &StatusCtx<'_>,
    usage: Option<&SessionUsage>,
    area: Rect,
) -> Vec<String> {
    let buf = paint_frame(
        buffer,
        input,
        cmdline,
        status,
        usage,
        area,
        &todo::TodoStrip::default(),
    );
    let mut out = Vec::new();
    for y in 0..buf.area.height {
        let mut row = String::new();
        for x in 0..buf.area.width {
            row.push_str(buf[(x, y)].symbol());
        }
        out.push(row.trim_end().to_owned());
    }
    out
}

fn paint_frame(
    buffer: &mut Buffer,
    input: &InputState,
    cmdline: Option<&CommandLine>,
    status: &StatusCtx<'_>,
    usage: Option<&SessionUsage>,
    area: Rect,
    todos: &todo::TodoStrip,
) -> ratatui::buffer::Buffer {
    let backend = TestBackend::new(area.width, area.height);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut captured: std::collections::BTreeMap<usize, Vec<CapturedCell>> =
        std::collections::BTreeMap::new();
    terminal
        .draw(|frame| {
            let heights = chrome_heights(status, usage, input, frame.area().width, todos);
            let regions = crate::layout::split(frame.area(), heights);
            render(
                frame,
                regions,
                buffer,
                input,
                cmdline,
                status,
                None,
                &mut captured,
                usage,
                &[],
            );
        })
        .unwrap();
    terminal.backend().buffer().clone()
}

#[test]
fn finished_thinking_folds_to_a_timed_line() {
    let mut buffer = Buffer::new();
    buffer.append_thinking_delta("step 1\nstep 2");
    buffer.finish_streaming();
    let input = InputState::new();
    let lines = snapshot_lines(&mut buffer, &input, Rect::new(0, 0, 40, 6));
    assert!(
        lines.iter().any(|l| l.contains("Thought for 1s")),
        "{lines:?}"
    );
    assert!(!lines.iter().any(|l| l.contains("step 1")), "{lines:?}");
}

/// The live path must record the real elapsed duration, not rely on
/// the 1s display floor: an aged start renders the actual seconds.
#[test]
fn a_finished_live_thinking_block_reports_its_real_duration() {
    let mut buffer = Buffer::new();
    buffer.append_thinking_delta("deliberating");
    buffer.age_live_thinking(Duration::from_millis(2900));
    buffer.finish_streaming();
    let input = InputState::new();
    let lines = snapshot_lines(&mut buffer, &input, Rect::new(0, 0, 40, 6));
    assert!(
        lines.iter().any(|l| l.contains("Thought for 2s")),
        "{lines:?}"
    );
}

#[test]
fn unfolded_thinking_includes_body() {
    let mut buffer = Buffer::new();
    buffer.append_thinking_delta("step 1\nstep 2");
    buffer.finish_streaming();
    assert!(buffer.toggle_fold(0));
    let input = InputState::new();
    let lines = snapshot_lines(&mut buffer, &input, Rect::new(0, 0, 40, 8));
    assert!(lines.iter().any(|l| l.contains("step 1")));
    assert!(lines.iter().any(|l| l.contains("step 2")));
}

#[test]
fn assistant_text_renders_without_header() {
    let mut buffer = Buffer::new();
    buffer.append_assistant_delta("hi there");
    buffer.finish_streaming();
    let input = InputState::new();
    let lines = snapshot_lines(&mut buffer, &input, Rect::new(0, 0, 40, 8));
    assert!(lines.iter().any(|l| l.contains("hi there")));
    // No `[assistant]` header tag.
    assert!(!lines.iter().any(|l| l.contains("[assistant]")));
}

#[test]
fn user_block_is_one_band_row_with_a_prompt_glyph() {
    let mut buffer = Buffer::new();
    buffer.push_user("hello");
    let input = InputState::new();
    let lines = snapshot_lines(&mut buffer, &input, Rect::new(0, 0, 40, 8));
    let band: Vec<&String> = lines.iter().filter(|l| l.starts_with('\u{258e}')).collect();
    assert_eq!(band.len(), 1, "no pad rows: {lines:?}");
    assert!(band[0].contains("> hello"), "{lines:?}");
}

#[test]
fn streaming_tool_call_reads_verb_first() {
    let mut buffer = Buffer::new();
    buffer.push_tool_call("c1", "shell", json!({"command": "ls -la"}));
    let input = InputState::new();
    let lines = snapshot_lines(&mut buffer, &input, Rect::new(0, 0, 60, 8));
    let header = lines
        .iter()
        .find(|l| l.contains("ls -la"))
        .expect("tool header present");
    assert!(header.contains("\u{2022} Run ls -la"), "{header:?}");
    assert!(
        !header.contains('[') && !header.contains("shell"),
        "{header:?}"
    );
    assert!(!lines.iter().any(|l| l.contains("\"command\"")));
}

#[test]
fn unfolded_tool_call_without_output_shows_its_arguments() {
    let mut buffer = Buffer::new();
    buffer.push_tool_call("c1", "shell", json!({"command": "ls -la"}));
    assert!(buffer.toggle_fold(0));
    let input = InputState::new();
    let lines = snapshot_lines(&mut buffer, &input, Rect::new(0, 0, 60, 12));
    assert!(
        lines.iter().any(|l| l.contains("command  ls -la")),
        "{lines:?}"
    );
}

#[test]
fn failed_shell_pair_shows_a_cross_and_the_exit_code() {
    let mut buffer = Buffer::new();
    buffer.push_tool_call("c1", "shell", json!({"command": "false"}));
    buffer.push_tool_result("c1", "stderr:\nnope\nexit: 1", true);
    let input = InputState::new();
    let lines = snapshot_lines(&mut buffer, &input, Rect::new(0, 0, 80, 12));
    let header = lines
        .iter()
        .find(|l| l.contains("Ran false"))
        .expect("merged tool header");
    assert!(header.contains("\u{2717} Ran false"), "{header:?}");
    assert!(header.contains("exit 1"), "{header:?}");
    assert!(
        lines.iter().any(|l| l.trim_end().ends_with("nope")),
        "{lines:?}"
    );
    assert!(!lines.iter().any(|l| l.contains("stderr:")), "{lines:?}");
}

#[test]
fn folded_read_pair_is_a_single_header_row() {
    let mut buffer = Buffer::new();
    buffer.push_tool_call("c1", "read", json!({"path": "README.md"}));
    buffer.push_tool_result("c1", "first line of file\nsecond line", false);
    let input = InputState::new();
    let lines = snapshot_lines(&mut buffer, &input, Rect::new(0, 0, 90, 12));
    assert!(
        lines.iter().any(|l| l.contains("\u{2022} Read README.md")),
        "{lines:?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains("first line of file")),
        "{lines:?}"
    );
}

#[test]
fn unfolded_pair_shows_its_output() {
    let mut buffer = Buffer::new();
    buffer.push_tool_call("c1", "ls", json!({"path": "."}));
    buffer.push_tool_result_with_duration("c1", "a.rs\nb.rs\nc.rs", false, Some(120));
    assert!(buffer.toggle_fold(0));
    let input = InputState::new();
    let lines = snapshot_lines(&mut buffer, &input, Rect::new(0, 0, 60, 16));
    let header = lines
        .iter()
        .find(|l| l.contains("Listed ."))
        .expect("unfolded ls header");
    assert!(header.ends_with("0.1s"), "{header:?}");
    assert!(lines.iter().any(|l| l.contains("a.rs")));
    assert!(lines.iter().any(|l| l.contains("c.rs")));
}

#[test]
fn folded_running_rows_show_their_latest_progress_line() {
    for (name, input) in [
        ("fix__slow", json!({"steps": 3})),
        ("read", json!({"path": "big.log"})),
        ("edit", json!({"path": "a.rs", "old": "a", "new": "b"})),
    ] {
        let mut buffer = Buffer::new();
        buffer.push_tool_call("c1", name, input);
        buffer.set_tool_phase("c1", tool_view::ToolPhase::Running);
        buffer.set_tool_progress("c1", "step 1 of 3\nstep 2 of 3");
        let input = InputState::new();
        let lines = snapshot_lines(&mut buffer, &input, Rect::new(0, 0, 60, 12));
        assert!(
            lines.iter().any(|l| l.trim_end().ends_with("step 2 of 3")),
            "{name}: {lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l.contains("step 1 of 3")),
            "{name}: {lines:?}"
        );
    }
}

#[test]
fn a_single_paragraph_streamed_word_by_word_shows_every_word() {
    let mut buffer = Buffer::new();
    let input = InputState::new();
    let area = Rect::new(0, 0, 60, 12);
    let mut text = String::new();
    for word in ["Streaming", " one", " word", " at", " a", " time"] {
        buffer.append_assistant_delta(word);
        text.push_str(word);
        let _ = snapshot_lines(&mut buffer, &input, area);
        // Age the pending-edit marker past the reparse throttle so the
        // next snapshot must rebuild, without a wall-clock sleep. The
        // test then passes whatever the throttle constant is. Arithmetic
        // aging is a silent no-op when the platform Instant epoch sits
        // within `age` of now (a freshly booted Windows runner), so fall
        // back to letting the real throttle elapse before failing.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            buffer.age_stream_edits(Duration::from_secs(3600));
            let lines = snapshot_lines(&mut buffer, &input, area);
            if lines.iter().any(|l| l.contains(&text)) {
                break;
            }
            assert!(Instant::now() < deadline, "{text:?}\n{lines:#?}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn push_read(buffer: &mut Buffer, id: &str, path: &str) {
    buffer.push_tool_call(id, "read", json!({"path": path}));
    buffer.push_tool_result(id, "contents", false);
}

#[test]
fn consecutive_reads_render_as_one_explored_row() {
    let mut buffer = Buffer::new();
    push_read(&mut buffer, "r1", "a.rs");
    push_read(&mut buffer, "r2", "b.rs");
    push_read(&mut buffer, "r3", "c.rs");
    buffer.push_tool_call("b1", "shell", json!({"command": "cargo test"}));
    buffer.push_tool_result("b1", "stdout:\nok\nexit: 0", false);
    let input = InputState::new();
    let lines = snapshot_lines(&mut buffer, &input, Rect::new(0, 0, 60, 20));
    let explored = lines
        .iter()
        .position(|l| l.contains("\u{2022} Explored 3 files"))
        .unwrap_or_else(|| panic!("{lines:?}"));
    assert!(
        lines[explored + 1].contains("Read a.rs, b.rs, c.rs"),
        "{lines:?}"
    );
    assert!(lines[explored + 2].contains("Ran cargo test"), "{lines:?}");
    assert_eq!(lines.iter().filter(|l| l.contains("Read ")).count(), 1);

    assert_eq!(buffer.block_virtual_rows(0), Some((0, 2)));
    assert_eq!(buffer.block_virtual_rows(2), None, "members are hidden");
    assert_eq!(buffer.block_virtual_rows(6), Some((2, 4)));

    assert!(buffer.toggle_fold(0), "unfolding the head splits the group");
    let lines = snapshot_lines(&mut buffer, &input, Rect::new(0, 0, 60, 30));
    for path in ["a.rs", "b.rs", "c.rs"] {
        assert!(
            lines.iter().any(|l| l.contains(&format!("Read {path}"))),
            "{lines:?}"
        );
    }
    let rows: Vec<(usize, usize)> = [0, 2, 4, 6]
        .iter()
        .map(|&i| buffer.block_virtual_rows(i).expect("painted"))
        .collect();
    for pair in rows.windows(2) {
        assert_eq!(pair[1].0, pair[0].1, "heights sum: {rows:?}");
    }

    assert!(buffer.toggle_fold(0), "folding the head regroups");
    snapshot_lines(&mut buffer, &input, Rect::new(0, 0, 60, 20));
    assert_eq!(buffer.block_virtual_rows(0), Some((0, 2)));
    assert_eq!(buffer.block_virtual_rows(6), Some((2, 4)));
}

#[test]
fn a_text_block_between_reads_prevents_grouping() {
    let mut buffer = Buffer::new();
    push_read(&mut buffer, "r1", "a.rs");
    buffer.append_assistant_delta("now the next one");
    buffer.finish_streaming();
    push_read(&mut buffer, "r2", "b.rs");
    let input = InputState::new();
    let lines = snapshot_lines(&mut buffer, &input, Rect::new(0, 0, 60, 20));
    assert!(!lines.iter().any(|l| l.contains("Explored")), "{lines:?}");
    assert!(lines.iter().any(|l| l.contains("Read a.rs")));
    assert!(lines.iter().any(|l| l.contains("Read b.rs")));
}

#[test]
fn a_read_finishing_after_a_group_joins_it() {
    let mut buffer = Buffer::new();
    push_read(&mut buffer, "r1", "a.rs");
    push_read(&mut buffer, "r2", "b.rs");
    buffer.push_tool_call("r3", "read", json!({"path": "c.rs"}));
    buffer.set_tool_phase("r3", tool_view::ToolPhase::Running);
    let input = InputState::new();
    let lines = snapshot_lines(&mut buffer, &input, Rect::new(0, 0, 60, 20));
    assert!(
        lines.iter().any(|l| l.contains("Explored 2 files")),
        "{lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("Reading c.rs")),
        "{lines:?}"
    );
    buffer.push_tool_result("r3", "contents", false);
    let lines = snapshot_lines(&mut buffer, &input, Rect::new(0, 0, 60, 20));
    assert!(
        lines.iter().any(|l| l.contains("Explored 3 files")),
        "{lines:?}"
    );
    assert_eq!(buffer.block_virtual_rows(0), Some((0, 2)));
}

#[test]
fn no_rendered_row_carries_a_bracket_tag() {
    let mut buffer = Buffer::new();
    buffer.append_thinking_delta("hmm");
    buffer.finish_streaming();
    buffer.push_custom("kage:error", "boom", false);
    buffer.push_custom("kage:shell", "$ ls\na.rs\n(exit code 0)", false);
    buffer.push_custom(
        "kage:truncated",
        "reply hit the max output token limit",
        false,
    );
    let input = InputState::new();
    let lines = snapshot_lines(&mut buffer, &input, Rect::new(0, 0, 60, 20));
    let text = lines.join("\n");
    for tag in [
        "[thinking]",
        "[error]",
        "[shell]",
        "[truncated]",
        "(exit code 0)",
    ] {
        assert!(!text.contains(tag), "{tag} in {lines:?}");
    }
    assert!(
        text.contains("\u{2717} boom") && text.contains("$ ls"),
        "{lines:?}"
    );
}

#[test]
fn fences_render_a_language_label_and_no_backticks() {
    let mut buffer = Buffer::new();
    buffer.append_assistant_delta("See:\n\n```rust\nlet x = 1;\n```\n");
    buffer.finish_streaming();
    let input = InputState::new();
    let lines = snapshot_lines(&mut buffer, &input, Rect::new(0, 0, 60, 12));
    assert!(lines.iter().any(|l| l.trim() == "rust"), "{lines:?}");
    assert!(
        lines.iter().any(|l| l.contains("    let x = 1;")),
        "{lines:?}"
    );
    assert!(!lines.iter().any(|l| l.contains("```")), "{lines:?}");
}

#[test]
fn the_fallback_focus_paints_no_rule() {
    let mut buffer = Buffer::new();
    buffer.append_assistant_delta("hello");
    buffer.finish_streaming();
    assert_eq!(buffer.focus(), None);
    assert_eq!(buffer.effective_focus(), Some(0));
    let input = InputState::new();
    let lines = snapshot_lines(&mut buffer, &input, Rect::new(0, 0, 40, 6));
    let row = lines.iter().find(|l| l.contains("hello")).unwrap();
    assert!(row.starts_with("  hello"), "{row:?}");

    buffer.set_focus(Some(0));
    let lines = snapshot_lines(&mut buffer, &input, Rect::new(0, 0, 40, 6));
    let row = lines.iter().find(|l| l.contains("hello")).unwrap();
    assert!(row.starts_with(Emphasis::Focused.rule_glyph()), "{row:?}");
}

#[test]
fn toggling_either_half_of_a_pair_flips_both() {
    let mut buffer = Buffer::new();
    buffer.push_tool_call("c1", "ls", json!({"path": "."}));
    buffer.push_tool_result("c1", "a", false);
    assert!(matches!(
        buffer.blocks()[0].as_ref(),
        Block::ToolCall { folded: true, .. }
    ));
    assert!(matches!(
        buffer.blocks()[1].as_ref(),
        Block::ToolResult { folded: true, .. }
    ));
    // Toggle the result; the call should flip too.
    assert!(buffer.toggle_fold(1));
    assert!(matches!(
        buffer.blocks()[0].as_ref(),
        Block::ToolCall { folded: false, .. }
    ));
    assert!(matches!(
        buffer.blocks()[1].as_ref(),
        Block::ToolResult { folded: false, .. }
    ));
}

#[test]
fn token_counts_scale_to_k_m_b_trimmed() {
    assert_eq!(super::format_token_count(999), "999");
    assert_eq!(super::format_token_count(1_000), "1k");
    assert_eq!(super::format_token_count(1_160), "1.16k");
    assert_eq!(super::format_token_count(78_700), "78.7k");
    assert_eq!(super::format_token_count(200_000), "200k");
    assert_eq!(super::format_token_count(1_500_000), "1.5M");
    assert_eq!(super::format_token_count(21_000_000), "21M");
    assert_eq!(super::format_token_count(200_000_000), "200M");
    assert_eq!(super::format_token_count(2_000_000_000), "2B");
}

/// Mirror what [`super::render_input`] does to derive the draft's
/// text rect (`body_area`) from a full input region rect: the rules
/// take the top and bottom rows, the prompt column the left
/// [`super::INPUT_GLYPH_WIDTH`] cells.
fn body_area_for(region: Rect) -> Rect {
    Rect::new(
        region.x + super::INPUT_GLYPH_WIDTH,
        region.y + 1,
        region.width.saturating_sub(super::INPUT_GLYPH_WIDTH),
        region.height.saturating_sub(2),
    )
}

#[test]
fn cursor_position_advances_with_typed_text() {
    let region = Rect::new(0, 4, 40, 4);
    let body = body_area_for(region);
    let mut input = InputState::new();
    // Default mode is Insert; no need to press 'i'.
    for c in "hello".chars() {
        input.handle_key(ratatui::crossterm::event::KeyEvent::new(
            ratatui::crossterm::event::KeyCode::Char(c),
            ratatui::crossterm::event::KeyModifiers::NONE,
        ));
    }
    let pos = super::input_cursor_position(&input, body, 0).unwrap();
    // body.x = 3 (prompt column), body.y = 5 (below the top rule);
    // 5 chars typed -> col 8, row 5.
    assert_eq!(pos, (body.x + 5, body.y));
}

#[test]
fn cursor_position_walks_to_next_row_on_newline() {
    let region = Rect::new(0, 0, 20, 5);
    let body = body_area_for(region);
    let mut input = InputState::new();
    // Default mode is Insert; no need to press 'i'.
    // Paste pre-builds multi-line content cheaply.
    input.paste("ab\ncd");
    let pos = super::input_cursor_position(&input, body, 0).unwrap();
    // Second logical row, 2 chars in -> col body.x + 2, row body.y + 1.
    assert_eq!(pos, (body.x + 2, body.y + 1));
}

#[test]
fn input_scrolls_when_cursor_row_exceeds_visible_height() {
    // Region height = 5 rows -> 2 chrome + 3 content rows.
    let region = Rect::new(0, 0, 40, 5);
    let body = body_area_for(region);
    assert_eq!(body.height, 3);
    let mut input = InputState::new();
    // Default mode is Insert; no need to press 'i'.
    // Five rows of content; cursor lands on row 4 (last line).
    input.paste("a\nb\nc\nd\ne");
    let off = super::input_scroll_offset(&input, body);
    // cursor_row=4, max_visible_row=2 -> scroll by 2.
    assert_eq!(off, 2);
    // Cursor renders on the last visible row of the body area.
    let pos = super::input_cursor_position(&input, body, off).unwrap();
    assert_eq!(pos.1, body.y + body.height - 1);
}

#[test]
fn input_does_not_scroll_when_text_fits() {
    let region = Rect::new(0, 0, 40, 6);
    let body = body_area_for(region);
    let mut input = InputState::new();
    // Default mode is Insert; no need to press 'i'.
    input.paste("a\nb\nc");
    assert_eq!(super::input_scroll_offset(&input, body), 0);
}

const RULE: char = '\u{2500}';

fn vim_normal() -> InputState {
    let mut input = InputState::new();
    input.handle_key(ratatui::crossterm::event::KeyEvent::from(
        ratatui::crossterm::event::KeyCode::Esc,
    ));
    input
}

#[test]
fn the_top_rule_carries_the_vim_mode_and_nothing_in_modeless() {
    let area = Rect::new(0, 0, 60, 8);
    let vim = snapshot_lines(&mut Buffer::new(), &vim_normal(), area);
    let top = &vim[area.height as usize - 4];
    assert!(
        top.starts_with("\u{2500}\u{2500} NORMAL \u{2500}"),
        "{vim:#?}"
    );
    let mut modeless = InputState::new();
    modeless.set_modeless(true);
    let rows = snapshot_lines(&mut Buffer::new(), &modeless, area);
    let top = &rows[area.height as usize - 4];
    assert!(top.chars().all(|c| c == RULE), "{rows:#?}");
    assert_eq!(top.chars().count(), 60);
}

#[test]
fn the_input_has_rules_and_no_side_borders() {
    let area = Rect::new(0, 0, 40, 8);
    let rows = snapshot_lines(&mut Buffer::new(), &vim_normal(), area);
    let [top, body, bottom] = [4, 3, 2].map(|back| &rows[area.height as usize - back]);
    assert!(top.starts_with(RULE), "{rows:#?}");
    assert!(bottom.chars().all(|c| c == RULE), "{rows:#?}");
    assert_eq!(body, " > Press i to type");
    assert!(
        rows.iter().all(|r| !r.contains(['\u{2502}', '|'])),
        "{rows:#?}"
    );
}

#[test]
fn the_glyph_stays_on_the_first_logical_line_and_the_rule_counts_hidden_lines() {
    let area = Rect::new(0, 0, 40, 16);
    let mut input = InputState::new();
    let lines = |range: std::ops::RangeInclusive<u32>| {
        range
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    input.paste(&lines(1..=6));
    input.paste(&format!("\n{}", lines(7..=12)));
    let rows = snapshot_lines(&mut Buffer::new(), &input, area);
    let footer = rows.len() - 1;
    let bottom = &rows[footer - 1];
    let body = &rows[footer - 1 - usize::from(crate::layout::INPUT_CONTENT_MAX_LINES)..footer - 1];
    assert_eq!(body.last().unwrap(), "   line 12", "{rows:#?}");
    assert!(body.iter().all(|r| !r.contains('>')), "{rows:#?}");
    assert!(
        bottom.ends_with("4 more lines above \u{2500}\u{2500}"),
        "{rows:#?}"
    );

    let mut short = InputState::new();
    short.paste("one\ntwo");
    let rows = snapshot_lines(&mut Buffer::new(), &short, area);
    assert!(rows.iter().any(|r| r == " > one"), "{rows:#?}");
    assert!(rows.iter().any(|r| r == "   two"), "{rows:#?}");
    assert!(rows[rows.len() - 2].chars().all(|c| c == RULE), "{rows:#?}");
}

#[test]
fn the_shell_look_swaps_the_glyph_and_the_placeholder() {
    let mut input = InputState::new();
    input.handle_key(ratatui::crossterm::event::KeyEvent::from(
        ratatui::crossterm::event::KeyCode::Char('!'),
    ));
    assert!(input.shell_armed());
    let rows = snapshot_lines(&mut Buffer::new(), &input, Rect::new(0, 0, 70, 8));
    assert!(
        rows.iter()
            .any(|r| r == " ! Run a shell command (backspace leaves shell mode)"),
        "{rows:#?}"
    );
    assert!(
        rows.iter().any(|r| r.contains("\u{2500} shell \u{2500}")),
        "{rows:#?}"
    );
}

#[test]
fn the_header_collapses_without_a_title_and_returns_with_one() {
    let area = Rect::new(0, 0, 40, 8);
    let mut buffer = Buffer::new();
    buffer.push_user("hello");
    let input = InputState::new();
    let rows = snapshot_frame(&mut buffer, &input, None, &StatusCtx::default(), None, area);
    assert!(rows[0].contains("hello"), "{rows:#?}");
    let status = StatusCtx {
        title: Some("fix the parser"),
        ..StatusCtx::default()
    };
    let rows = snapshot_frame(&mut buffer, &input, None, &status, None, area);
    assert!(rows[0].contains("fix the parser"), "{rows:#?}");
    assert!(rows[1].contains("hello"), "{rows:#?}");
}

#[test]
fn the_activity_row_sits_above_the_input_only_while_it_has_text() {
    let area = Rect::new(0, 0, 60, 10);
    let input = InputState::new();
    let status = StatusCtx {
        activity: Some("Running cargo test (3s, ctrl+c to interrupt)"),
        ..StatusCtx::default()
    };
    let mut buffer = Buffer::new();
    buffer.push_user("hi");
    let rows = snapshot_frame(&mut buffer, &input, None, &status, None, area);
    assert_eq!(
        rows[5].replace(['\u{25cb}', '\u{25cf}'], "/"),
        "  / Running cargo test (3s, ctrl+c to interrupt)",
        "{rows:#?}"
    );
    assert!(rows[6].starts_with(RULE), "{rows:#?}");
    let rows = snapshot_lines(&mut buffer, &input, area);
    assert!(rows[5].is_empty(), "{rows:#?}");
}

#[test]
fn the_footer_rows_hold_the_hint_the_model_and_the_session_facts() {
    let usage = SessionUsage {
        model: "fake:m".into(),
        input_tokens: 14_000,
        current_context: 24_000,
        context_window: 200_000,
        ..SessionUsage::default()
    };
    let agents = [AgentRow {
        background: true,
        ..agent_row(1, "general", "run the tests", "", 3)
    }];
    let status = StatusCtx {
        model: Some("Fake"),
        hint: Some("? for shortcuts"),
        cwd: Some("/work/kage"),
        branch: Some("main"),
        agents: &agents,
        ..StatusCtx::default()
    };
    let rows = snapshot_frame(
        &mut Buffer::new(),
        &InputState::new(),
        None,
        &status,
        Some(&usage),
        Rect::new(0, 0, 100, 8),
    );
    let [top, bottom] = &rows[rows.len() - 2..] else {
        unreachable!()
    };
    assert!(top.starts_with("  ? for shortcuts"), "{top:?}");
    assert!(top.ends_with("Fake \u{B7} 12% ctx (24k/200k)"), "{top:?}");
    assert!(
        bottom.starts_with(" ask when needed \u{B7} 1 agent (1 bg)"),
        "{bottom:?}"
    );
    assert!(
        bottom.ends_with("/work/kage (main) \u{B7} in 14k out 0"),
        "{bottom:?}"
    );
}

fn footer_with_rows(rows: Vec<kage_plugin::SlotSpec>) -> kage_plugin::SlotSpecs {
    let mut slots = kage_plugin::SlotSpecs::default();
    slots.set_spec(
        kage_plugin::SlotName::Footer,
        Some(std::sync::Arc::new(kage_plugin::SlotSpec {
            rows,
            ..kage_plugin::SlotSpec::default()
        })),
    );
    slots
}

#[test]
fn the_footer_shows_swarm_and_background_task_segments() {
    let usage = SessionUsage {
        model: "fake:m".into(),
        swarm: true,
        shells: 2,
        ..SessionUsage::default()
    };
    let status = StatusCtx {
        slots: footer_with_rows(vec![kage_plugin::SlotSpec {
            left: vec![
                kage_plugin::SlotItem::Builtin("permission"),
                kage_plugin::SlotItem::Builtin("swarm"),
                kage_plugin::SlotItem::Builtin("tasks"),
            ],
            sep: " \u{b7} ".into(),
            ..kage_plugin::SlotSpec::default()
        }]),
        ..StatusCtx::default()
    };
    let rows = snapshot_frame(
        &mut Buffer::new(),
        &InputState::new(),
        None,
        &status,
        Some(&usage),
        Rect::new(0, 0, 100, 8),
    );
    let footer = rows.last().unwrap();
    assert!(
        footer.starts_with(" ask when needed \u{b7} swarm \u{b7} 2 shells"),
        "{footer:?}"
    );

    // Off and none: the segments vanish instead of leaving gaps.
    let usage = SessionUsage {
        model: "fake:m".into(),
        ..SessionUsage::default()
    };
    let rows = snapshot_frame(
        &mut Buffer::new(),
        &InputState::new(),
        None,
        &status,
        Some(&usage),
        Rect::new(0, 0, 100, 8),
    );
    assert_eq!(rows.last().unwrap(), " ask when needed");
}

#[test]
fn the_footer_stacks_its_spec_rows_and_heights_follow() {
    let usage = SessionUsage {
        model: "fake:m".into(),
        input_tokens: 14_000,
        current_context: 24_000,
        context_window: 200_000,
        ..SessionUsage::default()
    };
    let status = StatusCtx {
        model: Some("Fake"),
        slots: footer_with_rows(vec![
            kage_plugin::SlotSpec {
                left: vec![kage_plugin::SlotItem::Builtin("permission")],
                ..kage_plugin::SlotSpec::default()
            },
            kage_plugin::SlotSpec {
                right: vec![
                    kage_plugin::SlotItem::Builtin("model"),
                    kage_plugin::SlotItem::Builtin("context"),
                ],
                sep: " | ".into(),
                ..kage_plugin::SlotSpec::default()
            },
        ]),
        ..StatusCtx::default()
    };
    let rows = snapshot_frame(
        &mut Buffer::new(),
        &InputState::new(),
        None,
        &status,
        Some(&usage),
        Rect::new(0, 0, 100, 8),
    );
    assert_eq!(rows[rows.len() - 2], " ask when needed");
    assert!(
        rows[rows.len() - 1].ends_with("Fake | 12% ctx (24k/200k)"),
        "{:?}",
        rows[rows.len() - 1]
    );
}

#[test]
fn an_empty_footer_row_collapses_but_one_row_always_stays() {
    let sources = |slots: kage_plugin::SlotSpecs| {
        let status = StatusCtx {
            slots,
            ..StatusCtx::default()
        };
        let src = slot::Sources::new(&status, None, &InputState::new(), 80);
        slot::footer_height(&src)
    };
    assert_eq!(sources(kage_plugin::SlotSpecs::default()), 1);
    assert_eq!(sources(footer_with_rows(vec![])), 1);
    assert_eq!(
        sources(footer_with_rows(vec![kage_plugin::SlotSpec {
            left: vec![kage_plugin::SlotItem::Builtin("version")],
            ..kage_plugin::SlotSpec::default()
        }])),
        1,
        "rows that paint nothing collapse to the one blank strip"
    );
}

#[test]
fn the_colon_line_holds_the_top_footer_row_and_the_rows_stay_below() {
    let usage = SessionUsage {
        model: "fake:m".into(),
        ..SessionUsage::default()
    };
    let status = StatusCtx {
        slots: footer_with_rows(vec![
            kage_plugin::SlotSpec {
                left: vec![kage_plugin::SlotItem::Builtin("version")],
                ..kage_plugin::SlotSpec::default()
            },
            kage_plugin::SlotSpec {
                left: vec![kage_plugin::SlotItem::Builtin("permission")],
                ..kage_plugin::SlotSpec::default()
            },
        ]),
        ..StatusCtx::default()
    };
    let empty = crate::cmdparse::Completions::default();
    let cl = CommandLine::for_test("quit", empty, true, None);
    let rows = snapshot_frame(
        &mut Buffer::new(),
        &InputState::new(),
        Some(&cl),
        &status,
        Some(&usage),
        Rect::new(0, 0, 60, 8),
    );
    assert_eq!(rows[rows.len() - 2], ":quit");
    assert_eq!(rows[rows.len() - 1], " ask when needed");
}

#[test]
fn the_colon_and_search_lines_paint_on_the_footer_row() {
    let area = Rect::new(0, 0, 40, 8);
    let empty = crate::cmdparse::Completions::default();
    let cl = CommandLine::for_test("quit", empty.clone(), true, None);
    let rows = snapshot_with_cmdline(&cl, area);
    assert_eq!(rows.last().unwrap(), ":quit", "{rows:#?}");
    let search = CommandLine::for_test("needle", empty, true, None);
    let status = StatusCtx {
        search_line: Some(&search),
        search_match_count: Some((2, 5)),
        ..StatusCtx::default()
    };
    let rows = snapshot_frame(
        &mut Buffer::new(),
        &InputState::new(),
        None,
        &status,
        None,
        area,
    );
    let footer = rows.last().unwrap();
    assert!(footer.starts_with("/needle"), "{rows:#?}");
    assert!(footer.ends_with("match 2/5"), "{rows:#?}");
}

#[test]
fn a_cursor_forced_mid_char_renders_without_panicking() {
    let cl = CommandLine::for_test_with_cursor("a\u{e9}b", 2);
    let rows = snapshot_with_cmdline(&cl, Rect::new(0, 0, 40, 8));
    assert_eq!(rows.last().unwrap(), ":a\u{e9}b", "{rows:#?}");
}

/// Paint a frame with `cmdline` open over a conversation, so no start
/// card competes with the popup.
fn snapshot_with_cmdline(cmdline: &CommandLine, area: Rect) -> Vec<String> {
    let mut buffer = Buffer::new();
    buffer.push_user("hi");
    snapshot_frame(
        &mut buffer,
        &InputState::new(),
        Some(cmdline),
        &StatusCtx::default(),
        None,
        area,
    )
}

fn cell_bg_at(cmdline: &CommandLine, area: Rect, x: u16, y: u16) -> Color {
    let backend = TestBackend::new(area.width, area.height);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut buffer = Buffer::new();
    let input = InputState::new();
    let mut captured: std::collections::BTreeMap<usize, Vec<CapturedCell>> =
        std::collections::BTreeMap::new();
    let status = StatusCtx::default();
    terminal
        .draw(|frame| {
            let heights = chrome_heights(
                &status,
                None,
                &input,
                frame.area().width,
                &todo::TodoStrip::default(),
            );
            let regions = crate::layout::split(frame.area(), heights);
            render(
                frame,
                regions,
                &mut buffer,
                &input,
                Some(cmdline),
                &status,
                None,
                &mut captured,
                None,
                &[],
            );
        })
        .unwrap();
    terminal.backend().buffer()[(x, y)].bg
}

fn completion(value: &str, description: Option<&str>) -> crate::cmdparse::Completion {
    crate::cmdparse::Completion {
        value: value.to_owned(),
        hint: None,
        description: description.map(str::to_owned),
        replace_range: 0..0,
    }
}

#[test]
fn popup_paints_nothing_with_zero_completions() {
    let empty = crate::cmdparse::Completions::default();
    let cl = CommandLine::for_test("", empty, true, None);
    let lines = snapshot_with_cmdline(&cl, Rect::new(0, 0, 40, 12));
    // Row 1 is the first row below the status; it should be blank
    // (or at least not contain any completion text we did not give).
    assert!(
        lines[1].chars().all(|c| c == ' '),
        "row 1 should be blank, got {:?}",
        lines[1]
    );
}

#[test]
fn popup_paints_single_item_text() {
    let completions = crate::cmdparse::Completions {
        items: vec![completion("model", Some("switch model"))],
        anchor: 0,
    };
    let cl = CommandLine::for_test("m", completions, true, None);
    let lines = snapshot_with_cmdline(&cl, Rect::new(0, 0, 40, 12));
    let popup_row = lines
        .iter()
        .skip(1)
        .find(|l| l.contains("model"))
        .expect("popup row containing 'model'");
    assert!(popup_row.contains("switch model"), "got {popup_row:?}");
}

#[test]
fn popup_paints_many_items_and_highlights_selected() {
    let completions = crate::cmdparse::Completions {
        items: vec![
            completion("model", Some("switch model")),
            completion("mouse", Some("toggle mouse")),
        ],
        anchor: 0,
    };
    let cl = CommandLine::for_test("mo", completions, true, Some(1));
    let area = Rect::new(0, 0, 50, 12);
    let lines = snapshot_with_cmdline(&cl, area);
    assert!(lines.iter().any(|l| l.contains("model")), "{lines:#?}");
    assert!(lines.iter().any(|l| l.contains("mouse")), "{lines:#?}");
    // The popup sits directly above the footer row: the selected row
    // (index 1) at y=10 has the overlay selection bg, the unselected
    // row at y=9 does not.
    let sel = crate::theme::current().overlay_selected_bg;
    assert_eq!(
        cell_bg_at(&cl, area, 3, 10),
        sel,
        "selected row bg should be the overlay selection color"
    );
    assert_ne!(
        cell_bg_at(&cl, area, 3, 9),
        sel,
        "unselected row bg should not be the overlay selection color"
    );
}

#[test]
fn popup_scrolls_to_keep_selected_in_view() {
    let items: Vec<crate::cmdparse::Completion> = (0..12)
        .map(|i| completion(&format!("cmd{i:02}"), None))
        .collect();
    let completions = crate::cmdparse::Completions { items, anchor: 0 };

    // selected near top: window starts at 0, no "above" indicator,
    // "below" indicator shows the off-screen tail.
    let cl_top = CommandLine::for_test("c", completions.clone(), true, Some(2));
    let lines = snapshot_with_cmdline(&cl_top, Rect::new(0, 0, 40, 20));
    assert!(
        lines.iter().any(|l| l.contains("cmd00")),
        "cmd00 should be visible near the top, got {lines:#?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("more below")),
        "expected 'more below' indicator, got {lines:#?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains("more above")),
        "no 'more above' indicator near top, got {lines:#?}"
    );

    // selected past the bottom: window slides so selected is the last
    // visible row, both indicators present.
    let cl_bottom = CommandLine::for_test("c", completions.clone(), true, Some(10));
    let lines = snapshot_with_cmdline(&cl_bottom, Rect::new(0, 0, 40, 20));
    assert!(
        lines.iter().any(|l| l.contains("cmd10")),
        "selected cmd10 must be visible, got {lines:#?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains("cmd00")),
        "cmd00 should have scrolled out of view, got {lines:#?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("more above")),
        "expected 'more above' indicator, got {lines:#?}"
    );
}

#[test]
fn popup_truncates_description_in_narrow_viewport() {
    let completions = crate::cmdparse::Completions {
        items: vec![completion(
            "model",
            Some("switch to a provider/model identifier from the catalog"),
        )],
        anchor: 0,
    };
    let cl = CommandLine::for_test("m", completions, true, None);
    // 28 cells total: leading "  " + "model" (5) + "  " + ~19 desc chars + ellipsis.
    let lines = snapshot_with_cmdline(&cl, Rect::new(0, 0, 28, 8));
    let popup_row = lines
        .iter()
        .skip(1)
        .find(|l| l.contains("model"))
        .expect("popup row");
    assert!(
        popup_row.contains('\u{2026}'),
        "expected ellipsis in narrow row, got {popup_row:?}",
    );
    assert!(
        !popup_row.contains("catalog"),
        "narrow viewport should drop the tail of the description, got {popup_row:?}",
    );
}

// --- Inline error rendering tests ---

#[test]
fn error_line_shows_marker_and_message() {
    let cl = CommandLine::for_test_with_error(
        "mouse mayb",
        "argument `state` must be one of on|off|toggle",
    );
    let lines = snapshot_with_cmdline(&cl, Rect::new(0, 0, 60, 12));
    // The footer row holds ":mouse mayb" and the row above it the
    // error marker and message.
    assert!(
        lines[11].contains("mouse mayb"),
        "footer row should show typed text, got {:?}",
        lines[11]
    );
    assert!(
        lines[10].contains('!'),
        "error row should contain the error marker, got {:?}",
        lines[10]
    );
    assert!(
        lines[10].contains("must be one of"),
        "error row should contain the error message, got {:?}",
        lines[10]
    );
}

#[test]
fn error_line_suppresses_popup() {
    let completions = crate::cmdparse::Completions {
        items: vec![
            completion("model", Some("switch model")),
            completion("mouse", Some("toggle mouse")),
        ],
        anchor: 0,
    };
    // Error is set even though completions are populated.
    let mut cl = CommandLine::for_test("mo", completions, true, None);
    cl.set_error("fix your input");
    let lines = snapshot_with_cmdline(&cl, Rect::new(0, 0, 50, 12));
    // The popup should be suppressed; only the error row appears.
    assert!(
        !lines.iter().any(|l| l.contains("switch model")),
        "popup should be suppressed when error is active, got {lines:#?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("fix your input")),
        "error message should be visible, got {lines:#?}"
    );
}

#[test]
fn error_line_truncates_in_narrow_viewport() {
    let long_msg = "this is a very long error message that should definitely be truncated when the viewport is narrow";
    let cl = CommandLine::for_test_with_error("x", long_msg);
    let lines = snapshot_with_cmdline(&cl, Rect::new(0, 0, 30, 8));
    let error_row = &lines[6];
    assert!(
        error_row.contains('\u{2026}'),
        "long error should be truncated with ellipsis, got {error_row:?}"
    );
}

fn modeline_rows(usage: Option<&SessionUsage>, width: u16) -> Vec<String> {
    let backend = TestBackend::new(width, 2);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            let status = StatusCtx::default();
            let input = InputState::new();
            let sources = slot::Sources::new(&status, usage, &input, width);
            slot::render_footer(frame, frame.area(), &sources);
        })
        .unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .chunks(width as usize)
        .map(|row| {
            row.iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect()
}

#[test]
fn modeline_paints_permission_pill_when_overridden() {
    let usage = SessionUsage {
        model: "test:model".to_owned(),
        permission_mode: Some(kage_core::permissions::PermissionAction::Ask),
        ..SessionUsage::default()
    };
    let rows = modeline_rows(Some(&usage), 60);
    assert!(
        rows.iter().any(|r| r.contains("ask mode")),
        "modeline should show the ask override, got {rows:?}"
    );
}

#[test]
fn modeline_hides_permission_pill_without_override() {
    let usage = SessionUsage {
        model: "test:model".to_owned(),
        ..SessionUsage::default()
    };
    let rows = modeline_rows(Some(&usage), 60);
    assert!(
        rows.iter().all(|r| !r.contains(" mode")),
        "no pill without an override, got {rows:?}"
    );
}

// --- Start card ---

const TIP: &str =
    "   Tip: Press tab to queue a message while kage works, or enter to steer the running turn.";

fn start_info(sessions: usize) -> StartInfo {
    let titles = [
        "fix the parser tests",
        "refactor slot painting",
        "list files please",
        "fourth session",
        "fifth session",
    ];
    StartInfo {
        sessions: titles[..sessions]
            .iter()
            .map(|title| {
                crate::picker::PickItem::simple(format!("/s/{title}"))
                    .with_label(*title)
                    .with_group("Today")
                    .with_right("09:30")
            })
            .collect(),
        notices: vec![(
            kage_core::protocol::NoticeLevel::Warning,
            "the `anthropic` login expires in 1 day. Run /login anthropic.".to_owned(),
        )],
        permissions: "built-in tools run without asking".to_owned(),
    }
}

fn card_status(info: &StartInfo) -> StatusCtx<'_> {
    StatusCtx {
        model: Some("Fake"),
        model_id: Some("fake:m"),
        cwd: Some("/work/kage"),
        start: Some(info),
        start_keys: StartKeys {
            model: Some("ctrl+p".to_owned()),
            thinking: Some("shift+tab".to_owned()),
            sessions: Some("ctrl+s".to_owned()),
        },
        ..StatusCtx::default()
    }
}

fn card_rows(buffer: &mut Buffer, info: &StartInfo, area: Rect) -> Vec<String> {
    let input = InputState::new();
    snapshot_frame(buffer, &input, None, &card_status(info), None, area)
}

#[test]
fn the_card_reads_as_labeled_rows_with_change_hints() {
    let info = start_info(1);
    let rows = card_rows(&mut Buffer::new(), &info, Rect::new(0, 0, 80, 24));
    let row = |label: &str| {
        rows.iter()
            .find(|r| r.starts_with(&format!("   {label} ")))
            .unwrap_or_else(|| panic!("no {label} row in {rows:#?}"))
            .clone()
    };
    assert_eq!(
        row("kage"),
        format!("   kage {}", kage_core::build::version())
    );
    assert!(row("model").starts_with("   model         Fake (fake:m)"));
    assert!(row("model").ends_with("ctrl+p to change"));
    assert_eq!(row("model").len(), 77);
    assert_eq!(row("directory"), "   directory     /work/kage");
    assert!(row("permissions").contains("built-in tools run without asking"));
    assert!(row("permissions").ends_with("/permission to change"));
    assert!(row("thinking").starts_with("   thinking      model default"));
    assert!(row("thinking").ends_with("shift+tab to change"));
    assert!(row("recent").starts_with("   recent        fix the parser tests  Today 09:30"));
    assert!(row("recent").ends_with("ctrl+s to resume"));
    assert!(
        rows.iter()
            .any(|r| r.starts_with("   ! the `anthropic` login"))
    );
}

#[test]
fn the_card_shows_below_notice_blocks_and_hides_after_the_first_prompt() {
    let info = start_info(3);
    let area = Rect::new(0, 0, 80, 24);
    let mut buffer = Buffer::new();
    buffer.push_custom("kage:error", "config: bad value", false);
    let rows = card_rows(&mut buffer, &info, area);
    let error = rows.iter().position(|r| r.contains("config: bad value"));
    let brand = rows.iter().position(|r| r.starts_with("   kage "));
    assert!(error.is_some() && brand > error, "{rows:#?}");
    buffer.push_user("hello");
    let rows = card_rows(&mut buffer, &info, area);
    assert!(rows.iter().all(|r| !r.contains("permissions")), "{rows:#?}");
}

#[test]
fn shell_output_hides_the_card() {
    let info = start_info(1);
    let mut buffer = Buffer::new();
    buffer.push_custom("kage:shell", "$ ls\na.rs\n(exit code 0)", false);
    let rows = card_rows(&mut buffer, &info, Rect::new(0, 0, 80, 24));
    assert!(rows.iter().all(|r| !r.contains("permissions")), "{rows:#?}");
}

#[test]
fn the_card_ends_directly_above_the_input_rule() {
    let info = start_info(3);
    let rows = card_rows(&mut Buffer::new(), &info, Rect::new(0, 0, 120, 36));
    let rule = rows.len() - 5;
    assert!(rows[rule].starts_with(RULE), "{rows:#?}");
    assert_eq!(rows[rule - 1], TIP, "{rows:#?}");
}

#[test]
fn the_card_leads_with_the_mark_and_drops_it_before_the_tip() {
    let info = start_info(3);
    let is_mark = |r: &String| r.contains('\u{2580}') || r.contains('\u{2584}');
    let tall = card_rows(&mut Buffer::new(), &info, Rect::new(0, 0, 80, 40));
    let brand = tall.iter().position(|r| r.starts_with("   kage ")).unwrap();
    let mark = &tall[brand - crate::view::mark::HEIGHT..brand];
    assert!(mark.iter().all(is_mark), "{tall:#?}");
    assert!(mark.iter().all(|r| r.starts_with("   ")), "{tall:#?}");
    let short = card_rows(&mut Buffer::new(), &info, Rect::new(0, 0, 80, 24));
    assert!(!short.iter().any(is_mark), "{short:#?}");
    assert!(short.iter().any(|r| r.contains("Tip:")), "{short:#?}");
}

#[test]
fn a_short_card_drops_the_tip_then_the_sessions() {
    let info = start_info(3);
    let area = Rect::new(0, 0, 100, 24);
    let filler = |lines: usize| {
        let mut buffer = Buffer::new();
        let text = vec!["config: bad value"; lines].join("\n");
        buffer.push_custom("kage:error", text, false);
        buffer
    };
    let full = card_rows(&mut Buffer::new(), &info, area);
    let card = full.len() - 4 - full.iter().position(|r| r.starts_with("   kage ")).unwrap();
    let room = |rows: &[String]| {
        let error = rows
            .iter()
            .rposition(|r| r.contains("config: bad value"))
            .unwrap();
        rows.len() - 4 - error - 2
    };
    let mut buffer = filler(1);
    let spare = room(&card_rows(&mut buffer, &info, area)) - card;
    let rows = card_rows(&mut filler(1 + spare + 1), &info, area);
    assert!(rows.iter().all(|r| !r.contains("Tip:")), "{rows:#?}");
    assert!(rows.iter().any(|r| r.contains("recent")), "{rows:#?}");
    let rows = card_rows(&mut filler(1 + spare + 3), &info, area);
    assert!(rows.iter().all(|r| !r.contains("recent")), "{rows:#?}");
    assert!(
        rows.iter().any(|r| r.contains("! the `anthropic`")),
        "{rows:#?}"
    );
    assert!(
        rows.iter().any(|r| r.starts_with("   thinking")),
        "{rows:#?}"
    );
}

#[test]
fn the_card_lists_at_most_three_sessions() {
    let info = start_info(5);
    let rows = card_rows(&mut Buffer::new(), &info, Rect::new(0, 0, 80, 24));
    let listed = rows.iter().filter(|r| r.contains("Today 09:30")).count();
    assert_eq!(listed, 3, "{rows:#?}");
    assert!(rows.iter().all(|r| !r.contains("fourth")), "{rows:#?}");
}

#[test]
fn notices_paint_in_the_warning_style() {
    let info = start_info(0);
    let input = InputState::new();
    let area = Rect::new(0, 0, 80, 24);
    let buf = paint_frame(
        &mut Buffer::new(),
        &input,
        None,
        &card_status(&info),
        None,
        area,
        &todo::TodoStrip::default(),
    );
    let y = (0..area.height)
        .find(|&y| buf[(3, y)].symbol() == "!")
        .expect("a notice row");
    let warning = crate::theme::current().warning_fg;
    assert_eq!(buf[(3, y)].fg, warning);
    assert_eq!(buf[(10, y)].fg, warning);
}

#[test]
fn clicking_a_partly_visible_block_focuses_it_without_scrolling() {
    let mut buffer = Buffer::new();
    let long: Vec<String> = (0..30).map(|i| format!("row {i}")).collect();
    buffer.append_assistant_delta(&long.join("\n\n"));
    buffer.finish_streaming();
    buffer.push_user("tail");
    let input = InputState::new();
    let area = Rect::new(0, 0, 40, 16);
    snapshot_lines(&mut buffer, &input, area);
    let top = buffer.last_virtual_top();
    assert!(top > 0, "the long reply starts above the viewport");

    buffer.focus_in_place(0);
    snapshot_lines(&mut buffer, &input, area);
    assert!(buffer.is_following());
    assert_eq!(buffer.last_virtual_top(), top);

    buffer.set_focus(Some(1));
    snapshot_lines(&mut buffer, &input, area);
    buffer.set_focus(Some(0));
    snapshot_lines(&mut buffer, &input, area);
    assert_eq!(buffer.scroll(), Some(0), "a keyboard move still scrolls");
}

fn agent_call(buffer: &mut Buffer, id: &str, description: &str) {
    let input = json!({"agent": "explore", "description": description, "prompt": "go"});
    buffer.push_tool_call(id, "agent", input);
}

fn agent_result(state: &str, body: &str) -> String {
    format!(
        "<agent name=\"explore\" session=\"01K62W8Q3T9V5M2C7X4B1N0R6S\" state=\"{state}\">\n{body}\n</agent>"
    )
}

#[test]
fn a_running_agent_row_shows_its_card() {
    let mut buffer = Buffer::new();
    agent_call(&mut buffer, "a1", "map exports");
    buffer.set_tool_phase("a1", tool_view::ToolPhase::Running);
    buffer.set_tool_progress("a1", "Read src/lib.rs\n3 tools \u{b7} 9k tok");
    let input = InputState::new();
    let lines = snapshot_lines(&mut buffer, &input, Rect::new(0, 0, 80, 10));
    let header = lines
        .iter()
        .position(|l| l.contains("Agent explore: map exports"))
        .unwrap_or_else(|| panic!("{lines:#?}"));
    assert!(lines[header + 1].contains("Read src/lib.rs"), "{lines:#?}");
    assert!(
        lines[header + 2].contains("3 tools \u{b7} 9k tok"),
        "{lines:#?}"
    );
}

#[test]
fn an_interrupted_agent_card_keeps_only_its_stats() {
    let mut buffer = Buffer::new();
    agent_call(&mut buffer, "a1", "map exports");
    buffer.set_tool_phase("a1", tool_view::ToolPhase::Running);
    buffer.set_tool_progress("a1", "Waiting for approval: $ ls\n3 tools \u{b7} 9k tok");
    buffer.interrupt_running_tools();
    let input = InputState::new();
    let lines = snapshot_lines(&mut buffer, &input, Rect::new(0, 0, 80, 10));
    let header = lines
        .iter()
        .position(|l| l.contains("Agent explore: map exports"))
        .unwrap();
    assert!(lines[header].ends_with("interrupted"), "{lines:#?}");
    assert!(
        lines[header + 1].ends_with("3 tools \u{b7} 9k tok"),
        "{lines:#?}"
    );
    assert!(lines.iter().all(|l| !l.contains("Waiting")), "{lines:#?}");
}

#[test]
fn finished_agent_rows_show_their_end_and_never_the_wrapper() {
    let mut buffer = Buffer::new();
    agent_call(&mut buffer, "a1", "map exports");
    buffer.push_tool_result_with_duration(
        "a1",
        agent_result(
            "completed",
            "{\"Button.tsx\": [\"Button\"]}\nline two\nline three",
        ),
        false,
        Some(52_000),
    );
    agent_call(&mut buffer, "a2", "find dead code");
    buffer.push_tool_result_with_duration(
        "a2",
        agent_result("cancelled", "two unused helpers"),
        true,
        Some(12_000),
    );
    agent_call(&mut buffer, "a3", "check the router tests");
    buffer.push_tool_result_with_duration(
        "a3",
        agent_result("failed", "provider error: rate limited (429)"),
        true,
        Some(3_000),
    );
    let input = InputState::new();
    let lines = snapshot_lines(&mut buffer, &input, Rect::new(0, 0, 100, 20));
    let row = |needle: &str| {
        lines
            .iter()
            .position(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("{needle}: {lines:#?}"))
    };
    let done = row("Agent explore: map exports");
    assert!(lines[done].contains("\u{2022} Agent"), "{lines:#?}");
    assert!(lines[done].ends_with("done \u{b7} 52s"), "{lines:#?}");
    assert!(lines[done + 1].contains("{\"Button.tsx\""), "{lines:#?}");
    assert!(lines[done + 2].contains("... 2 more lines"), "{lines:#?}");

    let stopped = row("Agent explore: find dead code");
    assert!(lines[stopped].contains("\u{2298} Agent"), "{lines:#?}");
    assert!(lines[stopped].ends_with("stopped \u{b7} 12s"), "{lines:#?}");
    assert!(
        lines[stopped + 1].contains("Stopped by you. Partial reply: two unused helpers"),
        "{lines:#?}"
    );

    let failed = row("Agent explore: check the router tests");
    assert!(lines[failed].contains("\u{2717} Agent"), "{lines:#?}");
    assert!(lines[failed].ends_with("failed \u{b7} 3s"), "{lines:#?}");
    assert!(
        lines[failed + 1].contains("provider error: rate limited (429)"),
        "{lines:#?}"
    );
    assert!(lines.iter().all(|l| !l.contains("<agent")), "{lines:#?}");
    assert!(lines.iter().all(|l| !l.contains("</agent")), "{lines:#?}");
}

// --- Pinned agents ---

fn agent_row(depth: usize, agent: &str, description: &str, activity: &str, secs: u64) -> AgentRow {
    AgentRow {
        session: kage_core::SessionId::new(),
        depth,
        agent: agent.to_owned(),
        description: description.to_owned(),
        item: String::new(),
        state: AgentRowState::Running,
        activity: activity.to_owned(),
        elapsed_ms: Some(secs * 1000),
        background: false,
    }
}

/// `rows` with every spinner frame read as `/`, so frames compare
/// whatever the clock says.
fn steady(rows: Vec<String>) -> Vec<String> {
    rows.into_iter()
        .map(|row| {
            row.chars()
                .map(|c| {
                    if c == '\u{25cb}' || c == '\u{25cf}' {
                        '/'
                    } else {
                        c
                    }
                })
                .collect()
        })
        .collect()
}

fn agents_status<'a>(agents: &'a [AgentRow], pending: &'a [PendingPrompt]) -> StatusCtx<'a> {
    StatusCtx {
        activity: Some("Waiting for 3 agents (41s, esc to interrupt)"),
        agents,
        pending,
        ..StatusCtx::default()
    }
}

#[test]
fn three_pinned_agents_sit_between_the_working_row_and_the_input() {
    let agents = [
        agent_row(
            1,
            "explore",
            "map exports under src/components",
            "Searched \"export \" in src/components",
            41,
        ),
        agent_row(
            1,
            "explore",
            "map exports under src/routes",
            "Read src/routes/index.ts",
            18,
        ),
        agent_row(
            1,
            "general",
            "check the router tests",
            "Running cargo test -p router",
            12,
        ),
    ];
    let mut buffer = Buffer::new();
    buffer.push_user("map the exports, then check the router tests");
    let input = InputState::new();
    let status = agents_status(&agents, &[]);
    let rows = steady(snapshot_frame(
        &mut buffer,
        &input,
        None,
        &status,
        None,
        Rect::new(0, 0, 100, 30),
    ));
    let top = format!("{RULE}{RULE} INSERT {}", RULE.to_string().repeat(90));
    let bottom = RULE.to_string().repeat(100);
    assert_eq!(
        rows[22..30],
        [
            "  / Waiting for 3 agents (41s, esc to interrupt)",
            "  / explore  map exports under src/components            Searched \"export \" in src/components \u{b7} 41s",
            "  / explore  map exports under src/routes                            Read src/routes/index.ts \u{b7} 18s",
            "  / general  check the router tests                              Running cargo test -p router \u{b7} 12s",
            top.as_str(),
            " > Ask kage anything",
            bottom.as_str(),
            "",
        ],
        "{rows:#?}"
    );
}

#[test]
fn pinned_agents_come_before_the_pending_prompts() {
    let agents = [agent_row(1, "explore", "map exports", "Read src/lib.rs", 4)];
    let pending = [PendingPrompt {
        text: "and the routes".to_owned(),
        queued: true,
    }];
    let input = InputState::new();
    let status = agents_status(&agents, &pending);
    let rows = snapshot_frame(
        &mut Buffer::new(),
        &input,
        None,
        &status,
        None,
        Rect::new(0, 0, 80, 12),
    );
    let agent = rows.iter().position(|r| r.contains("explore")).unwrap();
    assert!(
        rows[agent + 1].starts_with("  > and the routes"),
        "{rows:#?}"
    );
    assert!(rows[agent + 2].starts_with(RULE), "{rows:#?}");
}

#[test]
fn a_nested_agent_is_indented_under_its_parent() {
    let agents = [
        agent_row(
            1,
            "general",
            "refactor the provider crate",
            "Agent test: run the provider tests",
            130,
        ),
        agent_row(
            2,
            "test",
            "run the provider tests",
            "Running cargo test -p kage-provider",
            4,
        ),
    ];
    let input = InputState::new();
    let status = agents_status(&agents, &[]);
    let rows = steady(snapshot_frame(
        &mut Buffer::new(),
        &input,
        None,
        &status,
        None,
        Rect::new(0, 0, 100, 12),
    ));
    let parent = rows.iter().position(|r| r.contains("general")).unwrap();
    assert_eq!(
        rows[parent..parent + 2],
        [
            "  / general  refactor the provider crate                Agent test: run the provider tests \u{b7} 2m 10s",
            "    / test   run the provider tests                        Running cargo test -p kage-provider \u{b7} 4s",
        ],
        "{rows:#?}"
    );
}

#[test]
fn six_pinned_agents_show_four_and_fold_the_rest() {
    let agents: Vec<AgentRow> = (1..=6)
        .map(|n| agent_row(1, "explore", &format!("task {n}"), "Working", n))
        .collect();
    let input = InputState::new();
    let status = agents_status(&agents, &[]);
    let rows = snapshot_frame(
        &mut Buffer::new(),
        &input,
        None,
        &status,
        None,
        Rect::new(0, 0, 80, 16),
    );
    let first = rows.iter().position(|r| r.contains("task 1")).unwrap();
    assert!(rows[first + 3].contains("task 4"), "{rows:#?}");
    assert_eq!(rows[first + 4], "  +2 more", "{rows:#?}");
    assert!(rows[first + 5].starts_with(RULE), "{rows:#?}");
    assert!(rows.iter().all(|r| !r.contains("task 5")), "{rows:#?}");
}

#[test]
fn queued_and_waiting_agents_say_so() {
    let mut queued = agent_row(1, "explore", "map exports", "", 0);
    queued.state = AgentRowState::Queued;
    queued.elapsed_ms = None;
    let mut waiting = agent_row(1, "general", "check the router tests", "", 12);
    waiting.state = AgentRowState::Waiting;
    let agents = [queued, waiting];
    let input = InputState::new();
    let status = agents_status(&agents, &[]);
    let rows = snapshot_frame(
        &mut Buffer::new(),
        &input,
        None,
        &status,
        None,
        Rect::new(0, 0, 80, 12),
    );
    let queued = rows.iter().find(|r| r.contains("explore")).unwrap();
    assert!(queued.starts_with("  \u{2022} explore"), "{rows:#?}");
    assert!(queued.ends_with(" queued"), "{rows:#?}");
    let waiting = rows.iter().find(|r| r.contains("general")).unwrap();
    assert!(waiting.starts_with("  ! general"), "{rows:#?}");
    assert!(
        waiting.ends_with("waiting for approval \u{b7} 12s"),
        "{rows:#?}"
    );
}

#[test]
fn a_narrow_row_cuts_the_description_before_the_activity() {
    let agents = [agent_row(
        1,
        "explore",
        "map exports under src/components",
        "Searched \"export \" in src/components",
        41,
    )];
    let input = InputState::new();
    let status = agents_status(&agents, &[]);
    let rows = steady(snapshot_frame(
        &mut Buffer::new(),
        &input,
        None,
        &status,
        None,
        Rect::new(0, 0, 80, 10),
    ));
    let row = rows.iter().find(|r| r.contains("explore")).unwrap();
    assert_eq!(
        row,
        "  / explore  map exports under s...  Searched \"export \" in src/components \u{b7} 41s"
    );
}

/// A `todo_list` call in a transcript, built the way the buffer builds
/// one.
/// A buffer whose transcript holds one `todo_list` write, the way a
/// session that tracked a plan looks.
fn todo_buffer(input: serde_json::Value) -> Buffer {
    let mut buffer = Buffer::new();
    buffer.push_tool_call("call_1", "todo_list", input);
    buffer
}

/// A todo write in the transcript pins its box under the working row,
/// so the plan stays visible while the conversation scrolls. The box
/// shows the running task first, then the queue, capped at five rows.
#[test]
fn a_todo_write_pins_rows_above_the_input() {
    let input_json = serde_json::json!({"todos": [
        {"title": "read the code", "status": "in_progress"},
        {"title": "write the fix", "status": "pending"},
        {"title": "run the tests", "status": "pending"},
        {"title": "write the docs", "status": "pending"},
        {"title": "ship it", "status": "pending"},
        {"title": "celebrate", "status": "pending"},
    ]});
    let mut buffer = todo_buffer(input_json.clone());
    let strip = todo::from_blocks(buffer.blocks());
    assert!(strip.tracked);

    let input = InputState::new();
    let area = Rect::new(0, 0, 60, 20);
    let buf = paint_frame(
        &mut buffer,
        &input,
        None,
        &card_status(&start_info(0)),
        None,
        area,
        &strip,
    );
    let text: String = (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("0/6 todos"), "no progress row in:\n{text}");
    assert!(
        text.contains("read the code"),
        "no current task in:\n{text}"
    );
    assert!(text.contains("ship it"), "no last fitting task in:\n{text}");
    // The sixth item is beyond the cap and stays in the transcript.
    assert!(!text.contains("celebrate"), "the cap did not hold:\n{text}");
}

/// Without a todo write in the transcript the region collapses, so the
/// input keeps its rows.
#[test]
fn no_todos_leaves_the_input_region_alone() {
    let input = InputState::new();
    let area = Rect::new(0, 0, 60, 20);
    let mut buffer = Buffer::new();
    let bare = paint_frame(
        &mut buffer,
        &input,
        None,
        &card_status(&start_info(0)),
        None,
        area,
        &todo::TodoStrip::default(),
    );
    let mut tracked = todo_buffer(serde_json::json!({"todos": [
        {"title": "a", "status": "pending"},
    ]}));
    let todos = todo::from_blocks(tracked.blocks());
    let with = paint_frame(
        &mut tracked,
        &input,
        None,
        &card_status(&start_info(0)),
        None,
        area,
        &todos,
    );
    let rows = |b: &ratatui::buffer::Buffer| {
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| b[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert_ne!(rows(&bare), rows(&with), "the strip painted nothing");
}

#[test]
fn input_height_counts_the_pinned_agents() {
    let input = InputState::new();
    let bare = input_height(&input, 0, 0, 80);
    assert_eq!(input_height(&input, 3, 0, 80), bare + 3);
    assert_eq!(input_height(&input, 6, 0, 80), bare + 5);
    assert_eq!(input_height(&input, 2, 1, 80), bare + 3);
}

#[test]
fn recorded_block_rows_match_the_painted_rows_when_the_top_is_a_gap() {
    for count in [10, 11] {
        let mut buffer = Buffer::new();
        for i in 0..count {
            buffer.append_assistant_delta(&format!("reply {i}"));
            buffer.finish_streaming();
        }
        let rows = snapshot_lines(&mut buffer, &InputState::new(), Rect::new(0, 0, 40, 12));
        for (idx, _) in buffer.blocks().iter().enumerate() {
            let Some((top, _)) = buffer.screen_rows_of(idx) else {
                continue;
            };
            assert_eq!(
                rows[usize::from(top)].trim(),
                format!("reply {idx}"),
                "{count} blocks: {rows:#?}"
            );
        }
    }
}

#[test]
fn wrapped_list_items_hang_under_their_text() {
    let mut buffer = Buffer::new();
    buffer.append_assistant_delta(
        "- the first item is long enough to wrap onto a second row\n\
         1. an ordered item that also wraps past the edge of the row\n",
    );
    buffer.finish_streaming();
    let rows = snapshot_lines(&mut buffer, &InputState::new(), Rect::new(0, 0, 40, 14));
    let at = |text: &str| rows.iter().position(|r| r.contains(text)).expect(text);
    let bullet = at("\u{2022} the first");
    assert!(rows[bullet + 1].starts_with("    "), "{rows:#?}");
    assert!(!rows[bullet + 1].starts_with("     "), "{rows:#?}");
    let ordered = at("1. an ordered");
    assert!(rows[ordered + 1].starts_with("     "), "{rows:#?}");
    assert!(!rows[ordered + 1].starts_with("      "), "{rows:#?}");
}

#[test]
fn a_reply_shows_tables_as_columns_and_quotes_with_a_gutter() {
    let mut buffer = Buffer::new();
    buffer.append_assistant_delta(
        "| tool | calls |\n|---|---:|\n| read | 12 |\n| shell | 3 |\n\n\
         > a quoted line long enough to wrap onto the next row\n\n\
         > - a quoted item long enough to wrap onto the next row\n",
    );
    buffer.finish_streaming();
    let rows = snapshot_lines(&mut buffer, &InputState::new(), Rect::new(0, 0, 40, 18));
    let at = |text: &str| rows.iter().position(|r| r.contains(text)).expect(text);
    let head = at("tool   calls");
    assert!(
        rows[head + 1]
            .contains("\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}  \u{2500}\u{2500}\u{2500}\u{2500}"),
        "{rows:#?}"
    );
    assert!(rows[head + 2].contains("read      12"), "{rows:#?}");
    assert!(rows[head + 3].contains("shell      3"), "{rows:#?}");
    assert!(rows.iter().all(|r| !r.contains('|')), "{rows:#?}");
    let quote = at("> a quoted line");
    let gutter = rows[quote].find("> ").unwrap();
    assert_eq!(rows[quote + 1].find("> "), Some(gutter), "{rows:#?}");
    assert!(rows[quote + 1][..gutter].trim().is_empty(), "{rows:#?}");
    assert_ne!(rows[quote + 1].as_bytes()[gutter + 2], b' ', "{rows:#?}");
    let item = at("> \u{2022} a quoted item");
    assert!(rows[item + 1][gutter..].starts_with(">   "), "{rows:#?}");
    assert_ne!(rows[item + 1].as_bytes()[gutter + 4], b' ', "{rows:#?}");
}

#[test]
fn a_pinned_view_shows_when_new_output_arrives_below() {
    let mut buffer = Buffer::new();
    for i in 0..30 {
        buffer.append_assistant_delta(&format!("reply {i}"));
        buffer.finish_streaming();
    }
    let area = Rect::new(0, 0, 40, 14);
    let input = InputState::new();
    let marked = |rows: &[String]| rows.iter().any(|r| r.contains("new output below"));
    assert!(!marked(&snapshot_lines(&mut buffer, &input, area)));
    buffer.set_scroll(0);
    assert!(!marked(&snapshot_lines(&mut buffer, &input, area)));
    buffer.append_assistant_delta("streamed");
    let rows = snapshot_lines(&mut buffer, &input, area);
    assert!(marked(&rows), "{rows:#?}");
    buffer.follow();
    assert!(!marked(&snapshot_lines(&mut buffer, &input, area)));
}

#[test]
fn the_new_output_mark_records_a_hit_rect_only_while_painted() {
    let mut buffer = Buffer::new();
    for i in 0..30 {
        buffer.append_assistant_delta(&format!("reply {i}"));
        buffer.finish_streaming();
    }
    let area = Rect::new(0, 0, 40, 14);
    let input = InputState::new();
    snapshot_lines(&mut buffer, &input, area);
    buffer.set_scroll(0);
    snapshot_lines(&mut buffer, &input, area);
    // Detaching alone paints no mark, so nothing is clickable.
    let none_of = |buffer: &Buffer| {
        let y = buffer.last_area_y() + buffer.last_area_height() - 1;
        (0..40).all(|col| !buffer.mark_hit_at(y, col))
    };
    assert!(none_of(&buffer));
    buffer.append_assistant_delta("streamed");
    snapshot_lines(&mut buffer, &input, area);
    let y = buffer.last_area_y() + buffer.last_area_height() - 1;
    let hit: Vec<u16> = (0..area.width)
        .filter(|&c| buffer.mark_hit_at(y, c))
        .collect();
    assert_eq!(hit.len(), 20, "the mark's cells, contiguous at the right");
    assert!(
        hit.windows(2).all(|w| w[1] == w[0] + 1) && hit[0] + 20 < area.width,
        "{hit:?}"
    );
    assert!(!buffer.mark_hit_at(y - 1, hit[0]), "only the mark's row");
    buffer.follow();
    snapshot_lines(&mut buffer, &input, area);
    assert!(none_of(&buffer));
}

#[test]
fn unfolding_while_scrolled_up_is_not_new_output() {
    let mut buffer = Buffer::new();
    buffer.push_tool_call("c1", "shell", json!({"command": "seq 30"}));
    let out: Vec<String> = (1..=30).map(|i| format!("out {i}")).collect();
    buffer.push_tool_result("c1", format!("stdout:\n{}\nexit: 0", out.join("\n")), false);
    for i in 0..30 {
        buffer.append_assistant_delta(&format!("reply {i}"));
        buffer.finish_streaming();
    }
    let area = Rect::new(0, 0, 40, 14);
    let input = InputState::new();
    buffer.set_scroll(0);
    snapshot_lines(&mut buffer, &input, area);
    buffer.toggle_fold(0);
    let rows = snapshot_lines(&mut buffer, &input, area);
    assert!(
        !rows.iter().any(|r| r.contains("new output below")),
        "{rows:#?}"
    );
}

/// A prompt, a reply, three shell calls with their results, a closing
/// reply and a second prompt: blocks 0, 1, 2, 4, 6, 8 and 9 paint.
fn tool_burst() -> Buffer {
    let mut buffer = Buffer::new();
    buffer.push_user("fix the build");
    buffer.append_assistant_delta("Checking the tree.");
    buffer.finish_streaming();
    for (id, cmd) in [
        ("b1", "cargo build"),
        ("b2", "cargo test"),
        ("b3", "git status"),
    ] {
        buffer.push_tool_call(id, "shell", json!({"command": cmd}));
        buffer.push_tool_result(id, "exit: 0", false);
    }
    buffer.append_assistant_delta("All green.");
    buffer.finish_streaming();
    buffer.push_user("thanks");
    buffer
}

#[test]
fn consecutive_tool_rows_sit_flush_between_spaced_text() {
    let mut buffer = tool_burst();
    let input = InputState::new();
    let lines = snapshot_lines(&mut buffer, &input, Rect::new(0, 0, 40, 24));
    assert_eq!(
        lines[..14],
        [
            "\u{258e} > fix the build",
            "",
            "  Checking the tree.",
            "",
            "\u{258e} \u{2022} Ran cargo build",
            "\u{258e}     exit: 0",
            "\u{258e} \u{2022} Ran cargo test",
            "\u{258e}     exit: 0",
            "\u{258e} \u{2022} Ran git status",
            "\u{258e}     exit: 0",
            "",
            "  All green.",
            "",
            "\u{258e} > thanks",
        ],
        "{lines:#?}"
    );
}

#[test]
fn tool_row_gaps_keep_heights_and_click_rows_aligned() {
    let mut buffer = tool_burst();
    let input = InputState::new();
    snapshot_lines(&mut buffer, &input, Rect::new(0, 0, 40, 24));
    let spans: Vec<(usize, usize)> = [0, 1, 2, 4, 6, 8, 9]
        .iter()
        .map(|&i| buffer.block_virtual_rows(i).expect("painted"))
        .collect();
    assert_eq!(
        spans,
        [(0, 1), (2, 3), (4, 6), (6, 8), (8, 10), (11, 12), (13, 14)]
    );
    let y = buffer.last_area_y();
    let hits: Vec<Option<usize>> = (0..14).map(|r| buffer.block_at_screen_row(y + r)).collect();
    let expected = [
        Some(0),
        None,
        Some(1),
        None,
        Some(2),
        Some(2),
        Some(4),
        Some(4),
        Some(6),
        Some(6),
        None,
        Some(8),
        None,
        Some(9),
    ];
    assert_eq!(hits, expected);
}

#[test]
fn scroll_math_follows_the_tool_row_gaps() {
    let mut buffer = tool_burst();
    let input = InputState::new();
    let area = Rect::new(0, 0, 40, 10);
    snapshot_lines(&mut buffer, &input, area);
    buffer.set_scroll(5);
    let lines = snapshot_lines(&mut buffer, &input, area);
    assert_eq!(
        lines[..6],
        [
            "\u{258e}     exit: 0",
            "\u{258e} \u{2022} Ran cargo test",
            "\u{258e}     exit: 0",
            "\u{258e} \u{2022} Ran git status",
            "\u{258e}     exit: 0",
            "",
        ],
        "{lines:#?}"
    );
    let y = buffer.last_area_y();
    let hits: Vec<Option<usize>> = (0..6).map(|r| buffer.block_at_screen_row(y + r)).collect();
    assert_eq!(hits, [Some(2), Some(4), Some(4), Some(6), Some(6), None]);

    buffer.follow();
    let lines = snapshot_lines(&mut buffer, &input, area);
    assert_eq!(lines[5], "\u{258e} > thanks", "{lines:#?}");
    assert_eq!(buffer.last_virtual_top(), 8);

    let area = Rect::new(0, 0, 40, 8);
    snapshot_lines(&mut buffer, &input, area);
    buffer.set_focus(Some(4));
    let lines = snapshot_lines(&mut buffer, &input, area);
    assert_eq!(
        buffer.scroll(),
        Some(4),
        "the second call's bottom meets the viewport's"
    );
    assert!(lines[0].ends_with("Ran cargo build"), "{lines:#?}");
    assert!(lines[3].ends_with("exit: 0"), "{lines:#?}");
}

// --- Pinned todo strip ---

/// The pinned todo rows as painted: each row's visible text and the
/// distinct foregrounds its cells carry.
fn todo_rows_painted(todos: &serde_json::Value) -> Vec<(String, Vec<ratatui::style::Color>)> {
    todo_rows_painted_at(todos, 40)
}

/// [`todo_rows_painted`] at a chosen strip width.
fn todo_rows_painted_at(
    todos: &serde_json::Value,
    width: u16,
) -> Vec<(String, Vec<ratatui::style::Color>)> {
    // `render` re-derives the strip from the buffer's blocks, so the
    // call has to go through the buffer rather than being handed a
    // strip directly.
    let mut buffer = Buffer::new();
    buffer.push_tool_call("c", "todo_list", json!({ "todos": todos.clone() }));
    // The layout must reserve the same rows the painter fills, so the
    // strip handed to the layout is the one the buffer derives.
    let strip = todo::from_blocks(buffer.blocks());
    let painted = paint_frame(
        &mut buffer,
        &InputState::new(),
        None,
        &StatusCtx::default(),
        None,
        Rect::new(0, 0, width, 14),
        &strip,
    );
    painted
        .content()
        .chunks(usize::from(width))
        .map(|row| {
            let text: String = row.iter().map(ratatui::buffer::Cell::symbol).collect();
            let text = text.trim_end().to_owned();
            let mut colors: Vec<ratatui::style::Color> = Vec::new();
            for cell in row {
                if cell.symbol().trim().is_empty() || colors.contains(&cell.fg) {
                    continue;
                }
                colors.push(cell.fg);
            }
            (text, colors)
        })
        // Keep only the pinned strip: the heading, then rows led by a
        // task marker. The modeline and the input rules share the
        // frame and are not part of it.
        .filter(|(text, _)| {
            // Inner rows lead with the box's side border, then the
            // task marker.
            let body = text.trim_start_matches([' ', '\u{2502}']);
            text.contains(" todo")
                || text.contains(" todos")
                || body.starts_with(todo::MARK_RUNNING)
                || body.starts_with(todo::MARK_PENDING)
        })
        .collect()
}

#[test]
fn the_unfinished_heading_takes_no_task_marker() {
    // The heading's glyph is decorative and must never read as a task
    // marker: a check would claim "everything is done" on a list with
    // nothing done, a dot would read as a task row.
    let theme = crate::theme::current();
    let rows = todo_rows_painted(&json!([{ "title": "reading", "status": "in_progress" }]));
    let (text, colors) = &rows[0];
    assert!(text.contains("0/1 todo"), "{text:?}");
    assert!(
        !text.contains('\u{2713}') && !text.contains('\u{25cf}'),
        "heading painted a task marker: {text:?}"
    );
    assert!(
        colors.contains(&theme.tool_pending_rule),
        "unfinished heading misses the working accent: {colors:?}"
    );
}

#[test]
fn the_todo_box_is_rounded_and_muted() {
    let mut buffer =
        todo_buffer(json!({ "todos": [{ "title": "reading", "status": "in_progress" }] }));
    let strip = todo::from_blocks(buffer.blocks());
    let buf = paint_frame(
        &mut buffer,
        &InputState::new(),
        None,
        &card_status(&start_info(0)),
        None,
        Rect::new(0, 0, 40, 14),
        &strip,
    );
    let row = |y: u16| (0..40).map(|x| buf[(x, y)].symbol()).collect::<String>();
    let rows: Vec<String> = (0..14).map(row).collect();
    let top = rows
        .iter()
        .position(|r| r.starts_with('\u{256d}'))
        .expect("top border");
    let bottom = rows
        .iter()
        .position(|r| r.starts_with('\u{2570}'))
        .expect("bottom border");
    assert!(rows[top].contains("0/1 todo"), "{:?}", rows[top]);
    assert!(rows[top].ends_with('\u{256e}'), "{:?}", rows[top]);
    // The heading rides the top border, so the inner rows are the
    // painted rows minus it.
    assert_eq!(
        u16::try_from(bottom - top).unwrap(),
        todo::box_height(&strip) - 1
    );
    let theme = crate::theme::current();
    let all_muted = (0..40).all(|x| buf[(x, u16::try_from(bottom).unwrap())].fg == theme.muted_fg);
    assert!(all_muted, "bottom border is not one muted row");
}

#[test]
fn the_todo_box_hugs_its_last_task() {
    // The heading rides the top border, so the interior holds only
    // task rows; a spare blank row read as a bottom margin.
    let mut buffer = todo_buffer(json!({ "todos": [
        { "title": "one", "status": "pending" },
        { "title": "two", "status": "pending" },
    ] }));
    let strip = todo::from_blocks(buffer.blocks());
    let buf = paint_frame(
        &mut buffer,
        &InputState::new(),
        None,
        &card_status(&start_info(0)),
        None,
        Rect::new(0, 0, 40, 14),
        &strip,
    );
    let rows: Vec<String> = (0..14)
        .map(|y| (0..40).map(|x| buf[(x, y)].symbol()).collect::<String>())
        .collect();
    let bottom = rows
        .iter()
        .position(|r| r.starts_with('\u{2570}'))
        .expect("bottom border");
    let above = &rows[bottom - 1];
    assert!(
        above.contains('\u{25cb}') && above.contains("two"),
        "the row above the border is not the last task: {above:?}"
    );
}

#[test]
fn a_todo_list_with_nothing_running_is_not_monochrome() {
    // Every row in one tier flattened the strip into a single block of
    // grey; the accented heading must stand off the pending rows.
    let theme = crate::theme::current();
    let rows = todo_rows_painted(&json!([
        { "title": "one", "status": "pending" },
        { "title": "two", "status": "pending" },
    ]));
    let (heading_text, heading_colors) = &rows[0];
    let (_, task_colors) = &rows[1];
    assert!(heading_text.contains("todos"), "{heading_text:?}");
    assert_ne!(
        heading_colors, task_colors,
        "heading and task share a palette, so the strip reads flat"
    );
    assert!(
        task_colors.contains(&theme.muted_fg),
        "pending row misses the muted tier: {task_colors:?}"
    );
}

#[test]
fn the_running_todo_is_the_only_accented_row() {
    let theme = crate::theme::current();
    let rows = todo_rows_painted(&json!([
        { "title": "now", "status": "in_progress" },
        { "title": "later", "status": "pending" },
    ]));
    assert!(
        rows[1].1.contains(&theme.tool_pending_rule) && rows[1].1.contains(&theme.assistant_fg),
        "running row: accented marker, bright title: {:?}",
        rows[1].1
    );
    assert!(
        !rows[2].1.contains(&theme.tool_pending_rule),
        "pending row takes the working accent: {:?}",
        rows[2].1
    );
}

#[test]
fn the_heading_carries_a_progress_bar() {
    let theme = crate::theme::current();
    let rows = todo_rows_painted(&json!([
        { "title": "a", "status": "done" },
        { "title": "b", "status": "in_progress" },
        { "title": "c", "status": "pending" },
    ]));
    let (text, colors) = &rows[0];
    assert!(text.contains("1/3 todos"), "{text:?}");
    let bar: String = text
        .chars()
        .filter(|c| *c == '\u{25b0}' || *c == '\u{25b1}')
        .collect();
    assert_eq!(bar, "\u{25b0}\u{25b1}\u{25b1}", "{text:?}");
    assert!(
        colors.contains(&theme.tool_pending_rule),
        "bar misses the working accent: {colors:?}"
    );
}

#[test]
fn a_narrow_heading_drops_the_bar() {
    let rows = todo_rows_painted_at(&json!([{ "title": "a", "status": "pending" }]), 15);
    let (text, _) = &rows[0];
    assert!(text.contains("0/1 todo"), "{text:?}");
    assert!(
        !text.contains('\u{25b0}') && !text.contains('\u{25b1}'),
        "bar survived a narrow strip: {text:?}"
    );
}

#[test]
fn a_finished_list_unpins_the_box() {
    // A plan with nothing outstanding paints nothing: the transcript
    // carries the completion.
    let rows = todo_rows_painted(&json!([
        { "title": "a", "status": "done" },
        { "title": "b", "status": "done" },
    ]));
    assert!(rows.is_empty(), "a finished list still painted: {rows:?}");
}

#[test]
fn a_done_task_strikes_through() {
    let mut buffer = todo_buffer(json!({ "todos": [
        { "title": "finished deed", "status": "done" },
        { "title": "open task", "status": "in_progress" },
    ]}));
    let strip = todo::from_blocks(buffer.blocks());
    let buf = paint_frame(
        &mut buffer,
        &InputState::new(),
        None,
        &card_status(&start_info(0)),
        None,
        Rect::new(0, 0, 40, 14),
        &strip,
    );
    let row_text = |y: u16| (0..40).map(|x| buf[(x, y)].symbol()).collect::<String>();
    let mut struck = false;
    let mut checked = false;
    let theme = crate::theme::current();
    for y in 0..14 {
        if !row_text(y).contains("finished deed") {
            continue;
        }
        for x in 0..40 {
            let cell = &buf[(x, y)];
            if cell.symbol().trim().is_empty() {
                continue;
            }
            if cell
                .style()
                .add_modifier
                .contains(ratatui::style::Modifier::CROSSED_OUT)
            {
                struck = true;
            }
            if cell.fg == theme.success_fg {
                checked = true;
            }
        }
    }
    assert!(struck, "done task is not struck through");
    assert!(checked, "done task misses the success tier");
}

#[test]
fn the_todo_box_sits_below_the_working_row() {
    let mut buffer =
        todo_buffer(json!({ "todos": [{ "title": "reading", "status": "in_progress" }] }));
    let strip = todo::from_blocks(buffer.blocks());
    let info = start_info(0);
    let status = StatusCtx {
        activity: Some("  Working (2s, esc to interrupt)"),
        ..card_status(&info)
    };
    let buf = paint_frame(
        &mut buffer,
        &InputState::new(),
        None,
        &status,
        None,
        Rect::new(0, 0, 40, 14),
        &strip,
    );
    let row = |y: u16| (0..40).map(|x| buf[(x, y)].symbol()).collect::<String>();
    let rows: Vec<String> = (0..14).map(row).collect();
    let box_row = rows
        .iter()
        .position(|r| r.contains("0/1 todo"))
        .expect("todo box painted");
    let working = rows
        .iter()
        .position(|r| r.contains("Working (2s"))
        .expect("working row painted");
    assert!(
        working < box_row,
        "box did not sit under the working row:\n{}",
        rows.join("\n")
    );
    assert_eq!(
        u16::try_from(box_row - working).unwrap(),
        1,
        "the box must hug the working row"
    );
    // The box's bottom border sits on the input's top rule.
    let bottom = rows
        .iter()
        .position(|r| r.starts_with('\u{2570}'))
        .expect("bottom border");
    assert!(
        rows[bottom + 1].contains("INSERT"),
        "the border does not touch the input rule:\n{}",
        rows.join("\n")
    );
}

#[test]
fn an_exit_plan_row_shows_the_plan_under_its_title() {
    let mut buffer = Buffer::new();
    let plan = "# Remote rollout\n\n## Steps\n\n1. Add the transport\n2. Wire the host";
    buffer.push_tool_call("p1", "exit_plan", json!({ "plan": plan }));
    buffer.set_tool_phase("p1", tool_view::ToolPhase::Waiting);
    let input = InputState::new();
    let lines = snapshot_lines(&mut buffer, &input, Rect::new(0, 0, 80, 14));
    let header = lines
        .iter()
        .position(|l| l.contains("Plan Remote rollout"))
        .unwrap_or_else(|| panic!("{lines:#?}"));
    assert!(lines[header].contains("awaiting review"), "{lines:#?}");
    let body = lines[header + 1..].join("\n");
    assert!(body.contains("Steps"), "{lines:#?}");
    assert!(body.contains("Wire the host"), "{lines:#?}");
    assert!(!body.contains("# Remote rollout"), "the title shows once");
}

#[test]
fn a_terminal_too_small_for_the_buffer_says_so() {
    let mut buffer = Buffer::new();
    let input = InputState::new();
    let status = StatusCtx::default();
    let area = Rect::new(0, 0, 40, 5);
    let regions = crate::layout::split(
        area,
        crate::layout::Heights {
            header: 1,
            activity: 0,
            input: crate::layout::INPUT_MIN_LINES,
            footer: 1,
        },
    );
    assert_eq!(regions.buffer.height, 0);
    let backend = TestBackend::new(area.width, area.height);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut captured: std::collections::BTreeMap<usize, Vec<CapturedCell>> =
        std::collections::BTreeMap::new();
    terminal
        .draw(|frame| {
            render(
                frame,
                regions,
                &mut buffer,
                &input,
                None,
                &status,
                None,
                &mut captured,
                None,
                &[],
            );
        })
        .unwrap();
    let buf = terminal.backend().buffer();
    let rows: Vec<String> = (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect();
    assert!(
        rows.iter().any(|r| r.contains("terminal too small")),
        "{rows:#?}"
    );
}
