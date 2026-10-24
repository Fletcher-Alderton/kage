//! Checkouts of their own for agents whose definition asks for
//! `isolation: worktree`: a jj workspace, or a git worktree on a branch
//! of its own, outside the repository. One is made at spawn,
//! checkpointed at the end of each run and removed when the agent is
//! dropped. Nothing is merged: the agent's report says where its work
//! is and how to take it.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use kage_core::SessionId;
use kage_core::reaper;

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
    /// the agent's work in a git commit.
    pub(super) fn create(
        parent_workdir: &Path,
        dir: &Path,
        id: SessionId,
        label: String,
    ) -> Result<Self, String> {
        let short = short(id);
        let path = dir.join(id.to_string());
        let target = path.to_string_lossy().into_owned();
        let made = |what: &str, err: String| format!("cannot make the worktree {what}: {err}");
        if let Ok(root) = run(parent_workdir, "jj", &["root"]) {
            std::fs::create_dir_all(dir).map_err(|err| made("directory", err.to_string()))?;
            let workspace = format!("kage-{short}");
            let args = ["workspace", "add", "--name", &workspace, "-r", "@", &target];
            run(parent_workdir, "jj", &args).map_err(|err| made("workspace", err))?;
            let repo = PathBuf::from(root);
            return Ok(Self {
                workdir: inside(&path, &repo, parent_workdir),
                path,
                repo,
                label,
                vcs: Vcs::Jj { workspace },
            });
        }
        if let Ok(root) = run(parent_workdir, "git", &["rev-parse", "--show-toplevel"]) {
            std::fs::create_dir_all(dir).map_err(|err| made("directory", err.to_string()))?;
            let repo = PathBuf::from(root);
            let base =
                run(&repo, "git", &["rev-parse", "HEAD"]).map_err(|err| made("base", err))?;
            let branch = format!("kage/agent-{short}");
            let args = ["worktree", "add", "-b", &branch, &target, "HEAD"];
            run(&repo, "git", &args).map_err(|err| made("checkout", err))?;
            return Ok(Self {
                workdir: inside(&path, &repo, parent_workdir),
                path,
                repo,
                label,
                vcs: Vcs::Git { branch, base },
            });
        }
        Err("worktree isolation needs a git or jj repository".to_owned())
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
                let read = run(&self.path, "jj", &template).and_then(|change| {
                    run(&self.path, "jj", &["diff", "--stat", "-r", "@"]).map(|stat| (change, stat))
                });
                match read {
                    Ok((change, stat)) => match summary(&stat) {
                        Some(stat) => format!(
                            "Worktree: jj change {change} in workspace {workspace} ({stat}). To \
                             take it: jj squash --from {change}. Nothing was merged."
                        ),
                        None => format!("Worktree: workspace {workspace} has no changes."),
                    },
                    Err(err) => format!("Worktree: the checkout at {at} could not be read: {err}"),
                }
            }
            Vcs::Git { branch, base } => {
                let committed = run(&self.path, "git", &["status", "--porcelain"])
                    .and_then(|status| {
                        if status.is_empty() {
                            return Ok(());
                        }
                        run(&self.path, "git", &["add", "-A"])?;
                        run(&self.path, "git", &["commit", "-q", "-m", &self.label]).map(|_| ())
                    })
                    .and_then(|()| {
                        run(
                            &self.path,
                            "git",
                            &["diff", "--stat", &format!("{base}..HEAD")],
                        )
                    });
                match committed {
                    Ok(stat) => match summary(&stat) {
                        Some(stat) => format!(
                            "Worktree: branch {branch} ({stat}). To take it: git merge {branch}. \
                             Nothing was merged."
                        ),
                        None => format!("Worktree: branch {branch} has no changes."),
                    },
                    Err(err) => {
                        format!("Worktree: the work at {at} could not be committed: {err}")
                    }
                }
            }
            #[cfg(test)]
            Vcs::Command { .. } => {
                unreachable!("the test-only cleanup command has nothing to checkpoint")
            }
        }
    }
}

/// How long one drop-path VCS command may run before it is killed:
/// generous for a real `git` or `jj` on a slow disk, but bounded so a
/// stuck VCS cannot hold the cleanup thread forever.
const DROP_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

