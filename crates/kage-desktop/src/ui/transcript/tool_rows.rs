//! The transcript's tool-bearing rows, drawn with egui widgets.
//!
//! One renderer per row kind that carries a tool call or its output
//! rather than prose: the single tool call with its expandable detail,
//! the collapsed run of same-family calls, the files a run changed, the
//! run's end, the todo list, the `exit_plan` review card and the
//! agent and swarm cards. The text rows live in their own module.
//!
//! Each renderer takes `&mut egui::Ui` and the data its row carries,
//! draws with the design's tokens and hands back an [`egui::Response`]
//! plus the actions the row offers, exactly as [`super::text_rows`]
//! does: the caller owns the store, the composer and the clipboard, so
//! a renderer never reaches past its own rect. The click that opens or
//! closes a detail, a body or a nested list is the one exception the
//! old view kept local, so it is applied here to the [`UiState`] the
//! caller threads in, the way the old element handlers toggled the
//! view's own state.

use crate::icons::PaintSnapped as _;
use crate::ui::forms::ShowCrisp as _;
use std::time::Duration;

use kage_client::wire::{ContentBlock, NoticeTone, ToolCallStatus};
use kage_client::{Session, Subagent, ToolCallItem, TranscriptItem};

use super::head::{self, Head, Lead, Meta, Target};
use super::model::{
    ChangeEntry, ChipTone, DiffLine, Family, Outcome, Row, RowKey, UiState, chips_for, diff_lines,
    plan_decision, tool_verb,
};
use crate::clock;
use crate::store::{PlanChoice, plan_review};
use crate::theme::{
    FAMILY_BODY_MEDIUM, FAMILY_BODY_REGULAR, FAMILY_BODY_SEMIBOLD, FAMILY_MONO_REGULAR, FS_2XS,
    FS_SM, FS_XS, Palette, R_FULL, R_LG, R_MD, SP_1, SP_2, SP_3, SP_4, SP_5, SP_6, family,
};
use crate::timing::{RunEnd, SessionTimes};
use crate::ui::agents::Phase;
use crate::ui::settings_config::{BtnTone as SmTone, btn_sm_at};

/// The turn-end row's height: the action buttons it carries.
const TURN_END_H: f32 = 24.0;
/// The design's 12px detail size.
const DETAIL_SIZE: f32 = 12.0;
/// The design's small icon size.
const ICON_SM: f32 = 14.0;
/// The design's extra small icon size.
const ICON_XS: f32 = 12.0;
/// The shell output lines a detail shows before its earlier-lines
/// toggle.
const SHELL_TAIL: usize = 12;
/// The tallest a detail body grows before it scrolls.
const DETAIL_MAX_H: f32 = 320.0;
/// The tallest a plan document grows before it scrolls.
const PLAN_DOC_MAX_H: f32 = 420.0;
/// The left indent of a detail box or an expanded list.
const DETAIL_INDENT: f32 = 22.0;
/// The detail head's vertical padding.
const DETAIL_HEAD_PY: f32 = 7.0;
/// The mono body's vertical padding inside a detail box.
const DETAIL_PRE_PY: f32 = 10.0;
/// One mono line of a detail body.
const DETAIL_LINE: f32 = 19.0;
/// The column a diff line's marker sits centered in.
const DIFF_MARKER_W: f32 = 20.0;
/// The plan card's section padding, top and bottom.
const PLAN_PY: f32 = 10.0;
/// The state column of a swarm member row.
const MEMBER_STATE_W: f32 = 92.0;
/// The changes card's side padding.
const CHANGES_PX: f32 = 12.0;
/// The gap between the changes head's items.
const CHANGES_GAP: f32 = 10.0;

/// What a tool row's actions ask the caller to do.
///
/// The renderer owns the drawing; the caller owns the store, the
/// composer, the clipboard and the workbench, so the actions travel
/// back out rather than reaching into the shell from inside a row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowAction {
    /// Show this file in the workbench.
    OpenFile(String),
    /// Show the change this tool call made in the workbench.
    OpenChange(String),
    /// Show the fetched pages in the workbench.
    OpenBrowser,
    /// Show this subagent in the workbench.
    OpenAgent(String),
    /// Show the agents list in the workbench.
    OpenAgents,
    /// Answer the plan review with the given choice and revision text.
    Review(PlanChoice, Option<String>),
    /// Stop the subagent with this session id.
    CancelAgent(String),
    /// Resume the swarm members with these session ids.
    ResumeSwarm(Vec<String>),
    /// Copy this text to the clipboard.
    Copy(String),
    /// Put this message's text in the composer to edit and resend.
    Edit(String),
    /// Retry the run with this prompt.
    Retry(String),
    /// Fork the session through this item, when named.
    Fork(Option<usize>),
    /// Rewind the session to before the prompt at this item.
    Rewind(usize),
    /// A button pressed on the `nth` image or clip of this item.
    Media(usize, usize, super::media_rows::MediaPress),
}

/// The plan card's revise field: which card's field shows, and the
/// text typed into it. The caller owns one across the transcript.
#[derive(Debug, Default, Clone)]
pub struct PlanRevise {
    /// The plan card whose revise field shows, by item index.
    pub open: Option<usize>,
    /// The text the revise field holds.
    pub text: String,
}

/// The monospace font of the design's paths and commands.
fn mono(size: f32) -> egui::FontId {
    egui::FontId::new(size, family(FAMILY_MONO_REGULAR))
}

/// The regular body font at `size`.
fn body(size: f32) -> egui::FontId {
    egui::FontId::new(size, family(FAMILY_BODY_REGULAR))
}

/// One icon of the design's catalog at its extra small size.
fn icon_xs(ui: &mut egui::Ui, icon: crate::icons::Icon, color: egui::Color32) -> egui::Response {
    crate::icons::icon(ui, icon, ICON_XS, color)
}

/// The working spinner at the small icon size.
fn spinner(ui: &mut egui::Ui, color: egui::Color32) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ICON_SM, ICON_SM), egui::Sense::hover());
    head::paint_spinner(ui, rect, color);
    response
}

/// The foreground and background one chip tone renders in.
fn chip_tone(tone: ChipTone, palette: &Palette) -> (egui::Color32, egui::Color32) {
    match tone {
        ChipTone::Neutral => (palette.muted, palette.selected),
        ChipTone::Good => (palette.diff_add, palette.diff_add_bg),
        ChipTone::Bad => (palette.diff_del, palette.diff_del_bg),
    }
}

/// The icon one tool title renders with.
fn tool_icon(title: &str) -> crate::icons::Icon {
    match title {
        "read" => crate::icons::Icon::File,
        "grep" | "find" => crate::icons::Icon::Search,
        "ls" => crate::icons::Icon::ListTree,
        "shell" => crate::icons::Icon::Terminal,
        "edit" => crate::icons::Icon::Pencil,
        "write" => crate::icons::Icon::FilePlus,
        "web_search" | "web_fetch" => crate::icons::Icon::Globe,
        "todo_list" => crate::icons::Icon::ListTodo,
        "ask_user_question" => crate::icons::Icon::MessageSquare,
        "agent" => crate::icons::Icon::Bot,
        "swarm" => crate::icons::Icon::Waypoints,
        _ => crate::icons::Icon::Zap,
    }
}

/// `text` without its ANSI escape sequences: a run of styled output
/// shows as its plain characters, never as markup.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\u{7}' || (c == '\u{1b}' && chars.peek() == Some(&'\\')) {
                        if c == '\u{1b}' {
                            chars.next();
                        }
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// A string argument of the tool input.
fn input_str<'a>(input: Option<&'a serde_json::Value>, key: &str) -> &'a str {
    input
        .and_then(|input| input.get(key))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
}

/// One result a `web_search` call returned.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

/// The results a finished `web_search` call carried as data.
fn search_results(call: &ToolCallItem) -> Option<Vec<SearchResult>> {
    if call.title != "web_search" {
        return None;
    }
    let results = call.raw_output.as_ref()?.get("results")?.as_array()?;
    Some(
        results
            .iter()
            .map(|result| {
                let text = |key: &str| result[key].as_str().unwrap_or_default().to_owned();
                SearchResult {
                    title: text("title"),
                    url: text("url"),
                    snippet: text("snippet"),
                }
            })
            .collect(),
    )
}

/// Where one todo stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodoState {
    /// Not started.
    Pending,
    /// The one in progress.
    Running,
    /// Finished.
    Done,
    /// Given up on.
    Failed,
}

/// One todo of a todos row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Todo {
    /// The todo's text.
    pub text: String,
    /// Where the todo stands.
    pub state: TodoState,
}

/// The todos of an ACP plan update: `content` and `status` per entry.
#[must_use]
pub fn todos_of_plan(entries: &[serde_json::Value]) -> Vec<Todo> {
    entries
        .iter()
        .map(|entry| Todo {
            text: entry
                .get("content")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            state: match entry.get("status").and_then(serde_json::Value::as_str) {
                Some("completed") => TodoState::Done,
                Some("in_progress") => TodoState::Running,
                _ => TodoState::Pending,
            },
        })
        .collect()
}

/// The todos a `todo_list` call wrote: `title` and `status` per entry.
#[must_use]
pub fn todos_of_call(call: &ToolCallItem) -> Option<Vec<Todo>> {
    if call.title != "todo_list" {
        return None;
    }
    let todos = call.input.as_ref()?.get("todos")?.as_array()?;
    Some(
        todos
            .iter()
            .map(|todo| Todo {
                text: todo
                    .get("title")
                    .or_else(|| todo.get("content"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                state: match todo.get("status").and_then(serde_json::Value::as_str) {
                    Some("done" | "completed") => TodoState::Done,
                    Some("in_progress") => TodoState::Running,
                    Some("failed") => TodoState::Failed,
                    _ => TodoState::Pending,
                },
            })
            .collect(),
    )
}

/// The text of a prompt: its text blocks in order, a blank line apart.
fn prompt_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(ContentBlock::as_text)
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The prompt that started the run ending at `end`, and the reply text
/// the run produced, for the turn-end actions.
fn run_texts(session: &Session, end: usize) -> (Option<String>, String) {
    let start = session.items[..end]
        .iter()
        .rposition(|item| matches!(item, TranscriptItem::User { steered: false, .. }));
    let prompt = start.and_then(|ix| match &session.items[ix] {
        TranscriptItem::User { content, .. } => Some(prompt_text(content)),
        _ => None,
    });
    let reply = session.items[start.map_or(0, |ix| ix + 1)..end]
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::Assistant { text } => Some(text.trim()),
            _ => None,
        })
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    // A run woken by agent text has no prompt of the user's to retry.
    let typed = |text: &String| {
        !text.is_empty()
            && kage_client::agent_text::split(text).is_none()
            && kage_client::agent_text::engine_note(text).is_none()
            && kage_client::agent_text::compaction_summary(text).is_none()
    };
    (prompt.filter(typed), reply)
}

