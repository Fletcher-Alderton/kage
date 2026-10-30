//! `shell` tool: run a shell command with a timeout and truncated output.
//!
//! Commands run unsandboxed today; an OS-level isolation backend (for
//! example bubblewrap) can land later without changing the call shape.

use std::fmt::Write as _;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use kage_core::{Risk, ToolOutput, ToolUpdate, reaper, sync::lock};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::{ExecMode, Tool, ToolContext, ToolError, resolve, schema_for};

/// The shell the tool runs commands with when none is configured.
#[cfg(unix)]
pub const DEFAULT_SHELL: &str = "bash";
/// The shell the tool runs commands with when none is configured.
#[cfg(not(unix))]
pub const DEFAULT_SHELL: &str = "pwsh";
/// The bytes [`run`] keeps of each stream.
pub(crate) const MAX_STREAM_BYTES: usize = 100_000;
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
const TAIL_BYTES: usize = 4_096;
const TAIL_LINES: usize = 10;

/// The most recent output of both streams, reported as progress while the
/// command runs.
#[derive(Default)]
struct Tail {
    bytes: Vec<u8>,
    changed: bool,
}

impl Tail {
    fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend_from_slice(chunk);
        if self.bytes.len() > TAIL_BYTES {
            let excess = self.bytes.len() - TAIL_BYTES;
            self.bytes.drain(..excess);
        }
        self.changed = true;
    }

    /// The last [`TAIL_LINES`] lines, if output arrived since the last call.
    fn take_update(&mut self) -> Option<String> {
        if !std::mem::take(&mut self.changed) {
            return None;
        }
        let text = String::from_utf8_lossy(&self.bytes);
        let lines: Vec<&str> = text.lines().collect();
        Some(lines[lines.len().saturating_sub(TAIL_LINES)..].join("\n"))
    }
}

/// Input shape for the `shell` tool.
#[derive(Debug, Deserialize, JsonSchema)]
struct ShellInput {
    /// Shell command to run.
    command: String,
    /// Optional working directory, relative to the workdir. Defaults to the workdir.
    #[serde(default, deserialize_with = "super::optional_path")]
    cwd: Option<String>,
    /// Hard timeout in milliseconds. Defaults to 120000 (2 minutes).
    #[serde(default)]
    timeout_ms: Option<u64>,
}

/// Run a shell command.
#[derive(Debug)]
pub struct ShellTool {
    /// Program commands run with, from `[shell] program`.
    shell: String,
    description: String,
    env_scrub: Arc<[String]>,
    /// Timeout of a command whose call sets none.
    timeout_ms: u64,
    /// Bytes kept of each of stdout and stderr.
    max_output_bytes: usize,
}

impl Default for ShellTool {
    fn default() -> Self {
        Self::new(&kage_core::config::ShellToolConfig::default())
    }
}

/// The model-facing description for a shell program, naming it so the
/// model writes commands in that shell's syntax. Models default to
/// bash constructs no matter what the tool says, so shells that are
/// not bash get an explicit contrast note with the common translations.
fn describe_shell(shell: &str, timeout_ms: u64, max_output_bytes: usize) -> String {
    let flags = shell_args(shell).join(" ");
    let mut out = format!(
        "Run a shell command via `{shell} {flags}`. Returns combined stdout/stderr and the \
         exit code. Default timeout is {}; output is truncated at {}.",
        super::human_ms(timeout_ms),
        super::human_bytes(max_output_bytes as u64)
    );
    match shell.trim_end_matches(".exe") {
        "fish" => out.push_str(
            " Commands run in fish, NOT bash: `export FOO=bar` is `set -x FOO bar`, \
             `$(cmd)` is `(cmd)`, and `VAR=value cmd` is `env VAR=value cmd`.",
        ),
        "powershell" | "pwsh" => out.push_str(
            " Commands run in PowerShell, NOT bash: bash constructs like \
             `export` or `VAR=value cmd` do not work; use `$env:FOO = 'bar'`.",
        ),
        "nushell" => out.push_str(
            " Commands run in Nushell, NOT bash: bash constructs like \
             `export`, `$(cmd)`, and `VAR=value cmd` do not work.",
        ),
        _ => {}
    }
    out
}

