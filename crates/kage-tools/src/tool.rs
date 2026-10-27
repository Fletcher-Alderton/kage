//! [`Tool`] trait and per-call execution context.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use kage_core::{CancelFlag, Message, Risk, ToolCallId, ToolOutput, ToolUpdate};

use crate::ToolError;
use crate::path::{resolve, resolve_under};

/// Sink the dispatcher hands to a [`ToolContext`] so a long-running tool
/// can stream progress without blocking on the loop's emit closure.
///
/// Implementations must be `Send + Sync` because parallel dispatch hands
/// the same sink to multiple worker threads. The loop's built-in sink
/// translates each call into a [`kage_core::LoopEvent::ToolUpdate`].
pub trait ProgressSink: Send + Sync {
    /// Report one progress payload to the dispatcher.
    fn emit(&self, update: ToolUpdate);
}

/// Per-tool override for dispatch ordering.
///
/// The loop runs a batch of tool calls in parallel when its
/// `parallel_tools` flag is set and no tool in the batch returns
/// [`ExecMode::Sequential`], so two such tools never race for a shared
/// resource (e.g. the user's terminal in `bash`). A batch in which every
/// tool returns [`ExecMode::Parallel`] runs in parallel even when the
/// flag is off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecMode {
    /// Run sequentially regardless of the loop-wide parallel flag.
    Sequential,
    /// Safe to run alongside other calls. A batch made only of such calls
    /// runs in parallel regardless of the loop-wide parallel flag. In a
    /// mixed batch this has the same effect as returning `None`.
    Parallel,
}

/// One tool the agent can invoke.
///
/// Implementations are synchronous, `Send + Sync`, and `Debug` so they can
/// live inside `Arc<dyn Tool>` registry entries. They build a JSON Schema
/// describing their input via [`Tool::schema`] (typically derived through
/// `schemars` on a strongly-typed input struct), declare a coarse risk
/// classification, and run blocking work inside [`Tool::execute`].
pub trait Tool: Send + Sync + std::fmt::Debug {
    /// Stable name. Lowercase `snake_case`, matches what the model invokes.
    fn name(&self) -> &str;

    /// Short, model-readable description. Shown to the model verbatim.
    fn description(&self) -> &str;

    /// JSON Schema describing the tool's input shape.
    fn schema(&self) -> serde_json::Value;

    /// Coarse risk classification used by hosts to gate permission prompts.
    fn risk(&self) -> Risk;

    /// Override the loop's parallel-vs-sequential dispatch decision.
    ///
    /// Returning `Some(ExecMode::Sequential)` forces any batch that
    /// includes this tool to run sequentially, even when the loop's
    /// `parallel_tools` flag is set. Use this for tools that own a
    /// non-shareable resource (the user's terminal, a global lock).
    /// Returning `Some(ExecMode::Parallel)` lets a batch made only of such
    /// tools run in parallel even when the flag is off. Use this for tools
    /// that mostly wait and share nothing (a delegated agent).
    /// Default `None` means "follow the loop config."
    fn execution_mode(&self) -> Option<ExecMode> {
        None
    }

    /// Whether a call must be the only tool call in its message. The
    /// loop errors such a call when the batch holds anything else
    /// instead of running it beside other calls. Use this for a tool
    /// that blocks until a whole fleet of children settles. Default
    /// `false`.
    fn runs_alone(&self) -> bool {
        false
    }

    /// Run the tool, returning either a structured output or a typed error.
    ///
    /// User-visible failures (file missing, command exited non-zero, etc.)
    /// should be reported by returning `Ok(ToolOutput { is_error: true, .. })`.
    /// Reserve [`ToolError`] for issues the loop needs to surface differently.
    fn execute(
        &self,
        input: serde_json::Value,
        cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError>;
}

/// Per-call execution context handed to [`Tool::execute`].
///
/// Carries the working directory the tool must respect, a cancellation flag
/// the tool should poll at safe points for long-running work, an optional
/// [`ProgressSink`] long-running tools call to stream progress, the
/// opt-in path-confinement flag that switches [`ToolContext::resolve_path`]
/// to escape-checked resolution, the id of the call being served when
/// the dispatcher knows it, and the conversation so far when the
/// dispatcher hands it over.
#[derive(Clone)]
pub struct ToolContext<'a> {
    workdir: &'a Path,
    cancel: &'a CancelFlag,
    progress: Option<Arc<dyn ProgressSink>>,
    confine: bool,
    call_id: Option<&'a ToolCallId>,
    history: &'a [Arc<Message>],
}

