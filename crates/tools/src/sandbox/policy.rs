use std::{
    collections::BTreeSet,
    env, fs,
    path::{Path, PathBuf},
};

use serde_json::Value;
use types::{SafetyTier, ShellConfig};

const SHARED_DIR_NAME: &str = "shared";
const TMP_DIR_NAME: &str = "tmp";
const VAULT_DIR_NAME: &str = "vault";
const INTERNAL_DIR_NAME: &str = ".oxydra";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecurityPolicyViolationReason {
    InvalidArguments,
    PathResolutionFailed,
    PathOutsideAllowedRoots,
    CommandNotAllowed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecurityPolicyViolation {
    pub reason: SecurityPolicyViolationReason,
    pub detail: String,
}

impl SecurityPolicyViolation {
    fn new(reason: SecurityPolicyViolationReason, detail: impl Into<String>) -> Self {
        Self {
            reason,
            detail: detail.into(),
        }
    }
}

pub trait SecurityPolicy: Send + Sync {
    fn enforce(
        &self,
        tool_name: &str,
        safety_tier: SafetyTier,
        arguments: &Value,
    ) -> Result<(), SecurityPolicyViolation>;
}

#[derive(Debug, Clone)]
pub struct WorkspaceSecurityPolicy {
    read_only_roots: Vec<PathBuf>,
    read_write_roots: Vec<PathBuf>,
    denied_roots: Vec<PathBuf>,
    shell_command_allowlist: BTreeSet<String>,
    /// Glob patterns for the allowlist (e.g., `npm*`, `cargo-*`).
    shell_command_allow_globs: Vec<String>,
    allow_shell_operators: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileAccessMode {
    ReadOnly,
    ReadWrite,
}

impl WorkspaceSecurityPolicy {
    pub fn for_bootstrap_workspace(workspace_root: impl AsRef<Path>) -> Self {
        let workspace_root = workspace_root.as_ref();
        Self::for_mount_roots(
            workspace_root.join(SHARED_DIR_NAME),
            workspace_root.join(TMP_DIR_NAME),
            workspace_root.join(VAULT_DIR_NAME),
            vec![workspace_root.join(INTERNAL_DIR_NAME)],
        )
    }

    pub fn for_mount_roots(
        shared: impl AsRef<Path>,
        tmp: impl AsRef<Path>,
        vault: impl AsRef<Path>,
        denied: Vec<PathBuf>,
    ) -> Self {
        let shared = shared.as_ref().to_path_buf();
        let tmp = tmp.as_ref().to_path_buf();
        let vault = vault.as_ref().to_path_buf();
        Self::new(
            vec![shared.clone(), tmp.clone(), vault],
            vec![shared, tmp],
            denied,
        )
    }

    fn new(
        read_only_roots: Vec<PathBuf>,
        read_write_roots: Vec<PathBuf>,
        denied_roots: Vec<PathBuf>,
    ) -> Self {
        Self {
            read_only_roots: canonicalize_roots(read_only_roots),
            read_write_roots: canonicalize_roots(read_write_roots),
            denied_roots: canonicalize_roots(denied_roots),
            shell_command_allowlist: default_shell_command_allowlist(),
            shell_command_allow_globs: Vec::new(),
            allow_shell_operators: false,
        }
    }

    /// Applies shell configuration overrides (allow/deny lists, glob
    /// patterns, operator policy).
    pub fn with_shell_config(mut self, config: Option<&ShellConfig>) -> Self {
        let Some(config) = config else {
            return self;
        };

        let replace = config.replace_defaults.unwrap_or(false);
        if replace {
            self.shell_command_allowlist.clear();
            self.shell_command_allow_globs.clear();
        }

        if let Some(ref allow) = config.allow {
            for entry in allow {
                if entry.contains('*') {
                    self.shell_command_allow_globs.push(entry.to_ascii_lowercase());
                } else {
                    self.shell_command_allowlist.insert(entry.to_ascii_lowercase());
                }
            }
        }

        if let Some(ref deny) = config.deny {
            for entry in deny {
                if entry.contains('*') {
                    // Remove any exact matches that the glob would cover.
                    let pattern = entry.to_ascii_lowercase();
                    self.shell_command_allowlist
                        .retain(|cmd| !glob_matches(&pattern, cmd));
                    // Also store as a deny-glob so future glob-allow matches
                    // can be rejected at enforcement time.
                    // We prepend "!" to distinguish deny-globs from allow-globs.
                    self.shell_command_allow_globs
                        .retain(|g| !glob_matches(&pattern, g.trim_start_matches('!')));
                } else {
                    self.shell_command_allowlist.remove(&entry.to_ascii_lowercase());
                }
            }
        }

        self.allow_shell_operators = config.allow_operators.unwrap_or(false);
        self
    }

    fn enforce_file_path(
        &self,
        path: &str,
        access_mode: FileAccessMode,
    ) -> Result<(), SecurityPolicyViolation> {
        let canonical_target =
            canonicalize_target_path(path, access_mode == FileAccessMode::ReadWrite)?;

        // Deny-list takes precedence: reject paths inside denied roots
        // (e.g., .oxydra/ internal directory) before checking allow-lists.
        if self
            .denied_roots
            .iter()
            .any(|root| canonical_target == *root || canonical_target.starts_with(root))
        {
            return Err(SecurityPolicyViolation::new(
                SecurityPolicyViolationReason::PathOutsideAllowedRoots,
                format!(
                    "path `{}` resolved to `{}` which is inside a restricted internal directory",
                    path,
                    canonical_target.display()
                ),
            ));
        }

        let allowed_roots = match access_mode {
            FileAccessMode::ReadOnly => &self.read_only_roots,
            FileAccessMode::ReadWrite => &self.read_write_roots,
        };

        if allowed_roots
            .iter()
            .any(|root| canonical_target == *root || canonical_target.starts_with(root))
        {
            Ok(())
        } else {
            let roots = allowed_roots
                .iter()
                .map(|root| root.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            Err(SecurityPolicyViolation::new(
                SecurityPolicyViolationReason::PathOutsideAllowedRoots,
                format!(
                    "path `{}` resolved to `{}` outside allowed roots [{roots}]",
                    path,
                    canonical_target.display()
                ),
            ))
        }
    }

    fn enforce_shell_command(&self, command: &str) -> Result<(), SecurityPolicyViolation> {
        let command_name = parse_command_name(command, self.allow_shell_operators)?;
        if self.shell_command_allowlist.contains(&command_name) {
            return Ok(());
        }
        if self
            .shell_command_allow_globs
            .iter()
            .any(|pattern| glob_matches(pattern, &command_name))
        {
            return Ok(());
        }
        let mut allowed: Vec<_> = self.shell_command_allowlist.iter().cloned().collect();
        allowed.extend(self.shell_command_allow_globs.iter().cloned());
        Err(SecurityPolicyViolation::new(
            SecurityPolicyViolationReason::CommandNotAllowed,
            format!(
                "command `{command_name}` is not in shell allowlist [{}]",
                allowed.join(", ")
            ),
        ))
    }
}

impl SecurityPolicy for WorkspaceSecurityPolicy {
    fn enforce(
        &self,
        tool_name: &str,
        _safety_tier: SafetyTier,
        arguments: &Value,
    ) -> Result<(), SecurityPolicyViolation> {
        if let Some(access_mode) = file_access_mode(tool_name) {
            let path = arguments
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    SecurityPolicyViolation::new(
                        SecurityPolicyViolationReason::InvalidArguments,
                        format!("tool `{tool_name}` requires string argument `path`"),
                    )
                })?;
            self.enforce_file_path(path, access_mode)?;
        }

        if is_shell_tool(tool_name) {
            let command = arguments
                .get("command")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    SecurityPolicyViolation::new(
                        SecurityPolicyViolationReason::InvalidArguments,
                        format!("tool `{tool_name}` requires string argument `command`"),
                    )
                })?;
            self.enforce_shell_command(command)?;
        }

        Ok(())
    }
}