/// The flag(s) `program` uses to run a command string. Known Windows
/// shells get their own; everything POSIX-like, including fish and
/// zsh, takes `-c`.
fn shell_args(program: &str) -> &'static [&'static str] {
    match program.trim_end_matches(".exe") {
        "powershell" | "pwsh" => &["-NoProfile", "-Command"],
        "cmd" => &["/C"],
        _ => &["-c"],
    }
}

impl ShellTool {
    /// A `shell` tool with the limits `[tools.shell]` sets, running
    /// commands with [`DEFAULT_SHELL`] until [`Self::with_shell`].
    #[must_use]
    pub fn new(cfg: &kage_core::config::ShellToolConfig) -> Self {
        Self {
            shell: DEFAULT_SHELL.to_owned(),
            description: describe_shell(DEFAULT_SHELL, cfg.timeout_ms, cfg.max_output_bytes),
            env_scrub: Arc::from([]),
            timeout_ms: cfg.timeout_ms,
            max_output_bytes: cfg.max_output_bytes,
        }
    }

    /// Glob patterns of environment variables to strip from the shell
    /// child's environment, so a model-authored command cannot read
    /// variables the user withheld. Patterns match the whole variable
    /// name, case-sensitively on Unix and case-insensitively on
    /// Windows, following each platform's env name rules. Invalid
    /// patterns are skipped; config validation refuses them at startup.
    #[must_use]
    pub fn with_env_scrub(mut self, patterns: &[String]) -> Self {
        self.env_scrub = Arc::from(patterns);
        self
    }

    /// Run commands with `shell` instead of the default (see
    /// [`DEFAULT_SHELL`]). Unknown programs are driven with `-c`.
    #[must_use]
    pub fn with_shell(mut self, shell: Option<&str>) -> Self {
        if let Some(shell) = shell {
            shell.clone_into(&mut self.shell);
            self.description = describe_shell(&self.shell, self.timeout_ms, self.max_output_bytes);
        }
        self
    }
}

impl Tool for ShellTool {
    fn name(&self) -> &'static str {
        "shell"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> serde_json::Value {
        let mut schema = schema_for::<ShellInput>();
        crate::schema::set_property_doc(
            &mut schema,
            "timeout_ms",
            &format!(
                "Hard timeout in milliseconds. Defaults to {}.",
                self.timeout_ms
            ),
        );
        schema
    }

    fn risk(&self) -> Risk {
        Risk::Exec
    }

    fn execution_mode(&self) -> Option<ExecMode> {
        Some(ExecMode::Sequential)
    }

    fn execute(
        &self,
        input: serde_json::Value,
        cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let input: ShellInput = serde_json::from_value(input)?;
        let timeout = Duration::from_millis(input.timeout_ms.unwrap_or(self.timeout_ms));
        let cwd = match &input.cwd {
            Some(c) => resolve(cx.workdir(), Path::new(c))?,
            None => cx.workdir().to_path_buf(),
        };
        run_command(
            &input.command,
            &cwd,
            timeout,
            &self.env_scrub,
            &self.shell,
            self.max_output_bytes,
            cx,
        )
    }
}

/// What a finished command left: both streams, each capped at 100KB, and
/// how it exited.
#[derive(Debug)]
pub struct CommandOutput {
    /// Captured stdout.
    pub stdout: Vec<u8>,
    /// Whether stdout went past the cap.
    pub stdout_truncated: bool,
    /// Captured stderr.
    pub stderr: Vec<u8>,
    /// Whether stderr went past the cap.
    pub stderr_truncated: bool,
    /// Exit code, or `None` when a signal ended the command.
    pub exit_code: Option<i32>,
}

/// Give `cmd` the parent environment minus the names matched by `patterns`,
/// so a model-authored command cannot read variables the user withheld.
/// Invalid patterns are skipped; config validation refuses them at startup.
fn scrub_env(cmd: &mut Command, patterns: &[String]) {
    if patterns.is_empty() {
        return;
    }
    // Windows variable names are case-insensitive, so a configured
    // `PATH` pattern must match the variable spelled `Path` there.
    let case_insensitive = cfg!(windows);
    let matchers: Vec<_> = patterns
        .iter()
        .filter_map(|p| {
            globset::GlobBuilder::new(p)
                .case_insensitive(case_insensitive)
                .build()
                .ok()
                .map(|g| g.compile_matcher())
        })
        .collect();
    let parent: Vec<(std::ffi::OsString, std::ffi::OsString)> = std::env::vars_os().collect();
    cmd.env_clear();
    for (name, value) in parent {
        let name = name.to_string_lossy();
        if !matchers.iter().any(|m| m.is_match(name.as_ref())) {
            cmd.env(name.into_owned(), value);
        }
    }
}