impl std::fmt::Debug for ToolContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolContext")
            .field("workdir", &self.workdir)
            .field("has_progress", &self.progress.is_some())
            .field("confine", &self.confine)
            .field("call_id", &self.call_id)
            .field("history", &self.history.len())
            .finish_non_exhaustive()
    }
}

impl<'a> ToolContext<'a> {
    /// Construct a context with no progress sink, no path confinement,
    /// no call id and no history.
    #[must_use]
    pub fn new(workdir: &'a Path, cancel: &'a CancelFlag) -> Self {
        Self {
            workdir,
            cancel,
            progress: None,
            confine: false,
            call_id: None,
            history: &[],
        }
    }

    /// Name the tool call this context serves, so [`Self::call_id`]
    /// returns it.
    #[must_use]
    pub fn with_call_id(mut self, id: &'a ToolCallId) -> Self {
        self.call_id = Some(id);
        self
    }

    /// Hand over the conversation so far, so a tool can read what it
    /// holds, such as the images in it.
    #[must_use]
    pub fn with_history(mut self, history: &'a [Arc<Message>]) -> Self {
        self.history = history;
        self
    }

    /// Attach a progress sink so [`Self::update`] can emit mid-execution
    /// progress events. Without this the method is a no-op.
    #[must_use]
    pub fn with_progress(mut self, sink: Arc<dyn ProgressSink>) -> Self {
        self.progress = Some(sink);
        self
    }

    /// Switch [`Self::resolve_path`] to escape-checked resolution so
    /// every path the tool touches stays under the workdir. Off by
    /// default: the historical behavior resolves against the workdir
    /// but accepts absolute paths and `..` traversals that escape it.
    #[must_use]
    pub fn with_confine(mut self) -> Self {
        self.confine = true;
        self
    }

    /// The directory tools must scope all filesystem operations under.
    #[must_use]
    pub fn workdir(&self) -> &Path {
        self.workdir
    }

    /// Whether the caller requested path confinement.
    #[must_use]
    pub fn is_confined(&self) -> bool {
        self.confine
    }

    /// The id of the tool call this context serves. The loop always sets
    /// it. `None` when a caller built the context without one.
    #[must_use]
    pub fn call_id(&self) -> Option<&ToolCallId> {
        self.call_id
    }

    /// The conversation so far, oldest first. Empty when the caller
    /// handed none over.
    #[must_use]
    pub fn history(&self) -> &[Arc<Message>] {
        self.history
    }

    /// Resolve `candidate` against the context workdir. Unconfined
    /// contexts use [`crate::path::resolve`]; confined ones use
    /// [`crate::path::resolve_under`], which rejects `..` traversals,
    /// outside absolute paths, and escaping symlinks with
    /// [`ToolError::Path`].
    ///
    /// # Errors
    ///
    /// Whatever the selected resolution entry point returns; under
    /// confinement that includes the escape rejection.
    pub fn resolve_path(&self, candidate: &Path) -> Result<PathBuf, ToolError> {
        if self.confine {
            resolve_under(self.workdir, candidate)
        } else {
            resolve(self.workdir, candidate)
        }
    }

    /// Whether the caller has requested cancellation.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Return the cancellation flag, for tools that need to clone or share it.
    #[must_use]
    pub fn cancel_flag(&self) -> &CancelFlag {
        self.cancel
    }

    /// The attached progress sink, for a tool whose progress arrives on
    /// another thread that cannot borrow this context. `None` when no
    /// sink is attached.
    #[must_use]
    pub fn progress_sink(&self) -> Option<Arc<dyn ProgressSink>> {
        self.progress.clone()
    }

    /// Report mid-execution progress to the host. Long-running tools call
    /// this with structured progress (e.g. `12/45 crates compiled`); the
    /// dispatcher wraps each call in a `LoopEvent::ToolUpdate`. Without an
    /// attached sink this is a no-op so unit tests do not need to wire one.
    pub fn update(&self, update: ToolUpdate) {
        if let Some(sink) = &self.progress {
            sink.emit(update);
        }
    }
}
