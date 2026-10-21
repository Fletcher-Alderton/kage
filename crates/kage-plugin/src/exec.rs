//! The `exec` capability: `kage.exec`.
//!
//! Attached only onto the `kage` proxy of a plugin that was granted
//! `exec` (see [`crate::capabilities`]); an ungranted plugin never
//! sees it. It spawns a subprocess directly - no shell, so there is
//! no quoting or injection surface - with its working directory
//! pinned under the host workdir via [`kage_tools::resolve_under`]
//! (the same escape check `kage.fs` uses). The call waits until the
//! process exits or `timeout_secs` elapses (default 30) and returns
//! its captured output plus `timed_out` and `truncated` flags, the way
//! `kage.http.get` does: inside a driven coroutine the wait yields, and
//! anywhere else it blocks. A rewind plugin uses it to snapshot files
//! with `git` between turns. Each stream keeps at most 1 MB; output
//! past that is dropped and the `truncated` flag is set.
//!
//! The `exec` grant is coarse and all-or-nothing: there is no command
//! allowlist and no per-command scoping. A granted plugin may run any
//! executable on the process `PATH` (or any absolute path) with any
//! arguments; the only checks are that `cmd` is non-empty and that
//! `cwd` cannot escape the host workdir. Grant it only to plugins you
//! trust to run arbitrary programs as the kage process.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use kage_core::reaper;
use kage_core::sync::lock;
use kage_tools::resolve_under;
use mlua::{Lua, Table};

use crate::capabilities::{Capability, CapabilityRegistry};
use crate::tasks;

/// How long a spawned process may run when the spec sets no
/// `timeout_secs`.
const DEFAULT_EXEC_TIMEOUT: Duration = Duration::from_secs(30);

/// How often the waiter polls the child while it runs.
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Output bytes kept per stream before the rest is dropped and the
/// result's `truncated` flag is set.
const EXEC_OUTPUT_MAX: u64 = 1024 * 1024;

/// Largest accepted `timeout_secs`. A value near `Instant`'s range
/// would overflow `Instant::now() + timeout`; anything past a month
/// is a spec mistake, not a wait.
const MAX_EXEC_TIMEOUT_SECS: u64 = 30 * 24 * 60 * 60;

/// Read one pipe up to [`EXEC_OUTPUT_MAX`], reporting whether more
/// output was dropped.
fn drain(pipe: impl Read) -> (Vec<u8>, bool) {
    let mut buf = Vec::new();
    let mut taken = pipe.take(EXEC_OUTPUT_MAX + 1);
    let _ = taken.read_to_end(&mut buf);
    let cap = usize::try_from(EXEC_OUTPUT_MAX).unwrap_or(usize::MAX);
    let truncated = buf.len() > cap;
    if truncated {
        buf.truncate(cap);
    }
    (buf, truncated)
}

