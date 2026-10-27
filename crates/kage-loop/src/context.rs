//! Conversation state carried through one [`run`](crate::run()) invocation.

use std::path::PathBuf;
use std::sync::Arc;

use kage_core::{Message, TokenUsage};
use serde::{Deserialize, Serialize};

/// Running token totals consumed and produced over the life of an agent run.
///
/// Updated after every assistant turn from the provider-reported [`TokenUsage`].
/// The cumulative `used_*` fields are session-wide sums for cost and audit
/// purposes; [`Self::current_context`] is the most recent turn's
/// `input + output + cache_read + cache_write` and is what the compaction
/// threshold and the modeline percentage compare against the model's
/// context window. The two are different because each turn's prompt
/// already carries the entire prior conversation, so summing prompt sizes
/// across turns would count history again on every turn. The `OpenCode`
/// project takes the same per-turn snapshot approach in
/// `session/overflow.ts`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct TokenBudget {
    /// Cumulative uncached input tokens across all turns.
    pub used_input: u64,
    /// Cumulative output tokens emitted across all turns.
    pub used_output: u64,
    /// Cumulative cache-read tokens (counts against input but is cheaper).
    pub used_cache_read: u64,
    /// Cumulative cache-write tokens.
    pub used_cache_write: u64,
    /// Approximate active-context fill from the most recent turn:
    /// `input + output + cache_read + cache_write` of that single
    /// turn. Compaction and the modeline percentage compare this to
    /// the model's context window.
    pub current_context: u64,
}

impl TokenBudget {
    /// Sum input and output usage. Cache reads/writes are not double-counted.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.used_input.saturating_add(self.used_output)
    }

    /// Fold one turn's [`TokenUsage`] into the running totals and
    /// snapshot the per-turn context fill into [`Self::current_context`].
    pub fn add(&mut self, usage: TokenUsage) {
        self.used_input = self.used_input.saturating_add(usage.input);
        self.used_output = self.used_output.saturating_add(usage.output);
        self.used_cache_read = self.used_cache_read.saturating_add(usage.cache_read);
        self.used_cache_write = self.used_cache_write.saturating_add(usage.cache_write);
        self.current_context = usage
            .input
            .saturating_add(usage.output)
            .saturating_add(usage.cache_read)
            .saturating_add(usage.cache_write);
    }
}

/// Mutable state threaded through one agent run.
///
/// The loop appends to `history` after every turn, updates `budget` from the
/// provider's reported usage, and reads `model` + `system_prompt` to build
/// each provider request. Hosts may inspect or mutate the context between
/// calls to [`run`](crate::run()); during a run the loop owns it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentContext {
    /// Ordered conversation history. Last entry is typically the most recent
    /// user turn before [`run`](crate::run()) is invoked. Messages are
    /// shared, so building a request or announcing an append costs
    /// refcounts instead of deep copies.
    pub history: Vec<Arc<Message>>,
    /// Provider-qualified model id, for example `anthropic:claude-sonnet-4-6`.
    pub model: String,
    /// System prompt prepended to every model call.
    pub system_prompt: String,
    /// Working directory all filesystem-touching tools must scope under.
    pub workdir: PathBuf,
    /// Effective context window for the active model, in tokens. Used
    /// together with [`crate::LoopConfig::compaction_threshold`] to decide
    /// when to summarize older turns. Default is 200,000.
    pub context_window: u64,
    /// Maximum output tokens per turn for the active model. When set,
    /// the loop forwards this on every [`kage_provider::StreamRequest`]
    /// so the provider does not silently truncate large responses
    /// (typically tool calls with bulky JSON arguments) at its own
    /// conservative default. `None` defers to the provider default.
    pub max_output_tokens: Option<u32>,
    /// Thinking level the user chose. `None` asks for the automatic
    /// level: high, or the nearest level the model accepts. Fitted to
    /// [`Self::reasoning`] and forwarded on every
    /// [`kage_provider::StreamRequest`].
    pub thinking_level: Option<kage_provider::ThinkingLevel>,
    /// Thinking settings the active model accepts, set by the host from
    /// the model catalog. [`kage_core::Reasoning::Unknown`] sends a
    /// chosen level unchanged and no automatic level.
    pub reasoning: kage_core::Reasoning,
    /// Media the active model produces besides text, set by the host
    /// from the model catalog and forwarded on every request. Empty
    /// means text only.
    pub produces: kage_core::Modalities,
    /// Media the active model reads besides text, set by the host from
    /// the model catalog and forwarded on every request. Empty means
    /// not known, and the user's audio goes as a note.
    pub accepts: kage_core::Modalities,
    /// Path confinement for the built-in file tools: when `true`, every
    /// [`kage_tools::ToolContext`] the dispatcher builds resolves paths
    /// through escape-checked resolution, so reads and writes stay under
    /// `workdir`. Defaults to `true`; [`Self::without_confine_paths`]
    /// restores the historical unconfined resolution when the config
    /// disables confinement.
    pub confine_paths: bool,
    /// Running token totals.
    pub budget: TokenBudget,
    /// Id of the session this conversation belongs to, forwarded on
    /// every [`kage_provider::StreamRequest`] for gateways that route
    /// by conversation. `None` outside a session.
    pub session: Option<String>,
}