/// Opens or closes one row key in the expansion set, the way the old
/// view's `toggle` did.
pub(crate) fn toggle(ui_state: &mut UiState, key: RowKey) {
    if !ui_state.expanded.remove(&key) {
        ui_state.expanded.insert(key);
    }
}

/// One tool call: state icon, verb, target, honest chips, and the
/// detail box when expanded.
pub fn render_tool(
    ui: &mut egui::Ui,
    ix: usize,
    call: &ToolCallItem,
    nested: bool,
    first_nested: bool,
    took: Option<Duration>,
    ui_state: &mut UiState,
) -> (egui::Response, Vec<RowAction>) {
    let palette = Palette::active_of(ui.ctx());
    let mut actions = Vec::new();
    let (verb, target) = tool_verb(call);
    let live_shell = call.title == "shell"
        && call.status == ToolCallStatus::InProgress
        && !call.text().is_empty();
    let expanded = ui_state.expanded.contains(&RowKey::Item(ix)) || live_shell;
    let running = matches!(
        call.status,
        ToolCallStatus::InProgress | ToolCallStatus::Pending
    );
    let failed = call.status == ToolCallStatus::Failed;
    let output = call.text();
    let diff = if call.title == "edit" || call.title == "write" {
        diff_lines(call)
    } else {
        None
    };
    let verb_color = if failed {
        palette.danger
    } else if running {
        palette.muted
    } else {
        palette.ink
    };

    let clickable = diff.is_some() || !output.is_empty();
    let lead = if failed {
        Lead::Icon(crate::icons::Icon::CircleX, palette.danger)
    } else if running {
        Lead::Spinner(palette.accent)
    } else {
        Lead::Icon(tool_icon(&call.title), palette.muted)
    };
    let mut meta = Vec::new();
    if call.status == ToolCallStatus::Pending {
        meta.push(Meta::Chip {
            text: "waiting".to_owned(),
            fg: palette.muted,
            bg: palette.hover,
        });
    }
    for mark in chips_for(call) {
        let (fg, bg) = chip_tone(mark.tone, palette);
        meta.push(Meta::Chip {
            text: mark.text,
            fg,
            bg,
        });
    }
    if failed {
        meta.push(Meta::Chip {
            text: "failed".to_owned(),
            fg: palette.danger,
            bg: palette.danger_soft,
        });
    }
    if let Some(took) = took {
        meta.push(Meta::Faint(clock::span(took)));
    }
    if nested && !first_nested {
        hairline(ui, palette);
    }
    let response = head::show(
        ui,
        Head {
            lead,
            verb,
            verb_color,
            target: Some(Target::mono(target, palette)),
            meta,
            chevron: clickable.then_some(expanded),
            nested,
        },
    );
    if response.clicked() && clickable {
        toggle(ui_state, RowKey::Item(ix));
    }

    let mut more_click = false;
    if expanded {
        ui.add_space(SP_2);
        egui::Frame::NONE
            .outer_margin(egui::Margin {
                left: DETAIL_INDENT as i8,
                right: 0,
                top: 0,
                bottom: 0,
            })
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                let path = input_str(call.input.as_ref(), "path").to_owned();
                if let Some(lines) = &diff {
                    let glyph = if call.title == "write" {
                        crate::icons::Icon::FilePlus
                    } else {
                        crate::icons::Icon::FileDiff
                    };
                    let head = DetailHead {
                        icon: glyph,
                        text: path.clone(),
                        action: Some("Review"),
                    };
                    if detail(ui, palette, Some(head), None, |ui| {
                        render_diff(ui, lines, palette)
                    })
                    .action
                    {
                        actions.push(RowAction::OpenChange(call.tool_call_id.clone()));
                    }
                } else if call.title == "shell" && !output.is_empty() {
                    let lines: Vec<&str> = output.lines().collect();
                    let all = ui_state.full_output.contains(&ix);
                    let hidden = if all {
                        0
                    } else {
                        lines.len().saturating_sub(SHELL_TAIL)
                    };
                    let command = input_str(call.input.as_ref(), "command").to_owned();
                    let shown = strip_ansi(&lines[hidden..].join("\n"));
                    let head = DetailHead {
                        icon: crate::icons::Icon::Terminal,
                        text: format!("$ {command}"),
                        action: None,
                    };
                    let more = (hidden > 0).then(|| format!("{hidden} earlier lines"));
                    let hit = detail(ui, palette, Some(head), more.as_deref(), |ui| {
                        render_pre(ui, &shown, palette);
                    });
                    more_click = hit.footer;
                } else if let Some(results) = search_results(call)
                    && !results.is_empty()
                {
                    let _ = detail(ui, palette, None, None, |ui| {
                        render_search_results(ui, &results, palette);
                    });
                } else if !output.is_empty() {
                    let head = match call.title.as_str() {
                        "read" => Some(DetailHead {
                            icon: crate::icons::Icon::File,
                            text: path.clone(),
                            action: Some("Open"),
                        }),
                        "web_fetch" => Some(DetailHead {
                            icon: crate::icons::Icon::Globe,
                            text: input_str(call.input.as_ref(), "url").to_owned(),
                            action: Some("Reader"),
                        }),
                        _ => None,
                    };
                    let hit = detail(ui, palette, head, None, |ui| {
                        render_pre(ui, &output, palette)
                    });
                    if hit.action {
                        match call.title.as_str() {
                            "read" => actions.push(RowAction::OpenFile(path)),
                            "web_fetch" => actions.push(RowAction::OpenBrowser),
                            _ => {}
                        }
                    }
                }
            });
    }
    if more_click {
        ui_state.full_output.insert(ix);
    }
    (response, actions)
}

/// The head a detail box carries: its glyph, its mono text, and the
/// action the head offers, if any.
struct DetailHead {
    icon: crate::icons::Icon,
    text: String,
    action: Option<&'static str>,
}

/// What a click in a detail box hit.
#[derive(Default)]
struct DetailHit {
    /// The head's action.
    action: bool,
    /// The footer.
    footer: bool,
}

/// The detail box under a tool row: the head, the body capped at the
/// design's 320px and scrolling past it, and an optional footer line.
fn detail(
    ui: &mut egui::Ui,
    palette: &Palette,
    head: Option<DetailHead>,
    footer: Option<&str>,
    body: impl FnOnce(&mut egui::Ui),
) -> DetailHit {
    let mut hit = DetailHit::default();
    egui::Frame::NONE
        .fill(palette.well)
        .stroke(egui::Stroke::new(1.0, palette.line))
        .corner_radius(egui::CornerRadius::same(R_LG as u8))
        .show_crisp(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            if let Some(head) = head {
                hit.action = detail_head(ui, &head, palette);
            }
            egui::ScrollArea::vertical()
                .max_height(DETAIL_MAX_H)
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    body(ui);
                });
            if let Some(footer) = footer {
                let (rect, response) = ui.allocate_exact_size(
                    egui::vec2(
                        ui.available_width(),
                        SP_3 + crate::theme::line_h(FS_XS) + SP_3,
                    ),
                    egui::Sense::click(),
                );
                let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
                response.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::WidgetType::Button, true, footer)
                });
                if response.hovered() {
                    let radius = egui::CornerRadius {
                        nw: 0,
                        ne: 0,
                        sw: R_LG as u8,
                        se: R_LG as u8,
                    };
                    ui.painter().rect_filled(rect, radius, palette.hover);
                }
                ui.painter().hline(
                    rect.x_range(),
                    rect.top() + 0.5,
                    egui::Stroke::new(1.0, palette.subtle),
                );
                let ink = if response.hovered() {
                    palette.ink
                } else {
                    palette.faint
                };
                ui.painter().text(
                    egui::pos2(rect.left() + SP_5, rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    footer,
                    egui::FontId::new(FS_XS, family(FAMILY_BODY_REGULAR)),
                    ink,
                );
                hit.footer = response.clicked();
            }
        });
    hit
}

/// A detail box's head: its glyph and mono text over the hairline, and
/// the ghost action at the right. Answers whether the action fired.
fn detail_head(ui: &mut egui::Ui, head: &DetailHead, palette: &Palette) -> bool {
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(
            ui.available_width(),
            DETAIL_HEAD_PY + crate::theme::line_h(FS_XS) + DETAIL_HEAD_PY,
        ),
        egui::Sense::hover(),
    );
    ui.painter().hline(
        rect.x_range(),
        rect.bottom() - 0.5,
        egui::Stroke::new(1.0, palette.subtle),
    );
    let mid = rect.center().y;
    crate::icons::themed_image(head.icon, ICON_XS, palette.muted).paint_snapped(
        ui,
        egui::Rect::from_min_size(
            egui::pos2(rect.left() + SP_5, mid - ICON_XS / 2.0),
            egui::vec2(ICON_XS, ICON_XS),
        ),
    );
    let mut right = rect.right() - SP_5;
    let mut fired = false;
    if let Some(label) = head.action {
        let galley = ui.painter().layout_no_wrap(
            label.to_owned(),
            egui::FontId::new(FS_XS, family(FAMILY_BODY_MEDIUM)),
            palette.ink,
        );
        let button = egui::Rect::from_min_size(
            egui::pos2(right - galley.size().x - 16.0, mid - 11.0),
            egui::vec2(galley.size().x + 16.0, 22.0),
        );
        let response = ui
            .interact(
                button,
                ui.id().with(("detail-action", &head.text)),
                egui::Sense::click(),
            )
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));
        if response.hovered() {
            ui.painter()
                .rect_filled(button, egui::CornerRadius::same(R_MD as u8), palette.hover);
        }
        ui.painter()
            .galley(button.center() - galley.size() / 2.0, galley, palette.ink);
        fired = response.clicked();
        right = button.left() - SP_4;
    }
    let x = rect.left() + SP_5 + ICON_XS + SP_4;
    let text = head::one_line(
        ui,
        head.text.clone(),
        egui::TextFormat::simple(mono(FS_XS), palette.ink),
        right - x,
    );
    ui.painter().galley(
        egui::pos2(x, crate::theme::line_y(mid, 0.0, &text)),
        text,
        palette.ink,
    );
    fired
}

/// A one pixel rule in the faintest line token, across the full width.
fn hairline(ui: &mut egui::Ui, palette: &Palette) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), egui::Sense::hover());
    ui.painter()
        .rect_filled(rect, egui::CornerRadius::ZERO, palette.subtle);
}

/// The mono body of a detail box: the design's deep well at its 12px
/// detail size.
fn render_pre(ui: &mut egui::Ui, text: &str, palette: &Palette) {
    egui::Frame::NONE
        .fill(palette.deep)
        .inner_margin(egui::Margin::symmetric(SP_5 as i8, DETAIL_PRE_PY as i8))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.add(
                egui::Label::new(
                    egui::RichText::new(text)
                        .font(mono(DETAIL_SIZE))
                        .color(palette.ink),
                )
                .selectable(true),
            );
        });
}