/// Register the `exec` installer into `registry`.
///
/// The installer runs (via `request_capabilities`) against a granted
/// plugin's `kage` proxy and sets `exec` on it. `workdir` is the host
/// workdir; a spec `cwd` is resolved under it and may not escape.
pub(crate) fn register(registry: &CapabilityRegistry, workdir: PathBuf) {
    let mut reg = lock(registry);
    reg.entry(Capability::Exec)
        .or_default()
        .push(Box::new(move |lua: &Lua, pkage: &Table| {
            let root = workdir.clone();
            pkage.set(
                "exec",
                lua.create_async_function(move |lua, spec: Table| {
                    let root = root.clone();
                    async move {
                        let cmd: Option<String> = spec.get("cmd")?;
                        let cmd = cmd.filter(|c| !c.is_empty()).ok_or_else(|| {
                            mlua::Error::external("kage.exec: `cmd` must be a non-empty string")
                        })?;
                        let args: Vec<String> =
                            spec.get::<Option<Vec<String>>>("args")?.unwrap_or_default();
                        let cwd: Option<String> = spec.get("cwd")?;
                        let dir = match cwd {
                            Some(rel) => resolve_under(&root, Path::new(&rel)).map_err(|e| {
                                mlua::Error::external(format!("kage.exec: cwd {rel}: {e}"))
                            })?,
                            None => root.clone(),
                        };
                        let timeout_secs: Option<u64> = spec.get("timeout_secs")?;
                        if timeout_secs == Some(0) {
                            return Err(mlua::Error::external(
                                "kage.exec: `timeout_secs` must be at least 1",
                            ));
                        }
                        if timeout_secs.is_some_and(|s| s > MAX_EXEC_TIMEOUT_SECS) {
                            return Err(mlua::Error::external(format!(
                                "kage.exec: `timeout_secs` must be at most {MAX_EXEC_TIMEOUT_SECS}"
                            )));
                        }
                        let timeout = timeout_secs.map_or(DEFAULT_EXEC_TIMEOUT, |s| {
                            Duration::from_secs(s).max(Duration::from_secs(1))
                        });

                        let child = Command::new(&cmd)
                            .args(&args)
                            .current_dir(&dir)
                            .stdin(Stdio::null())
                            .stdout(Stdio::piped())
                            .stderr(Stdio::piped())
                            .spawn()
                            .map_err(|e| {
                                mlua::Error::external(format!("kage.exec: spawn {cmd}: {e}"))
                            })?;
                        let finished = if tasks::is_driven(&lua) {
                            tasks::off_thread(move || wait_for(child, timeout))
                                .await
                                .unwrap_or_else(|| Err("the waiting thread panicked".into()))
                        } else {
                            wait_for(child, timeout)
                        }
                        .map_err(|e| {
                            mlua::Error::external(format!("kage.exec: wait {cmd}: {e}"))
                        })?;
                        let out = lua.create_table()?;
                        out.set("code", finished.code)?;
                        out.set("timed_out", finished.timed_out)?;
                        out.set("truncated", finished.truncated)?;
                        out.set("stdout", lua.create_string(&finished.stdout)?)?;
                        out.set("stderr", lua.create_string(&finished.stderr)?)?;
                        Ok(out)
                    }
                })?,
            )?;
            Ok(())
        }));
}

/// How a process [`wait_for`] waited on ended.
struct Finished {
    code: i32,
    timed_out: bool,
    truncated: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Wait for `child` to exit, killing it once `timeout` passes, and
/// collect its output.
fn wait_for(mut child: Child, timeout: Duration) -> Result<Finished, String> {
    // Drain each pipe on its own thread so a child that outgrows the OS
    // pipe buffer never stalls on a full pipe while we wait for it.
    let stdout_thread = child
        .stdout
        .take()
        .map(|pipe| thread::spawn(move || drain(pipe)));
    let stderr_thread = child
        .stderr
        .take()
        .map(|pipe| thread::spawn(move || drain(pipe)));
    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        match reaper::try_wait(&mut child).map_err(|e| e.to_string())? {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                timed_out = true;
                break reaper::wait(&mut child).map_err(|e| e.to_string())?;
            }
            None => thread::sleep(POLL_INTERVAL),
        }
    };
    let (stdout, stdout_truncated) = stdout_thread
        .map(|h| h.join().unwrap_or_default())
        .unwrap_or_default();
    let (stderr, stderr_truncated) = stderr_thread
        .map(|h| h.join().unwrap_or_default())
        .unwrap_or_default();
    Ok(Finished {
        code: status.code().unwrap_or(-1),
        timed_out,
        truncated: stdout_truncated || stderr_truncated,
        stdout,
        stderr,
    })
}

#[cfg(test)]
mod tests {
    use crate::PluginRuntime;

    fn rt_with_exec() -> PluginRuntime {
        let mut caps = std::collections::BTreeMap::new();
        caps.insert("p".to_owned(), vec!["exec".to_owned()]);
        PluginRuntime::builder().capabilities(caps).build().unwrap()
    }

    #[test]
    #[cfg(unix)]
    fn exec_runs_a_process_and_reports_exit_code() {
        let rt = rt_with_exec();
        let v = rt
            .eval_plugin(
                "p",
                "kage.request_capabilities({'exec'}); \
                 local ok = kage.exec({ cmd = 'true' }); \
                 local no = kage.exec({ cmd = 'false' }); \
                 return ok.code == 0 and no.code ~= 0",
            )
            .unwrap();
        assert_eq!(v.as_boolean(), Some(true));
    }

    /// Windows port of `exec_runs_a_process_and_reports_exit_code`:
    /// `cmd /C exit N` replaces the `true`/`false` fixtures. Run on a
    /// Windows CI leg.
    #[test]
    #[cfg(windows)]
    fn exec_runs_a_process_and_reports_exit_code_windows() {
        let rt = rt_with_exec();
        let v = rt
            .eval_plugin(
                "p",
                "kage.request_capabilities({'exec'}); \
                 local ok = kage.exec({ cmd = 'cmd', args = { '/C', 'exit', '0' } }); \
                 local no = kage.exec({ cmd = 'cmd', args = { '/C', 'exit', '3' } }); \
                 return ok.code == 0 and no.code ~= 0",
            )
            .unwrap();
        assert_eq!(v.as_boolean(), Some(true));
    }

