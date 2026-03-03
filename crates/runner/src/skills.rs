//! Skill loader: discovers, evaluates, renders, and formats markdown skills
//! for injection into the system prompt.
//!
//! Skills are markdown files with YAML frontmatter that teach the LLM
//! domain-specific workflows using existing tools. The loader scans three
//! directory tiers (system → user → workspace), deduplicates by name
//! (workspace wins), evaluates activation conditions against tool readiness
//! and environment variables, renders `{{VAR}}` placeholders, and produces
//! prompt-ready text.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gray_matter::Matter;
use gray_matter::engine::YAML;
use rust_embed::Embed;
use tools::ToolAvailability;
use types::{RenderedSkill, Skill, SkillActivation, SkillMetadata};

/// Built-in skills embedded at compile time from `config/skills/`.
/// These serve as the lowest-priority tier — filesystem skills (system, user,
/// workspace) override embedded ones by name.
#[derive(Embed)]
#[folder = "../../config/skills/"]
struct BuiltinSkills;

/// Maximum estimated token count for a single skill body.
/// Estimated as `chars / 4`. Skills exceeding this are rejected.
const MAX_SKILL_TOKENS: usize = 3000;

/// Character-to-token ratio used for the token estimate.
const CHARS_PER_TOKEN: usize = 4;

/// Skill directories relative to each config tier.
const SKILLS_SUBDIR: &str = "skills";

/// The canonical skill file name inside a skill folder.
const SKILL_FILE_NAME: &str = "SKILL.md";

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Scans skill directories, deduplicates by `name`, and returns all
/// successfully parsed skills. Invalid files are logged and skipped.
///
/// Skills can be either:
/// - **Folder-based:** A subdirectory containing `SKILL.md` and optionally a
///   `references/` subdirectory with supplementary files. E.g.
///   `skills/BrowserAutomation/SKILL.md`.
/// - **Bare file:** A `.md` file directly in the `skills/` directory (for
///   simple skills without reference files).
///
/// Precedence (highest to lowest): `workspace_dir` → `user_dir` → `system_dir`
/// → embedded built-ins. A skill with the same `name` at a higher-precedence
/// tier replaces the lower-tier one entirely (no merging).
pub fn discover_skills(
    system_dir: &Path,
    user_dir: Option<&Path>,
    workspace_dir: &Path,
) -> Vec<Skill> {
    // Start with embedded built-in skills (lowest precedence).
    let mut skills_by_name: HashMap<String, Skill> = discover_embedded_skills();

    // System → user → workspace (each overriding lower tiers).
    let dirs: Vec<PathBuf> = [
        Some(system_dir.join(SKILLS_SUBDIR)),
        user_dir.map(|d| d.join(SKILLS_SUBDIR)),
        Some(workspace_dir.join(SKILLS_SUBDIR)),
    ]
    .into_iter()
    .flatten()
    .collect();

    for dir in &dirs {
        if !dir.is_dir() {
            tracing::debug!(path = %dir.display(), "skills directory does not exist, skipping");
            continue;
        }
        tracing::debug!(path = %dir.display(), "scanning skills directory");
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(err) => {
                tracing::warn!(path = %dir.display(), error = %err, "failed to read skills directory");
                continue;
            }
        };

        for entry in entries.flatten() {
            let path = entry.path();

            // Folder-based skill: subdirectory with SKILL.md inside.
            if path.is_dir() {
                let skill_file = path.join(SKILL_FILE_NAME);
                if skill_file.is_file() {
                    match parse_skill_file(&skill_file) {
                        Ok(skill) => {
                            tracing::debug!(
                                name = %skill.metadata.name,
                                path = %skill_file.display(),
                                "discovered folder-based skill"
                            );
                            skills_by_name.insert(skill.metadata.name.clone(), skill);
                        }
                        Err(err) => {
                            tracing::warn!(
                                path = %skill_file.display(),
                                error = %err,
                                "skipping invalid skill file"
                            );
                        }
                    }
                }
                continue;
            }

            // Bare-file skill: a .md file directly in the skills directory.
            if !is_skill_file(&path) {
                continue;
            }
            match parse_skill_file(&path) {
                Ok(skill) => {
                    tracing::debug!(
                        name = %skill.metadata.name,
                        path = %path.display(),
                        "discovered skill"
                    );
                    // Later directories (higher precedence) overwrite earlier ones.
                    skills_by_name.insert(skill.metadata.name.clone(), skill);
                }
                Err(err) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %err,
                        "skipping invalid skill file"
                    );
                }
            }
        }
    }

    let mut skills: Vec<Skill> = skills_by_name.into_values().collect();
    skills.sort_by(|a, b| a.metadata.priority.cmp(&b.metadata.priority));
    skills
}

/// Filters skills to those whose activation conditions are met.
///
/// - `Always` skills are always included.
/// - `Manual` skills are never auto-included (future: explicit request).
/// - `Auto` skills require all `requires` tools to be **ready** (not just
///   registered) and all `env_vars` to be set in `env`.
pub fn evaluate_activation<'a>(
    skills: &'a [Skill],
    availability: &ToolAvailability,
    env: &HashMap<String, String>,
) -> Vec<&'a Skill> {
    skills
        .iter()
        .filter(|skill| {
            match skill.metadata.activation {
                SkillActivation::Always => true,
                SkillActivation::Manual => false,
                SkillActivation::Auto => {
                    // All required tools must be ready (not just registered).
                    let tools_ready = skill
                        .metadata
                        .requires
                        .iter()
                        .all(|tool_name| is_tool_ready(tool_name, availability));

                    // All env vars must be present.
                    let env_present = skill
                        .metadata
                        .env_vars
                        .iter()
                        .all(|var| env.contains_key(var));

                    if !tools_ready {
                        let missing: Vec<_> = skill
                            .metadata
                            .requires
                            .iter()
                            .filter(|t| !is_tool_ready(t, availability))
                            .collect();
                        tracing::info!(
                            skill = %skill.metadata.name,
                            missing_tools = %missing.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "),
                            "skill not activated: required tool(s) not ready"
                        );
                    }
                    if !env_present {
                        let missing: Vec<_> = skill
                            .metadata
                            .env_vars
                            .iter()
                            .filter(|v| !env.contains_key(*v))
                            .collect();
                        tracing::info!(
                            skill = %skill.metadata.name,
                            missing_env = %missing.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "),
                            "skill not activated: required env var(s) not set"
                        );
                    }

                    tools_ready && env_present
                }
            }
        })
        .collect()
}

/// Renders a skill's content by substituting `{{VAR}}` placeholders with
/// values from `env`. Only non-sensitive values should be in `env`; secrets
/// are referenced as `$VAR` in the skill body for shell-time expansion.
pub fn render_skill(skill: &Skill, env: &HashMap<String, String>) -> RenderedSkill {
    let mut content = skill.content.clone();
    for (key, value) in env {
        let placeholder = format!("{{{{{key}}}}}");
        content = content.replace(&placeholder, value);
    }
    RenderedSkill {
        name: skill.metadata.name.clone(),
        content,
        priority: skill.metadata.priority,
    }
}