/// Run `command` with `shell` in `cwd`, in its own process group (a
/// job object on Windows), and report the last lines of its output
/// through `cx`'s progress sink while it runs. A cancel of `cx` or
/// passing `timeout` kills the whole group.
/// The child sees the environment minus the variables matched by
/// `env_scrub`.
///
/// # Errors
///
/// [`ToolError::Cancelled`] or [`ToolError::Timeout`] after a kill, an
/// error naming `cwd` when it does not exist, or an I/O error when the
/// shell cannot start.
pub fn run(
    command: &str,
    cwd: &Path,
    timeout: Duration,
    env_scrub: &[String],
    shell: &str,
    cx: &ToolContext<'_>,
) -> Result<CommandOutput, ToolError> {
    run_capped(
        command,
        cwd,
        timeout,
        env_scrub,
        shell,
        MAX_STREAM_BYTES,
        cx,
    )
}

/// [`run`] keeping at most `max_bytes` of each stream.
///
/// # Errors
///
/// As [`run`].
pub(crate) fn run_capped(
    command: &str,
    cwd: &Path,
    timeout: Duration,
    env_scrub: &[String],
    shell: &str,
    max_bytes: usize,
    cx: &ToolContext<'_>,
) -> Result<CommandOutput, ToolError> {
    if !cwd.exists() {
        return Err(ToolError::Other(format!(
            "working directory {} does not exist",
            cwd.display()
        )));
    }
    let mut cmd = Command::new(shell);
    cmd.args(shell_args(shell))
        .arg(command)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    scrub_env(&mut cmd, env_scrub);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Own process group, so the kill below reaches grandchildren
        // that inherited our pipes.
        cmd.process_group(0);
    }
    let mut child = cmd.spawn()?;
    let group = ProcessGroup::new(&child);

    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");

    let tail = Arc::new(Mutex::new(Tail::default()));
    let stdout_tail = Arc::clone(&tail);
    let stderr_tail = Arc::clone(&tail);
    let stdout_handle = thread::spawn(move || read_capped(&mut stdout, &stdout_tail, max_bytes));
    let stderr_handle = thread::spawn(move || read_capped(&mut stderr, &stderr_tail, max_bytes));

    let watch = cx.cancel_flag().watch();
    let start = Instant::now();
    let mut last_progress = start;
    let status = loop {
        if let Some(s) = reaper::try_wait(&mut child)? {
            break s;
        }
        if start.elapsed() > timeout {
            group.kill(&mut child);
            let _ = child.wait();
            return Err(ToolError::Timeout {
                name: "shell".into(),
                millis: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
            });
        }
        if last_progress.elapsed() >= PROGRESS_INTERVAL {
            last_progress = Instant::now();
            if let Some(content) = lock(&tail).take_update() {
                cx.update(ToolUpdate {
                    content,
                    structured: None,
                });
            }
        }
        if watch.receiver().recv_timeout(POLL_INTERVAL).is_ok() {
            group.kill(&mut child);
            let _ = child.wait();
            return Err(ToolError::Cancelled);
        }
    };

    // Bash is done, but backgrounded grandchildren may still hold our
    // pipes; they would block the joins below on EOF forever. Once the
    // shell has exited, anything left in its process group is a
    // straggler: release the group before reading.
    group.kill(&mut child);
    let _ = child.wait();
    let (stdout, stdout_truncated) = stdout_handle.join().unwrap_or_default();
    let (stderr, stderr_truncated) = stderr_handle.join().unwrap_or_default();
    Ok(CommandOutput {
        stdout,
        stdout_truncated,
        stderr,
        stderr_truncated,
        exit_code: status.code(),
    })
}