/// A unified diff with per-line added and removed coloring, computed
/// from the markers the delivered text carries. Only the lines inside
/// the clip are laid out, so a long diff costs what shows of it.
pub(crate) fn render_diff(ui: &mut egui::Ui, lines: &[DiffLine], palette: &Palette) {
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), lines.len() as f32 * DETAIL_LINE),
        egui::Sense::hover(),
    );
    ui.painter()
        .rect_filled(rect, egui::CornerRadius::ZERO, palette.deep);
    let clip = ui.clip_rect();
    for (n, line) in lines.iter().enumerate() {
        let row = egui::Rect::from_min_size(
            egui::pos2(rect.left(), rect.top() + n as f32 * DETAIL_LINE),
            egui::vec2(rect.width(), DETAIL_LINE),
        );
        if !clip.intersects(row) {
            continue;
        }
        let (color, bg) = match line {
            DiffLine::Add(_) => (palette.diff_add, palette.diff_add_bg),
            DiffLine::Del(_) => (palette.diff_del, palette.diff_del_bg),
            DiffLine::Head(_) => (palette.faint, palette.selected),
            DiffLine::Ctx(_) => (palette.ink, palette.deep),
        };
        ui.painter().rect_filled(row, egui::CornerRadius::ZERO, bg);
        ui.painter().text(
            egui::pos2(row.left() + DIFF_MARKER_W / 2.0, row.center().y),
            egui::Align2::CENTER_CENTER,
            line.marker(),
            mono(DETAIL_SIZE),
            palette.ghost,
        );
        let x = row.left() + DIFF_MARKER_W;
        let body = head::one_line(
            ui,
            line.body().to_owned(),
            egui::TextFormat::simple(mono(DETAIL_SIZE), color),
            row.right() - SP_5 - x,
        );
        ui.painter().galley(
            egui::pos2(x, crate::theme::line_y(row.center().y, 0.0, &body)),
            body,
            color,
        );
    }
}

/// The results of a `web_search` call: title, URL and snippet each.
fn render_search_results(ui: &mut egui::Ui, results: &[SearchResult], palette: &Palette) {
    ui.add_space(SP_2);
    for (n, result) in results.iter().enumerate() {
        if n > 0 {
            hairline(ui, palette);
        }
        egui::Frame::NONE
            .inner_margin(egui::Margin::symmetric(SP_5 as i8, 7))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing.y = 1.0;
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(&result.title)
                            .size(FS_SM)
                            .color(palette.ink_strong),
                    )
                    .wrap(),
                );
                let url = head::one_line(
                    ui,
                    result.url.clone(),
                    egui::TextFormat::simple(mono(11.0), palette.faint),
                    ui.available_width(),
                );
                let (rect, _) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), url.size().y),
                    egui::Sense::hover(),
                );
                ui.painter().galley(rect.min, url, palette.faint);
                if !result.snippet.is_empty() {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(&result.snippet)
                                .size(FS_XS)
                                .color(palette.muted),
                        )
                        .wrap(),
                    );
                }
            });
    }
    ui.add_space(SP_2);
}

/// The files a run changed, as the design's changes card: the count
/// and line totals with Review, then one row per file. Any row opens
/// that change in the workbench.
pub fn render_changes(
    ui: &mut egui::Ui,
    entries: &[ChangeEntry],
    end: usize,
) -> (egui::Response, Vec<RowAction>) {
    let palette = Palette::active_of(ui.ctx());
    let mut actions = Vec::new();
    let add: usize = entries.iter().map(|entry| entry.add).sum();
    let del: usize = entries.iter().map(|entry| entry.del).sum();
    let count = entries.len();
    let response = egui::Frame::NONE
        .fill(palette.surface)
        .stroke(egui::Stroke::new(1.0, palette.line))
        .corner_radius(egui::CornerRadius::same(R_LG as u8))
        .show_crisp(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            let (rect, _) = ui
                .allocate_exact_size(egui::vec2(ui.available_width(), 44.0), egui::Sense::hover());
            let mid = rect.center().y;
            let mut right = rect.right() - CHANGES_PX;
            if let Some(first) = entries.first() {
                let review = btn_sm_at(
                    ui,
                    egui::pos2(right, mid),
                    egui::Align2::RIGHT_CENTER,
                    ui.id().with(("changes-review", end)),
                    "Review",
                    None,
                    SmTone::Plain,
                    palette,
                );
                if review.clicked() {
                    actions.push(RowAction::OpenChange(first.call_id.clone()));
                }
                right = review.rect.left() - CHANGES_GAP;
            }
            right = paint_totals(ui, right, mid, entries.len(), add, del, palette);
            let x = paint_lead(ui, rect, crate::icons::Icon::FileDiff, palette) + CHANGES_GAP;
            let title = head::one_line(
                ui,
                format!("{count} file{} changed", if count == 1 { "" } else { "s" }),
                egui::TextFormat::simple(
                    egui::FontId::new(FS_SM, family(FAMILY_BODY_SEMIBOLD)),
                    palette.ink_strong,
                ),
                right - x,
            );
            ui.painter().galley(
                egui::pos2(x, crate::theme::line_y(mid, 0.0, &title)),
                title,
                palette.ink_strong,
            );
            for entry in entries {
                let (rect, response) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), 33.0),
                    egui::Sense::click(),
                );
                let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
                response.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &entry.path)
                });
                ui.painter().hline(
                    rect.x_range(),
                    rect.top() + 0.5,
                    egui::Stroke::new(1.0, palette.subtle),
                );
                let row = rect.with_min_y(rect.top() + 1.0);
                if response.hovered() {
                    ui.painter()
                        .rect_filled(row, egui::CornerRadius::ZERO, palette.hover);
                }
                let mid = row.center().y;
                let mut right = row.right() - CHANGES_PX;
                if entry.del > 0 {
                    right = head::paint_meta(
                        ui,
                        right,
                        mid,
                        Meta::Chip {
                            text: format!("-{}", entry.del),
                            fg: palette.diff_del,
                            bg: palette.diff_del_bg,
                        },
                        palette,
                    ) - SP_4;
                }
                right = head::paint_meta(
                    ui,
                    right,
                    mid,
                    Meta::Chip {
                        text: format!("+{}", entry.add),
                        fg: palette.diff_add,
                        bg: palette.diff_add_bg,
                    },
                    palette,
                ) - SP_4;
                if entry.created {
                    right = paint_new(ui, right, mid, palette) - SP_4;
                }
                let glyph = if entry.created {
                    crate::icons::Icon::FilePlus
                } else {
                    crate::icons::Icon::File
                };
                let x = paint_lead(ui, row, glyph, palette) + SP_4;
                let path = head::one_line(
                    ui,
                    entry.path.clone(),
                    egui::TextFormat::simple(mono(DETAIL_SIZE), palette.ink),
                    right - x,
                );
                ui.painter().galley(
                    egui::pos2(x, crate::theme::line_y(mid, 0.0, &path)),
                    path,
                    palette.ink,
                );
                if response.clicked() {
                    actions.push(RowAction::OpenChange(entry.call_id.clone()));
                }
            }
        })
        .response;
    (response, actions)
}

/// Paints a changes row's lead icon at its left padding, centered, and
/// gives back its right edge.
fn paint_lead(ui: &egui::Ui, row: egui::Rect, glyph: crate::icons::Icon, palette: &Palette) -> f32 {
    let lead = egui::Rect::from_min_size(
        egui::pos2(row.left() + CHANGES_PX, row.center().y - ICON_SM / 2.0),
        egui::vec2(ICON_SM, ICON_SM),
    );
    crate::icons::themed_image(glyph, ICON_SM, palette.faint).paint_snapped(ui, lead);
    lead.right()
}

/// Paints the changes card's line totals with their right edge at
/// `right` and gives back their left edge.
fn paint_totals(
    ui: &egui::Ui,
    right: f32,
    mid: f32,
    files: usize,
    add: usize,
    del: usize,
    palette: &Palette,
) -> f32 {
    let mut right = right;
    if files == 0 {
        return right;
    }
    if del > 0 {
        right = head::paint_meta(
            ui,
            right,
            mid,
            Meta::Chip {
                text: format!("-{del}"),
                fg: palette.diff_del,
                bg: palette.diff_del_bg,
            },
            palette,
        ) - CHANGES_GAP;
    }
    head::paint_meta(
        ui,
        right,
        mid,
        Meta::Chip {
            text: format!("+{add}"),
            fg: palette.diff_add,
            bg: palette.diff_add_bg,
        },
        palette,
    ) - CHANGES_GAP
}

/// Paints the `new` pill a created file carries, right edge at
/// `right`, and gives back its left edge.
fn paint_new(ui: &egui::Ui, right: f32, mid: f32, palette: &Palette) -> f32 {
    let galley = ui.painter().layout_no_wrap(
        "new".to_owned(),
        egui::FontId::new(10.5, family(FAMILY_BODY_REGULAR)),
        palette.ok,
    );
    let size = egui::vec2(galley.size().x + 14.0, galley.size().y + 2.0);
    let rect = egui::Rect::from_min_size(egui::pos2(right - size.x, mid - size.y / 2.0), size);
    let radius = egui::CornerRadius::same(R_FULL as u8);
    ui.painter().rect_filled(rect, radius, palette.ok_soft);
    ui.painter().rect_stroke(
        rect,
        radius,
        egui::Stroke::new(1.0, palette.ok_bd),
        egui::StrokeKind::Inside,
    );
    ui.painter()
        .galley(rect.center() - galley.size() / 2.0, galley, palette.ok);
    rect.left()
}

/// A collapsed run of same-family tool calls, expandable to list its
/// members.
pub fn render_group(
    ui: &mut egui::Ui,
    session: &Session,
    row: &Row,
    times: Option<&SessionTimes>,
    ui_state: &mut UiState,
) -> (egui::Response, Vec<RowAction>) {
    let mut actions = Vec::new();
    let Row::Group {
        key,
        label,
        family,
        members,
        failed,
        running,
        expanded,
    } = row
    else {
        return (
            ui.allocate_response(egui::Vec2::ZERO, egui::Sense::hover()),
            actions,
        );
    };
    let (key, family, failed, running, expanded) = (*key, *family, *failed, *running, *expanded);
    let palette = Palette::active_of(ui.ctx());
    let group_icon = match family {
        Family::Shell => crate::icons::Icon::Terminal,
        Family::Explore => crate::icons::Icon::Search,
    };

    let meta = if failed > 0 {
        vec![Meta::Chip {
            text: format!("{failed} failed"),
            fg: palette.danger,
            bg: palette.danger_soft,
        }]
    } else {
        Vec::new()
    };
    let response = head::show(
        ui,
        Head {
            lead: if running {
                Lead::Spinner(palette.accent)
            } else {
                Lead::Icon(group_icon, palette.muted)
            },
            verb: label.clone(),
            verb_color: if running {
                palette.ink_strong
            } else {
                palette.ink
            },
            target: None,
            meta,
            chevron: Some(expanded),
            nested: false,
        },
    );
    if response.clicked() {
        toggle(ui_state, key);
    }

    if expanded {
        ui.add_space(SP_2);
        egui::Frame::NONE
            .fill(palette.well)
            .stroke(egui::Stroke::new(1.0, palette.line))
            .corner_radius(egui::CornerRadius::same(R_LG as u8))
            .show_crisp(ui, |ui| {
                ui.set_min_width(ui.available_width());
                for (n, member) in members.iter().enumerate() {
                    if let Some(TranscriptItem::ToolCall(call)) = session.items.get(*member) {
                        let took = times.and_then(|t| t.tool(&call.tool_call_id));
                        let (_, member_actions) =
                            render_tool(ui, *member, call, true, n == 0, took, ui_state);
                        actions.extend(member_actions);
                    }
                }
            });
    }
    (response, actions)
}