/// Formats rendered skills into a single string suitable for appending to
/// the system prompt. Returns an empty string if no skills are active.
pub fn format_skills_prompt(skills: &[RenderedSkill]) -> String {
    if skills.is_empty() {
        return String::new();
    }

    let mut parts = Vec::with_capacity(skills.len() + 1);
    parts.push("\n\n## Active Skills".to_owned());
    for skill in skills {
        parts.push(format!("\n{}", skill.content.trim()));
    }
    parts.join("")
}

/// Convenience: discover → evaluate → render → format in one call.
pub fn load_and_render_skills(
    system_dir: &Path,
    user_dir: Option<&Path>,
    workspace_dir: &Path,
    availability: &ToolAvailability,
    env: &HashMap<String, String>,
) -> String {
    let skills = discover_skills(system_dir, user_dir, workspace_dir);
    tracing::info!(
        discovered = skills.len(),
        names = %skills.iter().map(|s| s.metadata.name.as_str()).collect::<Vec<_>>().join(", "),
        "skill discovery complete"
    );
    let active = evaluate_activation(&skills, availability, env);
    if active.is_empty() {
        tracing::info!("no skills activated");
    } else {
        tracing::info!(
            active_count = active.len(),
            names = %active.iter().map(|s| s.metadata.name.as_str()).collect::<Vec<_>>().join(", "),
            "skills activated"
        );
    }
    let rendered: Vec<RenderedSkill> = active.into_iter().map(|s| render_skill(s, env)).collect();
    format_skills_prompt(&rendered)
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/// Discovers skills from the embedded `BuiltinSkills` directory.
///
/// Iterates over embedded files looking for `<FolderName>/SKILL.md` entries
/// (folder-based skills) and bare `.md` files at the root level.
fn discover_embedded_skills() -> HashMap<String, Skill> {
    let mut skills = HashMap::new();

    for path_str in BuiltinSkills::iter() {
        let path = Path::new(path_str.as_ref());
        let components: Vec<_> = path.components().collect();

        let is_folder_skill =
            components.len() == 2 && path.file_name().is_some_and(|f| f == SKILL_FILE_NAME);
        let is_bare_skill = components.len() == 1
            && path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("md"));

        if !is_folder_skill && !is_bare_skill {
            continue;
        }

        let Some(file) = BuiltinSkills::get(path_str.as_ref()) else {
            continue;
        };
        let content = String::from_utf8_lossy(&file.data);

        match parse_skill_content(&content, path) {
            Ok(skill) => {
                let kind = if is_folder_skill {
                    "folder-based"
                } else {
                    "bare"
                };
                tracing::debug!(
                    name = %skill.metadata.name,
                    kind,
                    "discovered embedded skill"
                );
                skills.insert(skill.metadata.name.clone(), skill);
            }
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "skipping invalid embedded skill file"
                );
            }
        }
    }

    skills
}

/// Extracts embedded reference files to the shared directory so that the LLM
/// can read them via shell commands (e.g. `cat /shared/.oxydra/skills/.../references/...`).
///
/// Only writes files that match `<SkillFolder>/references/*` in the embedded
/// assets. Existing files at the target path are overwritten.
pub fn extract_builtin_references(shared_dir: &Path) {
    let target_base = shared_dir.join(".oxydra/skills");

    for path_str in BuiltinSkills::iter() {
        let path = Path::new(path_str.as_ref());
        let components: Vec<_> = path.components().collect();

        // Match pattern: <SkillFolder>/references/<filename> (3+ components).
        let is_reference = components.len() >= 3
            && components
                .get(1)
                .is_some_and(|c| c.as_os_str() == "references");

        if !is_reference {
            continue;
        }

        let Some(file) = BuiltinSkills::get(path_str.as_ref()) else {
            continue;
        };

        let target_path = target_base.join(path.as_os_str());

        if let Some(parent) = target_path.parent()
            && let Err(err) = std::fs::create_dir_all(parent)
        {
            tracing::warn!(
                path = %parent.display(),
                error = %err,
                "failed to create directory for skill reference"
            );
            continue;
        }

        if let Err(err) = std::fs::write(&target_path, &file.data) {
            tracing::warn!(
                path = %target_path.display(),
                error = %err,
                "failed to write skill reference file"
            );
        } else {
            tracing::debug!(
                path = %target_path.display(),
                "extracted embedded skill reference"
            );
        }
    }
}

/// Returns `true` if the path points to a `.md` file (case-insensitive).
fn is_skill_file(path: &Path) -> bool {
    path.is_file()
        && path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
}

/// Maps a tool name to the corresponding readiness field on
/// [`ToolAvailability`]. Unknown tool names are treated as unavailable.
fn is_tool_ready(tool_name: &str, availability: &ToolAvailability) -> bool {
    match tool_name {
        "shell_exec" => availability.shell.is_ready(),
        "browser" => availability.browser.is_ready(),
        _ => {
            tracing::debug!(
                tool = %tool_name,
                "unknown tool in skill requires list; treating as unavailable"
            );
            false
        }
    }
}

/// Parse a single `.md` file into a [`Skill`], validating frontmatter and
/// enforcing the token cap.
fn parse_skill_file(path: &Path) -> Result<Skill, SkillLoadError> {
    let raw =
        std::fs::read_to_string(path).map_err(|err| SkillLoadError::Io(path.to_path_buf(), err))?;
    parse_skill_content(&raw, path)
}

/// Parse skill content from a string (used for both file-based and embedded skills).
fn parse_skill_content(raw: &str, source_path: &Path) -> Result<Skill, SkillLoadError> {
    let matter = Matter::<YAML>::new();
    let parsed = matter
        .parse::<SkillMetadata>(raw)
        .map_err(|err| SkillLoadError::Parse(source_path.to_path_buf(), err.to_string()))?;

    let metadata: SkillMetadata = parsed.data.ok_or_else(|| {
        SkillLoadError::Parse(
            source_path.to_path_buf(),
            "missing YAML frontmatter".to_owned(),
        )
    })?;

    let content = parsed.content;

    // Token cap enforcement.
    let estimated_tokens = content.len() / CHARS_PER_TOKEN;
    if estimated_tokens > MAX_SKILL_TOKENS {
        return Err(SkillLoadError::TokenCap {
            path: source_path.to_path_buf(),
            estimated: estimated_tokens,
            max: MAX_SKILL_TOKENS,
        });
    }

    Ok(Skill {
        metadata,
        content,
        source_path: source_path.to_path_buf(),
    })
}

