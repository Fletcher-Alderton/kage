//! LLM provider abstraction and built-in implementations.
//!
//! Layering: depends only on `kage-core`.
//!
//! Synchronous, iterator-based streaming. Implementations use `ureq` for
//! HTTP and parse SSE responses by reading the body line by line. There
//! is no tokio, no async-trait, and no `Pin<Box<dyn Stream>>`.

pub mod anthropic;
pub mod bearer;
pub mod cancelable;
pub mod catalog;
pub mod compat;
pub mod error;
pub mod event;
pub mod gemini;
pub mod headers;
pub mod http;
pub mod images;
pub mod interrupt;
pub mod media;
pub mod metadata;
mod multipart;
pub mod openai;
pub mod openai_responses;
pub mod registry;
pub mod request;
pub mod sse;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use bearer::{BearerSource, SharedBearer};
pub use cancelable::{cancellable_call, make_cancelable};
pub use catalog::{ModelInfo, ProviderInfo};
pub use error::ProviderError;
pub use event::{ProviderEvent, StopReason};
pub use interrupt::KillRegistry;
pub use kage_core::ToolSpec;
pub use metadata::{ProviderMetadata, ProviderModel};
pub use registry::{ProviderRegistry, ResolvedProvider};
pub use request::{StreamRequest, ThinkingConfig, ThinkingLevel};

use kage_core::{CancelFlag, ModelCost};

/// Per-million-token prices for `model` as `provider` serves it. A
/// provider that declares its own models (custom and plugin providers)
/// is priced from those entries alone, even when its id matches a
/// catalog provider. One that declares none is priced from the catalog
/// entry under its id. `None` when the price is unknown.
#[must_use]
pub fn model_cost(provider: &dyn Provider, model: &str) -> Option<ModelCost> {
    let declared = provider.models();
    if declared.is_empty() {
        return catalog::model(&provider.metadata().id, model)?.cost;
    }
    declared.into_iter().find(|m| m.id == model)?.cost
}

/// Boxed iterator yielded by [`Provider::stream`].
///
/// Iterating it blocks on the next event. Dropping the iterator before it
/// is exhausted aborts the underlying request. The iterator is `Send` so
/// callers may run it on a worker thread.
pub type EventStream =
    Box<dyn Iterator<Item = Result<ProviderEvent, ProviderError>> + Send + 'static>;

/// LLM provider abstraction.
///
/// Implementations block synchronously inside [`Provider::stream`] until
/// the request has been accepted, then return an iterator the caller
/// drains for events. Built-in providers wrap their stream in
/// [`make_cancelable`], so setting `cancel` ends the iterator even in the
/// middle of a blocking read.
pub trait Provider: Send + Sync + std::fmt::Debug {
    /// Static metadata describing this provider.
    fn metadata(&self) -> &ProviderMetadata;

    /// Issue a streaming request.
    ///
    /// Errors raised here are setup errors (auth, malformed request,
    /// unknown model). Errors that happen mid-stream are yielded as
    /// `Err(ProviderError)` items inside the returned iterator.
    fn stream(&self, req: StreamRequest, cancel: &CancelFlag)
    -> Result<EventStream, ProviderError>;

    /// Models this provider advertises for the UI picker. Built-in
    /// providers leave this empty and let the catalog drive the picker.
    /// Custom and plugin providers return their declared model list,
    /// which takes the place of any catalog entry under the same id.
    fn models(&self) -> Vec<ProviderModel> {
        Vec::new()
    }

    /// Whether this provider sends `Content::Thinking` blocks back to
    /// its upstream itself. When `true`, the loop skips the
    /// flatten-to-`<thinking>` rewrite and the provider decides per
    /// block: native with the signature its model produced, native
    /// without one where the backend needs it, or dropped. Default is
    /// `false` for providers that drop or reject thinking blocks.
    fn preserves_thinking(&self) -> bool {
        false
    }
}