/// The end of a run: when it ended and how long it ran, when this
/// client watched it, the stop outcome, and the run's actions.
pub fn render_turn_end(
    ui: &mut egui::Ui,
    session: &Session,
    ix: usize,
    outcome: Option<Outcome>,
    end: Option<RunEnd>,
    pinned: bool,
) -> (egui::Response, Vec<RowAction>) {
    let palette = Palette::active_of(ui.ctx());
    let mut actions = Vec::new();
    let (prompt, reply) = run_texts(session, ix);
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), TURN_END_H),
        egui::Sense::hover(),
    );
    let mid = rect.center().y;
    let body = egui::FontId::new(FS_2XS, family(FAMILY_BODY_REGULAR));
    let mut x = rect.left();
    let text = |ui: &egui::Ui, x: &mut f32, text: String| {
        let galley = ui
            .painter()
            .layout_no_wrap(text, body.clone(), palette.faint);
        let width = galley.size().x;
        ui.painter().galley(
            egui::pos2(*x, crate::theme::line_y(mid, 0.0, &galley)),
            galley,
            palette.faint,
        );
        *x += width + SP_4;
    };
    if let Some(end) = end {
        text(ui, &mut x, clock::time_of_day(end.at));
        text(ui, &mut x, "\u{b7}".to_owned());
        text(ui, &mut x, clock::span(end.took));
    }
    if let Some(outcome) = outcome {
        let (label, fg, bg) = match outcome {
            Outcome::Interrupted => ("interrupted", palette.muted, palette.hover),
            Outcome::Failed => ("failed", palette.danger, palette.danger_soft),
        };
        x = head::paint_chip(ui, egui::pos2(x, mid), label, fg, bg).right() + SP_4;
    }
    let mut copy = false;
    let mut retry = false;
    let mut fork = false;
    let mut rewind = false;
    if ui.rect_contains_pointer(rect) {
        let mut button = |ui: &mut egui::Ui, glyph, tip: &str| {
            let at = egui::Rect::from_min_size(
                egui::pos2(x, mid - head::ICON_BTN / 2.0),
                egui::vec2(head::ICON_BTN, head::ICON_BTN),
            );
            x += head::ICON_BTN + SP_1;
            head::icon_button(ui, at, glyph, tip, palette).clicked()
        };
        if !reply.is_empty() {
            copy = button(ui, crate::icons::Icon::Copy, "Copy response");
        }
        if prompt.is_some() {
            retry = button(ui, crate::icons::Icon::RefreshCw, "Retry");
        }
        if !pinned {
            fork = button(ui, crate::icons::Icon::GitFork, "Fork from here");
            rewind = button(ui, crate::icons::Icon::Undo2, "Rewind to here");
        }
    }
    if copy {
        actions.push(RowAction::Copy(reply));
    }
    if retry && let Some(prompt) = prompt {
        actions.push(RowAction::Retry(prompt));
    }
    if fork {
        actions.push(RowAction::Fork(Some(ix)));
    }
    if rewind {
        actions.push(RowAction::Rewind(ix));
    }
    (response, actions)
}

/// The todos row: Updated todos, the one in progress or all done, the
/// done count, and the list when expanded.
pub fn render_todos(
    ui: &mut egui::Ui,
    ix: usize,
    todos: &[Todo],
    ui_state: &mut UiState,
) -> (egui::Response, Vec<RowAction>) {
    let palette = Palette::active_of(ui.ctx());
    let expanded = ui_state.expanded.contains(&RowKey::Item(ix));
    let done = todos
        .iter()
        .filter(|todo| todo.state == TodoState::Done)
        .count();
    let complete = done == todos.len() && !todos.is_empty();
    let current = todos
        .iter()
        .find(|todo| todo.state == TodoState::Running)
        .map(|todo| todo.text.clone())
        .unwrap_or_default();

    let (fg, bg) = if complete {
        (palette.ok, palette.ok_soft)
    } else {
        (palette.muted, palette.hover)
    };
    let response = head::show(
        ui,
        Head {
            lead: Lead::Icon(crate::icons::Icon::ListTodo, palette.muted),
            verb: "Updated todos".to_owned(),
            verb_color: palette.ink,
            target: Some(Target::mono(
                if complete {
                    "all done".to_owned()
                } else {
                    current
                },
                palette,
            )),
            meta: vec![Meta::Chip {
                text: format!("{done}/{}", todos.len()),
                fg,
                bg,
            }],
            chevron: Some(expanded),
            nested: false,
        },
    );
    if response.clicked() {
        toggle(ui_state, RowKey::Item(ix));
    }

    if expanded {
        ui.add_space(SP_2);
        egui::Frame::NONE
            .fill(palette.well)
            .stroke(egui::Stroke::new(1.0, palette.line))
            .corner_radius(egui::CornerRadius::same(R_LG as u8))
            .show_crisp(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.horizontal_top(|ui| {
                    ui.add_space(DETAIL_INDENT);
                    ui.vertical(|ui| {
                        ui.add_space(SP_3);
                        for todo in todos {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 9.0;
                                ui.add_space(SP_5);
                                ui.set_min_height(20.0);
                                let text_color = match todo.state {
                                    TodoState::Done => palette.ok,
                                    TodoState::Running => palette.accent,
                                    TodoState::Failed => palette.danger,
                                    TodoState::Pending => palette.ghost,
                                };
                                if todo.state == TodoState::Running {
                                    spinner(ui, palette.accent);
                                } else {
                                    let glyph = match todo.state {
                                        TodoState::Done => crate::icons::Icon::CircleCheck,
                                        TodoState::Failed => crate::icons::Icon::CircleX,
                                        _ => crate::icons::Icon::Circle,
                                    };
                                    icon_xs(ui, glyph, text_color);
                                }
                                let mut text = egui::RichText::new(&todo.text).size(FS_SM).color(
                                    if todo.state == TodoState::Done {
                                        palette.muted
                                    } else {
                                        palette.ink
                                    },
                                );
                                if todo.state == TodoState::Done {
                                    text = text.strikethrough();
                                }
                                ui.add(egui::Label::new(text).wrap_mode(egui::TextWrapMode::Wrap));
                            });
                            ui.add_space(SP_1);
                        }
                        ui.add_space(SP_2);
                    });
                });
            });
    }
    (response, Vec::new())
}

/// The plan an `exit_plan` call presents, as the design's plan card:
/// the title from the document's first heading, the review state, the
/// markdown document while open, and the three answers while the
/// review is pending.
pub fn render_plan_card(
    ui: &mut egui::Ui,
    pipeline: &mut super::markdown::Pipeline,
    session: &Session,
    ix: usize,
    call: &ToolCallItem,
    revise: &mut PlanRevise,
    ui_state: &mut UiState,
) -> (egui::Response, Vec<RowAction>) {
    let palette = Palette::active_of(ui.ctx());
    let mut actions = Vec::new();
    let pending = plan_review(session).is_some_and(|review| review.call_id == call.tool_call_id);
    let doc = input_str(call.input.as_ref(), "plan").to_owned();
    let title = doc
        .lines()
        .find_map(|line| line.strip_prefix("# "))
        .unwrap_or("Plan")
        .to_owned();
    let state: Option<(&str, egui::Color32, egui::Color32, egui::Color32)> = if pending {
        Some((
            "Pending review",
            palette.accent,
            palette.accent_soft,
            palette.accent_bd,
        ))
    } else {
        plan_decision(session, ix).map(|choice| match choice {
            PlanChoice::Approve => ("Approved", palette.ok, palette.ok_soft, palette.ok_bd),
            PlanChoice::Revise => ("Revising", palette.warn, palette.warn_soft, palette.warn_bd),
            PlanChoice::Reject => (
                "Rejected",
                palette.danger,
                palette.danger_soft,
                palette.danger_bd,
            ),
        })
    };
    let open = pending != ui_state.expanded.contains(&RowKey::Item(ix));

    let mut approve = false;
    let mut revise_click = false;
    let mut reject = false;
    let mut submit = false;
    let mut toggled = false;
    let response = egui::Frame::NONE
        .fill(palette.surface)
        .stroke(egui::Stroke::new(1.0, palette.accent_bd))
        .corner_radius(egui::CornerRadius::same(R_LG as u8))
        .show_crisp(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            toggled = plan_head(ui, &title, state, pending, open, palette).clicked();
            if open && !doc.is_empty() {
                hairline(ui, palette);
                egui::ScrollArea::vertical()
                    .id_salt(("plan-doc", ix))
                    .max_height(PLAN_DOC_MAX_H)
                    .show(ui, |ui| {
                        egui::Frame::NONE
                            .inner_margin(egui::Margin::symmetric(SP_6 as i8, SP_5 as i8))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                pipeline.show(&format!("plan-{ix}"), &doc, false, ui);
                            });
                    });
            }
            if pending {
                hairline(ui, palette);
                let (rect, _) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), 26.0 + 2.0 * PLAN_PY),
                    egui::Sense::hover(),
                );
                let mid = rect.center().y;
                let mut right = rect.right() - CHANGES_PX;
                let answers = [
                    (
                        "Approve plan",
                        Some(crate::icons::Icon::Check),
                        SmTone::Primary,
                    ),
                    ("Revise", None, SmTone::Plain),
                    ("Reject and exit", None, SmTone::Danger),
                ];
                for (n, (label, glyph, tone)) in answers.into_iter().enumerate() {
                    let id = ui.id().with(("plan-answer", ix, n));
                    let button = btn_sm_at(
                        ui,
                        egui::pos2(right, mid),
                        egui::Align2::RIGHT_CENTER,
                        id,
                        label,
                        glyph,
                        tone,
                        palette,
                    );
                    match n {
                        0 => approve = button.clicked(),
                        1 => revise_click = button.clicked(),
                        _ => reject = button.clicked(),
                    }
                    right = button.rect.left() - SP_4;
                }
                let x = rect.left() + CHANGES_PX;
                let ask = head::one_line(
                    ui,
                    "Ready to build with this plan?".to_owned(),
                    egui::TextFormat::simple(
                        egui::FontId::new(FS_SM, family(FAMILY_BODY_SEMIBOLD)),
                        palette.ink_strong,
                    ),
                    right - x,
                );
                ui.painter().galley(
                    egui::pos2(x, crate::theme::line_y(mid, 0.0, &ask)),
                    ask,
                    palette.ink_strong,
                );
                if revise.open == Some(ix) {
                    egui::Frame::NONE
                        .inner_margin(egui::Margin {
                            left: CHANGES_PX as i8,
                            right: CHANGES_PX as i8,
                            top: 0,
                            bottom: PLAN_PY as i8,
                        })
                        .show(ui, |ui| {
                            let field = egui::TextEdit::singleline(&mut revise.text)
                                .hint_text("What should change?")
                                .desired_width(ui.available_width())
                                .font(egui::FontSelection::FontId(egui::FontId::new(
                                    FS_SM,
                                    family(FAMILY_BODY_REGULAR),
                                )));
                            let field = ui.add(field);
                            field.request_focus();
                            submit = field.lost_focus()
                                && ui.input(|i| i.key_pressed(egui::Key::Enter))
                                && !revise.text.trim().is_empty();
                        });
                }
            }
        })
        .response;
    if toggled {
        toggle(ui_state, RowKey::Item(ix));
    }
    if pending {
        if revise_click {
            revise.open = if revise.open == Some(ix) {
                None
            } else {
                Some(ix)
            };
            revise.text.clear();
        }
        if submit {
            actions.push(RowAction::Review(
                PlanChoice::Revise,
                Some(revise.text.trim().to_owned()),
            ));
            revise.open = None;
            revise.text.clear();
        }
        if approve {
            actions.push(RowAction::Review(PlanChoice::Approve, None));
        }
        if reject {
            actions.push(RowAction::Review(PlanChoice::Reject, None));
        }
    }

    (response, actions)
}

