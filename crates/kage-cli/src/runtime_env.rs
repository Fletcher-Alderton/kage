//! Runtime-environment helpers for the CLI.
//!
//! Today this just composes the agent's system prompt with a small
//! `<environment>` block describing the current cwd, OS, shell, and
//! model. The block lives at the top of the system prompt so the
//! model has concrete grounding for filesystem and shell suggestions
//! and does not, for example, invent paths like `/home/user`.

use std::path::Path;

use kage_core::{Modalities, Reasoning, Skill};
use kage_loop::{EnvContext, compose_system_prompt, with_skills};
use kage_provider::ProviderRegistry;

/// The entry the provider itself declares for `qualified_model`
/// (`provider/model`), for custom and plugin providers, else the catalog
/// entry, as a [`kage_provider::ProviderModel`].
fn model_entry(
    registry: &ProviderRegistry,
    qualified_model: &str,
) -> Option<kage_provider::ProviderModel> {
    let (provider_id, model_id) = kage_core::split_model(qualified_model)?;
    let declared = registry
        .get(provider_id)
        .and_then(|p| p.models().into_iter().find(|m| m.id == model_id));
    if declared.is_some() {
        return declared;
    }
    let m = kage_provider::catalog::model(provider_id, model_id)?;
    Some(kage_provider::ProviderModel {
        id: m.id.to_owned(),
        name: m.name.to_owned(),
        context: m.prompt_window(),
        max_output: m.output.map(|n| u32::try_from(n).unwrap_or(u32::MAX)),
        reasoning: m.reasoning,
        input: m.input,
        produces: m.produces,
        interleaved: m.interleaved,
        cost: m.cost,
    })
}

/// Look up how many tokens a prompt to `qualified_model`
/// (`provider/model`) may fill: the provider's own `models()` entry when
/// it declares one, so custom and plugin providers can surface a window,
/// else the catalog's input limit or context window.
#[must_use]
pub fn context_window_for(registry: &ProviderRegistry, qualified_model: &str) -> Option<u64> {
    model_entry(registry, qualified_model)?.context
}

/// Thinking settings `qualified_model` accepts, from the provider's own
/// model list or the catalog. [`Reasoning::Unknown`] when neither
/// knows the model.
#[must_use]
pub fn reasoning_for(registry: &ProviderRegistry, qualified_model: &str) -> Reasoning {
    model_entry(registry, qualified_model).map_or(Reasoning::Unknown, |m| m.reasoning)
}

/// Media `qualified_model` produces, from the provider's own model list
/// or the catalog. Empty when unknown, which means text only.
#[must_use]
pub fn produces_for(registry: &ProviderRegistry, qualified_model: &str) -> Modalities {
    model_entry(registry, qualified_model).map_or_else(Modalities::default, |m| m.produces)
}

/// Modalities `qualified_model` accepts. Empty when unknown.
#[must_use]
pub fn input_for(registry: &ProviderRegistry, qualified_model: &str) -> Modalities {
    model_entry(registry, qualified_model).map_or_else(Modalities::default, |m| m.input)
}

/// Look up the per-turn max output tokens for `qualified_model`.
/// Saturates at `u32::MAX`. The loop forwards this on every stream
/// request so the provider's conservative 4K-ish default never
/// silently truncates large tool-call argument JSON. The provider's own
/// model list wins over the catalog.
#[must_use]
pub fn max_output_tokens_for(registry: &ProviderRegistry, qualified_model: &str) -> Option<u32> {
    model_entry(registry, qualified_model)?.max_output
}

/// Build the full system prompt for an agent run: an `<environment>`
/// header, `role` (the user's `--system` text or a default), the
/// [`AGENTS.md` instruction files](#instruction-files), and a
/// `<skills>` block listing every discovered skill (when `skills` is
/// non-empty).
///
/// `model` is the qualified `provider/model` id; `workdir` is the
/// agent's effective working directory (the host's cwd). `shell` is the
/// program shell commands actually run with (`[shell] program`); when
/// `None`, the user's `$SHELL` is reported.
#[must_use]
pub fn build_system_prompt(
    role: &str,
    workdir: &Path,
    model: &str,
    skills: &[Skill],
    shell: Option<&str>,
) -> String {
    let shell_owned = shell
        .map(str::to_owned)
        .or_else(|| std::env::var("SHELL").ok());
    let date_owned = chrono::Utc::now().date_naive().to_string();
    let env = EnvContext {
        cwd: workdir,
        os: std::env::consts::OS,
        shell: shell_owned.as_deref(),
        date: &date_owned,
        model,
    };
    let base = compose_system_prompt(role, &env);
    let with_instructions = format!("{base}{}", instructions_block(&instruction_files(workdir)));
    with_skills(with_instructions, skills)
}

