//! Project trust: the TUI prompt, the non-interactive warning and
//! `kage trust`.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use kage_core::trust::{self, TrustSummary};

/// Before the TUI starts, ask whether to trust `workdir`'s project
/// when it has untrusted risky settings or agents. Yes records trust,
/// and anything else runs with those settings and agents ignored.
/// Without a terminal on stdin this only prints the warning.
pub(crate) fn confirm_project_trust(workdir: &Path) {
    let Some(summary) = trust::untrusted_project(workdir) else {
        return;
    };
    if !std::io::stdin().is_terminal() {
        eprintln!("{}", warning(workdir, &summary));
        return;
    }
    let mut err = std::io::stderr().lock();
    let _ = writeln!(
        err,
        "kage: {} wants to enable settings you have not trusted:",
        summary.path.display()
    );
    for item in &summary.items {
        let _ = writeln!(err, "  - {item}");
    }
    let _ = write!(err, "Trust this project? [y/N] ");
    let _ = err.flush();
    drop(err);
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    if !matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        eprintln!("kage: continuing without them. Run `kage trust` later to allow them.");
        return;
    }
    if let Err(e) = trust::trust_project(workdir, &summary) {
        eprintln!("kage: trust: {e}");
    }
}

/// Print one stderr line when `workdir`'s project has untrusted risky
/// settings or agents that this run ignores.
pub(crate) fn warn_if_untrusted(workdir: &Path) {
    if let Some(summary) = trust::untrusted_project(workdir) {
        eprintln!("{}", warning(workdir, &summary));
    }
}

fn warning(workdir: &Path, summary: &TrustSummary) -> String {
    let settings: Vec<&str> = summary
        .keys
        .iter()
        .copied()
        .filter(|key| *key != "agents" && *key != "skills")
        .collect();
    let mut parts = Vec::new();
    if !settings.is_empty() {
        parts.push(format!(
            ".kage/config.toml settings ({})",
            settings.join(", ")
        ));
    }
    if !summary.agents.is_empty() {
        parts.push(format!("project agents ({})", summary.agents.join(", ")));
    }
    if !summary.skills.is_empty() {
        parts.push(format!("project skills ({})", summary.skills.join(", ")));
    }
    format!(
        "kage: ignoring untrusted {} in {}. \
         Run `kage trust` in that directory to allow them.",
        parts.join(" and "),
        workdir.display()
    )
}

/// `kage trust [--revoke]` for the current directory.
pub(crate) fn run(revoke: bool) -> ExitCode {
    let workdir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if revoke {
        return match trust::revoke_project(&workdir) {
            Ok(true) => {
                eprintln!("kage: revoked trust for {}", workdir.display());
                ExitCode::SUCCESS
            }
            Ok(false) => {
                eprintln!("kage: {} was not trusted", workdir.display());
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("kage: trust: {e}");
                ExitCode::from(1)
            }
        };
    }
    let Some(presented) = trust::untrusted_project(&workdir) else {
        eprintln!(
            "kage: {} has no mcp, permissions, plugins.capabilities or tools settings and no agents to trust",
            workdir.join(".kage").display()
        );
        return ExitCode::SUCCESS;
    };
    match trust::trust_project(&workdir, &presented) {
        Ok(Some(summary)) => {
            eprintln!("kage: trusted {}:", summary.path.display());
            for item in &summary.items {
                eprintln!("  - {item}");
            }
            ExitCode::SUCCESS
        }
        Ok(None) => {
            eprintln!("kage: nothing left to trust in {}", workdir.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("kage: trust: {e}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(keys: Vec<&'static str>, agents: &[&str], skills: &[&str]) -> TrustSummary {
        TrustSummary {
            path: PathBuf::from("/p/.kage"),
            keys,
            items: Vec::new(),
            agents: agents.iter().map(|a| (*a).to_owned()).collect(),
            skills: skills.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn warning_names_settings_and_agents() {
        let dir = Path::new("/p");
        assert_eq!(
            warning(
                dir,
                &summary(vec!["mcp", "agents"], &["reviewer", "auditor"], &[])
            ),
            "kage: ignoring untrusted .kage/config.toml settings (mcp) and project agents \
             (reviewer, auditor) in /p. Run `kage trust` in that directory to allow them."
        );
        assert_eq!(
            warning(dir, &summary(vec!["agents"], &["reviewer"], &[])),
            "kage: ignoring untrusted project agents (reviewer) in /p. \
             Run `kage trust` in that directory to allow them."
        );
        assert_eq!(
            warning(dir, &summary(vec!["skills"], &[], &["helper"])),
            "kage: ignoring untrusted project skills (helper) in /p. \
             Run `kage trust` in that directory to allow them."
        );
    }
}
