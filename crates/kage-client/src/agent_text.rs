//! Agent text in a user message: the `<agent>` reports and `<message>`
//! mail the engine hands a session as a prompt of its own, such as a
//! background agent's result, and the notes it adds when a mode
//! switches. A client shows them as report cards, message rows and
//! note lines instead of words the user typed.
//!
//! ```text
//! <agent name="general" session="01K6..." state="completed" run_ms="4200">
//! the reply
//! </agent>
//! ```

/// One element of agent text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentText {
    /// An agent's report.
    Report(Report),
    /// A message from another session.
    Mail {
        /// Who sent it: `kage` for a main session, else an agent's name.
        from: String,
        /// What it says.
        body: String,
    },
}

/// An agent's report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// The agent definition's name.
    pub name: String,
    /// The agent's session id.
    pub session: String,
    /// How the run stands: `started`, `completed`, `cancelled` or
    /// `failed`.
    pub state: String,
    /// The limit that ended the run: `turns`, `time` or `budget`.
    pub limit: Option<String>,
    /// How long the run took, when recorded.
    pub run_ms: Option<u64>,
    /// The reply, or the error the run ended on.
    pub body: String,
}

/// A user message's text split into the words the user typed in front
/// (empty when there are none) and the agent text after them. `None`
/// for plain words, or when an element does not read.
#[must_use]
pub fn split(text: &str) -> Option<(&str, Vec<AgentText>)> {
    let starts = |at: usize| at == 0 || text[..at].ends_with('\n');
    let first = ["<agent ", "<message "]
        .iter()
        .filter_map(|open| {
            text.match_indices(open)
                .map(|(at, _)| at)
                .find(|at| starts(*at))
        })
        .min()?;
    let mut parts = Vec::new();
    let mut rest = &text[first..];
    while !rest.is_empty() {
        let (tag, close) = if rest.starts_with("<agent ") {
            ("agent", "\n</agent>")
        } else if rest.starts_with("<message ") {
            ("message", "\n</message>")
        } else {
            return None;
        };
        let end = rest.find(close)? + close.len();
        let (head, body) = rest[..end - close.len()].split_once('\n')?;
        let attrs = head
            .strip_prefix('<')?
            .strip_prefix(tag)?
            .strip_suffix('>')?;
        let attr = |key: &str| attr(attrs, key);
        let body = body
            .replace(&format!("<\\/{tag}"), &format!("</{tag}"))
            .replace("\\\\", "\\");
        parts.push(if tag == "agent" {
            AgentText::Report(Report {
                name: attr("name")?,
                session: attr("session")?,
                state: attr("state")?,
                limit: attr("limit"),
                run_ms: attr("run_ms").and_then(|ms| ms.parse().ok()),
                body,
            })
        } else {
            AgentText::Mail {
                from: attr("from")?,
                body,
            }
        });
        rest = rest[end..].trim_start();
    }
    Some((text[..first].trim(), parts))
}

/// What a note the engine adds to the conversation as a user message
/// says, in the line a client shows instead of a user bubble: plan or
/// swarm mode switching, resumed swarm members reporting back, or a
/// goal message: the goal set for the session, or a goal check
/// sending the model back to work. `None` for anything else.
#[must_use]
pub fn engine_note(text: &str) -> Option<String> {
    const MODES: [(&str, &str); 4] = [
        ("[plan mode on]", "Plan mode on"),
        ("[plan mode off]", "Plan mode off"),
        ("[swarm mode on]", "Swarm mode on"),
        ("[swarm mode off]", "Swarm mode off"),
    ];
    let text = text.trim_start();
    if let Some((_, label)) = MODES.iter().find(|(tag, _)| text.starts_with(tag)) {
        return Some((*label).to_owned());
    }
    if text.starts_with("[swarm resume]") {
        return Some("Resumed swarm members reported back".to_owned());
    }
    let goal = text.strip_prefix("[goal]")?.trim_start();
    if let Some(set) = goal.strip_prefix("Work toward this goal: ") {
        let first = set.split_inclusive(". ").next().unwrap_or(set).trim();
        return Some(format!("Goal: {first}"));
    }
    let first = goal.split_inclusive(". ").next().unwrap_or(goal).trim();
    Some(format!("Goal check: {first}"))
}

