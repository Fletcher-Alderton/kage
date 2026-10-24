//! Checkouts of their own for agents whose definition asks for
//! `isolation: worktree`: a jj workspace, or a git worktree on a branch
//! of its own, outside the repository. One is made at spawn,
//! checkpointed at the end of each run and removed when the agent is
//! dropped. Nothing is merged: the agent's report says where its work
//! is and how to take it.

use std::fmt::{self, Display};
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use kage_core::reaper;
use kage_core::sync::lock;
use kage_core::{CancelFlag, SessionId};

/// One agent's checkout. Dropping it removes the checkout in the
/// background, without blocking the caller; work the agent left stays
/// in the repository as a jj change or a git branch.
#[derive(Debug)]
pub(super) struct Worktree {
    /// Where the checkout is.
    path: PathBuf,
    /// The agent's working directory: the parent's place in the
    /// repository, inside the checkout.
    workdir: PathBuf,
    /// The repository's main checkout, where the drop runs.
    repo: PathBuf,
    /// The git commit message: the agent and its task.
    label: String,
    vcs: Vcs,
    /// Set when the last checkpoint's command was stopped: the work may
    /// be uncommitted, so the drop leaves the checkout in place.
    keep: AtomicBool,
}

#[derive(Debug)]
enum Vcs {
    Jj {
        workspace: String,
    },
    Git {
        branch: String,
        base: String,
    },
    /// A raw cleanup command, so tests can make the drop path slow.
    #[cfg(test)]
    Command {
        program: String,
        args: Vec<String>,
    },
}

impl Worktree {
    /// Make a checkout for the agent `id` under `dir`, of the repository
    /// that holds `parent_workdir`: a jj workspace when jj manages it,
    /// else a git worktree on a new branch from `HEAD`. `label` names
    /// the agent's work in a git commit. Setting `cancel` stops the
    /// command in flight.
    pub(super) fn create(
        parent_workdir: &Path,
        dir: &Path,
        id: SessionId,
        label: String,
        cancel: &CancelFlag,
    ) -> Result<Self, String> {
        let run = |dir: &Path, program: &str, args: &[&str]| {
            run_until(dir, program, args, CREATE_COMMAND_TIMEOUT, Some(cancel))
        };
        let short = short(id);
        let path = dir.join(id.to_string());
        let target = path.to_string_lossy().into_owned();
        let made =
            |what: &str, err: &dyn Display| format!("cannot make the worktree {what}: {err}");
        // A stopped add may have left part of the checkout behind. A
        // failed one is undone by the VCS itself, and may have failed
        // on something that is not ours, like a branch of that name.
        let added = |what: &str, result: Result<String, RunError>, repo: &Path, vcs: &Vcs| {
            if let Err(RunError::Stopped(_)) = &result {
                undo(&path, repo, vcs);
            }
            result.map_err(|err| made(what, &err))
        };
        if let Ok(root) = run(parent_workdir, "jj", &["root"]) {
            std::fs::create_dir_all(dir).map_err(|err| made("directory", &err))?;
            let repo = PathBuf::from(root);
            let workspace = format!("kage-{short}");
            let args = ["workspace", "add", "--name", &workspace, "-r", "@", &target];
            let result = run(parent_workdir, "jj", &args);
            let vcs = Vcs::Jj { workspace };
            added("workspace", result, &repo, &vcs)?;
            return Ok(Self::new(path, &repo, parent_workdir, label, vcs));
        }
        if let Ok(root) = run(parent_workdir, "git", &["rev-parse", "--show-toplevel"]) {
            std::fs::create_dir_all(dir).map_err(|err| made("directory", &err))?;
            let repo = PathBuf::from(root);
            let base =
                run(&repo, "git", &["rev-parse", "HEAD"]).map_err(|err| made("base", &err))?;
            let branch = format!("kage/agent-{short}");
            let args = ["worktree", "add", "-b", &branch, &target, "HEAD"];
            let result = run(&repo, "git", &args);
            let vcs = Vcs::Git { branch, base };
            added("checkout", result, &repo, &vcs)?;
            return Ok(Self::new(path, &repo, parent_workdir, label, vcs));
        }
        if cancel.is_cancelled() {
            return Err("cannot make the worktree: cancelled".to_owned());
        }
        Err("worktree isolation needs a git or jj repository".to_owned())
    }