fn file_access_mode(tool_name: &str) -> Option<FileAccessMode> {
    match tool_name {
        "file_read" | "file_search" | "file_list" => Some(FileAccessMode::ReadOnly),
        "file_write" | "file_edit" | "file_delete" => Some(FileAccessMode::ReadWrite),
        _ => None,
    }
}

fn is_shell_tool(tool_name: &str) -> bool {
    matches!(tool_name, "shell_exec")
}

fn canonicalize_roots(roots: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut canonical_roots = Vec::new();
    for root in roots {
        let normalized = normalize_absolute(root);
        let canonical = fs::canonicalize(&normalized).unwrap_or(normalized);
        if !canonical_roots
            .iter()
            .any(|existing| existing == &canonical)
        {
            canonical_roots.push(canonical);
        }
    }
    canonical_roots
}

fn canonicalize_target_path(
    raw_path: &str,
    allow_missing_leaf: bool,
) -> Result<PathBuf, SecurityPolicyViolation> {
    let path = raw_path.trim();
    if path.is_empty() {
        return Err(SecurityPolicyViolation::new(
            SecurityPolicyViolationReason::InvalidArguments,
            "path argument must not be empty",
        ));
    }

    let target_path = normalize_absolute(PathBuf::from(path));
    if !allow_missing_leaf || target_path.exists() {
        return fs::canonicalize(&target_path).map_err(|error| {
            SecurityPolicyViolation::new(
                SecurityPolicyViolationReason::PathResolutionFailed,
                format!(
                    "failed to canonicalize `{}`: {error}",
                    target_path.display()
                ),
            )
        });
    }

    let parent = target_path.parent().ok_or_else(|| {
        SecurityPolicyViolation::new(
            SecurityPolicyViolationReason::PathResolutionFailed,
            format!(
                "path `{}` does not include a canonicalizable parent directory",
                target_path.display()
            ),
        )
    })?;
    let canonical_parent = fs::canonicalize(parent).map_err(|error| {
        SecurityPolicyViolation::new(
            SecurityPolicyViolationReason::PathResolutionFailed,
            format!(
                "failed to canonicalize parent `{}` for `{}`: {error}",
                parent.display(),
                target_path.display()
            ),
        )
    })?;
    let leaf = target_path.file_name().ok_or_else(|| {
        SecurityPolicyViolation::new(
            SecurityPolicyViolationReason::PathResolutionFailed,
            format!(
                "path `{}` must include a terminal file name for write/edit operations",
                target_path.display()
            ),
        )
    })?;
    Ok(canonical_parent.join(leaf))
}

