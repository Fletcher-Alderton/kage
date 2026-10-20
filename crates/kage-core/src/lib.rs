//! Core types, events, configuration, and errors shared across the workspace.
//!
//! Layering: workspace leaf; depends on no other `kage-*` crate.

pub mod agent_report;
pub mod agents;
pub mod audio;
pub mod build;
pub mod cancel;
pub mod catalog_url;
pub mod config;
pub mod config_edit;
pub mod error;
pub mod event;
pub mod fsutil;
pub mod highlight;
pub mod image;
pub mod keymap;
pub mod message;
pub mod modality;
pub mod model_id;
pub mod options;
pub mod permissions;
pub mod protocol;
pub mod reaper;
pub mod resource_block;
pub mod risk;
pub mod skills;
pub mod sse;
pub mod sync;
pub mod templates;
pub mod thinking;
pub mod tool_spec;
pub mod tools_config;
pub mod trust;

pub use agents::{AgentDef, AgentDefs, AgentError, AgentSource, load_agent_file, load_agents_dir};
pub use cancel::{CancelFlag, CancelWatch};
pub use catalog_url::{
    CATALOG_URL, CATALOG_URL_ENV, LOGOS_PATH, catalog_origin, catalog_url, logo_url,
};
pub use config::{
    AgentsConfig, Config, KeybindingsConfig, PluginsConfig, ProviderConfig, UiConfig,
};
pub use error::{Error, Result};
pub use event::{
    LoopError, LoopEvent, ModelCost, StopReason, TokenCost, TokenUsage, ToolMedia, ToolOutput,
    ToolUpdate,
};
pub use message::{
    Content, MAX_TOOL_RESULT_BYTES, MediaSource, Message, MessageId, Role, ThinkingSignature,
    ToolCallId, ToolResultParts, cap_tool_result, is_agent_result, tool_results,
};
pub use modality::{Modalities, Modality};
pub use model_id::{canonical_model, qualify_model, split_model};
pub use protocol::SessionId;
pub use risk::Risk;
pub use skills::{Skill, SkillError, load_skill_file, load_skills_dir};
pub use templates::{
    Template, TemplateError, load_template_file, load_templates_dir, render_template,
};
pub use thinking::{Effort, Efforts, Reasoning, ReasoningField, ThinkingLevel};
pub use tool_spec::ToolSpec;

#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::{Mutex, MutexGuard};

    /// Serializes tests that touch figment's process-global state.
    /// `Jail` mutates the process cwd, and `jail.set_env` mutates the
    /// process environment, which every config load observes.
    static PROCESS_GLOBALS: Mutex<()> = Mutex::new(());

    pub(crate) fn process_globals() -> MutexGuard<'static, ()> {
        PROCESS_GLOBALS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