    fn new(path: PathBuf, repo: &Path, parent_workdir: &Path, label: String, vcs: Vcs) -> Self {
        Self {
            workdir: inside(&path, repo, parent_workdir),
            path,
            repo: repo.to_path_buf(),
            label,
            vcs,
            keep: AtomicBool::new(false),
        }
    }

    /// Where the agent works.
    pub(super) fn workdir(&self) -> &Path {
        &self.workdir
    }

    /// Record the agent's work so far and describe it for the report:
    /// where it is and the command that takes it.
    pub(super) fn checkpoint(&self) -> String {
        let at = self.path.display();
        match &self.vcs {
            Vcs::Jj { workspace } => {
                let template = ["log", "-r", "@", "--no-graph", "-T", "change_id.short()"];
                let read = run(&self.path, "jj", &template, COMMAND_TIMEOUT).and_then(|change| {
                    run(
                        &self.path,
                        "jj",
                        &["diff", "--stat", "-r", "@"],
                        COMMAND_TIMEOUT,
                    )
                    .map(|stat| (change, stat))
                });
                let kept = self.keep_if_stopped(&read);
                match read {
                    Ok((change, stat)) => match summary(&stat) {
                        Some(stat) => format!(
                            "Worktree: jj change {change} in workspace {workspace} ({stat}). To \
                             take it: jj squash --from {change}. Nothing was merged."
                        ),
                        None => format!("Worktree: workspace {workspace} has no changes."),
                    },
                    Err(err) => {
                        format!("Worktree: the checkout at {at} could not be read: {err}{kept}")
                    }
                }
            }
            Vcs::Git { branch, base } => {
                let committed = run(
                    &self.path,
                    "git",
                    &["status", "--porcelain"],
                    COMMAND_TIMEOUT,
                )
                .and_then(|status| {
                    if status.is_empty() {
                        return Ok(());
                    }
                    run(&self.path, "git", &["add", "-A"], COMMAND_TIMEOUT)?;
                    run(
                        &self.path,
                        "git",
                        &["commit", "-q", "-m", &self.label],
                        COMMAND_TIMEOUT,
                    )
                    .map(|_| ())
                })
                .and_then(|()| {
                    run(
                        &self.path,
                        "git",
                        &["diff", "--stat", &format!("{base}..HEAD")],
                        COMMAND_TIMEOUT,
                    )
                });
                let kept = self.keep_if_stopped(&committed);
                match committed {
                    Ok(stat) => match summary(&stat) {
                        Some(stat) => format!(
                            "Worktree: branch {branch} ({stat}). To take it: git merge {branch}. \
                             Nothing was merged."
                        ),
                        None => format!("Worktree: branch {branch} has no changes."),
                    },
                    Err(err) => {
                        format!("Worktree: the work at {at} could not be committed: {err}{kept}")
                    }
                }
            }
            #[cfg(test)]
            Vcs::Command { .. } => {
                unreachable!("the test-only cleanup command has nothing to checkpoint")
            }
        }
    }

    /// Keep the checkout on drop when `result` is a stopped command:
    /// its work may be uncommitted. Returns the note the report adds.
    fn keep_if_stopped<T>(&self, result: &Result<T, RunError>) -> &'static str {
        let stopped = matches!(result, Err(RunError::Stopped(_)));
        self.keep.store(stopped, Ordering::SeqCst);
        if stopped {
            "; the checkout is kept so the work is not lost"
        } else {
            ""
        }
    }
}

/// How long one VCS command of [`Worktree::create`] may run before it
/// is killed: a large repository can take minutes to check out, and
/// the agent being set up is the only thing waiting.
const CREATE_COMMAND_TIMEOUT: Duration = Duration::from_secs(600);

/// How long one VCS command of [`Worktree::checkpoint`] may run before
/// it is killed: long enough for a slow pre-commit hook or a large
/// `git add -A`, but bounded so a stuck VCS cannot keep the parent from
/// ever getting the agent's result. Tests use a short one, so a hung
/// checkpoint is quick to exercise.
const COMMAND_TIMEOUT: Duration = if cfg!(test) {
    Duration::from_secs(5)
} else {
    Duration::from_secs(60)
};