impl AgentContext {
    /// Construct a fresh context with empty history, zero budget, and the
    /// process current working directory as `workdir`.
    ///
    /// When `current_dir` fails (deleted or unreadable CWD) this falls
    /// back to the relative `.`, which [`run`](crate::run()) refuses, so
    /// hosts should prefer [`Self::try_new`], which surfaces the failure
    /// instead. Kept for tests and hosts that set an explicit
    /// [`Self::with_workdir`] afterwards.
    #[must_use]
    pub fn new(model: impl Into<String>, system_prompt: impl Into<String>) -> Self {
        Self {
            history: Vec::new(),
            model: model.into(),
            system_prompt: system_prompt.into(),
            workdir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            context_window: 200_000,
            max_output_tokens: None,
            thinking_level: None,
            reasoning: kage_core::Reasoning::Unknown,
            produces: kage_core::Modalities::default(),
            accepts: kage_core::Modalities::default(),
            confine_paths: true,
            budget: TokenBudget::default(),
            session: None,
        }
    }

    /// Like [`Self::new`], but propagates a failing `current_dir` as an
    /// I/O error instead of falling back to the relative `.`. Hosts
    /// building the main-session context should use this so a broken
    /// working directory fails at startup with a clear cause.
    ///
    /// # Errors
    ///
    /// Whatever [`std::env::current_dir`] returns when the process
    /// working directory cannot be read.
    pub fn try_new(
        model: impl Into<String>,
        system_prompt: impl Into<String>,
    ) -> std::io::Result<Self> {
        let workdir = std::env::current_dir()?;
        Ok(Self {
            workdir,
            ..Self::new(model, system_prompt)
        })
    }

    /// Override the working directory.
    #[must_use]
    pub fn with_workdir(mut self, workdir: impl Into<PathBuf>) -> Self {
        self.workdir = workdir.into();
        self
    }

    /// Override the model's context window in tokens.
    #[must_use]
    pub fn with_context_window(mut self, window: u64) -> Self {
        self.context_window = window;
        self
    }

    /// Override the per-turn output token cap forwarded to the provider.
    #[must_use]
    pub fn with_max_output_tokens(mut self, tokens: u32) -> Self {
        self.max_output_tokens = Some(tokens);
        self
    }

    /// Override the active thinking level forwarded to the provider
    /// on every turn.
    #[must_use]
    pub fn with_thinking_level(mut self, level: kage_provider::ThinkingLevel) -> Self {
        self.thinking_level = Some(level);
        self
    }

    /// Switch tool dispatch to escape-checked path resolution so
    /// built-in file tools stay under `workdir`.
    #[must_use]
    pub fn with_confine_paths(mut self) -> Self {
        self.confine_paths = true;
        self
    }

    /// Restore the historical unconfined path resolution so built-in
    /// file tools accept absolute paths and `..` traversals that escape
    /// `workdir`. Hosts call this when the config disables confinement.
    #[must_use]
    pub fn without_confine_paths(mut self) -> Self {
        self.confine_paths = false;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_add_accumulates() {
        let mut b = TokenBudget::default();
        b.add(TokenUsage {
            input: 100,
            output: 50,
            cache_read: 10,
            cache_write: 5,
        });
        b.add(TokenUsage {
            input: 200,
            output: 75,
            cache_read: 20,
            cache_write: 0,
        });
        assert_eq!(b.used_input, 300);
        assert_eq!(b.used_output, 125);
        assert_eq!(b.used_cache_read, 30);
        assert_eq!(b.used_cache_write, 5);
        assert_eq!(b.total(), 425);
    }

    #[test]
    fn budget_add_saturates_on_overflow() {
        let mut b = TokenBudget {
            used_input: u64::MAX - 5,
            ..Default::default()
        };
        b.add(TokenUsage {
            input: 100,
            output: 0,
            cache_read: 0,
            cache_write: 0,
        });
        assert_eq!(b.used_input, u64::MAX);
    }

    #[test]
    fn agent_context_new_starts_empty() {
        let cx = AgentContext::new("anthropic:claude-sonnet-4-6", "you are helpful");
        assert_eq!(cx.model, "anthropic:claude-sonnet-4-6");
        assert_eq!(cx.system_prompt, "you are helpful");
        assert!(cx.history.is_empty());
        assert_eq!(cx.budget, TokenBudget::default());
    }

    /// `try_new` yields the same shape as `new` but with a verified
    /// absolute workdir. (A deleted-CWD failure cannot be exercised
    /// portably here: `set_current_dir` is process-wide and tests run in
    /// parallel.)
    #[test]
    fn try_new_succeeds_with_an_absolute_workdir() {
        let cx = AgentContext::try_new("m", "").expect("current_dir is readable");
        assert!(cx.workdir.is_absolute());
        assert_eq!(cx.model, "m");
        assert!(cx.history.is_empty());
    }

    #[test]
    fn agent_context_confines_paths_by_default() {
        let cx = AgentContext::new("m", "");
        assert!(cx.confine_paths);
        assert!(!cx.clone().without_confine_paths().confine_paths);
        assert!(
            cx.without_confine_paths()
                .with_confine_paths()
                .confine_paths
        );
    }

    /// Pins the subagent semantics the engine builds on: a child copies
    /// the parent's flag instead of the constructor default, and a fresh
    /// worktree forces confinement on top of the copy.
    #[test]
    fn subagent_inherits_parent_confinement_and_worktree_forces_it() {
        let parent = AgentContext::new("m", "");
        assert!(parent.confine_paths, "the default parent is confined");

        let mut child = AgentContext::new("m", "");
        child.confine_paths = parent.confine_paths;
        assert!(
            child.confine_paths,
            "non-worktree child inherits the parent"
        );

        let opted_out = parent.without_confine_paths();
        let mut child = AgentContext::new("m", "");
        child.confine_paths = opted_out.confine_paths;
        assert!(
            !child.confine_paths,
            "opted-out parent keeps its child unconfined"
        );

        child.confine_paths |= true;
        assert!(
            child.confine_paths,
            "a worktree child is confined regardless"
        );
    }
}