/// A plan card's head: the pen badge, the title over its note, the
/// review state and the chevron. Pending plans sit on the accent tint.
fn plan_head(
    ui: &mut egui::Ui,
    title: &str,
    state: Option<(&str, egui::Color32, egui::Color32, egui::Color32)>,
    pending: bool,
    open: bool,
    palette: &Palette,
) -> egui::Response {
    let title_h = crate::theme::line_h(FS_SM);
    let note_h = crate::theme::line_h(FS_XS);
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), title_h + note_h + 2.0 * PLAN_PY),
        egui::Sense::click(),
    );
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, title));
    let fill = if response.hovered() {
        palette.hover
    } else if pending {
        palette.accent_soft
    } else {
        palette.surface
    };
    let top = egui::CornerRadius {
        nw: R_LG as u8,
        ne: R_LG as u8,
        sw: 0,
        se: 0,
    };
    ui.painter().rect_filled(rect.shrink(1.0), top, fill);
    let mid = rect.center().y;
    let badge = egui::Rect::from_min_size(
        egui::pos2(rect.left() + CHANGES_PX, mid - 14.0),
        egui::vec2(28.0, 28.0),
    );
    ui.painter().rect_filled(
        badge,
        egui::CornerRadius::same(R_MD as u8),
        palette.accent_soft,
    );
    crate::icons::themed_image(crate::icons::Icon::PenLine, ICON_SM, palette.accent).paint_snapped(
        ui,
        egui::Rect::from_center_size(badge.center(), egui::vec2(ICON_SM, ICON_SM)),
    );
    let mut right = rect.right() - CHANGES_PX - ICON_SM;
    let chevron = if open {
        crate::icons::Icon::ChevronUp
    } else {
        crate::icons::Icon::ChevronDown
    };
    crate::icons::themed_image(chevron, ICON_SM, palette.faint).paint_snapped(
        ui,
        egui::Rect::from_min_size(
            egui::pos2(right, mid - ICON_SM / 2.0),
            egui::vec2(ICON_SM, ICON_SM),
        ),
    );
    right -= CHANGES_GAP;
    if let Some((label, fg, bg, border)) = state {
        let galley = ui.painter().layout_no_wrap(
            label.to_owned(),
            egui::FontId::new(10.5, family(FAMILY_BODY_REGULAR)),
            fg,
        );
        let size = egui::vec2(galley.size().x + 14.0, galley.size().y + 2.0);
        let pill = egui::Rect::from_min_size(egui::pos2(right - size.x, mid - size.y / 2.0), size);
        let radius = egui::CornerRadius::same(R_FULL as u8);
        ui.painter().rect_filled(pill, radius, bg);
        ui.painter().rect_stroke(
            pill,
            radius,
            egui::Stroke::new(1.0, border),
            egui::StrokeKind::Inside,
        );
        ui.painter()
            .galley(pill.center() - galley.size() / 2.0, galley, fg);
        right = pill.left() - CHANGES_GAP;
    }
    let x = badge.right() + CHANGES_GAP;
    let text_top = mid - (title_h + note_h) / 2.0;
    let heading = head::one_line(
        ui,
        title.to_owned(),
        egui::TextFormat::simple(
            egui::FontId::new(FS_SM, family(FAMILY_BODY_SEMIBOLD)),
            palette.ink_strong,
        ),
        right - x,
    );
    ui.painter().galley(
        egui::pos2(x, text_top + (title_h - heading.size().y) / 2.0),
        heading,
        palette.ink_strong,
    );
    let note = head::one_line(
        ui,
        "Plan written in plan mode".to_owned(),
        egui::TextFormat::simple(
            egui::FontId::new(FS_XS, family(FAMILY_BODY_REGULAR)),
            palette.faint,
        ),
        right - x,
    );
    ui.painter().galley(
        egui::pos2(x, text_top + title_h + (note_h - note.size().y) / 2.0),
        note,
        palette.faint,
    );
    response
}

/// What a card shows about one subagent.
#[derive(Debug, Clone, PartialEq)]
struct AgentFacts {
    id: String,
    name: String,
    task: String,
    phase: crate::ui::agents::Phase,
    reason: Option<String>,
    elapsed: Option<Duration>,
    tokens: Option<u64>,
    background: bool,
    last: Option<String>,
    result: Option<String>,
    stoppable: bool,
    item: Option<(String, u32)>,
}

/// The children the call `call_id` started, in launch order.
fn children_of<'a>(parent: &'a Session, call_id: &str) -> Vec<(&'a String, &'a Subagent)> {
    let mut children: Vec<_> = parent
        .agents
        .iter()
        .filter(|(_, agent)| agent.tool_call_id.as_deref() == Some(call_id))
        .collect();
    children.sort_by_key(|(_, agent)| agent.swarm.as_ref().map(|swarm| swarm.index));
    children
}

/// The first non-empty line of `text`, without inline code ticks.
fn first_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.replace('`', ""))
}

/// The facts of child `id` of `parent`.
fn agent_facts(
    store: &crate::store::Store,
    parent: &Session,
    id: &str,
    agent: &Subagent,
) -> AgentFacts {
    let child = store.state().session(id);
    let usage = agent.usage.as_ref();
    let phase = Phase::of(agent);
    let last = child
        .filter(|_| phase.live())
        .and_then(|child| {
            child.items.iter().rev().find_map(|item| match item {
                TranscriptItem::ToolCall(call) => Some(call),
                _ => None,
            })
        })
        .map(|call| {
            let (verb, target) = tool_verb(call);
            format!("{verb} {target}").trim().to_owned()
        });
    let watched = child.filter(|_| !phase.live()).and_then(|child| {
        child.items.iter().rev().find_map(|item| match item {
            TranscriptItem::Assistant { text } => first_line(text),
            TranscriptItem::Notice {
                tone: NoticeTone::Error,
                text,
            } => first_line(text),
            _ => None,
        })
    });
    let result = watched.or_else(|| {
        agent
            .tool_call_id
            .as_deref()
            .filter(|_| !phase.live())
            .and_then(|call| recorded_reply(parent, call, id))
    });
    AgentFacts {
        id: id.to_owned(),
        name: agent.name.clone().unwrap_or_else(|| "agent".to_owned()),
        task: agent.task.clone().unwrap_or_default(),
        phase,
        reason: agent.reason.clone(),
        elapsed: store
            .timings(&parent.id)
            .and_then(|times| times.agent(id))
            .or_else(|| {
                usage
                    .and_then(|usage| usage.run_ms)
                    .map(Duration::from_millis)
            }),
        tokens: usage
            .map(|usage| usage.input + usage.output)
            .or_else(|| child.map(|child| child.usage.used))
            .filter(|used| *used > 0),
        background: agent.background,
        last,
        result,
        stoppable: phase.live() && agent.capabilities.is_some_and(|caps| caps.cancel),
        item: agent
            .swarm
            .as_ref()
            .map(|swarm| (swarm.item.clone(), swarm.index)),
    }
}

/// The first line of the reply the parent recorded for child `id`.
fn recorded_reply(parent: &Session, call_id: &str, id: &str) -> Option<String> {
    let text = parent.items.iter().find_map(|item| match item {
        TranscriptItem::ToolCall(call) if call.tool_call_id == call_id => Some(call.text()),
        _ => None,
    })?;
    let at = text.find(&format!("session=\"{id}\""))?;
    let body = &text[at..];
    let body = &body[body.find('>')? + 1..];
    first_line(body.split("</agent>").next()?)
}

/// How a swarm's members stand, counted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct SwarmCounts {
    running: usize,
    paused: usize,
    done: usize,
    failed: usize,
    cancelled: usize,
    total: usize,
}

impl SwarmCounts {
    fn of(members: &[AgentFacts]) -> Self {
        let mut counts = Self {
            total: members.len(),
            ..Self::default()
        };
        for member in members {
            match member.phase {
                Phase::Running => counts.running += 1,
                Phase::Paused => counts.paused += 1,
                Phase::Done => counts.done += 1,
                Phase::Failed => counts.failed += 1,
                Phase::Cancelled => counts.cancelled += 1,
            }
        }
        counts
    }

    fn live(self) -> bool {
        self.running + self.paused > 0
    }
}

/// A short token count: `840`, `2.3k`, `1.2M`.
fn tokens(n: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1}k", n as f64 / 1_000.0),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

/// The design's string hash, so an avatar hue agrees with the web
/// client.
pub(crate) fn design_hash(text: &str) -> u32 {
    let mut x: i32 = 0;
    for unit in text.encode_utf16() {
        x = x.wrapping_mul(31).wrapping_add(i32::from(unit));
    }
    x.unsigned_abs()
}

/// `4s` and `2.3k tok` joined by a middle dot, from what was measured.
fn meta_line(facts: &AgentFacts) -> String {
    [
        facts.elapsed.map(clock::span),
        facts.tokens.map(|n| format!("{} tok", tokens(n))),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" \u{b7} ")
}