/// How long one drop-path VCS command may run before it is killed:
/// generous for a real `git` or `jj` on a slow disk, but bounded so a
/// stuck VCS cannot hold the cleanup thread forever.
const DROP_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a pipe may stay open after the child has exited. The
/// child's own writers are gone by then, so everything it wrote is
/// already buffered; the grace only covers the reader's final copy
/// before giving up on an orphaned grandchild holding the pipe open.
const DRAIN_GRACE: Duration = Duration::from_millis(250);

/// How long a timed-out command has between SIGTERM and SIGKILL:
/// enough for git to remove its lock files on the way out.
const TERM_GRACE: Duration = Duration::from_secs(2);

/// Drop-path cleanups started but not finished. Production drops are
/// detached and unsignalled; only tests wait for this to reach zero.
static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

impl Drop for Worktree {
    /// Removes the checkout on a detached thread so a slow VCS never
    /// blocks the caller, with every command killed at
    /// [`DROP_COMMAND_TIMEOUT`]. Best-effort: when the process exits
    /// before the thread finishes, the checkout is left behind. A
    /// checkout whose last checkpoint was stopped is left on purpose.
    fn drop(&mut self) {
        if self.keep.load(Ordering::SeqCst) {
            return;
        }
        let vcs = std::mem::replace(
            &mut self.vcs,
            Vcs::Jj {
                workspace: String::new(),
            },
        );
        let path = std::mem::take(&mut self.path);
        let repo = std::mem::take(&mut self.repo);
        IN_FLIGHT.fetch_add(1, Ordering::SeqCst);
        let thread = std::thread::Builder::new()
            .name("kage-worktree-drop".to_owned())
            .spawn(move || {
                cleanup(&path, &repo, &vcs);
                IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
            });
        if thread.is_err() {
            // No thread available: skip the cleanup rather than block
            // the caller. A leftover workspace or worktree is
            // recoverable by hand.
            IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

/// Remove a dropped worktree's checkout: `jj workspace forget` plus
/// the directory, or `git worktree remove` plus the branch when it
/// holds no commits.
fn cleanup(path: &Path, repo: &Path, vcs: &Vcs) {
    let target = path.to_string_lossy().into_owned();
    match vcs {
        Vcs::Jj { workspace } => {
            if run(
                repo,
                "jj",
                &["workspace", "forget", workspace],
                DROP_COMMAND_TIMEOUT,
            )
            .is_ok()
            {
                let _ = std::fs::remove_dir_all(path);
            }
        }
        Vcs::Git { branch, base } => {
            // A dirty or staged checkout refuses the plain remove,
            // which the checkpoint could not clean up by committing;
            // the stale directory is not worth keeping either.
            if run(
                repo,
                "git",
                &["worktree", "remove", &target],
                DROP_COMMAND_TIMEOUT,
            )
            .is_err()
                && run(
                    repo,
                    "git",
                    &["worktree", "remove", "--force", &target],
                    DROP_COMMAND_TIMEOUT,
                )
                .is_err()
            {
                return;
            }
            drop_empty_branch(repo, branch, base);
        }
        #[cfg(test)]
        Vcs::Command { program, args } => {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let _ = run(repo, program, &args, DROP_COMMAND_TIMEOUT);
        }
    }
}

/// Remove what a stopped [`Worktree::create`] may have made of the
/// checkout at `path`: the workspace or worktree entry, the directory,
/// and a git branch with no commits of its own.
fn undo(path: &Path, repo: &Path, vcs: &Vcs) {
    let target = path.to_string_lossy().into_owned();
    match vcs {
        Vcs::Jj { workspace } => {
            let args = ["workspace", "forget", workspace.as_str()];
            let _ = run(repo, "jj", &args, DROP_COMMAND_TIMEOUT);
            let _ = std::fs::remove_dir_all(path);
        }
        Vcs::Git { branch, base } => {
            let args = ["worktree", "remove", "--force", target.as_str()];
            let _ = run(repo, "git", &args, DROP_COMMAND_TIMEOUT);
            let _ = std::fs::remove_dir_all(path);
            let _ = run(repo, "git", &["worktree", "prune"], DROP_COMMAND_TIMEOUT);
            drop_empty_branch(repo, branch, base);
        }
        #[cfg(test)]
        Vcs::Command { .. } => {}
    }
}

/// Delete the agent's git `branch` when it holds no commits past `base`.
fn drop_empty_branch(repo: &Path, branch: &str, base: &str) {
    let range = format!("{base}..{branch}");
    let ahead = run(
        repo,
        "git",
        &["rev-list", "--count", &range],
        DROP_COMMAND_TIMEOUT,
    );
    if matches!(ahead.as_deref(), Ok("0")) {
        let _ = run(repo, "git", &["branch", "-D", branch], DROP_COMMAND_TIMEOUT);
    }
}

/// Why [`run`] has no output for the caller.
#[derive(Debug)]
enum RunError {
    /// The command could not start, or ran and failed.
    Failed(String),
    /// The command was stopped by its timeout or a cancel.
    Stopped(String),
}

impl Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (Self::Failed(text) | Self::Stopped(text)) = self;
        f.write_str(text)
    }
}

/// Wait until every drop-path cleanup has finished, or `timeout`
/// passes. Test hook: `false` on timeout.
#[cfg(test)]
fn wait_for_drops(timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if IN_FLIGHT.load(Ordering::SeqCst) == 0 {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    IN_FLIGHT.load(Ordering::SeqCst) == 0
}

/// The trimmed output of `program args` run in `dir`, or what it
/// printed on failure. The child is killed once it outlives `timeout`,
/// and a pipe still open after the child exits is drained for
/// [`DRAIN_GRACE`] at most: no caller may wait on a stuck VCS forever,
/// not even on an orphaned grandchild holding a pipe open.
fn run(dir: &Path, program: &str, args: &[&str], timeout: Duration) -> Result<String, RunError> {
    run_until(dir, program, args, timeout, None)
}

/// Like [`run`], but setting `cancel` stops the child as a timeout
/// does.
fn run_until(
    dir: &Path,
    program: &str,
    args: &[&str],
    timeout: Duration,
    cancel: Option<&CancelFlag>,
) -> Result<String, RunError> {
    let mut command = Command::new(program);
    // No one can answer a prompt: fail it instead of waiting on it.
    command
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Own process group, so a stop also reaches the hooks the VCS
        // started. The group is not the terminal's foreground one: a
        // terminal Ctrl-C does not reach it, and a hook reading the
        // terminal is stopped until the timeout.
        command.process_group(0);
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| RunError::Failed(format!("{program}: {err}")))?;
    // Drain both pipes while polling: a child that fills a pipe would
    // otherwise block until the deadline and be killed.
    let stdout = child.stdout.take().and_then(Drain::start);
    let stderr = child.stderr.take().and_then(Drain::start);
    let deadline = Instant::now() + timeout;
    let status = loop {
        match reaper::try_wait(&mut child) {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(err) => return Err(RunError::Failed(format!("{program}: {err}"))),
        }
        if Instant::now() >= deadline {
            stop(&mut child);
            return Err(RunError::Stopped(format!(
                "{program} {}: killed after {timeout:?}",
                args.first().unwrap_or(&"")
            )));
        }
        if cancel.is_some_and(CancelFlag::is_cancelled) {
            stop(&mut child);
            return Err(RunError::Stopped(format!(
                "{program} {}: cancelled",
                args.first().unwrap_or(&"")
            )));
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    let cutoff = Instant::now() + DRAIN_GRACE;
    let stdout = stdout.map(|drain| drain.take(cutoff)).unwrap_or_default();
    let stderr = stderr.map(|drain| drain.take(cutoff)).unwrap_or_default();
    if status.success() {
        Ok(String::from_utf8_lossy(&stdout).trim().to_owned())
    } else {
        Err(RunError::Failed(format!(
            "{program} {}: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&stderr).trim()
        )))
    }
}

/// Stop a child that timed out or was cancelled, with its process
/// group: SIGTERM first, so git can remove its lock files, then SIGKILL
/// after [`TERM_GRACE`] for whatever is left.
fn stop(child: &mut Child) {
    #[cfg(unix)]
    {
        use nix::sys::signal::{Signal, killpg};
        let pgid = nix::unistd::Pid::from_raw(child.id().cast_signed());
        if killpg(pgid, Signal::SIGTERM).is_ok() {
            let deadline = Instant::now() + TERM_GRACE;
            while matches!(reaper::try_wait(child), Ok(None)) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(25));
            }
            let _ = killpg(pgid, Signal::SIGKILL);
            let _ = reaper::wait(child);
            return;
        }
    }
    let _ = child.kill();
    let _ = reaper::wait(child);
}

/// A pipe drained into a shared buffer on its own thread, so the
/// child never blocks on a full pipe.
struct Drain {
    buffer: Arc<Mutex<Vec<u8>>>,
    handle: JoinHandle<()>,
}

impl Drain {
    /// Start draining `pipe`. `None` when no thread could be started;
    /// the pipe is then closed, so the child cannot block on it.
    fn start(pipe: impl Read + Send + 'static) -> Option<Self> {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let shared = Arc::clone(&buffer);
        let handle = std::thread::Builder::new()
            .spawn(move || {
                let mut pipe = pipe;
                let mut chunk = [0; 8192];
                loop {
                    match pipe.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => lock(&shared).extend_from_slice(&chunk[..n]),
                        Err(err) if err.kind() == ErrorKind::Interrupted => {}
                        Err(_) => break,
                    }
                }
            })
            .ok()?;
        Some(Self { buffer, handle })
    }