fn run_command(
    command: &str,
    cwd: &Path,
    timeout: Duration,
    env_scrub: &[String],
    shell: &str,
    max_bytes: usize,
    cx: &ToolContext<'_>,
) -> Result<ToolOutput, ToolError> {
    let CommandOutput {
        stdout,
        stdout_truncated,
        stderr,
        stderr_truncated,
        exit_code,
    } = run_capped(command, cwd, timeout, env_scrub, shell, max_bytes, cx)?;
    let cap = super::human_bytes(max_bytes as u64);
    let stdout_text = String::from_utf8_lossy(&stdout).into_owned();
    let stderr_text = String::from_utf8_lossy(&stderr).into_owned();

    let mut text = String::new();
    if !stdout_text.is_empty() {
        text.push_str("stdout:\n");
        text.push_str(&stdout_text);
        if stdout_truncated {
            let _ = write!(text, "\n[... stdout truncated at {cap} ...]");
        }
        if !stderr_text.is_empty() {
            text.push('\n');
        }
    }
    if !stderr_text.is_empty() {
        text.push_str("stderr:\n");
        text.push_str(&stderr_text);
        if stderr_truncated {
            let _ = write!(text, "\n[... stderr truncated at {cap} ...]");
        }
    }
    if text.is_empty() {
        text.push_str("(no output)");
    }
    let _ = write!(
        text,
        "\nexit: {}",
        exit_code.map_or_else(|| "signal".to_owned(), |c| c.to_string()),
    );

    Ok(ToolOutput {
        is_error: exit_code != Some(0),
        text,
        structured: Some(serde_json::json!({
            "exit_code": exit_code,
            "stdout_truncated": stdout_truncated,
            "stderr_truncated": stderr_truncated,
            "cwd": cwd_display(cwd),
        })),
        terminate: false,
        media: Vec::new(),
    })
}

fn cwd_display(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Kill handle for the child's whole process tree. Killing only the
/// shell can leave grandchildren that inherited our pipes alive, and
/// the reader threads then block on EOF forever, so the kill must
/// reach the tree. Unix: the child runs in its own process group
/// (`pgid == pid`) and gets a group `SIGKILL`. Windows: the child is
/// assigned to a kill-on-close job object, so releasing the job
/// terminates the tree at once, including grandchildren that detached
/// from the console.
#[cfg(unix)]
struct ProcessGroup;

#[cfg(unix)]
impl ProcessGroup {
    fn new(child: &std::process::Child) -> Self {
        let _ = child;
        Self
    }

    fn kill(self, child: &mut std::process::Child) {
        let _ = self;
        let pgid = nix::unistd::Pid::from_raw(child.id().cast_signed());
        if nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGKILL).is_err() {
            let _ = child.kill();
        }
    }
}

#[cfg(windows)]
struct ProcessGroup {
    /// The job holds the tree: dropping the kill-on-close job
    /// terminates every process still in it, including a panic path
    /// that never reaches `kill`.
    job: Option<win32job::Job>,
}

#[cfg(windows)]
impl ProcessGroup {
    fn new(child: &std::process::Child) -> Self {
        Self {
            job: attach_job(child).ok(),
        }
    }

    fn kill(self, child: &mut std::process::Child) {
        drop(self.job);
        let _ = child.kill();
    }
}

/// No tracked group on other targets; only the shell can be killed.
#[cfg(not(any(unix, windows)))]
struct ProcessGroup;

#[cfg(not(any(unix, windows)))]
impl ProcessGroup {
    fn new(child: &std::process::Child) -> Self {
        let _ = child;
        Self
    }

    fn kill(self, child: &mut std::process::Child) {
        let _ = self;
        let _ = child.kill();
    }
}

/// Put `child` in a kill-on-close job object, so dropping the job
/// terminates the whole process tree at once. kage itself may already
/// sit in a job (a terminal or CI runner); nested jobs are supported
/// on Windows 8+. Any failure leaves the caller with `job: None`,
/// where a kill falls back to the shell process alone.
#[cfg(windows)]
fn attach_job(child: &std::process::Child) -> Result<win32job::Job, win32job::JobError> {
    use std::os::windows::io::AsRawHandle;

    let job = win32job::Job::create()?;
    let mut info = job.query_extended_limit_info()?;
    info.limit_kill_on_job_close();
    job.set_extended_limit_info(&info)?;
    job.assign_process(child.as_raw_handle() as _)?;
    Ok(job)
}