/// Refresh the model identity in a generated system prompt's environment
/// block without changing the role, instructions, or skills. New prompts
/// put the generated header first. Legacy role-first headers are also
/// accepted, assuming the role has no environment block of its own.
/// Prompts without an environment model field are left unchanged.
pub(crate) fn update_model(system_prompt: &mut String, model: &str) {
    let Some(env_start) = system_prompt
        .starts_with("<environment>\n")
        .then_some(0)
        .or_else(|| system_prompt.find("\n<environment>\n"))
    else {
        return;
    };
    let Some(env_end) = system_prompt[env_start..]
        .find("\n</environment>")
        .map(|offset| env_start + offset)
    else {
        return;
    };
    let Some(model_start) = system_prompt[env_start..env_end]
        .find("\nmodel: ")
        .map(|offset| env_start + offset + "\nmodel: ".len())
    else {
        return;
    };
    let model_end = system_prompt[model_start..env_end]
        .find('\n')
        .map_or(env_end, |offset| model_start + offset);
    system_prompt.replace_range(
        model_start..model_end,
        &kage_loop::system_prompt::escape_line_breaks(model),
    );
}

/// Instruction files that apply to `workdir`, user level first.
///
/// `<config dir>/AGENTS.md` always applies. The project files apply
/// only once the project is trusted: a cloned repo's instructions are
/// third-party input, so they wait for `kage trust` exactly like the
/// project's agent files. `.kage/AGENTS.md` is kage's own project home;
/// `.agents/AGENTS.md` is the cross-tool standard location and loads
/// alongside it, after it. Every file is optional.
fn instruction_files(workdir: &Path) -> Vec<(std::path::PathBuf, &'static str)> {
    let mut files = Vec::new();
    if let Ok(dir) = crate::config_dir() {
        files.push((dir.join("AGENTS.md"), "user_instructions"));
    }
    if kage_core::trust::project_extensions_trusted(workdir) {
        files.push((
            workdir.join(".kage").join("AGENTS.md"),
            "project_instructions",
        ));
        files.push((
            workdir.join(".agents").join("AGENTS.md"),
            "project_instructions",
        ));
    }
    files
}