    /// Everything read so far, waiting for EOF until `cutoff` at most.
    /// A reader still running then is detached with what it has: an
    /// orphaned grandchild holding the pipe open must not extend the
    /// wait.
    fn take(self, cutoff: Instant) -> Vec<u8> {
        while !self.handle.is_finished() && Instant::now() < cutoff {
            std::thread::sleep(Duration::from_millis(5));
        }
        if self.handle.is_finished() {
            let _ = self.handle.join();
        }
        std::mem::take(&mut lock(&self.buffer))
    }
}

/// The last line of a diff stat, or `None` when nothing changed.
fn summary(stat: &str) -> Option<String> {
    let last = stat.lines().last()?.trim();
    (!last.is_empty() && !last.starts_with("0 files changed")).then(|| last.to_owned())
}

/// `parent_workdir`'s place in `repo`, inside the checkout at `path`.
fn inside(path: &Path, repo: &Path, parent_workdir: &Path) -> PathBuf {
    let canonical = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    canonical(parent_workdir)
        .strip_prefix(canonical(repo))
        .map_or_else(|_| path.to_path_buf(), |rel| path.join(rel))
}

/// The random tail of `id`, short enough for a branch name.
fn short(id: SessionId) -> String {
    let text = id.to_string().to_ascii_lowercase();
    text[text.len() - 8..].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has(program: &str) -> bool {
        Command::new(program).arg("--version").output().is_ok()
    }

    fn sh(dir: &Path, program: &str, args: &[&str]) -> String {
        run(dir, program, args, COMMAND_TIMEOUT).unwrap_or_else(|err| panic!("{err}"))
    }

    /// Install a `name` hook in `repo` that marks the returned file,
    /// then hangs.
    #[cfg(unix)]
    fn hanging_hook(repo: &Path, name: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let started = repo.join(".git").join(format!("{name}-started"));
        let hook = repo.join(".git/hooks").join(name);
        let script = format!("#!/bin/sh\ntouch '{}'\nsleep 30\n", started.display());
        std::fs::write(&hook, script).unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        started
    }

    /// A git repository with one commit and a `sub` directory.
    fn git_repo() -> tempfile::TempDir {
        let repo = tempfile::tempdir().unwrap();
        let dir = repo.path();
        sh(dir, "git", &["init", "-q", "-b", "main"]);
        sh(dir, "git", &["config", "user.name", "kage test"]);
        sh(dir, "git", &["config", "user.email", "test@invalid"]);
        sh(dir, "git", &["config", "commit.gpgsign", "false"]);
        std::fs::create_dir(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/a.txt"), "a\n").unwrap();
        sh(dir, "git", &["add", "-A"]);
        sh(dir, "git", &["commit", "-q", "-m", "init"]);
        repo
    }

    #[test]
    fn a_git_worktree_keeps_a_changed_branch_and_drops_an_unchanged_one() {
        if !has("git") {
            return;
        }
        let repo = git_repo();
        let dir = tempfile::tempdir().unwrap();
        let id = SessionId::new();
        let tree = Worktree::create(
            &repo.path().join("sub"),
            dir.path(),
            id,
            "general: add b".into(),
            &CancelFlag::new(),
        )
        .unwrap();
        assert!(tree.workdir().ends_with("sub"), "{:?}", tree.workdir());
        std::fs::write(tree.workdir().join("b.txt"), "b\n").unwrap();
        let report = tree.checkpoint();
        let branch = format!("kage/agent-{}", short(id));
        assert!(
            report.contains(&format!("branch {branch} (1 file changed")),
            "{report}"
        );
        assert!(report.contains(&format!("git merge {branch}")), "{report}");
        let path = tree.path.clone();
        drop(tree);
        assert!(wait_for_drops(Duration::from_secs(30)));
        assert!(!path.exists());
        assert_eq!(
            sh(repo.path(), "git", &["branch", "--list", &branch]),
            branch
        );

        let id = SessionId::new();
        let tree = Worktree::create(
            repo.path(),
            dir.path(),
            id,
            "general: look".into(),
            &CancelFlag::new(),
        )
        .unwrap();
        assert!(tree.checkpoint().contains("has no changes"));
        drop(tree);
        assert!(wait_for_drops(Duration::from_secs(30)));
        let branch = format!("kage/agent-{}", short(id));
        assert_eq!(sh(repo.path(), "git", &["branch", "--list", &branch]), "");
    }

    #[test]
    fn a_dirty_checkout_is_force_removed_with_its_unchanged_branch() {
        if !has("git") {
            return;
        }
        let repo = git_repo();
        let dir = tempfile::tempdir().unwrap();
        let id = SessionId::new();
        let tree = Worktree::create(
            repo.path(),
            dir.path(),
            id,
            "general: add b".into(),
            &CancelFlag::new(),
        )
        .unwrap();
        // An empty identity makes the checkpoint's commit fail even on
        // a machine with a global one, so the checkout stays staged
        // and the plain remove refuses it.
        sh(tree.workdir(), "git", &["config", "user.name", ""]);
        sh(tree.workdir(), "git", &["config", "user.email", ""]);
        std::fs::write(tree.workdir().join("b.txt"), "b\n").unwrap();
        let report = tree.checkpoint();
        assert!(report.contains("could not be committed"), "{report}");
        let branch = format!("kage/agent-{}", short(id));
        let path = tree.path.clone();
        drop(tree);
        assert!(wait_for_drops(Duration::from_secs(30)));
        assert!(!path.exists(), "the dirty checkout is removed anyway");
        assert_eq!(sh(repo.path(), "git", &["branch", "--list", &branch]), "");
    }

    #[test]
    fn a_jj_workspace_leaves_its_change_after_the_drop() {
        if !has("jj") {
            return;
        }
        let repo = tempfile::tempdir().unwrap();
        sh(repo.path(), "jj", &["git", "init", "--quiet"]);
        std::fs::write(repo.path().join("a.txt"), "a\n").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let tree = Worktree::create(
            repo.path(),
            dir.path(),
            SessionId::new(),
            "general".into(),
            &CancelFlag::new(),
        )
        .unwrap();
        assert!(tree.checkpoint().contains("has no changes"));
        std::fs::write(tree.workdir().join("b.txt"), "b\n").unwrap();
        let report = tree.checkpoint();
        assert!(report.contains("(1 file changed"), "{report}");
        let change = report
            .split("jj change ")
            .nth(1)
            .and_then(|rest| rest.split(' ').next())
            .unwrap()
            .to_owned();
        let path = tree.path.clone();
        drop(tree);
        assert!(wait_for_drops(Duration::from_secs(30)));
        assert!(!path.exists());
        let files = sh(repo.path(), "jj", &["file", "list", "-r", &change]);
        assert!(files.lines().any(|f| f == "b.txt"), "{files}");
    }

    #[test]
    fn outside_a_repository_there_is_no_worktree() {
        let plain = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let err = Worktree::create(
            plain.path(),
            dir.path(),
            SessionId::new(),
            String::new(),
            &CancelFlag::new(),
        )
        .unwrap_err();
        assert_eq!(err, "worktree isolation needs a git or jj repository");
    }

    /// A cleanup command that writes more than a pipe holds still
    /// finishes instead of blocking until the bounded kill.
    #[cfg(unix)]
    #[test]
    fn a_chatty_cleanup_command_is_not_killed() {
        let dir = tempfile::tempdir().unwrap();
        let script = "head -c 200000 /dev/zero | tr '\\0' a; head -c 200000 /dev/zero >&2";
        let out = run(dir.path(), "sh", &["-c", script], DROP_COMMAND_TIMEOUT).unwrap();
        assert_eq!(out.len(), 200_000);
    }

    /// A command that exits at once but leaves an orphaned grandchild
    /// holding stdout open returns with its output instead of waiting
    /// for the grandchild to let go of the pipe.
    #[cfg(unix)]
    #[test]
    fn an_orphaned_grandchild_does_not_extend_the_wait() {
        let dir = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let out = run(
            dir.path(),
            "sh",
            &["-c", "printf ready; sleep 5 &"],
            DROP_COMMAND_TIMEOUT,
        )
        .unwrap();
        assert_eq!(out, "ready");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "waited {:?} for a grandchild holding the pipe",
            started.elapsed()
        );
    }

    /// A hung create or checkpoint command is killed at its timeout and
    /// reported, so the parent still gets the agent's result.
    #[cfg(unix)]
    #[test]
    fn a_hanging_command_is_killed_and_reported() {
        let dir = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let err = run(dir.path(), "sleep", &["30"], Duration::from_millis(200)).unwrap_err();
        assert!(
            matches!(&err, RunError::Stopped(text) if text.contains("killed after")),
            "{err}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "waited {:?} for a hung command",
            started.elapsed()
        );
    }

    /// Cancelling stops a command in flight, as a timeout does.
    #[cfg(unix)]
    #[test]
    fn a_cancelled_command_is_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancelFlag::new();
        let started = Instant::now();
        let stopper = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            stopper.cancel();
        });
        let err =
            run_until(dir.path(), "sleep", &["30"], COMMAND_TIMEOUT, Some(&cancel)).unwrap_err();
        assert!(
            matches!(&err, RunError::Stopped(text) if text.ends_with("cancelled")),
            "{err}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "waited {:?} for a cancelled command",
            started.elapsed()
        );
    }

    /// A timed-out command gets SIGTERM before SIGKILL, so git can
    /// remove its lock files.
    #[cfg(unix)]
    #[test]
    fn a_timed_out_command_is_terminated_before_it_is_killed() {
        let dir = tempfile::tempdir().unwrap();
        let script = "trap 'touch terminated; exit 1' TERM; sleep 30 & wait";
        let err = run(
            dir.path(),
            "sh",
            &["-c", script],
            Duration::from_millis(200),
        )
        .unwrap_err();
        assert!(
            matches!(&err, RunError::Stopped(text) if text.contains("killed after")),
            "{err}"
        );
        assert!(dir.path().join("terminated").exists());
    }

    /// A timeout stops the whole process group, not only the direct
    /// child, so a hook the VCS started does not outlive it.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_timeout_stops_the_grandchildren_too() {
        let dir = tempfile::tempdir().unwrap();
        let script = "trap '' TERM; sleep 30 & echo $! > pid; wait";
        let err = run(
            dir.path(),
            "sh",
            &["-c", script],
            Duration::from_millis(200),
        )
        .unwrap_err();
        assert!(
            matches!(&err, RunError::Stopped(text) if text.contains("killed after")),
            "{err}"
        );
        let pid = std::fs::read_to_string(dir.path().join("pid")).unwrap();
        let stat = format!("/proc/{}/stat", pid.trim());
        let deadline = Instant::now() + Duration::from_secs(5);
        let alive = loop {
            let alive = std::fs::read_to_string(&stat).is_ok_and(|text| {
                text.rsplit(')')
                    .next()
                    .is_some_and(|rest| !rest.trim_start().starts_with('Z'))
            });
            if !alive || Instant::now() >= deadline {
                break alive;
            }
            std::thread::sleep(Duration::from_millis(25));
        };
        assert!(!alive, "the grandchild survived the timeout");
    }

    /// A create stopped while `git worktree add` runs undoes what it
    /// made: no worktree entry, branch or directory is left behind.
    #[cfg(unix)]
    #[test]
    fn a_stopped_git_checkout_is_undone() {
        if !has("git") {
            return;
        }
        let repo = git_repo();
        let started = hanging_hook(repo.path(), "post-checkout");
        let dir = tempfile::tempdir().unwrap();
        let id = SessionId::new();
        let cancel = CancelFlag::new();
        let stopper = cancel.clone();
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(20);
            while !started.exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            stopper.cancel();
        });
        let err =
            Worktree::create(repo.path(), dir.path(), id, String::new(), &cancel).unwrap_err();
        assert!(err.contains("cancelled"), "{err}");
        assert!(!dir.path().join(id.to_string()).exists());
        let branch = format!("kage/agent-{}", short(id));
        assert_eq!(sh(repo.path(), "git", &["branch", "--list", &branch]), "");
        let worktrees = sh(repo.path(), "git", &["worktree", "list"]);
        assert_eq!(worktrees.lines().count(), 1, "{worktrees}");
    }

    /// A checkpoint whose commit is stopped keeps the checkout and its
    /// branch on drop, so the uncommitted work is not deleted.
    #[cfg(unix)]
    #[test]
    fn a_stopped_checkpoint_keeps_the_checkout() {
        if !has("git") {
            return;
        }
        let repo = git_repo();
        let dir = tempfile::tempdir().unwrap();
        let id = SessionId::new();
        let tree = Worktree::create(
            repo.path(),
            dir.path(),
            id,
            "general: add b".into(),
            &CancelFlag::new(),
        )
        .unwrap();
        hanging_hook(repo.path(), "pre-commit");
        std::fs::write(tree.workdir().join("b.txt"), "b\n").unwrap();
        let report = tree.checkpoint();
        assert!(report.contains("killed after"), "{report}");
        assert!(report.contains("is kept"), "{report}");
        let path = tree.path.clone();
        drop(tree);
        assert!(wait_for_drops(Duration::from_secs(30)));
        assert!(
            path.join("b.txt").exists(),
            "the uncommitted work was deleted"
        );
        let branch = format!("kage/agent-{}", short(id));
        let listed = sh(repo.path(), "git", &["branch", "--list", &branch]);
        assert!(
            listed.contains(&branch),
            "the branch was deleted: {listed:?}"
        );
    }

    /// A drop whose cleanup command hangs must return at once, and the
    /// bounded kill must end the command well inside the command's own
    /// runtime.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_hanging_cleanup_command_does_not_block_the_drop_and_is_killed() {
        let repo = tempfile::tempdir().unwrap();
        let tree = Worktree {
            path: repo.path().join("wt"),
            workdir: repo.path().join("wt"),
            repo: repo.path().to_path_buf(),
            label: String::new(),
            vcs: Vcs::Command {
                program: "sleep".to_owned(),
                args: vec!["30".to_owned()],
            },
            keep: AtomicBool::new(false),
        };
        let start = Instant::now();
        drop(tree);
        let drop_took = start.elapsed();
        assert!(
            drop_took < Duration::from_secs(1),
            "drop blocked for {drop_took:?}"
        );
        assert!(
            wait_for_drops(Duration::from_secs(20)),
            "the cleanup thread never finished"
        );
        let cleanup_took = start.elapsed();
        assert!(
            cleanup_took < Duration::from_secs(15),
            "cleanup ran {cleanup_took:?}, not bounded well under the 30s sleep"
        );
        assert!(
            cleanup_took >= DROP_COMMAND_TIMEOUT.saturating_sub(Duration::from_secs(2)),
            "cleanup returned in {cleanup_took:?}; the kill path never fired"
        );
    }
}