fn normalize_absolute(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        env::current_dir()
            .map(|cwd| cwd.join(&path))
            .unwrap_or(path)
    }
}

fn parse_command_name(
    command: &str,
    allow_operators: bool,
) -> Result<String, SecurityPolicyViolation> {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return Err(SecurityPolicyViolation::new(
            SecurityPolicyViolationReason::InvalidArguments,
            "command argument must not be empty",
        ));
    }
    if !allow_operators && contains_disallowed_shell_syntax(trimmed) {
        return Err(SecurityPolicyViolation::new(
            SecurityPolicyViolationReason::CommandNotAllowed,
            "command contains shell control operators; only a single allowlisted command is permitted",
        ));
    }

    let first_token = trimmed.split_whitespace().next().ok_or_else(|| {
        SecurityPolicyViolation::new(
            SecurityPolicyViolationReason::InvalidArguments,
            "command argument must include an executable name",
        )
    })?;
    let command_name = Path::new(first_token)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(first_token)
        .to_ascii_lowercase();
    if command_name.is_empty() {
        return Err(SecurityPolicyViolation::new(
            SecurityPolicyViolationReason::InvalidArguments,
            "command executable name must not be empty",
        ));
    }
    Ok(command_name)
}

/// Simple glob matching: supports `*` as prefix, suffix, or both.
///
/// Examples: `npm*` matches `npm`, `npx`; `cargo-*` matches `cargo-fmt`;
/// `*test*` matches `pytest`.
fn glob_matches(pattern: &str, candidate: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let starts = pattern.starts_with('*');
    let ends = pattern.ends_with('*');
    match (starts, ends) {
        (true, true) => {
            let inner = &pattern[1..pattern.len() - 1];
            candidate.contains(inner)
        }
        (true, false) => {
            let suffix = &pattern[1..];
            candidate.ends_with(suffix)
        }
        (false, true) => {
            let prefix = &pattern[..pattern.len() - 1];
            candidate.starts_with(prefix)
        }
        (false, false) => candidate == pattern,
    }
}

fn contains_disallowed_shell_syntax(command: &str) -> bool {
    command.contains("&&")
        || command.contains("||")
        || command.contains(';')
        || command.contains('|')
        || command.contains('>')
        || command.contains('<')
        || command.contains('`')
        || command.contains("$(")
        || command.contains('\n')
        || command.contains('\r')
}

