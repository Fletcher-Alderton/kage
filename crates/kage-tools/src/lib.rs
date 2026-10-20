//! Tool trait, registry, and built-in tools.
//!
//! Layering: depends only on `kage-core`.

pub mod atomic;
pub mod builtin;
pub mod error;
pub mod path;
pub mod path_lock;
pub mod registry;
pub mod schema;
pub mod ssrf;
pub mod tool;

pub use builtin::{builtin_registry, configured_registry};
pub use error::ToolError;
pub use path::{resolve, resolve_under};
pub use registry::{ToolOverrides, ToolRegistry};
pub use schema::schema_for;
pub use tool::{ExecMode, ProgressSink, Tool, ToolContext};