/// Drop-path cleanups started but not finished. Production drops are
/// detached and unsignalled; only tests wait for this to reach zero.
static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

impl Drop for Worktree {
    /// Removes the checkout on a detached thread so a slow VCS never
    /// blocks the caller, with every command killed at
    /// [`DROP_COMMAND_TIMEOUT`]. Best-effort: when the process exits
    /// before the thread finishes, the checkout is left behind.
    fn drop(&mut self) {
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
            if run_bounded(repo, "jj", &["workspace", "forget", workspace]).is_ok() {
                let _ = std::fs::remove_dir_all(path);
            }
        }
        Vcs::Git { branch, base } => {
            // A dirty or staged checkout refuses the plain remove,
            // which the checkpoint could not clean up by committing;
            // the stale directory is not worth keeping either.
            if run_bounded(repo, "git", &["worktree", "remove", &target]).is_err()
                && run_bounded(repo, "git", &["worktree", "remove", "--force", &target]).is_err()
            {
                return;
            }
            let ahead = run_bounded(
                repo,
                "git",
                &["rev-list", "--count", &format!("{base}..{branch}")],
            );
            if ahead.as_deref() == Ok("0") {
                let _ = run_bounded(repo, "git", &["branch", "-D", branch]);
            }
        }
        #[cfg(test)]
        Vcs::Command { program, args } => {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let _ = run_bounded(repo, program, &args);
        }
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
/// printed on failure.
fn run(dir: &Path, program: &str, args: &[&str]) -> Result<String, String> {
    let mut command = Command::new(program);
    command.args(args).current_dir(dir);
    let output = reaper::output(&mut command).map_err(|err| format!("{program}: {err}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        Err(format!(
            "{program} {}: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// Like [`run`], but the child is killed once it outlives
/// [`DROP_COMMAND_TIMEOUT`]: a drop must never wait on a stuck VCS
/// forever.
fn run_bounded(dir: &Path, program: &str, args: &[&str]) -> Result<String, String> {
    let mut command = Command::new(program);
    command.args(args).current_dir(dir);
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("{program}: {err}"))?;
    // Drain both pipes while polling: a child that fills a pipe would
    // otherwise block until the deadline and be killed.
    let stdout = child.stdout.take().and_then(drain);
    let stderr = child.stderr.take().and_then(drain);
    let deadline = Instant::now() + DROP_COMMAND_TIMEOUT;
    let status = loop {
        match reaper::try_wait(&mut child) {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(err) => return Err(format!("{program}: {err}")),
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = reaper::wait(&mut child);
            return Err(format!(
                "{program} {}: killed after {DROP_COMMAND_TIMEOUT:?}",
                args.first().unwrap_or(&"")
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    let stdout = collect(stdout);
    let stderr = collect(stderr);
    if status.success() {
        Ok(String::from_utf8_lossy(&stdout).trim().to_owned())
    } else {
        Err(format!(
            "{program} {}: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&stderr).trim()
        ))
    }
}

/// Read `pipe` to EOF on its own thread. `None` when no thread could
/// be started; the pipe is then closed, so the child cannot block on it.
fn drain(mut pipe: impl Read + Send + 'static) -> Option<JoinHandle<Vec<u8>>> {
    std::thread::Builder::new()
        .spawn(move || {
            let mut buf = Vec::new();
            let _ = pipe.read_to_end(&mut buf);
            buf
        })
        .ok()
}

/// What a [`drain`] thread read, or nothing if it never ran.
fn collect(reader: Option<JoinHandle<Vec<u8>>>) -> Vec<u8> {
    reader
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default()
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
        run(dir, program, args).unwrap_or_else(|err| panic!("{err}"))
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
        let tree = Worktree::create(repo.path(), dir.path(), id, "general: look".into()).unwrap();
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
        let tree = Worktree::create(repo.path(), dir.path(), id, "general: add b".into()).unwrap();
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
        let tree =
            Worktree::create(repo.path(), dir.path(), SessionId::new(), "general".into()).unwrap();
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
        let err = Worktree::create(plain.path(), dir.path(), SessionId::new(), String::new())
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
        let out = run_bounded(dir.path(), "sh", &["-c", script]).unwrap();
        assert_eq!(out.len(), 200_000);
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