    #[test]
    #[cfg(unix)]
    fn a_tool_waiting_on_a_process_leaves_other_calls_free() {
        use std::time::{Duration, Instant};

        use kage_core::CancelFlag;
        use kage_tools::ToolContext;

        let rt = rt_with_exec();
        rt.eval_plugin(
            "p",
            "kage.request_capabilities({'exec'}); \
             kage.register_tool({ name = 'slow', description = '', \
                 execute = function() kage.exec({ cmd = 'sleep', args = { '0.6' } }) return 'slow' end }); \
             kage.register_tool({ name = 'quick', description = '', \
                 execute = function() return 'quick' end })",
        )
        .unwrap();
        let tool = |name: &str| {
            rt.registered_tools()
                .into_iter()
                .find(|tool| tool.name() == name)
                .unwrap()
        };
        let run = |tool: &dyn kage_tools::Tool| {
            let cancel = CancelFlag::new();
            let workdir = std::path::PathBuf::from(".");
            tool.execute(serde_json::json!({}), &ToolContext::new(&workdir, &cancel))
                .unwrap()
                .text
        };
        let slow = tool("slow");
        let waiting = std::thread::spawn(move || run(slow.as_ref()));
        std::thread::sleep(Duration::from_millis(50));
        let start = Instant::now();
        assert_eq!(run(tool("quick").as_ref()), "quick");
        assert!(
            start.elapsed() < Duration::from_millis(300),
            "{:?}",
            start.elapsed()
        );
        assert_eq!(waiting.join().unwrap(), "slow");
    }

    #[test]
    #[cfg(unix)]
    fn exec_captures_stdout() {
        let rt = rt_with_exec();
        let v = rt
            .eval_plugin(
                "p",
                "kage.request_capabilities({'exec'}); \
                 local r = kage.exec({ cmd = 'echo', args = { 'hello' } }); \
                 return r.code == 0 and r.stdout:find('hello') ~= nil",
            )
            .unwrap();
        assert_eq!(v.as_boolean(), Some(true));
    }

    /// Windows port of `exec_captures_stdout`. Run on a Windows CI leg.
    #[test]
    #[cfg(windows)]
    fn exec_captures_stdout_windows() {
        let rt = rt_with_exec();
        let v = rt
            .eval_plugin(
                "p",
                "kage.request_capabilities({'exec'}); \
                 local r = kage.exec({ cmd = 'cmd', args = { '/C', 'echo hello' } }); \
                 return r.code == 0 and r.stdout:find('hello') ~= nil",
            )
            .unwrap();
        assert_eq!(v.as_boolean(), Some(true));
    }

    #[test]
    fn exec_cwd_may_not_escape_the_workdir() {
        let rt = rt_with_exec();
        let res = rt.eval_plugin(
            "p",
            "kage.request_capabilities({'exec'}); kage.exec({ cmd = 'true', cwd = '../etc' })",
        );
        assert!(res.is_err(), "escaping cwd must raise, got {res:?}");
    }

    #[test]
    fn exec_requires_a_command() {
        let rt = rt_with_exec();
        let res = rt.eval_plugin("p", "kage.request_capabilities({'exec'}); kage.exec({})");
        assert!(res.is_err(), "missing cmd must raise, got {res:?}");
    }

    #[test]
    fn exec_rejects_an_oversized_timeout() {
        let rt = rt_with_exec();
        let err = rt
            .eval_plugin(
                "p",
                "kage.request_capabilities({'exec'}); \
                 kage.exec({ cmd = 'true', timeout_secs = math.maxinteger })",
            )
            .unwrap_err();
        assert!(err.to_string().contains("timeout_secs"), "{err}");
    }