/// Errors that can occur during skill loading.
#[derive(Debug)]
enum SkillLoadError {
    Io(PathBuf, std::io::Error),
    Parse(PathBuf, String),
    TokenCap {
        path: PathBuf,
        estimated: usize,
        max: usize,
    },
}

impl std::fmt::Display for SkillLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(path, err) => write!(f, "failed to read {}: {err}", path.display()),
            Self::Parse(path, msg) => {
                write!(f, "failed to parse skill at {}: {msg}", path.display())
            }
            Self::TokenCap {
                path,
                estimated,
                max,
            } => write!(
                f,
                "skill at {} exceeds token cap ({estimated} estimated > {max} max)",
                path.display()
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::fs;

    use tools::sandbox::{
        SessionConnection, SessionStatus, SessionUnavailable, SessionUnavailableReason,
    };

    use super::*;

    /// The subdirectory within a skill folder that holds reference files.
    const REFERENCES_SUBDIR: &str = "references";

    /// Helper: create a temporary directory tree for testing.
    fn temp_dir(label: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("skill-test-{label}-"))
            .tempdir()
            .expect("failed to create temp dir")
    }

    /// Helper: write a skill file into the `skills/` subdirectory of `base`.
    fn write_skill(base: &Path, filename: &str, content: &str) {
        let dir = base.join(SKILLS_SUBDIR);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(filename), content).unwrap();
    }

    /// Helper: write a folder-based skill with `SKILL.md` inside
    /// `skills/<folder_name>/`.
    fn write_folder_skill(base: &Path, folder_name: &str, content: &str) {
        let dir = base.join(SKILLS_SUBDIR).join(folder_name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(SKILL_FILE_NAME), content).unwrap();
    }

    /// Helper: write a reference file into `skills/<folder_name>/references/`.
    fn write_skill_reference(base: &Path, folder_name: &str, filename: &str, content: &str) {
        let dir = base
            .join(SKILLS_SUBDIR)
            .join(folder_name)
            .join(REFERENCES_SUBDIR);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(filename), content).unwrap();
    }

    /// Helper: build a ready `ToolAvailability`.
    fn ready_availability() -> ToolAvailability {
        ToolAvailability {
            shell: SessionStatus::Ready(SessionConnection::LocalProcess),
            browser: SessionStatus::Ready(SessionConnection::LocalProcess),
        }
    }

    /// Helper: build an unavailable `ToolAvailability`.
    fn unavailable_availability() -> ToolAvailability {
        ToolAvailability {
            shell: SessionStatus::Unavailable(SessionUnavailable {
                reason: SessionUnavailableReason::Disabled,
                detail: "disabled for test".to_owned(),
            }),
            browser: SessionStatus::Unavailable(SessionUnavailable {
                reason: SessionUnavailableReason::Disabled,
                detail: "disabled for test".to_owned(),
            }),
        }
    }

    /// Helper: shell ready, browser unavailable.
    fn shell_only_availability() -> ToolAvailability {
        ToolAvailability {
            shell: SessionStatus::Ready(SessionConnection::LocalProcess),
            browser: SessionStatus::Unavailable(SessionUnavailable {
                reason: SessionUnavailableReason::Disabled,
                detail: "disabled for test".to_owned(),
            }),
        }
    }

    const BASIC_SKILL: &str = r#"---
name: test-skill
description: A test skill
activation: auto
requires:
  - shell_exec
env:
  - MY_URL
priority: 50
---

## Test Skill

Use `curl {{MY_URL}}/api` to do things.
"#;

    const ALWAYS_SKILL: &str = r#"---
name: always-skill
description: Always active
activation: always
priority: 10
---

## Always Active

This is always injected.
"#;

    const MANUAL_SKILL: &str = r#"---
name: manual-skill
description: Manually activated
activation: manual
priority: 20
---

## Manual Skill