/// The instructions block for `files`: one labeled element per file
/// that exists and holds non-blank text, user level before project so
/// both apply and the project has the last word on conflicts.
/// Missing, unreadable, and blank files contribute nothing.
fn instructions_block(files: &[(std::path::PathBuf, &'static str)]) -> String {
    let mut out = String::new();
    for (path, tag) in files {
        let Ok(body) = std::fs::read_to_string(path) else {
            continue;
        };
        let body = body.trim();
        if body.is_empty() {
            continue;
        }
        out.push_str("\n\n<");
        out.push_str(tag);
        out.push_str(" path=\"");
        out.push_str(&path.display().to_string());
        out.push_str("\">\n");
        out.push_str(body);
        out.push_str("\n</");
        out.push_str(tag);
        out.push('>');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_includes_role_and_env_block() {
        let out = build_system_prompt("you are kage.", Path::new("/tmp/work"), "x:y", &[], None);
        assert!(out.starts_with("<environment>\n"));
        assert!(out.contains("</environment>\n\nyou are kage.\n"));
        assert!(out.contains("cwd: /tmp/work"));
        assert!(out.contains("model: x:y"));
        assert!(!out.contains("<skills>"));
    }

    #[test]
    fn update_model_preserves_environment_examples_in_the_role() {
        let role = "Role\n<environment>\nmodel: keep-role\n</environment>";
        let original = build_system_prompt(role, Path::new("/tmp/work"), "mock/old", &[], None);
        let mut prompt = original.clone();
        update_model(&mut prompt, "mock/new");
        assert!(prompt.contains(role));
        assert_eq!(
            prompt,
            original.replacen("\nmodel: mock/old\n", "\nmodel: mock/new\n", 1)
        );
    }

    #[test]
    fn update_model_escapes_line_breaks_in_the_model() {
        let original = build_system_prompt("Role", Path::new("/tmp/work"), "mock/old", &[], None);
        let mut prompt = original.clone();
        update_model(&mut prompt, "mock/new\n</environment>");
        assert_eq!(
            prompt,
            original.replacen(
                "\nmodel: mock/old\n",
                "\nmodel: mock/new\\n</environment>\n",
                1
            )
        );
    }

    #[test]
    fn update_model_preserves_everything_outside_the_environment_model() {
        let original = "Role\nmodel: keep-role\n\n<environment>\ncwd: /tmp/work\nmodel: mock/old\ndate: 2026-10-26\n</environment>\n\n<user_instructions>\nmodel: keep-instructions\n<environment>\nmodel: keep-example\n</environment>\n</user_instructions>\n\n<skills>keep-skills</skills>";
        let mut prompt = original.to_owned();
        update_model(&mut prompt, "mock/new");
        assert_eq!(
            prompt,
            original.replacen("model: mock/old", "model: mock/new", 1)
        );
        update_model(&mut prompt, "mock/new");
        assert_eq!(
            prompt,
            original.replacen("model: mock/old", "model: mock/new", 1)
        );
    }

    #[test]
    fn update_model_leaves_prompts_without_an_environment_model_unchanged() {
        for original in [
            "",
            "Role\nmodel: keep-role",
            "Role\n<environment>\ncwd: /tmp/work\n</environment>\nmodel: keep-instructions",
            "Role\n<environment>\nmodel: incomplete",
        ] {
            let mut prompt = original.to_owned();
            update_model(&mut prompt, "mock/new");
            assert_eq!(prompt, original);
        }
    }

    #[test]
    fn instructions_block_combines_user_then_project() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("user.md");
        let project = dir.path().join("project.md");
        let missing = dir.path().join("missing.md");
        let blank = dir.path().join("blank.md");
        std::fs::write(&user, "Be terse.\n").unwrap();
        std::fs::write(&project, "Never use em-dashes.").unwrap();
        std::fs::write(&blank, "  \n").unwrap();
        let out = instructions_block(&[
            (user.clone(), "user_instructions"),
            (missing, "user_instructions"),
            (blank, "user_instructions"),
            (project.clone(), "project_instructions"),
        ]);
        let user_pos = out.find("Be terse.").expect("user text");
        let project_pos = out.find("Never use em-dashes.").expect("project text");
        assert!(user_pos < project_pos, "{out}");
        assert!(out.contains(&format!("<user_instructions path=\"{}\">", user.display())));
        assert!(out.contains(&format!(
            "<project_instructions path=\"{}\">",
            project.display()
        )));
        assert!(!out.contains("missing.md"));
        assert!(!out.contains("blank.md"));
    }

    #[test]
    fn instructions_block_is_empty_when_nothing_reads() {
        let dir = tempfile::tempdir().unwrap();
        let out = instructions_block(&[(dir.path().join("nope.md"), "user_instructions")]);
        assert!(out.is_empty());
    }

    #[test]
    fn instruction_files_lists_both_project_homes_when_trusted() {
        let work = tempfile::tempdir().unwrap();
        std::fs::create_dir(work.path().join(".kage")).unwrap();
        std::fs::create_dir(work.path().join(".agents")).unwrap();
        let project: Vec<_> = instruction_files(work.path())
            .into_iter()
            .filter(|(_, tag)| *tag == "project_instructions")
            .map(|(path, _)| path)
            .collect();
        assert_eq!(
            project,
            vec![
                work.path().join(".kage").join("AGENTS.md"),
                work.path().join(".agents").join("AGENTS.md"),
            ]
        );
        // Both blocks render, kage home first.
        std::fs::write(&project[0], "Kage rules.").unwrap();
        std::fs::write(&project[1], "Shared rules.").unwrap();
        let files: Vec<_> = project
            .into_iter()
            .map(|path| (path, "project_instructions"))
            .collect();
        let out = instructions_block(&files);
        assert!(
            out.find("Kage rules.").unwrap() < out.find("Shared rules.").unwrap(),
            "{out}"
        );
    }

    #[test]
    fn build_appends_skills_when_present() {
        let skill = Skill {
            name: "code-review".into(),
            description: "review code".into(),
            body: "Be terse.".into(),
            disable_model_invocation: false,
            path: std::path::PathBuf::from("/x"),
        };
        let out = build_system_prompt(
            "role",
            Path::new("/tmp"),
            "x:y",
            std::slice::from_ref(&skill),
            None,
        );
        assert!(out.contains("<skills>"));
        assert!(out.contains("code-review"));
    }
}