    #[test]
    #[cfg(unix)]
    fn exec_kills_a_runaway_process_at_the_deadline() {
        let rt = rt_with_exec();
        let start = std::time::Instant::now();
        let v = rt
            .eval_plugin(
                "p",
                "kage.request_capabilities({'exec'}); \
                 local r = kage.exec({ cmd = 'sleep', args = { '30' }, timeout_secs = 1 }); \
                 return r.timed_out and r.code ~= 0 and r.stdout == ''",
            )
            .unwrap();
        assert_eq!(v.as_boolean(), Some(true), "timed_out must be set");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(8),
            "deadline must kill the child promptly, took {:?}",
            start.elapsed()
        );
    }

    /// Windows port of `exec_kills_a_runaway_process_at_the_deadline`:
    /// `ping -n 30` runs for about 30 seconds and needs no console, so
    /// `timeout` cannot be used (it fails without a console). ping
    /// prints replies, so unlike the Unix twin the `stdout` assert is
    /// dropped. Run on a Windows CI leg.
    #[test]
    #[cfg(windows)]
    fn exec_kills_a_runaway_process_at_the_deadline_windows() {
        let rt = rt_with_exec();
        let start = std::time::Instant::now();
        let v = rt
            .eval_plugin(
                "p",
                "kage.request_capabilities({'exec'}); \
                 local r = kage.exec({ cmd = 'ping', args = { '-n', '30', '127.0.0.1' }, timeout_secs = 1 }); \
                 return r.timed_out and r.code ~= 0",
            )
            .unwrap();
        assert_eq!(v.as_boolean(), Some(true), "timed_out must be set");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(8),
            "deadline must kill the child promptly, took {:?}",
            start.elapsed()
        );
    }

    #[test]
    #[cfg(unix)]
    fn exec_reports_no_timeout_for_a_fast_exit() {
        let rt = rt_with_exec();
        let v = rt
            .eval_plugin(
                "p",
                "kage.request_capabilities({'exec'}); \
                 return kage.exec({ cmd = 'true' }).timed_out == false",
            )
            .unwrap();
        assert_eq!(v.as_boolean(), Some(true));
    }

    /// Windows port of `exec_reports_no_timeout_for_a_fast_exit`. Run
    /// on a Windows CI leg.
    #[test]
    #[cfg(windows)]
    fn exec_reports_no_timeout_for_a_fast_exit_windows() {
        let rt = rt_with_exec();
        let v = rt
            .eval_plugin(
                "p",
                "kage.request_capabilities({'exec'}); \
                 return kage.exec({ cmd = 'cmd', args = { '/C', 'exit', '0' } }).timed_out == false",
            )
            .unwrap();
        assert_eq!(v.as_boolean(), Some(true));
    }

    #[test]
    #[cfg(unix)]
    fn exec_truncates_output_past_the_cap_and_sets_the_flag() {
        let rt = rt_with_exec();
        let v = rt
            .eval_plugin(
                "p",
                "kage.request_capabilities({'exec'}); \
                 local r = kage.exec({ cmd = 'head', args = { '-c', '2097152', '/dev/zero' } }); \
                 return r.truncated and r.stdout:len() == 1048576 and r.stderr:len() == 0",
            )
            .unwrap();
        assert_eq!(v.as_boolean(), Some(true));
    }

    /// Windows port of
    /// `exec_truncates_output_past_the_cap_and_sets_the_flag`: no
    /// `/dev/zero`, so a 3 MB file is typed through `cmd` from a
    /// workdir the test controls. stderr is deliberately not
    /// asserted: Windows has no SIGPIPE, so once the drain stops at
    /// the cap the writer's next write fails and cmd reports that
    /// failure on stderr, while the Unix twin's `head` dies silently
    /// on SIGPIPE. Run on a Windows CI leg.
    #[test]
    #[cfg(windows)]
    fn exec_truncates_output_past_the_cap_and_sets_the_flag_windows() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("big.txt"), "x".repeat(3_000_000)).unwrap();
        let mut caps = std::collections::BTreeMap::new();
        caps.insert("p".to_owned(), vec!["exec".to_owned()]);
        let rt = PluginRuntime::builder()
            .capabilities(caps)
            .workdir(dir.path().to_path_buf())
            .build()
            .unwrap();
        let v = rt
            .eval_plugin(
                "p",
                "kage.request_capabilities({'exec'}); \
                 local r = kage.exec({ cmd = 'cmd', args = { '/C', 'type', 'big.txt' } }); \
                 return r.truncated and r.stdout:len() == 1048576",
            )
            .unwrap();
        assert_eq!(v.as_boolean(), Some(true));
    }

    #[test]
    fn ungranted_plugin_has_no_exec() {
        let rt = rt_with_exec();
        let v = rt.eval_plugin("other", "return kage.exec == nil").unwrap();
        assert_eq!(v.as_boolean(), Some(true));
    }
}