This is never auto-injected.
"#;

    // -----------------------------------------------------------------------
    // Frontmatter parsing
    // -----------------------------------------------------------------------

    #[test]
    fn parse_skill_file_extracts_metadata_and_content() {
        let tmp = temp_dir("parse");
        write_skill(tmp.path(), "test.md", BASIC_SKILL);

        let path = tmp.path().join(SKILLS_SUBDIR).join("test.md");
        let skill = parse_skill_file(&path).expect("should parse");

        assert_eq!(skill.metadata.name, "test-skill");
        assert_eq!(skill.metadata.description, "A test skill");
        assert_eq!(skill.metadata.activation, SkillActivation::Auto);
        assert_eq!(skill.metadata.requires, vec!["shell_exec"]);
        assert_eq!(skill.metadata.env_vars, vec!["MY_URL"]);
        assert_eq!(skill.metadata.priority, 50);
        assert!(skill.content.contains("curl {{MY_URL}}/api"));
    }

    #[test]
    fn parse_skill_file_uses_defaults_for_optional_fields() {
        let tmp = temp_dir("defaults");
        let content = r#"---
name: minimal
description: Minimal skill
---

Body text.
"#;
        write_skill(tmp.path(), "minimal.md", content);

        let path = tmp.path().join(SKILLS_SUBDIR).join("minimal.md");
        let skill = parse_skill_file(&path).expect("should parse");

        assert_eq!(skill.metadata.activation, SkillActivation::Auto);
        assert!(skill.metadata.requires.is_empty());
        assert!(skill.metadata.env_vars.is_empty());
        assert_eq!(skill.metadata.priority, 100);
    }

    #[test]
    fn parse_skill_file_rejects_missing_frontmatter() {
        let tmp = temp_dir("no-fm");
        write_skill(tmp.path(), "nofm.md", "# Just markdown\n\nNo frontmatter.");

        let path = tmp.path().join(SKILLS_SUBDIR).join("nofm.md");
        let err = parse_skill_file(&path).unwrap_err();
        assert!(
            err.to_string().contains("missing YAML frontmatter"),
            "got: {err}"
        );
    }

    #[test]
    fn parse_skill_file_rejects_missing_required_fields() {
        let tmp = temp_dir("missing-name");
        let content = r#"---
description: No name field
---

Body.
"#;
        write_skill(tmp.path(), "bad.md", content);

        let path = tmp.path().join(SKILLS_SUBDIR).join("bad.md");
        let err = parse_skill_file(&path).unwrap_err();
        assert!(
            err.to_string().contains("name"),
            "expected error about missing 'name', got: {err}"
        );
    }

    #[test]
    fn parse_skill_file_enforces_token_cap() {
        let tmp = temp_dir("token-cap");
        // 3000 tokens * 4 chars/token = 12000 chars. Add 1 more to exceed.
        let body = "x".repeat(MAX_SKILL_TOKENS * CHARS_PER_TOKEN + CHARS_PER_TOKEN);
        let content = format!("---\nname: big\ndescription: Too big\n---\n\n{body}");
        write_skill(tmp.path(), "big.md", &content);

        let path = tmp.path().join(SKILLS_SUBDIR).join("big.md");
        let err = parse_skill_file(&path).unwrap_err();
        assert!(
            err.to_string().contains("token cap"),
            "expected token cap error, got: {err}"
        );
    }

    #[test]
    fn parse_skill_file_allows_body_at_token_cap() {
        let tmp = temp_dir("at-cap");
        // Exactly at the cap should be fine.
        let body = "x".repeat(MAX_SKILL_TOKENS * CHARS_PER_TOKEN);
        let content = format!("---\nname: exact\ndescription: Exactly at cap\n---\n\n{body}");
        write_skill(tmp.path(), "exact.md", &content);

        let path = tmp.path().join(SKILLS_SUBDIR).join("exact.md");
        assert!(parse_skill_file(&path).is_ok());
    }

    // -----------------------------------------------------------------------
    // Discovery
    // -----------------------------------------------------------------------

    #[test]
    fn discover_skills_finds_files_in_system_dir() {
        let sys = temp_dir("sys");
        write_skill(sys.path(), "a.md", BASIC_SKILL);

        let ws = temp_dir("ws-empty");
        let skills = discover_skills(sys.path(), None, ws.path());
        assert!(
            skills.iter().any(|s| s.metadata.name == "test-skill"),
            "should find the filesystem skill alongside embedded builtins"
        );
    }

    #[test]
    fn discover_skills_ignores_non_md_files() {
        let sys = temp_dir("sys-ignore");
        let dir = sys.path().join(SKILLS_SUBDIR);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("readme.txt"), "not a skill").unwrap();
        fs::write(dir.join("notes.json"), "{}").unwrap();

        let ws = temp_dir("ws-ignore");
        let skills = discover_skills(sys.path(), None, ws.path());
        // Only embedded builtins (no filesystem .md skills added).
        assert!(
            !skills
                .iter()
                .any(|s| s.metadata.name == "readme" || s.metadata.name == "notes"),
            "non-.md files should not be parsed as skills"
        );
    }

    // -----------------------------------------------------------------------
    // Deduplication / precedence
    // -----------------------------------------------------------------------

    #[test]
    fn discover_skills_workspace_overrides_system() {
        let sys = temp_dir("sys-dup");
        let ws = temp_dir("ws-dup");

        // Same `name` in both. Workspace version has different description.
        write_skill(sys.path(), "s.md", BASIC_SKILL);

        let ws_version = BASIC_SKILL.replace("A test skill", "Workspace override");
        write_skill(ws.path(), "s.md", &ws_version);

        let skills = discover_skills(sys.path(), None, ws.path());
        let test_skill = skills
            .iter()
            .find(|s| s.metadata.name == "test-skill")
            .expect("test-skill should exist");
        assert_eq!(test_skill.metadata.description, "Workspace override");
    }

    #[test]
    fn discover_skills_workspace_overrides_user_overrides_system() {
        let sys = temp_dir("sys-3");
        let usr = temp_dir("usr-3");
        let ws = temp_dir("ws-3");

        write_skill(sys.path(), "s.md", BASIC_SKILL);
        write_skill(
            usr.path(),
            "s.md",
            &BASIC_SKILL.replace("A test skill", "User version"),
        );
        write_skill(
            ws.path(),
            "s.md",
            &BASIC_SKILL.replace("A test skill", "Workspace version"),
        );

        let skills = discover_skills(sys.path(), Some(usr.path()), ws.path());
        let test_skill = skills
            .iter()
            .find(|s| s.metadata.name == "test-skill")
            .expect("test-skill should exist");
        assert_eq!(test_skill.metadata.description, "Workspace version");
    }

    #[test]
    fn discover_skills_sorts_by_priority() {
        let sys = temp_dir("sys-sort");
        let ws = temp_dir("ws-sort");

        write_skill(sys.path(), "high.md", BASIC_SKILL); // priority: 50
        write_skill(sys.path(), "always.md", ALWAYS_SKILL); // priority: 10

        let skills = discover_skills(sys.path(), None, ws.path());
        // Verify ordering: always-skill (10) should come before test-skill (50).
        let names: Vec<&str> = skills.iter().map(|s| s.metadata.name.as_str()).collect();
        let always_pos = names.iter().position(|n| *n == "always-skill");
        let test_pos = names.iter().position(|n| *n == "test-skill");
        assert!(
            always_pos.is_some() && test_pos.is_some(),
            "both skills should be present; found: {names:?}"
        );
        assert!(
            always_pos.unwrap() < test_pos.unwrap(),
            "always-skill (priority 10) should come before test-skill (priority 50); order: {names:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Activation evaluation
    // -----------------------------------------------------------------------

    #[test]
    fn evaluate_activation_always_skill_is_active_regardless() {
        let tmp = temp_dir("act-always");
        write_skill(tmp.path(), "a.md", ALWAYS_SKILL);
        let ws = temp_dir("ws-act");

        let skills = discover_skills(tmp.path(), None, ws.path());
        let active = evaluate_activation(&skills, &unavailable_availability(), &HashMap::new());
        assert!(
            active.iter().any(|s| s.metadata.name == "always-skill"),
            "always-skill should be active regardless of tool availability"
        );
    }

    #[test]
    fn evaluate_activation_manual_skill_is_never_auto_active() {
        let tmp = temp_dir("act-manual");
        write_skill(tmp.path(), "m.md", MANUAL_SKILL);
        let ws = temp_dir("ws-manual");

        let skills = discover_skills(tmp.path(), None, ws.path());
        let active = evaluate_activation(&skills, &ready_availability(), &HashMap::new());
        assert!(
            !active.iter().any(|s| s.metadata.name == "manual-skill"),
            "manual-skill should never be auto-activated"
        );
    }

    #[test]
    fn evaluate_activation_auto_skill_activates_when_conditions_met() {
        let tmp = temp_dir("act-auto-ok");
        write_skill(tmp.path(), "s.md", BASIC_SKILL);
        let ws = temp_dir("ws-auto-ok");

        let skills = discover_skills(tmp.path(), None, ws.path());
        let mut env = HashMap::new();
        env.insert("MY_URL".to_owned(), "http://localhost:9867".to_owned());

        let active = evaluate_activation(&skills, &ready_availability(), &env);
        assert!(
            active.iter().any(|s| s.metadata.name == "test-skill"),
            "test-skill should activate when all conditions met"
        );
    }

    #[test]
    fn evaluate_activation_auto_skill_inactive_when_tool_not_ready() {
        let tmp = temp_dir("act-no-tool");
        write_skill(tmp.path(), "s.md", BASIC_SKILL);
        let ws = temp_dir("ws-no-tool");

        let skills = discover_skills(tmp.path(), None, ws.path());
        let mut env = HashMap::new();
        env.insert("MY_URL".to_owned(), "http://localhost:9867".to_owned());

        // shell_exec is required but unavailable.
        let active = evaluate_activation(&skills, &unavailable_availability(), &env);
        assert!(active.is_empty());
    }

    #[test]
    fn evaluate_activation_auto_skill_inactive_when_env_missing() {
        let tmp = temp_dir("act-no-env");
        write_skill(tmp.path(), "s.md", BASIC_SKILL);
        let ws = temp_dir("ws-no-env");

        let skills = discover_skills(tmp.path(), None, ws.path());
        // MY_URL not set.
        let active = evaluate_activation(&skills, &ready_availability(), &HashMap::new());
        assert!(active.is_empty());
    }

    /// Regression test: shell_exec is registered (exists in tool registry)
    /// but the shell sidecar is *unavailable*. The skill must NOT activate.
    #[test]
    fn evaluate_activation_does_not_activate_when_shell_registered_but_unavailable() {
        let tmp = temp_dir("act-shell-unavail");
        write_skill(tmp.path(), "s.md", BASIC_SKILL);
        let ws = temp_dir("ws-shell-unavail");

        let skills = discover_skills(tmp.path(), None, ws.path());
        let mut env = HashMap::new();
        env.insert("MY_URL".to_owned(), "http://localhost:9867".to_owned());

        // Shell unavailable even though it is "registered" in the tool
        // registry (which the skill system does not check — it only checks
        // ToolAvailability readiness).
        let active = evaluate_activation(&skills, &unavailable_availability(), &env);
        assert!(
            active.is_empty(),
            "skill should not activate when shell is unavailable"
        );
    }

    #[test]
    fn evaluate_activation_shell_ready_but_browser_required_and_unavailable() {
        let tmp = temp_dir("act-browser-req");
        let content = r#"---
name: browser-skill
description: Needs browser
activation: auto
requires:
  - browser
env: []
---

Browser content.
"#;
        write_skill(tmp.path(), "b.md", content);
        let ws = temp_dir("ws-browser-req");

        let skills = discover_skills(tmp.path(), None, ws.path());
        let active = evaluate_activation(&skills, &shell_only_availability(), &HashMap::new());
        assert!(active.is_empty());
    }

    // -----------------------------------------------------------------------
    // Template rendering
    // -----------------------------------------------------------------------

    #[test]
    fn render_skill_substitutes_env_vars() {
        let tmp = temp_dir("render");
        write_skill(tmp.path(), "s.md", BASIC_SKILL);
        let ws = temp_dir("ws-render");

        let skills = discover_skills(tmp.path(), None, ws.path());
        let test_skill = skills
            .iter()
            .find(|s| s.metadata.name == "test-skill")
            .expect("test-skill should exist");
        let mut env = HashMap::new();
        env.insert("MY_URL".to_owned(), "http://localhost:9867".to_owned());

        let rendered = render_skill(test_skill, &env);
        assert!(rendered.content.contains("http://localhost:9867/api"));
        assert!(!rendered.content.contains("{{MY_URL}}"));
        assert_eq!(rendered.name, "test-skill");
        assert_eq!(rendered.priority, 50);
    }

    #[test]
    fn render_skill_leaves_unknown_placeholders_intact() {
        let tmp = temp_dir("render-unknown");
        write_skill(tmp.path(), "s.md", BASIC_SKILL);
        let ws = temp_dir("ws-render-unknown");

        let skills = discover_skills(tmp.path(), None, ws.path());
        let test_skill = skills
            .iter()
            .find(|s| s.metadata.name == "test-skill")
            .expect("test-skill should exist");
        // No env vars — {{MY_URL}} stays as-is.
        let rendered = render_skill(test_skill, &HashMap::new());
        assert!(rendered.content.contains("{{MY_URL}}"));
    }

    // -----------------------------------------------------------------------
    // Prompt formatting
    // -----------------------------------------------------------------------

    #[test]
    fn format_skills_prompt_empty_when_no_skills() {
        let result = format_skills_prompt(&[]);
        assert!(result.is_empty());
    }

    #[test]
    fn format_skills_prompt_contains_header_and_content() {
        let rendered = vec![RenderedSkill {
            name: "test".to_owned(),
            content: "## Test\n\nDo something.".to_owned(),
            priority: 50,
        }];
        let result = format_skills_prompt(&rendered);
        assert!(result.contains("## Active Skills"));
        assert!(result.contains("## Test"));
        assert!(result.contains("Do something."));
    }

    #[test]
    fn format_skills_prompt_includes_multiple_skills() {
        let rendered = vec![
            RenderedSkill {
                name: "a".to_owned(),
                content: "Skill A content".to_owned(),
                priority: 10,
            },
            RenderedSkill {
                name: "b".to_owned(),
                content: "Skill B content".to_owned(),
                priority: 50,
            },
        ];
        let result = format_skills_prompt(&rendered);
        assert!(result.contains("Skill A content"));
        assert!(result.contains("Skill B content"));
    }

    // -----------------------------------------------------------------------
    // End-to-end convenience function
    // -----------------------------------------------------------------------

    #[test]
    fn load_and_render_skills_end_to_end() {
        let sys = temp_dir("e2e-sys");
        let ws = temp_dir("e2e-ws");

        write_skill(sys.path(), "basic.md", BASIC_SKILL);
        write_skill(sys.path(), "always.md", ALWAYS_SKILL);
        write_skill(sys.path(), "manual.md", MANUAL_SKILL);

        let mut env = HashMap::new();
        env.insert("MY_URL".to_owned(), "http://localhost:9867".to_owned());

        let result =
            load_and_render_skills(sys.path(), None, ws.path(), &ready_availability(), &env);

        // always-skill and test-skill should be active; manual-skill should not.
        assert!(result.contains("Always Active"));
        assert!(result.contains("http://localhost:9867/api"));
        assert!(!result.contains("Manual Skill"));
    }

    #[test]
    fn load_and_render_skills_returns_empty_when_no_skills_found() {
        let sys = temp_dir("e2e-empty-sys");
        let ws = temp_dir("e2e-empty-ws");

        let result = load_and_render_skills(
            sys.path(),
            None,
            ws.path(),
            &ready_availability(),
            &HashMap::new(),
        );
        assert!(result.is_empty());
    }

    /// The `env` field alias allows using `env` instead of `env_vars`.
    #[test]
    fn parse_skill_file_supports_env_alias() {
        let tmp = temp_dir("env-alias");
        let content = r#"---
name: env-alias
description: Uses env alias
env:
  - FOO
---

Body.
"#;
        write_skill(tmp.path(), "alias.md", content);

        let path = tmp.path().join(SKILLS_SUBDIR).join("alias.md");
        let skill = parse_skill_file(&path).expect("should parse");
        assert_eq!(skill.metadata.env_vars, vec!["FOO"]);
    }

    // -----------------------------------------------------------------------
    // Browser skill activation scenarios
    // -----------------------------------------------------------------------

    /// A browser skill requiring `shell_exec` + `PINCHTAB_URL` activates when
    /// shell is ready and the env var is set.
    #[test]
    fn browser_skill_activates_when_shell_ready_and_pinchtab_url_set() {
        let tmp = temp_dir("browser-activate");
        let content = r#"---
name: browser-automation
description: Browser skill
activation: auto
requires:
  - shell_exec
env:
  - PINCHTAB_URL
priority: 50
---

Use curl {{PINCHTAB_URL}}/api.
"#;
        write_skill(tmp.path(), "browser.md", content);
        let ws = temp_dir("ws-browser-activate");

        let skills = discover_skills(tmp.path(), None, ws.path());
        let mut env = HashMap::new();
        env.insert(
            "PINCHTAB_URL".to_owned(),
            "http://127.0.0.1:9867".to_owned(),
        );

        let active = evaluate_activation(&skills, &ready_availability(), &env);
        assert!(
            active
                .iter()
                .any(|s| s.metadata.name == "browser-automation"),
            "browser-automation should activate with shell ready + PINCHTAB_URL set"
        );
    }

    /// A browser skill does NOT activate when `PINCHTAB_URL` is missing
    /// (browser health check failed → env var not set).
    #[test]
    fn browser_skill_inactive_when_pinchtab_url_missing() {
        let tmp = temp_dir("browser-no-url");
        let content = r#"---
name: browser-automation
description: Browser skill
activation: auto
requires:
  - shell_exec
env:
  - PINCHTAB_URL
---

Browser content.
"#;
        write_skill(tmp.path(), "browser.md", content);
        let ws = temp_dir("ws-browser-no-url");

        let skills = discover_skills(tmp.path(), None, ws.path());
        // Shell is ready but PINCHTAB_URL not set.
        let active = evaluate_activation(&skills, &ready_availability(), &HashMap::new());
        assert!(
            active.is_empty(),
            "browser skill should not activate without PINCHTAB_URL"
        );
    }

    /// A browser skill does NOT activate when shell is unavailable, even if
    /// PINCHTAB_URL is set. This simulates a Pinchtab health failure scenario
    /// where the shell sidecar itself is down.
    #[test]
    fn browser_skill_inactive_when_shell_unavailable() {
        let tmp = temp_dir("browser-no-shell");
        let content = r#"---
name: browser-automation
description: Browser skill
activation: auto
requires:
  - shell_exec
env:
  - PINCHTAB_URL
---

Browser content.
"#;
        write_skill(tmp.path(), "browser.md", content);
        let ws = temp_dir("ws-browser-no-shell");

        let skills = discover_skills(tmp.path(), None, ws.path());
        let mut env = HashMap::new();
        env.insert(
            "PINCHTAB_URL".to_owned(),
            "http://127.0.0.1:9867".to_owned(),
        );

        let active = evaluate_activation(&skills, &unavailable_availability(), &env);
        assert!(
            active.is_empty(),
            "browser skill should not activate when shell is unavailable"
        );
    }

    /// Verifies that rendered browser skill content has PINCHTAB_URL
    /// substituted correctly.
    #[test]
    fn browser_skill_renders_pinchtab_url() {
        let tmp = temp_dir("browser-render");
        let content = r#"---
name: browser-automation
description: Browser skill
activation: auto
requires:
  - shell_exec
env:
  - PINCHTAB_URL
---

Navigate: `curl {{PINCHTAB_URL}}/navigate`
"#;
        write_skill(tmp.path(), "browser.md", content);
        let ws = temp_dir("ws-browser-render");

        let skills = discover_skills(tmp.path(), None, ws.path());
        let browser_skill = skills
            .iter()
            .find(|s| s.metadata.name == "browser-automation")
            .expect("browser-automation should exist");
        let mut env = HashMap::new();
        env.insert(
            "PINCHTAB_URL".to_owned(),
            "http://127.0.0.1:9867".to_owned(),
        );

        let rendered = render_skill(browser_skill, &env);
        assert!(
            rendered.content.contains("http://127.0.0.1:9867/navigate"),
            "PINCHTAB_URL should be substituted"
        );
        assert!(
            !rendered.content.contains("{{PINCHTAB_URL}}"),
            "placeholder should be replaced"
        );
    }

    // -----------------------------------------------------------------------
    // Folder-based skill discovery
    // -----------------------------------------------------------------------

    #[test]
    fn discover_skills_finds_folder_based_skill() {
        let sys = temp_dir("folder-sys");
        write_folder_skill(sys.path(), "MySkill", BASIC_SKILL);

        let ws = temp_dir("folder-ws-empty");
        let skills = discover_skills(sys.path(), None, ws.path());
        let test_skill = skills
            .iter()
            .find(|s| s.metadata.name == "test-skill")
            .expect("test-skill should be discovered from folder");
        assert!(
            test_skill
                .source_path
                .to_string_lossy()
                .contains("MySkill/SKILL.md"),
            "source_path should reference the SKILL.md inside the folder"
        );
    }

    #[test]
    fn discover_skills_ignores_folder_without_skill_md() {
        let sys = temp_dir("folder-no-skill");
        // Create a directory but don't put SKILL.md in it.
        let dir = sys.path().join(SKILLS_SUBDIR).join("EmptyFolder");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("README.md"), "not a skill").unwrap();

        let ws = temp_dir("folder-no-skill-ws");
        let skills = discover_skills(sys.path(), None, ws.path());
        // Only embedded builtins should be present — not the empty folder.
        assert!(
            !skills
                .iter()
                .any(|s| s.source_path.to_string_lossy().contains("EmptyFolder")),
            "empty folder should not produce a skill"
        );
    }

    #[test]
    fn discover_skills_mixes_folder_and_bare_file_skills() {
        let sys = temp_dir("folder-mixed");
        // Folder-based skill.
        write_folder_skill(sys.path(), "FolderSkill", ALWAYS_SKILL);
        // Bare-file skill.
        write_skill(sys.path(), "bare.md", BASIC_SKILL);

        let ws = temp_dir("folder-mixed-ws");
        let skills = discover_skills(sys.path(), None, ws.path());
        let names: Vec<&str> = skills.iter().map(|s| s.metadata.name.as_str()).collect();
        assert!(
            names.contains(&"always-skill"),
            "should find folder-based skill"
        );
        assert!(names.contains(&"test-skill"), "should find bare-file skill");
    }

    #[test]
    fn discover_skills_folder_in_workspace_overrides_folder_in_system() {
        let sys = temp_dir("folder-override-sys");
        let ws = temp_dir("folder-override-ws");

        // Same skill name in both tiers but different descriptions.
        write_folder_skill(sys.path(), "BrowserAutomation", BASIC_SKILL);
        let ws_version = BASIC_SKILL.replace("A test skill", "Workspace folder override");
        write_folder_skill(ws.path(), "BrowserAutomation", &ws_version);

        let skills = discover_skills(sys.path(), None, ws.path());
        let test_skill = skills
            .iter()
            .find(|s| s.metadata.name == "test-skill")
            .expect("test-skill should exist from folder override");
        assert_eq!(test_skill.metadata.description, "Workspace folder override");
    }

    #[test]
    fn discover_skills_folder_skill_overrides_bare_file_with_same_name() {
        let sys = temp_dir("folder-vs-bare-sys");
        let ws = temp_dir("folder-vs-bare-ws");

        // System has bare file.
        write_skill(sys.path(), "test.md", BASIC_SKILL);
        // Workspace has folder-based with same `name`.
        let ws_version = BASIC_SKILL.replace("A test skill", "Folder wins");
        write_folder_skill(ws.path(), "TestSkill", &ws_version);

        let skills = discover_skills(sys.path(), None, ws.path());
        let test_skill = skills
            .iter()
            .find(|s| s.metadata.name == "test-skill")
            .expect("test-skill should exist");
        assert_eq!(test_skill.metadata.description, "Folder wins");
    }

    #[test]
    fn discover_skills_folder_with_references_is_found() {
        let sys = temp_dir("folder-refs");
        write_folder_skill(sys.path(), "BrowserAutomation", BASIC_SKILL);
        write_skill_reference(
            sys.path(),
            "BrowserAutomation",
            "api.md",
            "# API Reference\n\nSome docs.",
        );

        let ws = temp_dir("folder-refs-ws");
        let skills = discover_skills(sys.path(), None, ws.path());
        assert!(
            skills.iter().any(|s| s.metadata.name == "test-skill"),
            "folder-based skill with references should be discovered"
        );

        // Verify the references directory exists alongside the skill.
        let refs_dir = sys
            .path()
            .join(SKILLS_SUBDIR)
            .join("BrowserAutomation")
            .join(REFERENCES_SUBDIR);
        assert!(refs_dir.join("api.md").is_file());
    }

    // -----------------------------------------------------------------------
    // Actual browser skill (config/skills/BrowserAutomation/SKILL.md)
    // -----------------------------------------------------------------------

    /// Returns the path to the repo-root `config/` directory containing built-in
    /// skills.
    fn repo_config_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("runner crate should have a parent")
            .parent()
            .expect("crates/ should have a parent")
            .join("config")
    }

    /// The actual `config/skills/BrowserAutomation/SKILL.md` parses
    /// successfully and extracts the expected metadata.
    #[test]
    fn actual_browser_skill_parses_with_correct_metadata() {
        let skill_path = repo_config_dir()
            .join(SKILLS_SUBDIR)
            .join("BrowserAutomation")
            .join(SKILL_FILE_NAME);
        assert!(
            skill_path.is_file(),
            "expected browser skill at {}",
            skill_path.display()
        );

        let skill = parse_skill_file(&skill_path).expect("browser SKILL.md should parse");
        assert_eq!(skill.metadata.name, "browser-automation");
        assert_eq!(
            skill.metadata.description,
            "Control a headless Chrome browser via Pinchtab's REST API"
        );
        assert_eq!(skill.metadata.activation, SkillActivation::Auto);
        assert_eq!(skill.metadata.requires, vec!["shell_exec"]);
        assert_eq!(skill.metadata.env_vars, vec!["PINCHTAB_URL"]);
        assert_eq!(skill.metadata.priority, 50);
    }

    /// The actual browser skill body is within the 3000-token cap and
    /// contains essential content for the LLM.
    #[test]
    fn actual_browser_skill_content_within_token_cap_and_has_key_sections() {
        let skill_path = repo_config_dir()
            .join(SKILLS_SUBDIR)
            .join("BrowserAutomation")
            .join(SKILL_FILE_NAME);
        let skill = parse_skill_file(&skill_path).expect("should parse");

        let estimated_tokens = skill.content.len() / CHARS_PER_TOKEN;
        assert!(
            estimated_tokens <= MAX_SKILL_TOKENS,
            "browser skill body is {estimated_tokens} estimated tokens, exceeds cap {MAX_SKILL_TOKENS}"
        );

        // Essential sections that the LLM needs to see.
        assert!(
            skill.content.contains("## Browser Automation (Pinchtab)"),
            "should have main heading"
        );
        assert!(
            skill.content.contains("### Core Loop"),
            "should have core loop section"
        );
        assert!(
            skill.content.contains("### Key Endpoints"),
            "should have key endpoints table"
        );
        assert!(
            skill.content.contains("### Best Practices"),
            "should have best practices section"
        );
        assert!(
            skill.content.contains("### File Integration"),
            "should have file integration section"
        );
        assert!(
            skill.content.contains("### If Blocked"),
            "should have if-blocked section"
        );

        // Key template placeholders.
        assert!(
            skill.content.contains("{{PINCHTAB_URL}}"),
            "should contain PINCHTAB_URL placeholder for template substitution"
        );
        // Auth token referenced as shell env var (not template).
        assert!(
            skill.content.contains("$BRIDGE_TOKEN"),
            "should reference BRIDGE_TOKEN as a shell env var"
        );
        // Reference to lazy-loaded API docs.
        assert!(
            skill
                .content
                .contains("/shared/.oxydra/skills/BrowserAutomation/references/pinchtab-api.md"),
            "should point to the lazy-loaded API reference file"
        );
    }

    /// The actual browser skill discovers from the repo config directory and
    /// activates when shell is ready + `PINCHTAB_URL` is set.
    #[test]
    fn actual_browser_skill_discovers_and_activates() {
        let config_dir = repo_config_dir();
        let ws = temp_dir("actual-skill-ws");

        let skills = discover_skills(&config_dir, None, ws.path());
        assert!(
            skills
                .iter()
                .any(|s| s.metadata.name == "browser-automation"),
            "browser-automation skill should be discovered from config/skills/"
        );

        let mut env = HashMap::new();
        env.insert(
            "PINCHTAB_URL".to_owned(),
            "http://127.0.0.1:9867".to_owned(),
        );

        let active = evaluate_activation(&skills, &ready_availability(), &env);
        assert!(
            active
                .iter()
                .any(|s| s.metadata.name == "browser-automation"),
            "browser-automation skill should activate with shell ready + PINCHTAB_URL set"
        );
    }

    /// The actual browser skill does NOT activate when shell is unavailable,
    /// even though `PINCHTAB_URL` is set.
    #[test]
    fn actual_browser_skill_does_not_activate_without_shell() {
        let config_dir = repo_config_dir();
        let ws = temp_dir("actual-skill-no-shell");

        let skills = discover_skills(&config_dir, None, ws.path());
        let mut env = HashMap::new();
        env.insert(
            "PINCHTAB_URL".to_owned(),
            "http://127.0.0.1:9867".to_owned(),
        );

        let active = evaluate_activation(&skills, &unavailable_availability(), &env);
        assert!(
            !active
                .iter()
                .any(|s| s.metadata.name == "browser-automation"),
            "browser-automation skill should NOT activate when shell is unavailable"
        );
    }

    /// The actual browser skill renders with `PINCHTAB_URL` substituted
    /// and the rendered content contains all essential curl examples.
    #[test]
    fn actual_browser_skill_renders_with_pinchtab_url_substituted() {
        let config_dir = repo_config_dir();
        let ws = temp_dir("actual-skill-render");

        let skills = discover_skills(&config_dir, None, ws.path());
        let browser_skill = skills
            .iter()
            .find(|s| s.metadata.name == "browser-automation")
            .expect("browser-automation skill should be discovered");

        let mut env = HashMap::new();
        env.insert(
            "PINCHTAB_URL".to_owned(),
            "http://127.0.0.1:9867".to_owned(),
        );

        let rendered = render_skill(browser_skill, &env);

        // All {{PINCHTAB_URL}} placeholders should be replaced.
        assert!(
            !rendered.content.contains("{{PINCHTAB_URL}}"),
            "all PINCHTAB_URL placeholders should be substituted"
        );
        // The rendered content should contain the actual URL.
        assert!(
            rendered.content.contains("http://127.0.0.1:9867"),
            "rendered content should contain the substituted URL"
        );
        // Key curl examples should be present with the substituted URL.
        assert!(
            rendered.content.contains("http://127.0.0.1:9867/navigate"),
            "should contain navigate endpoint with substituted URL"
        );
        assert!(
            rendered
                .content
                .contains("http://127.0.0.1:9867/tabs/$TAB/snapshot"),
            "should contain snapshot endpoint with substituted URL"
        );
        assert!(
            rendered
                .content
                .contains("http://127.0.0.1:9867/tabs/$TAB/action"),
            "should contain action endpoint with substituted URL"
        );
        // $BRIDGE_TOKEN should remain as a shell env var (NOT substituted).
        assert!(
            rendered.content.contains("$BRIDGE_TOKEN"),
            "BRIDGE_TOKEN should remain as a shell env var reference"
        );
    }

    /// Full pipeline: discover → evaluate → render → format produces a
    /// prompt section containing the browser skill content.
    #[test]
    fn actual_browser_skill_appears_in_formatted_prompt() {
        let config_dir = repo_config_dir();
        let ws = temp_dir("actual-skill-prompt");

        let mut env = HashMap::new();
        env.insert(
            "PINCHTAB_URL".to_owned(),
            "http://127.0.0.1:9867".to_owned(),
        );

        let prompt =
            load_and_render_skills(&config_dir, None, ws.path(), &ready_availability(), &env);

        assert!(
            prompt.contains("## Active Skills"),
            "prompt should contain the active skills header"
        );
        assert!(
            prompt.contains("## Browser Automation (Pinchtab)"),
            "prompt should contain the browser skill heading"
        );
        assert!(
            prompt.contains("http://127.0.0.1:9867/navigate"),
            "prompt should contain rendered navigate URL"
        );
        assert!(
            prompt.contains("### Core Loop"),
            "prompt should contain the core loop section"
        );
        assert!(
            prompt.contains("### Key Endpoints"),
            "prompt should contain the key endpoints table"
        );
    }

    /// When `PINCHTAB_URL` is not set, the formatted prompt should NOT
    /// contain the browser skill.
    #[test]
    fn actual_browser_skill_absent_from_prompt_without_pinchtab_url() {
        let config_dir = repo_config_dir();
        let ws = temp_dir("actual-skill-no-url");

        let prompt = load_and_render_skills(
            &config_dir,
            None,
            ws.path(),
            &ready_availability(),
            &HashMap::new(),
        );

        assert!(
            !prompt.contains("Browser Automation"),
            "prompt should NOT contain browser skill when PINCHTAB_URL is missing"
        );
    }

    // -----------------------------------------------------------------------
    // Embedded skill discovery + reference extraction
    // -----------------------------------------------------------------------

    #[test]
    fn embedded_skills_include_browser_automation() {
        let skills = discover_embedded_skills();
        assert!(
            skills.contains_key("browser-automation"),
            "embedded skills should include browser-automation; found: {:?}",
            skills.keys().collect::<Vec<_>>()
        );

        let skill = &skills["browser-automation"];
        assert_eq!(skill.metadata.activation, SkillActivation::Auto);
        assert_eq!(skill.metadata.requires, vec!["shell_exec"]);
        assert_eq!(skill.metadata.env_vars, vec!["PINCHTAB_URL"]);
    }

    #[test]
    fn embedded_skills_found_with_empty_filesystem_dirs() {
        let sys = temp_dir("embed-empty-sys");
        let ws = temp_dir("embed-empty-ws");

        let skills = discover_skills(sys.path(), None, ws.path());
        assert!(
            skills
                .iter()
                .any(|s| s.metadata.name == "browser-automation"),
            "embedded browser-automation should appear even with empty filesystem dirs"
        );
    }

    #[test]
    fn filesystem_skill_overrides_embedded_builtin() {
        let sys = temp_dir("embed-override-sys");
        let ws = temp_dir("embed-override-ws");

        // Create a workspace-level skill with the same name as the embedded one.
        let override_content = r#"---
name: browser-automation
description: Custom browser override
activation: auto
requires:
  - shell_exec
env:
  - PINCHTAB_URL
---

Custom browser content for override test.
"#;
        write_folder_skill(ws.path(), "BrowserOverride", override_content);

        let skills = discover_skills(sys.path(), None, ws.path());
        let browser = skills
            .iter()
            .find(|s| s.metadata.name == "browser-automation")
            .expect("browser-automation should exist");
        assert!(
            browser
                .content
                .contains("Custom browser content for override test"),
            "workspace skill should override embedded; got: {}",
            browser.content
        );
    }

    #[test]
    fn extract_builtin_references_writes_to_shared_dir() {
        let shared = temp_dir("extract-shared");
        extract_builtin_references(shared.path());

        let ref_path = shared
            .path()
            .join(".oxydra/skills/BrowserAutomation/references/pinchtab-api.md");
        assert!(
            ref_path.is_file(),
            "reference file should be extracted to {}",
            ref_path.display()
        );

        let content = fs::read_to_string(&ref_path).unwrap();
        assert!(
            content.contains("Pinchtab"),
            "extracted reference should contain Pinchtab API docs"
        );
    }

    #[test]
    fn extract_builtin_references_is_idempotent() {
        let shared = temp_dir("extract-idempotent");

        extract_builtin_references(shared.path());
        extract_builtin_references(shared.path()); // second call

        let ref_path = shared
            .path()
            .join(".oxydra/skills/BrowserAutomation/references/pinchtab-api.md");
        assert!(
            ref_path.is_file(),
            "reference file should still exist after re-extraction"
        );
    }
}