/// The card an `agent` or `swarm` call renders as, once the engine
/// announced the children it started; until then the call renders as
/// its plain row. `None` asks the caller to render the plain tool row.
pub fn render_agents(
    ui: &mut egui::Ui,
    store: &crate::store::Store,
    session: &Session,
    ix: usize,
    call: &ToolCallItem,
    origin: web_time::Instant,
    ui_state: &mut UiState,
) -> Option<(egui::Response, Vec<RowAction>)> {
    let members: Vec<AgentFacts> = children_of(session, &call.tool_call_id)
        .into_iter()
        .map(|(id, agent)| agent_facts(store, session, id, agent))
        .collect();
    let palette = Palette::active_of(ui.ctx());
    let mut actions = Vec::new();
    if call.title == "agent" {
        let facts = members.first()?;
        let response = agent_card(ui, ix, facts, palette, &mut actions);
        return Some((response, actions));
    }
    if members.is_empty() {
        return None;
    }
    let counts = SwarmCounts::of(&members);
    let live = counts.live();
    let open = live != ui_state.expanded.contains(&RowKey::Item(ix));
    let took = store
        .timings(&session.id)
        .and_then(|times| times.tool(&call.tool_call_id));
    let template = call
        .swarm
        .as_ref()
        .and_then(|meta| meta.template.clone())
        .or_else(|| {
            let typed = input_str(call.input.as_ref(), "prompt_template").to_owned();
            (!typed.is_empty()).then_some(typed)
        });
    let response = swarm_card(
        ui,
        ix,
        call,
        &members,
        counts,
        open,
        store.constellation().then_some(origin),
        ui_state.templates.contains(&ix),
        template.as_deref(),
        took,
        palette,
        ui_state,
        &mut actions,
    );
    Some((response, actions))
}

/// The card an `agent` call renders as: avatar, name and kind, the
/// task or the latest tool line, the state with elapsed time and
/// tokens, a stop button while it runs, and the result once it ended.
/// Painted at fixed heights, so it lays out the same in the measure
/// pass and on screen.
fn agent_card(
    ui: &mut egui::Ui,
    ix: usize,
    facts: &AgentFacts,
    palette: &Palette,
    actions: &mut Vec<RowAction>,
) -> egui::Response {
    let line = 1.618;
    let name_h = FS_SM * line;
    let sub_h = FS_XS * line;
    let head_h = (SP_4 + name_h + sub_h + SP_4).max(44.0);
    let result = facts.result.as_ref().filter(|_| !facts.phase.live());
    let result_h = if result.is_some() {
        SP_4 + sub_h + SP_4
    } else {
        0.0
    };
    let width = ui.available_width();
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(width, head_h + result_h), egui::Sense::hover());
    let radius = egui::CornerRadius::same(R_LG as u8);
    ui.painter().rect_filled(rect, radius, palette.surface);
    let head = egui::Rect::from_min_size(rect.min, egui::vec2(width, head_h));
    let response = ui
        .interact(head, ui.id().with(("agent-card", ix)), egui::Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &facts.name));
    if response.hovered() {
        let top = egui::CornerRadius {
            nw: R_LG as u8,
            ne: R_LG as u8,
            sw: if result.is_some() { 0 } else { R_LG as u8 },
            se: if result.is_some() { 0 } else { R_LG as u8 },
        };
        ui.painter().rect_filled(head, top, palette.hover);
    }
    let mid = head.center().y;
    let tile = egui::Rect::from_min_size(
        egui::pos2(head.left() + 12.0, mid - 13.0),
        egui::vec2(26.0, 26.0),
    );
    crate::ui::agents::paint_avatar(ui, tile, &facts.name, palette);
    let mut right = head.right() - 12.0;
    let mut stop = false;
    if facts.stoppable {
        let at =
            egui::Rect::from_min_size(egui::pos2(right - 24.0, mid - 12.0), egui::vec2(24.0, 24.0));
        stop =
            head::icon_button(ui, at, crate::icons::Icon::Square, "Stop agent", palette).clicked();
        right = at.left() - 10.0;
    } else {
        let at =
            egui::Rect::from_min_size(egui::pos2(right - 14.0, mid - 7.0), egui::vec2(14.0, 14.0));
        crate::icons::themed_image(crate::icons::Icon::ChevronRight, 14.0, palette.faint)
            .paint_snapped(ui, at);
        right = at.left() - 10.0;
    }
    let color = facts.phase.color(palette);
    let state = ui
        .painter()
        .layout_no_wrap(facts.phase.label().to_owned(), body(FS_XS), color);
    let meta = ui
        .painter()
        .layout_no_wrap(meta_line(facts), mono(11.0), palette.faint);
    let column_w = (12.0 + 5.0 + state.size().x).max(meta.size().x);
    let text_top = mid - (name_h + sub_h) / 2.0;
    let state_w = state.size().x;
    ui.painter().galley(
        egui::pos2(right - state_w, text_top + (name_h - state.size().y) / 2.0),
        state,
        color,
    );
    crate::icons::themed_image(facts.phase.icon(), 12.0, color).paint_snapped(
        ui,
        egui::Rect::from_min_size(
            egui::pos2(right - state_w - 5.0 - 12.0, text_top + name_h / 2.0 - 6.0),
            egui::vec2(12.0, 12.0),
        ),
    );
    let meta_w = meta.size().x;
    ui.painter().galley(
        egui::pos2(
            right - meta_w,
            text_top + name_h + (sub_h - meta.size().y) / 2.0,
        ),
        meta,
        palette.faint,
    );
    right -= column_w + 10.0;
    let x = tile.right() + 10.0;
    let (pill_bg, pill_fg, pill_text) = if facts.background {
        (palette.accent_soft, palette.accent, "background")
    } else {
        (palette.fill, palette.muted, facts.name.as_str())
    };
    let pill = ui
        .painter()
        .layout_no_wrap(pill_text.to_owned(), body(11.0), pill_fg);
    let pill_w = pill.size().x + 14.0;
    let name = head::one_line(
        ui,
        facts.name.clone(),
        egui::TextFormat::simple(
            egui::FontId::new(FS_SM, family(crate::theme::FAMILY_BODY_SEMIBOLD)),
            palette.ink_strong,
        ),
        (right - x - SP_4 - pill_w).max(0.0),
    );
    let name_w = name.size().x;
    ui.painter().galley(
        egui::pos2(x, text_top + (name_h - name.size().y) / 2.0),
        name,
        palette.ink_strong,
    );
    let pill_rect = egui::Rect::from_min_size(
        egui::pos2(x + name_w + SP_4, text_top + name_h / 2.0 - 9.0),
        egui::vec2(pill_w, 18.0),
    );
    ui.painter()
        .rect_filled(pill_rect, egui::CornerRadius::same(9), pill_bg);
    ui.painter()
        .galley(pill_rect.center() - pill.size() / 2.0, pill, pill_fg);
    let sub = match (&facts.last, facts.phase.live()) {
        (Some(last), true) => last.clone(),
        _ => facts.task.clone(),
    };
    let sub = head::one_line(
        ui,
        sub,
        egui::TextFormat::simple(body(FS_XS), palette.faint),
        (right - x).max(0.0),
    );
    ui.painter().galley(
        egui::pos2(x, text_top + name_h + (sub_h - sub.size().y) / 2.0),
        sub,
        palette.faint,
    );
    if let Some(result) = result {
        let band = egui::Rect::from_min_max(egui::pos2(rect.left(), head.bottom()), rect.max);
        ui.painter().hline(
            band.x_range(),
            band.top() + 0.5,
            egui::Stroke::new(1.0, palette.subtle),
        );
        let mid = band.center().y;
        let glyph = if facts.phase == Phase::Done {
            crate::icons::Icon::CircleCheck
        } else {
            crate::icons::Icon::CircleX
        };
        crate::icons::themed_image(glyph, 12.0, palette.muted).paint_snapped(
            ui,
            egui::Rect::from_min_size(
                egui::pos2(band.left() + 12.0, mid - 6.0),
                egui::vec2(12.0, 12.0),
            ),
        );
        let x = band.left() + 12.0 + 12.0 + SP_4;
        let text = head::one_line(
            ui,
            result.clone(),
            egui::TextFormat::simple(body(FS_XS), palette.muted),
            band.right() - 12.0 - x,
        );
        ui.painter().galley(
            egui::pos2(x, crate::theme::line_y(mid, 0.0, &text)),
            text,
            palette.muted,
        );
    }
    ui.painter().rect_stroke(
        rect,
        radius,
        egui::Stroke::new(1.0, palette.line),
        egui::StrokeKind::Inside,
    );
    if stop {
        actions.push(RowAction::CancelAgent(facts.id.clone()));
    } else if response.clicked() {
        actions.push(RowAction::OpenAgent(facts.id.clone()));
    }
    response
}