fn default_shell_command_allowlist() -> BTreeSet<String> {
    [
        "awk", "cat", "cargo", "cp", "cut", "echo", "env", "find", "git", "go", "grep", "head",
        "ls", "mkdir", "mv", "node", "printf", "pwd", "python", "python3", "rustc", "sed", "sort",
        "stat", "tail", "touch", "tr", "uniq", "wc", "which",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    use serde_json::json;
    use types::SafetyTier;

    use super::*;

    #[test]
    fn bootstrap_policy_allows_reads_within_workspace_mounts() {
        let workspace = unique_temp_workspace("policy-allow-read");
        let shared_file = workspace.join(SHARED_DIR_NAME).join("allowed.txt");
        fs::write(&shared_file, "ok").expect("shared file should be writable");

        let policy = WorkspaceSecurityPolicy::for_bootstrap_workspace(&workspace);
        let result = policy.enforce(
            "file_read",
            SafetyTier::ReadOnly,
            &json!({ "path": shared_file.to_string_lossy() }),
        );
        assert!(result.is_ok());

        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn bootstrap_policy_denies_paths_outside_workspace_mounts() {
        let workspace = unique_temp_workspace("policy-deny-outside");
        let outside_file = unique_temp_workspace("policy-outside-file").join("outside.txt");
        fs::write(&outside_file, "deny").expect("outside file should be writable");

        let policy = WorkspaceSecurityPolicy::for_bootstrap_workspace(&workspace);
        let result = policy.enforce(
            "file_read",
            SafetyTier::ReadOnly,
            &json!({ "path": outside_file.to_string_lossy() }),
        );
        assert!(matches!(
            result,
            Err(SecurityPolicyViolation {
                reason: SecurityPolicyViolationReason::PathOutsideAllowedRoots,
                ..
            })
        ));

        let _ = fs::remove_dir_all(workspace);
        if let Some(parent) = outside_file.parent() {
            let _ = fs::remove_dir_all(parent);
        }
    }

    #[test]
    fn bootstrap_policy_allows_write_path_with_missing_leaf_inside_tmp() {
        let workspace = unique_temp_workspace("policy-write-missing");
        let target = workspace.join(TMP_DIR_NAME).join("new-file.txt");

        let policy = WorkspaceSecurityPolicy::for_bootstrap_workspace(&workspace);
        let result = policy.enforce(
            "file_write",
            SafetyTier::SideEffecting,
            &json!({ "path": target.to_string_lossy(), "content": "x" }),
        );
        assert!(result.is_ok());

        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn shell_policy_allows_printf_and_denies_disallowed_commands() {
        let workspace = unique_temp_workspace("policy-shell");
        let policy = WorkspaceSecurityPolicy::for_bootstrap_workspace(&workspace);

        let allow = policy.enforce(
            "shell_exec",
            SafetyTier::Privileged,
            &json!({ "command": "printf hello" }),
        );
        assert!(allow.is_ok());

        let deny = policy.enforce(
            "shell_exec",
            SafetyTier::Privileged,
            &json!({ "command": "curl https://example.com" }),
        );
        assert!(matches!(
            deny,
            Err(SecurityPolicyViolation {
                reason: SecurityPolicyViolationReason::CommandNotAllowed,
                ..
            })
        ));

        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn shell_policy_rejects_control_operators() {
        let workspace = unique_temp_workspace("policy-shell-ops");
        let policy = WorkspaceSecurityPolicy::for_bootstrap_workspace(&workspace);

        let deny = policy.enforce(
            "shell_exec",
            SafetyTier::Privileged,
            &json!({ "command": "printf ok && ls" }),
        );
        assert!(matches!(
            deny,
            Err(SecurityPolicyViolation {
                reason: SecurityPolicyViolationReason::CommandNotAllowed,
                ..
            })
        ));

        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn policy_denies_read_and_write_to_internal_directory() {
        let workspace = unique_temp_workspace("policy-deny-internal-direct");
        let internal_file = workspace.join(INTERNAL_DIR_NAME).join("memory.db");
        fs::write(&internal_file, "secret data").expect("internal file should be writable");

        let policy = WorkspaceSecurityPolicy::for_bootstrap_workspace(&workspace);

        let read_result = policy.enforce(
            "file_read",
            SafetyTier::ReadOnly,
            &json!({ "path": internal_file.to_string_lossy() }),
        );
        assert!(
            matches!(
                read_result,
                Err(SecurityPolicyViolation {
                    reason: SecurityPolicyViolationReason::PathOutsideAllowedRoots,
                    ..
                })
            ),
            "direct workspace policy should deny reads inside .oxydra/"
        );

        let write_result = policy.enforce(
            "file_write",
            SafetyTier::SideEffecting,
            &json!({ "path": internal_file.to_string_lossy(), "content": "x" }),
        );
        assert!(
            matches!(
                write_result,
                Err(SecurityPolicyViolation {
                    reason: SecurityPolicyViolationReason::PathOutsideAllowedRoots,
                    ..
                })
            ),
            "direct workspace policy should deny writes inside .oxydra/"
        );

        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn bootstrap_policy_denies_access_to_internal_directory() {
        let workspace = unique_temp_workspace("policy-deny-internal-bootstrap");
        let internal_file = workspace.join(INTERNAL_DIR_NAME).join("memory.db");
        fs::write(&internal_file, "secret data").expect("internal file should be writable");

        let policy = WorkspaceSecurityPolicy::for_bootstrap_workspace(&workspace);

        let result = policy.enforce(
            "file_read",
            SafetyTier::ReadOnly,
            &json!({ "path": internal_file.to_string_lossy() }),
        );
        // Bootstrap policy already doesn't include .oxydra/ in allowed roots,
        // but the deny-list provides defense in depth.
        assert!(
            result.is_err(),
            "bootstrap workspace policy should deny reads inside .oxydra/"
        );

        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn policy_allows_normal_files_in_shared_directory() {
        let workspace = unique_temp_workspace("policy-allow-non-internal");
        let normal_file = workspace.join(SHARED_DIR_NAME).join("readme.txt");
        fs::write(&normal_file, "ok").expect("normal file should be writable");

        let policy = WorkspaceSecurityPolicy::for_bootstrap_workspace(&workspace);

        let result = policy.enforce(
            "file_read",
            SafetyTier::ReadOnly,
            &json!({ "path": normal_file.to_string_lossy() }),
        );
        assert!(
            result.is_ok(),
            "workspace policy should allow reads of normal files inside shared/"
        );

        let _ = fs::remove_dir_all(workspace);
    }

    // ─── Shell config tests ───

    #[test]
    fn shell_config_allow_extends_defaults() {
        let workspace = unique_temp_workspace("shell-cfg-allow");
        let config = ShellConfig {
            allow: Some(vec!["npm".to_owned(), "curl".to_owned()]),
            deny: None,
            replace_defaults: None,
            allow_operators: None,
        };
        let policy =
            WorkspaceSecurityPolicy::for_bootstrap_workspace(&workspace).with_shell_config(Some(&config));

        // Default command still allowed
        let result = policy.enforce("shell_exec", SafetyTier::SideEffecting, &json!({"command": "ls /shared"}));
        assert!(result.is_ok(), "default command 'ls' should still be allowed");

        // Newly added command allowed
        let result = policy.enforce("shell_exec", SafetyTier::SideEffecting, &json!({"command": "npm install"}));
        assert!(result.is_ok(), "added command 'npm' should be allowed");

        let result = policy.enforce("shell_exec", SafetyTier::SideEffecting, &json!({"command": "curl https://example.com"}));
        assert!(result.is_ok(), "added command 'curl' should be allowed");

        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn shell_config_deny_removes_from_defaults() {
        let workspace = unique_temp_workspace("shell-cfg-deny");
        let config = ShellConfig {
            allow: None,
            deny: Some(vec!["ls".to_owned()]),
            replace_defaults: None,
            allow_operators: None,
        };
        let policy =
            WorkspaceSecurityPolicy::for_bootstrap_workspace(&workspace).with_shell_config(Some(&config));

        let result = policy.enforce("shell_exec", SafetyTier::SideEffecting, &json!({"command": "ls /shared"}));
        assert!(result.is_err(), "'ls' should be denied after deny config");

        // Other defaults still allowed
        let result = policy.enforce("shell_exec", SafetyTier::SideEffecting, &json!({"command": "pwd"}));
        assert!(result.is_ok(), "'pwd' should still be allowed");

        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn shell_config_replace_defaults_replaces_entirely() {
        let workspace = unique_temp_workspace("shell-cfg-replace");
        let config = ShellConfig {
            allow: Some(vec!["npm".to_owned(), "curl".to_owned()]),
            deny: None,
            replace_defaults: Some(true),
            allow_operators: None,
        };
        let policy =
            WorkspaceSecurityPolicy::for_bootstrap_workspace(&workspace).with_shell_config(Some(&config));

        // Default commands should no longer work
        let result = policy.enforce("shell_exec", SafetyTier::SideEffecting, &json!({"command": "ls /shared"}));
        assert!(result.is_err(), "'ls' should not be allowed when defaults are replaced");

        // Only explicitly allowed commands work
        let result = policy.enforce("shell_exec", SafetyTier::SideEffecting, &json!({"command": "npm install"}));
        assert!(result.is_ok(), "'npm' should be allowed");

        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn shell_config_glob_pattern_matching() {
        let workspace = unique_temp_workspace("shell-cfg-glob");
        let config = ShellConfig {
            allow: Some(vec!["cargo-*".to_owned(), "*test*".to_owned()]),
            deny: None,
            replace_defaults: None,
            allow_operators: None,
        };
        let policy =
            WorkspaceSecurityPolicy::for_bootstrap_workspace(&workspace).with_shell_config(Some(&config));

        let result = policy.enforce("shell_exec", SafetyTier::SideEffecting, &json!({"command": "cargo-fmt"}));
        assert!(result.is_ok(), "'cargo-fmt' should match glob 'cargo-*'");

        let result = policy.enforce("shell_exec", SafetyTier::SideEffecting, &json!({"command": "pytest foo.py"}));
        assert!(result.is_ok(), "'pytest' should match glob '*test*'");

        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn shell_config_allow_operators() {
        let workspace = unique_temp_workspace("shell-cfg-operators");
        let config = ShellConfig {
            allow: None,
            deny: None,
            replace_defaults: None,
            allow_operators: Some(true),
        };
        let policy =
            WorkspaceSecurityPolicy::for_bootstrap_workspace(&workspace).with_shell_config(Some(&config));

        let result = policy.enforce("shell_exec", SafetyTier::SideEffecting, &json!({"command": "ls /shared && pwd"}));
        assert!(result.is_ok(), "'&&' should be allowed when allow_operators=true");

        let result = policy.enforce("shell_exec", SafetyTier::SideEffecting, &json!({"command": "echo foo | grep foo"}));
        assert!(result.is_ok(), "'|' should be allowed when allow_operators=true");

        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn shell_config_none_preserves_defaults() {
        let workspace = unique_temp_workspace("shell-cfg-none");
        let policy =
            WorkspaceSecurityPolicy::for_bootstrap_workspace(&workspace).with_shell_config(None);

        let result = policy.enforce("shell_exec", SafetyTier::SideEffecting, &json!({"command": "ls /shared"}));
        assert!(result.is_ok(), "default 'ls' should be allowed with None config");

        let result = policy.enforce("shell_exec", SafetyTier::SideEffecting, &json!({"command": "ls && pwd"}));
        assert!(result.is_err(), "operators should be denied with None config");

        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn glob_matches_basic_patterns() {
        assert!(glob_matches("npm*", "npm"));
        assert!(glob_matches("npm*", "npmrc"));
        assert!(!glob_matches("npm*", "npx"));
        assert!(glob_matches("cargo-*", "cargo-fmt"));
        assert!(!glob_matches("cargo-*", "cargo"));
        assert!(glob_matches("*test*", "pytest"));
        assert!(glob_matches("*test*", "testing"));
        assert!(glob_matches("*test", "pytest"));
        assert!(!glob_matches("*test", "testing"));
        assert!(glob_matches("*", "anything"));
        assert!(!glob_matches("npm", "npx"));
        assert!(glob_matches("npm", "npm"));
    }

    fn unique_temp_workspace(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after unix epoch")
            .as_nanos();
        let root = env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()));
        let shared = root.join(SHARED_DIR_NAME);
        let tmp = root.join(TMP_DIR_NAME);
        let vault = root.join(VAULT_DIR_NAME);
        let internal = root.join(INTERNAL_DIR_NAME);
        fs::create_dir_all(&shared).expect("shared dir should be created");
        fs::create_dir_all(&tmp).expect("tmp dir should be created");
        fs::create_dir_all(&vault).expect("vault dir should be created");
        fs::create_dir_all(&internal).expect("internal dir should be created");
        root
    }
}