/// The body of the summary the engine writes in place of compacted
/// history, which a replayed session carries as a user message. `None`
/// for any other text. The framing is `kage_core`'s
/// `COMPACTION_SUMMARY_PREFIX` and `COMPACTION_SUMMARY_SUFFIX`.
#[must_use]
pub fn compaction_summary(text: &str) -> Option<&str> {
    let body = text
        .strip_prefix("The conversation history before this point was compacted")?
        .split_once("<summary>")?
        .1;
    let body = body
        .rsplit_once("</summary>")
        .map_or(body, |(body, _)| body);
    Some(body.trim())
}

/// The value of `key="..."` in an attribute list.
fn attr(attrs: &str, key: &str) -> Option<String> {
    let padded = format!(" {attrs}");
    let needle = format!(" {key}=\"");
    let start = padded.find(&needle)? + needle.len();
    let value = &padded[start..];
    Some(value[..value.find('"')?].to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_burst_after_steering_splits_into_its_parts() {
        let text = "stop after this\n\n\
            <agent name=\"general\" session=\"01K6\" state=\"completed\" limit=\"turns\" \
            tools=\"2\" run_ms=\"4200\">\nall <\\/agent> done\n</agent>\n\n\
            <message from=\"kage\" session=\"01K5\">\nalso this\n</message>";
        let (words, parts) = split(text).unwrap();
        assert_eq!(words, "stop after this");
        assert_eq!(
            parts,
            [
                AgentText::Report(Report {
                    name: "general".into(),
                    session: "01K6".into(),
                    state: "completed".into(),
                    limit: Some("turns".into()),
                    run_ms: Some(4200),
                    body: "all </agent> done".into(),
                }),
                AgentText::Mail {
                    from: "kage".into(),
                    body: "also this".into(),
                },
            ]
        );
    }

    #[test]
    fn a_backslash_in_a_report_body_is_unescaped() {
        let text = r#"<agent name="a" session="s" state="completed">
C:\\Users\\me
</agent>"#;
        let (_, parts) = split(text).unwrap();
        assert_eq!(
            parts,
            [AgentText::Report(Report {
                name: "a".into(),
                session: "s".into(),
                state: "completed".into(),
                limit: None,
                run_ms: None,
                body: r"C:\Users\me".into(),
            })]
        );
    }

    #[test]
    fn a_backslash_in_a_message_body_is_unescaped() {
        let text = r#"<message from="x">
C:\\Users\\me
</message>"#;
        let (_, parts) = split(text).unwrap();
        assert_eq!(
            parts,
            [AgentText::Mail {
                from: "x".into(),
                body: r"C:\Users\me".into(),
            }]
        );
    }

    #[test]
    fn a_backslash_body_round_trips_from_the_engine() {
        let body = r"a \ b \\ c <\/agent> d </agent> e";
        let mut report = kage_core::agent_report::AgentReport::header(
            r#"<agent name="general" session="01K62W8Q3T9V5M2C7X4B1N0R6S" state="completed">"#,
        )
        .unwrap();
        report.body = body.into();
        let (_, parts) = split(&report.to_text()).unwrap();
        let [AgentText::Report(read)] = parts.as_slice() else {
            panic!("expected one report, got {parts:?}");
        };
        assert_eq!(read.body, body);
    }

    #[test]
    fn engine_notes_read_as_one_line() {
        assert_eq!(
            engine_note("[swarm mode on] Split the work early.").as_deref(),
            Some("Swarm mode on")
        );
        assert_eq!(
            engine_note("[goal] The goal is not met yet: tests pass. Keep working.").as_deref(),
            Some("Goal check: The goal is not met yet: tests pass.")
        );
        assert_eq!(engine_note("swarm mode on please"), None);
    }

    #[test]
    fn a_compaction_summary_reads_without_its_framing() {
        let text = format!(
            "{}## Goal\nShip it.{}",
            kage_core::message::COMPACTION_SUMMARY_PREFIX,
            kage_core::message::COMPACTION_SUMMARY_SUFFIX
        );
        let text = text.as_str();
        assert_eq!(compaction_summary(text), Some("## Goal\nShip it."));
        assert_eq!(compaction_summary("<summary>typed by hand</summary>"), None);
    }

    #[test]
    fn plain_words_and_broken_elements_are_not_agent_text() {
        assert_eq!(split("hello"), None);
        assert_eq!(split("say <agent name=\"x\"> inline"), None);
        assert_eq!(
            split("<agent name=\"x\" session=\"y\" state=\"completed\">\nno end"),
            None
        );
    }
}
