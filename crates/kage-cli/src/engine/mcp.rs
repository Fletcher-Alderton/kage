//! MCP restarts and list reloads off the dispatcher.

use std::sync::Arc;
use std::thread;

use kage_core::protocol::{HostEvent, McpServerInfo, NoticeLevel};
use kage_core::sync::lock;
use kage_core::{Content, Message, Role, SessionId};
use kage_mcp::{McpError, McpManager};
use kage_tools::{Tool, ToolRegistry};

use super::bus::Bus;
use super::runner::Work;
use super::{Idle, Input, notice, record_late_title, settle_working, warn_unmatched};

/// The manager back from MCP maintenance off the dispatcher, with the
/// tool changes to apply to the session's registry.
pub(super) struct McpDone {
    pub(super) session: SessionId,
    pub(super) manager: McpManager,
    pub(super) tools: ToolDelta,
    /// What an idle restart took from the session, so no run started
    /// while the manager was away.
    pub(super) idle: Option<Idle>,
}

/// The tools an MCP refresh added, replaced or removed.
pub(super) struct ToolDelta {
    removed: Vec<String>,
    changed: Vec<Arc<dyn Tool>>,
}

impl ToolDelta {
    pub(super) fn between(before: &ToolRegistry, after: &ToolRegistry) -> Self {
        let removed = before
            .names()
            .filter(|name| after.get(name).is_none())
            .map(str::to_owned)
            .collect();
        let changed = after
            .names()
            .filter_map(|name| {
                let tool = after.get(name)?;
                let same = before.get(name).is_some_and(|old| Arc::ptr_eq(old, tool));
                (!same).then(|| Arc::clone(tool))
            })
            .collect();
        Self { removed, changed }
    }

    pub(super) fn apply(self, tools: &mut ToolRegistry) {
        for name in &self.removed {
            tools.unregister(name);
        }
        for tool in self.changed {
            tools.register(tool);
        }
    }
}

impl super::Dispatcher {
    /// Restart MCP server `server` now when the session is idle, or at
    /// the next run start when a run or another restart is in flight.
    pub(super) fn restart_mcp(&mut self, id: SessionId, server: String) {
        let session = self.sessions.get_mut(&id).expect("session checked");
        if !session.mcp_restarts.contains(&server) {
            session.mcp_restarts.push(server);
        }
        if session.idle.is_some() {
            self.restart_now(id);
        }
    }

    /// Bring the session's not-yet-started MCP servers up on a worker
    /// thread. The host opened its UI first and handed the engine a
    /// [`McpManager::unstarted`]; the session stays busy until
    /// [`Self::mcp_done`] gives it back, so a prompt submitted while
    /// servers connect runs once they are up.
    pub(super) fn start_mcp(&mut self, id: SessionId) {
        let bus = Arc::clone(&self.bus);
        let session = self.sessions.get_mut(&id).expect("session checked");
        let Some(mut manager) = session.mcp.take() else {
            return;
        };
        let names: Vec<String> = manager.starting_names().map(str::to_owned).collect();
        if names.is_empty() {
            session.mcp = Some(manager);
            // Nothing left to register, so the registry is final.
            warn_unmatched(&bus, id, session.tools.unmatched_overrides());
            return;
        }
        let idle = session.idle.take();
        let before = session.tools.clone();
        let bus = Arc::clone(&self.bus);
        let tx = self.tx.clone();
        thread::spawn(move || {
            let mut tools = before.clone();
            for name in &names {
                if let Err(err) = manager.restart(name, &mut tools) {
                    restart_failed(&bus, id, name, &err);
                }
            }
            bus.publish(
                id,
                HostEvent::McpServers {
                    servers: manager.catalog(),
                },
            );
            let _ = tx.send(Input::McpDone(Box::new(McpDone {
                session: id,
                manager,
                tools: ToolDelta::between(&before, &tools),
                idle,
            })));
        });
    }

    /// Apply the idle session's pending restarts on a worker thread. The
    /// session stays busy until [`Self::mcp_done`] gives it back.
    fn restart_now(&mut self, id: SessionId) {
        let session = self.sessions.get_mut(&id).expect("session checked");
        let restarts = std::mem::take(&mut session.mcp_restarts);
        let Some(mut manager) = session.mcp.take() else {
            for name in restarts {
                restart_failed(&self.bus, id, &name, &McpError::Unknown(name.clone()));
            }
            return;
        };
        let idle = session.idle.take();
        let before = session.tools.clone();
        let bus = Arc::clone(&self.bus);
        let tx = self.tx.clone();
        thread::spawn(move || {
            let mut tools = before.clone();
            refresh_mcp(&bus, id, &mut manager, &restarts, &mut tools);
            let _ = tx.send(Input::McpDone(Box::new(McpDone {
                session: id,
                manager,
                tools: ToolDelta::between(&before, &tools),
                idle,
            })));
        });
    }

    /// Take back the MCP manager and its tool changes. After an idle
    /// restart, also give the session back and start what was submitted
    /// meanwhile: a prompt, else the restarts that arrived.
    pub(super) fn mcp_done(&mut self, done: McpDone) {
        let McpDone {
            session: id,
            manager,
            tools,
            idle,
        } = done;
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        tools.apply(&mut session.tools);
        session.mcp = Some(manager);
        // Every server has registered, so a `[tools.<name>]` table that
        // still matches nothing is a typo.
        let unmatched = session.tools.unmatched_overrides();
        warn_unmatched(&self.bus, id, unmatched);
        let Some(idle) = idle else {
            return;
        };
        session.idle = Some(idle);
        record_late_title(&self.bus, id, session);
        settle_working(&self.bus, id, session);
        let steered: Vec<Vec<Content>> = lock(&session.steering).drain(..).collect();
        for content in steered.into_iter().rev() {
            session.queued.push_front(content);
        }
        if self.shutting_down {
            return;
        }
        if let Some(content) = session.queued.pop_front() {
            self.start_run(id, Work::Prompt(Message::new(Role::User, content, None)));
        } else if !session.mcp_restarts.is_empty() {
            self.restart_now(id);
        }
    }
}

/// Restart `restarts`, then reload the lists `mcp`'s servers announced
/// changes for, updating `tools`. Publishes `McpServers` after a restart
/// or when the catalog changed, and returns the catalog.
pub(super) fn refresh_mcp(
    bus: &Bus,
    id: SessionId,
    mcp: &mut McpManager,
    restarts: &[String],
    tools: &mut ToolRegistry,
) -> Vec<McpServerInfo> {
    let before = mcp.catalog();
    for name in restarts {
        match mcp.restart(name, tools) {
            Ok(()) => notice(bus, id, NoticeLevel::Info, format!("restarted `{name}`")),
            Err(err) => restart_failed(bus, id, name, &err),
        }
    }
    for (server, err) in mcp.refresh_into(tools) {
        notice(
            bus,
            id,
            NoticeLevel::Error,
            format!("mcp `{server}`: {err}"),
        );
    }
    let catalog = mcp.catalog();
    if !restarts.is_empty() || catalog != before {
        bus.publish(
            id,
            HostEvent::McpServers {
                servers: catalog.clone(),
            },
        );
    }
    catalog
}

pub(super) fn restart_failed(bus: &Bus, id: SessionId, name: &str, err: &McpError) {
    notice(
        bus,
        id,
        NoticeLevel::Error,
        format!("mcp restart `{name}`: {err}"),
    );
}