/// The card a `swarm` call renders as: the batch head with live counts,
/// the progress bar, and while open the member rows and the footer with
/// the prompt template, the summary and Retry for failed members.
#[allow(clippy::too_many_arguments)]
fn swarm_card(
    ui: &mut egui::Ui,
    ix: usize,
    call: &ToolCallItem,
    members: &[AgentFacts],
    counts: SwarmCounts,
    open: bool,
    field: Option<web_time::Instant>,
    template_open: bool,
    template: Option<&str>,
    took: Option<Duration>,
    palette: &Palette,
    ui_state: &mut UiState,
    actions: &mut Vec<RowAction>,
) -> egui::Response {
    let input = call.input.as_ref();
    let text = |key: &str| {
        input
            .and_then(|input| input.get(key))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let description = Some(text("description"))
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "Swarm".to_owned());
    let agent = text("agent");
    let sub = if agent.is_empty() {
        format!("{} agents", counts.total)
    } else {
        format!("{agent} \u{b7} {} agents", counts.total)
    };
    let mut toggle_click = false;
    let mut open_member: Option<String> = None;
    let mut template_click = false;
    let mut agents_click = false;
    let retry: Vec<String> = members
        .iter()
        .filter(|member| matches!(member.phase, Phase::Failed | Phase::Cancelled))
        .map(|member| member.id.clone())
        .collect();
    let response = egui::Frame::NONE
        .fill(palette.surface)
        .stroke(egui::Stroke::new(1.0, palette.done_bd))
        .corner_radius(egui::CornerRadius::same(R_LG as u8))
        .show_crisp(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            toggle_click = swarm_head(ui, &description, &sub, counts, open, palette).clicked();
            if let Some(origin) = field {
                hairline(ui, palette);
                let stars: Vec<crate::ui::constellation::Star> = members
                    .iter()
                    .map(|member| crate::ui::constellation::Star {
                        id: member.id.clone(),
                        item: member
                            .item
                            .clone()
                            .map_or_else(|| member.name.clone(), |(item, _)| item),
                        phase: member.phase,
                    })
                    .collect();
                egui::Frame::NONE
                    .inner_margin(egui::Margin {
                        left: 10,
                        right: 10,
                        top: 2,
                        bottom: 6,
                    })
                    .show(ui, |ui| {
                        let (asked, animate) =
                            crate::ui::constellation::show(ui, &stars, None, origin);
                        for event in asked {
                            open_member = match event {
                                crate::ui::constellation::ConstellationEvent::Open(id) => Some(id),
                            };
                        }
                        if animate {
                            ui.ctx().request_repaint();
                        }
                    });
            } else {
                paint_progress(ui, counts, palette);
            }
            if open {
                hairline(ui, palette);
                egui::ScrollArea::vertical()
                    .id_salt(("swarm-members", ix))
                    .max_height(DETAIL_MAX_H)
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.spacing_mut().item_spacing.y = 0.0;
                        for (n, member) in members.iter().enumerate() {
                            if member_row(ui, n, member, palette).clicked() {
                                open_member = Some(member.id.clone());
                            }
                        }
                    });
            }
            // A closed card ends at its field or bar, as the reference's
            // does: the template and the footer belong to the open card.
            if !open {
                return;
            }
            if template_open && let Some(template) = template {
                hairline(ui, palette);
                let mut job = egui::text::LayoutJob::default();
                for (n, part) in template.split("{{item}}").enumerate() {
                    if n > 0 {
                        job.append(
                            "{{item}}",
                            0.0,
                            egui::TextFormat::simple(mono(FS_XS), palette.done),
                        );
                    }
                    job.append(
                        part,
                        0.0,
                        egui::TextFormat::simple(mono(FS_XS), palette.muted),
                    );
                }
                egui::Frame::NONE
                    .inner_margin(egui::Margin::symmetric(CHANGES_PX as i8, SP_4 as i8))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.add(egui::Label::new(job).wrap());
                    });
            }
            hairline(ui, palette);
            let (rect, _) = ui.allocate_exact_size(
                egui::vec2(ui.available_width(), 26.0 + 2.0 * SP_4),
                egui::Sense::hover(),
            );
            let mid = rect.center().y;
            let mut left = rect.left() + CHANGES_PX;
            if template.is_some() {
                let label = if template_open {
                    "Hide template"
                } else {
                    "Prompt template"
                };
                let button = btn_sm_at(
                    ui,
                    egui::pos2(left, mid),
                    egui::Align2::LEFT_CENTER,
                    ui.id().with(("swarm-template", ix)),
                    label,
                    Some(crate::icons::Icon::File),
                    SmTone::Plain,
                    palette,
                );
                template_click = button.clicked();
                left = button.rect.right() + SP_4;
            }
            let mut right = rect.right() - CHANGES_PX;
            let agents = btn_sm_at(
                ui,
                egui::pos2(right, mid),
                egui::Align2::RIGHT_CENTER,
                ui.id().with(("swarm-agents", ix)),
                "Agents",
                Some(crate::icons::Icon::Users),
                SmTone::Plain,
                palette,
            );
            agents_click = agents.clicked();
            right = agents.rect.left() - SP_4;
            if !counts.live() && !retry.is_empty() {
                let button = btn_sm_at(
                    ui,
                    egui::pos2(right, mid),
                    egui::Align2::RIGHT_CENTER,
                    ui.id().with(("swarm-retry", ix)),
                    &format!("Retry {}", retry.len()),
                    Some(crate::icons::Icon::RefreshCw),
                    SmTone::Plain,
                    palette,
                );
                if button.clicked() {
                    actions.push(RowAction::ResumeSwarm(retry.clone()));
                }
                right = button.rect.left() - SP_4;
            }
            let mut summary = format!("{} finished", counts.done);
            if counts.failed > 0 {
                summary.push_str(&format!(", {} failed", counts.failed));
            }
            if counts.cancelled > 0 {
                summary.push_str(&format!(", {} aborted", counts.cancelled));
            }
            if let Some(took) = took {
                summary.push_str(&format!(" \u{b7} {}", clock::span(took)));
            }
            let summary = head::one_line(
                ui,
                summary,
                egui::TextFormat::simple(body(FS_XS), palette.muted),
                (right - left).max(0.0),
            );
            ui.painter().galley(
                egui::pos2(
                    right - summary.size().x,
                    crate::theme::line_y(mid, 0.0, &summary),
                ),
                summary,
                palette.muted,
            );
        })
        .response;
    if toggle_click {
        toggle(ui_state, RowKey::Item(ix));
    }
    if let Some(id) = open_member {
        actions.push(RowAction::OpenAgent(id));
    }
    if template_click && !ui_state.templates.remove(&ix) {
        ui_state.templates.insert(ix);
    }
    if agents_click {
        actions.push(RowAction::OpenAgents);
    }
    response
}

/// A swarm card's head: the waypoints badge, the description over the
/// agent count, the live count, the finished tally and the chevron.
fn swarm_head(
    ui: &mut egui::Ui,
    description: &str,
    sub: &str,
    counts: SwarmCounts,
    open: bool,
    palette: &Palette,
) -> egui::Response {
    let title_h = crate::theme::line_h(FS_SM);
    let note_h = crate::theme::line_h(FS_XS);
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(
            ui.available_width(),
            (title_h + note_h + 2.0 * SP_4).max(44.0),
        ),
        egui::Sense::click(),
    );
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, description));
    let fill = if response.hovered() {
        palette.hover
    } else {
        palette.done_soft
    };
    let top = egui::CornerRadius {
        nw: R_LG as u8,
        ne: R_LG as u8,
        sw: 0,
        se: 0,
    };
    ui.painter().rect_filled(rect.shrink(1.0), top, fill);
    let mid = rect.center().y;
    let badge = egui::Rect::from_min_size(
        egui::pos2(rect.left() + CHANGES_PX, mid - 14.0),
        egui::vec2(28.0, 28.0),
    );
    ui.painter().rect_filled(
        badge,
        egui::CornerRadius::same(R_MD as u8),
        palette.done_soft,
    );
    crate::icons::themed_image(crate::icons::Icon::Waypoints, ICON_SM, palette.done).paint_snapped(
        ui,
        egui::Rect::from_center_size(badge.center(), egui::vec2(ICON_SM, ICON_SM)),
    );
    let mut right = rect.right() - CHANGES_PX - ICON_SM;
    let chevron = if open {
        crate::icons::Icon::ChevronUp
    } else {
        crate::icons::Icon::ChevronDown
    };
    crate::icons::themed_image(chevron, ICON_SM, palette.faint).paint_snapped(
        ui,
        egui::Rect::from_min_size(
            egui::pos2(right, mid - ICON_SM / 2.0),
            egui::vec2(ICON_SM, ICON_SM),
        ),
    );
    right -= CHANGES_GAP;
    let mut job = egui::text::LayoutJob::default();
    let ended = counts.done + counts.failed + counts.cancelled;
    job.append(
        &ended.to_string(),
        0.0,
        egui::TextFormat::simple(mono(FS_SM), palette.ink_strong),
    );
    job.append(
        &format!(" / {}", counts.total),
        0.0,
        egui::TextFormat::simple(mono(FS_SM), palette.faint),
    );
    let tally = ui.painter().layout_job(job);
    right -= tally.size().x;
    ui.painter().galley(
        egui::pos2(right, crate::theme::line_y(mid, 0.0, &tally)),
        tally,
        palette.ink_strong,
    );
    right -= CHANGES_GAP;
    if counts.live() {
        let mut live = format!("{} running", counts.running);
        if counts.paused > 0 {
            live.push_str(&format!(", {} limited", counts.paused));
        }
        let width = status_width(ui, &live);
        paint_status(
            ui,
            right - width,
            mid,
            crate::icons::Icon::LoaderCircle,
            &live,
            palette.accent,
        );
        right -= width + CHANGES_GAP;
    }
    let x = badge.right() + CHANGES_GAP;
    let text_top = mid - (title_h + note_h) / 2.0;
    let heading = head::one_line(
        ui,
        description.to_owned(),
        egui::TextFormat::simple(
            egui::FontId::new(FS_SM, family(FAMILY_BODY_SEMIBOLD)),
            palette.ink_strong,
        ),
        right - x,
    );
    ui.painter().galley(
        egui::pos2(x, text_top + (title_h - heading.size().y) / 2.0),
        heading,
        palette.ink_strong,
    );
    let note = head::one_line(
        ui,
        sub.to_owned(),
        egui::TextFormat::simple(body(FS_XS), palette.faint),
        right - x,
    );
    ui.painter().galley(
        egui::pos2(x, text_top + title_h + (note_h - note.size().y) / 2.0),
        note,
        palette.faint,
    );
    response
}

/// The swarm's progress bar: done, failed and live shares of the batch
/// over the empty track.
fn paint_progress(ui: &mut egui::Ui, counts: SwarmCounts, palette: &Palette) {
    let (track, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 3.0), egui::Sense::hover());
    ui.painter()
        .rect_filled(track, egui::CornerRadius::ZERO, palette.fill);
    if counts.total == 0 {
        return;
    }
    let mut x = track.left();
    for (n, color) in [
        (counts.done, palette.ok),
        (counts.failed, palette.danger),
        (
            counts.running + counts.paused,
            palette.done.gamma_multiply(0.55),
        ),
    ] {
        let w = track.width() * n as f32 / counts.total as f32;
        if w > 0.0 {
            ui.painter().rect_filled(
                egui::Rect::from_min_size(egui::pos2(x, track.top()), egui::vec2(w, 3.0)),
                egui::CornerRadius::ZERO,
                color,
            );
            x += w;
        }
    }
}

/// One member row of an open swarm card: the index, the item, the
/// latest line or result, the tokens and the state.
fn member_row(
    ui: &mut egui::Ui,
    n: usize,
    member: &AgentFacts,
    palette: &Palette,
) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 36.0), egui::Sense::click());
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    let (item, index) = member
        .item
        .clone()
        .unwrap_or_else(|| (member.name.clone(), u32::try_from(n).unwrap_or(0)));
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &item));
    if response.hovered() {
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::ZERO, palette.hover);
    }
    if n > 0 {
        ui.painter().hline(
            rect.x_range(),
            rect.top() + 0.5,
            egui::Stroke::new(1.0, palette.subtle),
        );
    }
    let mid = rect.center().y;
    let left = rect.left() + CHANGES_PX;
    ui.painter().text(
        egui::pos2(left, mid),
        egui::Align2::LEFT_CENTER,
        format!("{:02}", index + 1),
        mono(11.0),
        palette.faint,
    );
    let state_x = rect.right() - CHANGES_PX - MEMBER_STATE_W;
    paint_status(
        ui,
        state_x,
        mid,
        member.phase.icon(),
        member.phase.label(),
        member.phase.color(palette),
    );
    let tokens_right = state_x - CHANGES_GAP;
    ui.painter().text(
        egui::pos2(tokens_right, mid),
        egui::Align2::RIGHT_CENTER,
        member.tokens.map(tokens).unwrap_or_default(),
        mono(11.0),
        palette.faint,
    );
    let x = left + 18.0 + CHANGES_GAP;
    let room = (tokens_right - 44.0 - CHANGES_GAP - x).max(0.0);
    let natural = ui
        .painter()
        .layout_no_wrap(item.clone(), body(FS_SM), palette.ink)
        .size()
        .x;
    let item_w = natural.min(room * 0.6).max(60.0_f32.min(room));
    let item = head::one_line(
        ui,
        item,
        egui::TextFormat::simple(body(FS_SM), palette.ink),
        item_w,
    );
    // The item takes at least its 60px column, so the lines start
    // together, as the reference's flex column sets them.
    let line_x = x + item.size().x.max(item_w) + CHANGES_GAP;
    ui.painter().galley(
        egui::pos2(x, crate::theme::line_y(mid, 0.0, &item)),
        item,
        palette.ink,
    );
    let line = if member.phase.live() {
        member
            .reason
            .clone()
            .or_else(|| member.last.clone())
            .unwrap_or_else(|| "starting".to_owned())
    } else {
        member.result.clone().unwrap_or_default()
    };
    let line = head::one_line(
        ui,
        line,
        egui::TextFormat::simple(mono(11.0), palette.faint),
        (x + room - line_x).max(0.0),
    );
    ui.painter().galley(
        egui::pos2(line_x, crate::theme::line_y(mid, 0.0, &line)),
        line,
        palette.faint,
    );
    response
}