/// Read `reader` to EOF, keeping at most `max_bytes` and feeding every
/// chunk to `tail`. Reading continues past the cap so the command never
/// blocks on a full pipe.
fn read_capped<R: Read>(reader: &mut R, tail: &Mutex<Tail>, max_bytes: usize) -> (Vec<u8>, bool) {
    let mut buf = Vec::with_capacity(4_096);
    let mut chunk = [0u8; 8_192];
    let mut truncated = false;
    loop {
        let n = match reader.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        lock(tail).push(&chunk[..n]);
        let room = max_bytes - buf.len();
        if n > room {
            truncated = true;
        }
        buf.extend_from_slice(&chunk[..n.min(room)]);
    }
    (buf, truncated)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use kage_core::CancelFlag;

    use super::*;

    fn run(workdir: &Path, input: serde_json::Value) -> Result<ToolOutput, ToolError> {
        let cancel = CancelFlag::new();
        let cx = ToolContext::new(workdir, &cancel);
        ShellTool::default().execute(input, &cx)
    }

    #[test]
    fn echoes_to_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let out = run(dir.path(), serde_json::json!({"command":"echo hello"})).unwrap();
        assert!(!out.is_error);
        assert!(out.text.contains("hello"));
        assert!(out.text.contains("exit: 0"));
    }

    #[test]
    fn nonzero_exit_marks_error() {
        let dir = tempfile::tempdir().unwrap();
        let out = run(dir.path(), serde_json::json!({"command":"exit 7"})).unwrap();
        assert!(out.is_error);
        assert!(out.text.contains("exit: 7"));
        assert_eq!(out.structured.unwrap()["exit_code"], 7);
    }

    #[test]
    #[cfg(unix)]
    fn captures_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let out = run(dir.path(), serde_json::json!({"command":"echo oops 1>&2"})).unwrap();
        assert!(out.text.contains("stderr:"));
        assert!(out.text.contains("oops"));
    }

    #[derive(Default)]
    #[cfg(unix)]
    struct Collect(Mutex<Vec<String>>);

    #[cfg(unix)]
    impl crate::ProgressSink for Collect {
        fn emit(&self, update: ToolUpdate) {
            lock(&self.0).push(update.content);
        }
    }

    #[test]
    #[cfg(unix)]
    fn streams_output_while_running() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancelFlag::new();
        let sink = Arc::new(Collect::default());
        let cx = ToolContext::new(dir.path(), &cancel).with_progress(sink.clone());
        let out = ShellTool::default()
            .execute(
                serde_json::json!({"command":"echo first; sleep 0.4; echo second"}),
                &cx,
            )
            .unwrap();
        assert!(out.text.contains("second"));
        let updates = lock(&sink.0);
        assert!(
            updates.iter().any(|u| u == "first"),
            "expected a progress update before the command finished: {updates:?}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn timeout_returns_timeout_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = run(
            dir.path(),
            serde_json::json!({"command":"sleep 1","timeout_ms":150}),
        )
        .unwrap_err();
        assert!(matches!(err, ToolError::Timeout { .. }));
    }

    #[test]
    #[cfg(unix)]
    fn timeout_reaps_pipe_holding_grandchildren() {
        // `sleep` inherits our pipes; killing only the shell leaves it
        // alive and the reader threads blocked on EOF until it exits,
        // so this call would take the full sleep duration.
        let dir = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let err = run(
            dir.path(),
            serde_json::json!({"command":"sleep 5; echo done","timeout_ms":150}),
        )
        .unwrap_err();
        let elapsed = started.elapsed();
        assert!(matches!(err, ToolError::Timeout { .. }));
        assert!(
            elapsed < Duration::from_secs(3),
            "kill took {elapsed:?}; process group was not killed"
        );
    }

    #[test]
    #[cfg(unix)]
    fn cancel_kills_the_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancelFlag::new();
        let trip = cancel.clone();
        let canceller = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            trip.cancel();
        });
        let started = Instant::now();
        let cx = ToolContext::new(dir.path(), &cancel);
        let err = super::run(
            "sleep 5; echo done",
            dir.path(),
            Duration::MAX,
            &[],
            DEFAULT_SHELL,
            &cx,
        )
        .unwrap_err();
        canceller.join().unwrap();
        assert!(matches!(err, ToolError::Cancelled));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    /// Windows mirror of `timeout_reaps_pipe_holding_grandchildren`:
    /// the nested pwsh sleeps with our pipes inherited, so the reader
    /// threads only drain once the job object has terminated the whole
    /// tree. Run on a Windows CI leg.
    #[test]
    #[cfg(windows)]
    fn timeout_reaps_pipe_holding_grandchildren_windows() {
        let dir = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let err = run(
            dir.path(),
            serde_json::json!({"command":"pwsh -NoProfile -Command 'Start-Sleep -Seconds 30'","timeout_ms":150}),
        )
        .unwrap_err();
        let elapsed = started.elapsed();
        assert!(matches!(err, ToolError::Timeout { .. }));
        assert!(
            elapsed < Duration::from_secs(3),
            "kill took {elapsed:?}; the job object was not released"
        );
    }

    /// Windows mirror of `cancel_kills_the_process_group`. Run on a
    /// Windows CI leg.
    #[test]
    #[cfg(windows)]
    fn cancel_kills_the_process_group_windows() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancelFlag::new();
        let trip = cancel.clone();
        let canceller = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            trip.cancel();
        });
        let started = Instant::now();
        let cx = ToolContext::new(dir.path(), &cancel);
        let err = super::run(
            "pwsh -NoProfile -Command 'Start-Sleep -Seconds 30'",
            dir.path(),
            Duration::MAX,
            &[],
            DEFAULT_SHELL,
            &cx,
        )
        .unwrap_err();
        canceller.join().unwrap();
        assert!(matches!(err, ToolError::Cancelled));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    /// Windows mirror of
    /// `backgrounded_grandchild_does_not_block_success_path`, and the
    /// one test that truly discriminates the job object: `Start-Process
    /// -NoNewWindow` leaves a detached grandchild holding our pipe,
    /// and the success path joins the readers, so a kill that misses
    /// the tree blocks here for the full sleep. Run on a Windows CI
    /// leg.
    #[test]
    #[cfg(windows)]
    fn backgrounded_grandchild_does_not_block_success_path_windows() {
        let dir = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let out = run(
            dir.path(),
            serde_json::json!({"command":"Start-Process pwsh '-NoProfile','-Command','Start-Sleep -Seconds 30' -NoNewWindow; 'started'"}),
        )
        .unwrap();
        let elapsed = started.elapsed();
        assert!(out.text.contains("started"));
        assert!(
            elapsed < Duration::from_secs(5),
            "blocked on orphaned pipe holder for {elapsed:?}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn backgrounded_grandchild_does_not_block_success_path() {
        // Bash exits immediately; the backgrounded sleep keeps the pipe
        // write end open. Without releasing the process group before the
        // joins, this call blocks until the sleep finishes.
        let dir = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let out = run(
            dir.path(),
            serde_json::json!({"command":"sleep 30 & echo started"}),
        )
        .unwrap();
        let elapsed = started.elapsed();
        assert!(out.text.contains("started"));
        assert!(
            elapsed < Duration::from_secs(5),
            "blocked on orphaned pipe holder for {elapsed:?}"
        );
    }

    #[test]
    fn missing_working_directory_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("gone");
        let err = run(&gone, serde_json::json!({"command":"pwd"})).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("working directory {} does not exist", gone.display())
        );
    }

    #[test]
    fn a_null_string_cwd_runs_in_the_workdir() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("marker.txt"), "in the workdir").unwrap();
        for cwd in ["null", ""] {
            let out = run(
                dir.path(),
                serde_json::json!({"command":"cat marker.txt","cwd":cwd}),
            )
            .unwrap();
            assert!(out.text.contains("in the workdir"), "{}", out.text);
        }
    }

    #[test]
    fn cwd_resolves_under_workdir() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/marker.txt"), "x").unwrap();
        let out = run(
            dir.path(),
            serde_json::json!({"command":"cat marker.txt","cwd":"sub"}),
        )
        .unwrap();
        assert!(!out.is_error);
        assert!(out.text.contains('x'));
    }

    #[cfg(unix)]
    fn run_with(
        workdir: &Path,
        scrub: &[String],
        input: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let cancel = CancelFlag::new();
        let cx = ToolContext::new(workdir, &cancel);
        ShellTool::default()
            .with_env_scrub(scrub)
            .execute(input, &cx)
    }

    #[test]
    #[cfg(unix)]
    fn scrub_pattern_strips_the_variable_from_the_child() {
        let dir = tempfile::tempdir().unwrap();
        let out = run_with(
            dir.path(),
            &["HOME".to_owned()],
            serde_json::json!({"command": "test -z \"$HOME\" && echo stripped"}),
        )
        .unwrap();
        assert!(!out.is_error);
        assert!(out.text.contains("stripped"), "{}", out.text);
    }

    #[test]
    #[cfg(unix)]
    fn scrub_glob_strips_the_suffix_family() {
        let dir = tempfile::tempdir().unwrap();
        let out = run_with(
            dir.path(),
            &["*OME".to_owned()],
            serde_json::json!({"command": "test -z \"$HOME\" && echo stripped"}),
        )
        .unwrap();
        assert!(out.text.contains("stripped"), "{}", out.text);
    }

    #[test]
    #[cfg(unix)]
    fn invalid_scrub_pattern_is_skipped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let out = run_with(
            dir.path(),
            &["[".to_owned()],
            serde_json::json!({"command": "test -n \"$PATH\" && echo has-path"}),
        )
        .unwrap();
        assert!(out.text.contains("has-path"), "{}", out.text);
    }

    /// Windows env names are case-insensitive, so a configured `PATH`
    /// must strip the variable whatever its spelling. Asserted on the
    /// scrubbed command's env map rather than through a spawned shell:
    /// pwsh rebuilds a `Path` of its own at startup, so a shell-level
    /// probe cannot witness the scrub. Run on a Windows CI leg.
    #[test]
    #[cfg(windows)]
    fn scrub_matches_windows_env_names_case_insensitively() {
        let mut cmd = Command::new("pwsh");
        scrub_env(&mut cmd, &["PATH".to_owned()]);
        assert!(
            cmd.get_envs()
                .all(|(name, _)| !name.eq_ignore_ascii_case("PATH")),
            "a PATH case variant survived the scrub: {:?}",
            cmd.get_envs()
                .map(|(name, _)| name.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    #[cfg(unix)]
    fn child_sees_a_nonempty_environment() {
        let dir = tempfile::tempdir().unwrap();
        let out = run(
            dir.path(),
            serde_json::json!({"command": "test -n \"$PATH\" && echo has-path"}),
        )
        .unwrap();
        assert!(!out.is_error);
        assert!(out.text.contains("has-path"), "{}", out.text);
    }

    #[test]
    fn known_shells_get_their_own_command_flag() {
        assert_eq!(shell_args("fish"), &["-c"]);
        assert_eq!(shell_args("pwsh"), &["-NoProfile", "-Command"]);
        assert_eq!(shell_args("powershell.exe"), &["-NoProfile", "-Command"]);
        assert_eq!(shell_args("cmd"), &["/C"]);
        assert_eq!(shell_args("bash"), &["-c"]);
    }

    #[test]
    fn description_names_the_configured_shell() {
        let tool = ShellTool::default().with_shell(Some("fish"));
        assert!(
            tool.description().contains("fish -c"),
            "{}",
            tool.description()
        );
        assert!(
            tool.description().contains("NOT bash"),
            "{}",
            tool.description()
        );
        let bash = ShellTool::default().with_shell(Some("bash"));
        assert!(
            !bash.description().contains("NOT bash"),
            "{}",
            bash.description()
        );
        assert_eq!(
            ShellTool::default().description(),
            describe_shell(DEFAULT_SHELL, 120_000, MAX_STREAM_BYTES)
        );
    }

    #[cfg(unix)]
    #[test]
    fn configured_shell_is_used() {
        // `$0` under `-c` is the shell as invoked: `sh` here, `bash`
        // with the default tool.
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancelFlag::new();
        let cx = ToolContext::new(dir.path(), &cancel);
        let out = ShellTool::default()
            .with_shell(Some("sh"))
            .execute(serde_json::json!({"command":"echo $0"}), &cx)
            .unwrap();
        assert!(out.text.contains("sh"), "{}", out.text);
        assert!(!out.text.contains("shell"), "{}", out.text);
    }
}
