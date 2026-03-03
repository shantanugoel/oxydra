use std::path::PathBuf;

use serde::Deserialize;

/// Activation mode for a skill.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillActivation {
    /// Inject when all conditions (required tools ready, env vars set) are met.
    #[default]
    Auto,
    /// Only inject on explicit request (future use).
    Manual,
    /// Always inject regardless of conditions.
    Always,
}

/// YAML frontmatter metadata for a skill file.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SkillMetadata {
    /// Unique identifier (kebab-case). Used for deduplication across directories.
    pub name: String,
    /// One-line summary for diagnostics and future UI.
    pub description: String,
    /// Activation mode. Default: `auto`.
    #[serde(default)]
    pub activation: SkillActivation,
    /// Tool names that must be registered **and available** for this skill to activate.
    #[serde(default)]
    pub requires: Vec<String>,
    /// Environment variables that must be set. Values available for `{{VAR}}`
    /// template substitution in the skill body.
    #[serde(default, alias = "env")]
    pub env_vars: Vec<String>,
    /// Ordering when multiple skills are active (lower = earlier in prompt).
    #[serde(default = "default_priority")]
    pub priority: i32,
}

fn default_priority() -> i32 {
    100
}

/// A discovered skill: metadata + markdown body + source location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub metadata: SkillMetadata,
    /// Markdown body (everything after the YAML frontmatter).
    pub content: String,
    /// Filesystem path where this skill was loaded from.
    pub source_path: PathBuf,
}

/// A skill whose `{{VAR}}` placeholders have been replaced with env values,
/// ready for injection into the system prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedSkill {
    pub name: String,
    pub content: String,
    pub priority: i32,
}