/// How wide [`paint_status`] paints `label`.
fn status_width(ui: &egui::Ui, label: &str) -> f32 {
    let galley =
        ui.painter()
            .layout_no_wrap(label.to_owned(), body(FS_XS), egui::Color32::PLACEHOLDER);
    ICON_XS + 5.0 + galley.size().x
}

/// The design's status chip from `left`, centered on `mid`: the icon and
/// the label in one color.
fn paint_status(
    ui: &egui::Ui,
    left: f32,
    mid: f32,
    glyph: crate::icons::Icon,
    label: &str,
    color: egui::Color32,
) {
    crate::icons::themed_image(glyph, ICON_XS, color).paint_snapped(
        ui,
        egui::Rect::from_min_size(
            egui::pos2(left, mid - ICON_XS / 2.0),
            egui::vec2(ICON_XS, ICON_XS),
        ),
    );
    ui.painter().text(
        egui::pos2(left + ICON_XS + 5.0, mid),
        egui::Align2::LEFT_CENTER,
        label,
        body(FS_XS),
        color,
    );
}

#[cfg(test)]
mod tests {
    use kage_client::wire::{
        ContentBlock, MessageChunk, ToolCallContent, ToolCallStatus, ToolKind,
    };
    use kage_client::{Session, ToolCallItem, TranscriptItem};

    use super::{
        ChipTone, RowAction, TodoState, chip_tone, design_hash, first_line, recorded_reply,
        run_texts, search_results, strip_ansi, todos_of_call, todos_of_plan, tokens,
    };
    use crate::theme::Palette;

    /// A completed tool call with the given fields.
    fn call(
        id: &str,
        title: &str,
        kind: ToolKind,
        status: ToolCallStatus,
        content: Vec<ToolCallContent>,
        raw_output: Option<serde_json::Value>,
        input: Option<serde_json::Value>,
    ) -> ToolCallItem {
        ToolCallItem {
            tool_call_id: id.into(),
            title: title.into(),
            kind,
            status,
            input,
            swarm: None,
            content,
            raw_output,
            took_ms: None,
        }
    }

    /// Text content holding one chunk.
    fn text_chunk(text: &str) -> ToolCallContent {
        ToolCallContent::Content(MessageChunk {
            content: ContentBlock::text(text),
            meta: None,
        })
    }

    #[test]
    fn strip_ansi_drops_csi_and_osc_escapes_only() {
        assert_eq!(strip_ansi("plain"), "plain");
        assert_eq!(strip_ansi("\u{1b}[31mred\u{1b}[0m"), "red");
        assert_eq!(strip_ansi("\u{1b}]0;title\u{7}body"), "body");
        assert_eq!(
            strip_ansi("a\u{1b}b"),
            "a",
            "a lone escape eats its next byte"
        );
    }

    #[test]
    fn todos_of_call_reads_title_and_status_and_refuses_other_tools() {
        let todo_list = call(
            "t",
            "todo_list",
            ToolKind::Other,
            ToolCallStatus::Completed,
            Vec::new(),
            None,
            Some(serde_json::json!({"todos": [
                {"title": "read", "status": "done"},
                {"title": "write", "status": "in_progress"},
                {"title": "test", "status": "failed"},
                {"title": "ship", "status": "pending"},
            ]})),
        );
        let todos = todos_of_call(&todo_list).expect("the todos parse");
        let states: Vec<TodoState> = todos.iter().map(|todo| todo.state).collect();
        assert_eq!(todos[0].text, "read");
        assert_eq!(
            states,
            [
                TodoState::Done,
                TodoState::Running,
                TodoState::Failed,
                TodoState::Pending
            ]
        );
        let other = call(
            "r",
            "read",
            ToolKind::Read,
            ToolCallStatus::Completed,
            Vec::new(),
            None,
            None,
        );
        assert!(todos_of_call(&other).is_none());
    }

    #[test]
    fn todos_of_call_falls_back_to_content_when_title_is_absent() {
        let todo_list = call(
            "t",
            "todo_list",
            ToolKind::Other,
            ToolCallStatus::Completed,
            Vec::new(),
            None,
            Some(
                serde_json::json!({"todos": [{"content": "from content", "status": "completed"}]}),
            ),
        );
        let todos = todos_of_call(&todo_list).expect("the todos parse");
        assert_eq!(todos[0].text, "from content");
        assert_eq!(todos[0].state, TodoState::Done);
    }

    #[test]
    fn todos_of_plan_reads_content_and_status_per_entry() {
        let entries = vec![
            serde_json::json!({"content": "one", "status": "completed"}),
            serde_json::json!({"content": "two", "status": "in_progress"}),
            serde_json::json!({"content": "three"}),
        ];
        let todos = todos_of_plan(&entries);
        let states: Vec<TodoState> = todos.iter().map(|todo| todo.state).collect();
        assert_eq!(todos[1].text, "two");
        assert_eq!(
            states,
            [TodoState::Done, TodoState::Running, TodoState::Pending]
        );
    }

    #[test]
    fn run_texts_split_the_prompt_and_the_reply_of_a_run() {
        let session = Session {
            items: vec![
                TranscriptItem::User {
                    content: vec![ContentBlock::text("fix the bug")],
                    steered: false,
                },
                TranscriptItem::Assistant {
                    text: "looking".into(),
                },
                TranscriptItem::Assistant {
                    text: "fixed".into(),
                },
                TranscriptItem::TurnEnd {
                    reason: None,
                    at: None,
                    took_ms: None,
                },
            ],
            ..Session::new("s1")
        };
        let (prompt, reply) = run_texts(&session, 3);
        assert_eq!(prompt.as_deref(), Some("fix the bug"));
        assert_eq!(reply, "looking\n\nfixed");
    }

    #[test]
    fn run_texts_hide_a_prompt_the_engine_built_from_agent_text() {
        let session = Session {
            items: vec![
                TranscriptItem::User {
                    content: vec![ContentBlock::text(
                        "<agent name=\"general\" session=\"c1\" state=\"completed\">\nreport body\n</agent>",
                    )],
                    steered: false,
                },
                TranscriptItem::Assistant {
                    text: "done".into(),
                },
                TranscriptItem::TurnEnd {
                    reason: None,
                    at: None,
                    took_ms: None,
                },
            ],
            ..Session::new("s1")
        };
        let (prompt, reply) = run_texts(&session, 2);
        assert!(prompt.is_none(), "an engine-built prompt cannot be retried");
        assert_eq!(reply, "done");
    }

    #[test]
    fn search_results_read_the_delivered_results_of_a_web_search() {
        let search = call(
            "w",
            "web_search",
            ToolKind::Other,
            ToolCallStatus::Completed,
            Vec::new(),
            Some(serde_json::json!({"results": [
                {"title": "A", "url": "https://a", "snippet": "first"},
            ]})),
            None,
        );
        let results = search_results(&search).expect("results parse");
        assert_eq!(results[0].title, "A");
        assert_eq!(results[0].url, "https://a");
        assert_eq!(results[0].snippet, "first");
        let other = call(
            "r",
            "read",
            ToolKind::Read,
            ToolCallStatus::Completed,
            Vec::new(),
            None,
            None,
        );
        assert!(search_results(&other).is_none());
    }

    #[test]
    fn token_counts_shorten_like_the_design() {
        assert_eq!(tokens(840), "840");
        assert_eq!(tokens(2_300), "2.3k");
        assert_eq!(tokens(1_200_000), "1.2M");
    }

    #[test]
    fn the_hash_matches_the_design_so_hues_agree() {
        assert_eq!(design_hash("explore"), 1_309_148_525);
        assert_eq!(design_hash(""), 0);
    }

    #[test]
    fn first_line_trims_and_drops_inline_code_ticks() {
        assert_eq!(first_line("\n  `answer`\nmore").as_deref(), Some("answer"));
        assert_eq!(first_line("  \n  "), None);
    }

    #[test]
    fn recorded_reply_reads_the_child_report_a_call_recorded() {
        let output = "<agent name=\"general\" session=\"c1\" state=\"completed\">\nfirst `answer`\nmore\n</agent>";
        let parent = Session {
            items: vec![TranscriptItem::ToolCall(call(
                "call_s",
                "swarm",
                ToolKind::Other,
                ToolCallStatus::Completed,
                vec![text_chunk(output)],
                None,
                None,
            ))],
            ..Session::new("p")
        };
        assert_eq!(
            recorded_reply(&parent, "call_s", "c1").as_deref(),
            Some("first answer")
        );
        assert_eq!(recorded_reply(&parent, "call_s", "c3"), None);
        assert_eq!(recorded_reply(&parent, "other", "c1"), None);
    }

    #[test]
    fn chip_tones_map_to_the_design_fills() {
        let palette = Palette::shadow();
        let (good_fg, good_bg) = chip_tone(ChipTone::Good, &palette);
        assert_eq!(good_fg, palette.diff_add);
        assert_eq!(good_bg, palette.diff_add_bg);
        let (bad_fg, bad_bg) = chip_tone(ChipTone::Bad, &palette);
        assert_eq!(bad_fg, palette.diff_del);
        assert_eq!(bad_bg, palette.diff_del_bg);
    }

    #[test]
    fn row_actions_carry_their_payloads() {
        assert_eq!(
            RowAction::OpenChange("c1".to_owned()),
            RowAction::OpenChange("c1".to_owned())
        );
        assert_ne!(
            RowAction::OpenChange("c1".to_owned()),
            RowAction::OpenFile("c1".to_owned())
        );
        assert_eq!(
            RowAction::ResumeSwarm(vec!["a".to_owned()]),
            RowAction::ResumeSwarm(vec!["a".to_owned()])
        );
    }
}
