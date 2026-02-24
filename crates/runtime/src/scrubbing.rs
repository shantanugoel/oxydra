use std::{
    collections::{BTreeMap, BTreeSet},
    sync::OnceLock,
};

use regex::{Captures, Regex, RegexSet};

const REDACTION_MARKER: &str = "[REDACTED]";
const MIN_ENTROPY_TOKEN_LENGTH: usize = 24;
const MAX_ENTROPY_TOKEN_LENGTH: usize = 512;

/// A mapping from a host path prefix to a virtual path visible to the LLM.
///
/// For example, mapping `/Users/alice/.oxydra/workspaces/bob/shared` → `/shared`
/// ensures the LLM never sees host-specific filesystem details.
#[derive(Debug, Clone)]
pub struct PathScrubMapping {
    /// Host path prefix to find (must be a canonical absolute path).
    pub host_prefix: String,
    /// Virtual path to replace it with.
    pub virtual_path: String,
}

/// Replaces host path prefixes in `output` with their virtual path equivalents.
///
/// Mappings are applied longest-first to avoid partial replacements (e.g.,
/// `/a/b/shared` is replaced before `/a/b`).
pub(crate) fn scrub_host_paths(output: &str, mappings: &[PathScrubMapping]) -> String {
    if mappings.is_empty() || output.is_empty() {
        return output.to_owned();
    }

    // Sort by descending host prefix length so more-specific paths match first.
    let mut sorted: Vec<&PathScrubMapping> = mappings.iter().collect();
    sorted.sort_by(|a, b| b.host_prefix.len().cmp(&a.host_prefix.len()));

    let mut result = output.to_owned();
    for mapping in sorted {
        result = result.replace(&mapping.host_prefix, &mapping.virtual_path);
    }
    result
}

/// Translates virtual path prefixes in tool arguments to host paths.
///
/// This is the reverse of [`scrub_host_paths`]: it converts LLM-facing virtual
/// paths (e.g., `/shared/notes.txt`) into host filesystem paths (e.g.,
/// `/Users/alice/.oxydra/workspaces/bob/shared/notes.txt`) so that the
/// security policy and file operations resolve correctly.
///
/// Only known path-bearing argument fields are translated to avoid corrupting
/// non-path values (e.g., file content that happens to contain `/shared`).
pub(crate) fn translate_tool_arg_paths(
    tool_name: &str,
    args: &serde_json::Value,
    mappings: &[PathScrubMapping],
) -> serde_json::Value {
    if mappings.is_empty() {
        return args.clone();
    }

    let mut translated = args.clone();

    match tool_name {
        "file_read" | "file_search" | "file_list" | "file_write" | "file_edit" | "file_delete" => {
            translate_field(&mut translated, "path", mappings);
        }
        "vault_copyto" => {
            translate_field(&mut translated, "source_path", mappings);
            translate_field(&mut translated, "destination_path", mappings);
        }
        "shell_exec" => {
            // For shell commands, translate path prefixes embedded in the
            // command string.  This is best-effort since shell quoting makes
            // exact parsing impossible.
            if let Some(cmd) = translated.get("command").and_then(|v| v.as_str()) {
                let resolved = resolve_virtual_prefixes_in_text(cmd, mappings);
                translated["command"] = serde_json::Value::String(resolved);
            }
        }
        _ => {}
    }

    translated
}

/// Replaces the value of a single JSON object field if it starts with a
/// virtual path prefix.
fn translate_field(
    args: &mut serde_json::Value,
    field: &str,
    mappings: &[PathScrubMapping],
) {
    if let Some(value) = args.get(field).and_then(|v| v.as_str()) {
        let resolved = resolve_virtual_prefix(value, mappings);
        args[field] = serde_json::Value::String(resolved);
    }
}

/// Replaces the leading virtual-path prefix in a single path value with its
/// host equivalent.  Only matches at the very start of the string and at a
/// path boundary.  Longer virtual prefixes are matched first.
fn resolve_virtual_prefix(input: &str, mappings: &[PathScrubMapping]) -> String {
    let mut sorted: Vec<&PathScrubMapping> = mappings
        .iter()
        .filter(|m| !m.virtual_path.is_empty())
        .collect();
    sorted.sort_by(|a, b| b.virtual_path.len().cmp(&a.virtual_path.len()));

    for mapping in &sorted {
        if input.starts_with(&mapping.virtual_path) {
            let remainder = &input[mapping.virtual_path.len()..];
            if remainder.is_empty() || remainder.starts_with('/') {
                return format!("{}{remainder}", mapping.host_prefix);
            }
        }
    }

    input.to_owned()
}

/// Replaces all occurrences of virtual-path prefixes within free-form text
/// (such as a shell command) with their host equivalents.  Matches at path
/// boundaries to minimise false positives.
fn resolve_virtual_prefixes_in_text(input: &str, mappings: &[PathScrubMapping]) -> String {
    let mut sorted: Vec<&PathScrubMapping> = mappings
        .iter()
        .filter(|m| !m.virtual_path.is_empty())
        .collect();
    sorted.sort_by(|a, b| b.virtual_path.len().cmp(&a.virtual_path.len()));

    let mut result = input.to_owned();
    for mapping in &sorted {
        // Replace occurrences like "/shared/" with "/host/.../shared/".
        let virtual_with_slash = format!("{}/", mapping.virtual_path);
        let host_with_slash = format!("{}/", mapping.host_prefix);
        result = result.replace(&virtual_with_slash, &host_with_slash);

        // Also handle " /shared" at end-of-string (without trailing slash).
        let virtual_at_end = format!(" {}", mapping.virtual_path);
        let host_at_end = format!(" {}", mapping.host_prefix);
        if result.ends_with(&virtual_at_end) {
            let prefix_len = result.len() - virtual_at_end.len();
            result = format!("{}{host_at_end}", &result[..prefix_len]);
        }
    }
    result
}
const HIGH_ENTROPY_THRESHOLD: f64 = 3.8;

pub(crate) fn scrub_tool_output(output: &str) -> String {
    let mut scrubbed = output.to_owned();
    if keyword_set().is_match(output) {
        scrubbed = redact_keyword_values(&scrubbed);
    }
    redact_high_entropy_tokens(&scrubbed)
}

fn keyword_set() -> &'static RegexSet {
    static KEYWORDS: OnceLock<RegexSet> = OnceLock::new();
    KEYWORDS.get_or_init(|| {
        RegexSet::new([
            r"(?i)\bapi[_-]?key\b",
            r"(?i)\baccess[_-]?token\b",
            r"(?i)\brefresh[_-]?token\b",
            r"(?i)\btoken\b",
            r"(?i)\bpassword\b",
            r"(?i)\bbearer\b",
            r"(?i)\bsecret\b",
        ])
        .expect("credential keyword patterns should compile")
    })
}

fn keyword_value_patterns() -> &'static Vec<Regex> {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            r#"(?i)(?P<prefix>"(?:api[_-]?key|access[_-]?token|refresh[_-]?token|token|password|secret)"\s*:\s*")(?P<value>[^"\n]+)(?P<suffix>")"#,
            r"(?i)(?P<prefix>\b(?:api[_-]?key|access[_-]?token|refresh[_-]?token|token|password|secret)\b\s*(?:[:=]\s*|is\s+))(?P<value>[^\s,;]+)",
            r"(?i)(?P<prefix>\b(?:authorization\s*:\s*)?bearer\s+)(?P<value>[^\s,;]+)",
            r"(?i)(?P<prefix>[?&](?:api[_-]?key|access_token|token|password)=)(?P<value>[^&\s]+)",
        ]
        .into_iter()
        .map(|pattern| Regex::new(pattern).expect("credential redaction pattern should compile"))
        .collect()
    })
}

fn high_entropy_candidate_regex() -> &'static Regex {
    static TOKENS: OnceLock<Regex> = OnceLock::new();
    TOKENS.get_or_init(|| {
        Regex::new(r"[A-Za-z0-9+/_-]{24,512}={0,2}")
            .expect("high-entropy token candidate pattern should compile")
    })
}

fn redact_keyword_values(input: &str) -> String {
    let mut scrubbed = input.to_owned();
    for pattern in keyword_value_patterns() {
        scrubbed = redact_with_pattern(pattern, &scrubbed);
    }
    scrubbed
}

fn redact_with_pattern(pattern: &Regex, input: &str) -> String {
    pattern
        .replace_all(input, |captures: &Captures<'_>| {
            let prefix = captures
                .name("prefix")
                .map_or_else(String::new, |value| value.as_str().to_owned());
            let suffix = captures
                .name("suffix")
                .map_or_else(String::new, |value| value.as_str().to_owned());
            format!("{prefix}{REDACTION_MARKER}{suffix}")
        })
        .into_owned()
}

fn redact_high_entropy_tokens(input: &str) -> String {
    high_entropy_candidate_regex()
        .replace_all(input, |captures: &Captures<'_>| {
            let candidate = captures.get(0).map_or("", |value| value.as_str());
            if looks_like_secret_token(candidate) {
                REDACTION_MARKER.to_owned()
            } else {
                candidate.to_owned()
            }
        })
        .into_owned()
}

/// Returns `true` for tokens that match known non-secret identifier patterns.
///
/// Some internal identifiers (e.g., memory note IDs like `note-{uuid4}`) are
/// high-entropy by construction but are NOT secrets.  We exempt them from
/// redaction so downstream consumers (including the LLM) can reference them.
fn is_known_internal_identifier(token: &str) -> bool {
    static NOTE_ID_PATTERN: OnceLock<Regex> = OnceLock::new();
    let pattern = NOTE_ID_PATTERN.get_or_init(|| {
        // Matches `note-{uuid4}` where uuid4 is 8-4-4-4-12 hex digits.
        Regex::new(r"^note-[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$")
            .expect("note ID pattern should compile")
    });
    pattern.is_match(token)
}

fn looks_like_secret_token(token: &str) -> bool {
    if token.len() < MIN_ENTROPY_TOKEN_LENGTH || token.len() > MAX_ENTROPY_TOKEN_LENGTH {
        return false;
    }

    // Exempt well-known internal identifier patterns (e.g., memory note IDs)
    // from high-entropy redaction.  These are `note-{uuid4}` strings that look
    // random but are safe to expose.
    if is_known_internal_identifier(token) {
        return false;
    }

    if token.chars().all(|character| character.is_ascii_hexdigit()) {
        return false;
    }

    let has_letter = token
        .chars()
        .any(|character| character.is_ascii_alphabetic());
    let has_digit = token.chars().any(|character| character.is_ascii_digit());
    let has_symbol = token
        .chars()
        .any(|character| matches!(character, '+' | '/' | '=' | '_' | '-'));
    if !(has_letter && (has_digit || has_symbol)) {
        return false;
    }

    let unique_chars = token.chars().collect::<BTreeSet<_>>().len();
    if unique_chars < 8 {
        return false;
    }

    shannon_entropy(token) >= HIGH_ENTROPY_THRESHOLD
}

fn shannon_entropy(token: &str) -> f64 {
    let mut frequencies: BTreeMap<char, usize> = BTreeMap::new();
    for character in token.chars() {
        *frequencies.entry(character).or_default() += 1;
    }

    let token_length = token.chars().count() as f64;
    frequencies
        .values()
        .map(|count| {
            let probability = *count as f64 / token_length;
            -(probability * probability.log2())
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::{scrub_tool_output, PathScrubMapping, scrub_host_paths, translate_tool_arg_paths};

    #[test]
    fn scrubber_redacts_keyword_value_pairs() {
        let output = r#"api_key=sk_live_ABC123DEF456GHI789JKL012MNO345PQR678
Authorization: Bearer very-secret-token
{"password":"CorrectHorseBatteryStaple"}"#;
        let scrubbed = scrub_tool_output(output);
        assert!(scrubbed.contains("api_key=[REDACTED]"));
        assert!(scrubbed.contains("Authorization: Bearer [REDACTED]"));
        assert!(scrubbed.contains(r#""password":"[REDACTED]""#));
        assert!(!scrubbed.contains("very-secret-token"));
    }

    #[test]
    fn scrubber_redacts_high_entropy_candidates_without_keywords() {
        let token = "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8S9t0U1v2";
        let scrubbed = scrub_tool_output(format!("artifact_id={token}").as_str());
        assert_eq!(scrubbed, "artifact_id=[REDACTED]");
    }

    #[test]
    fn scrubber_preserves_non_sensitive_normal_output() {
        let output = "status=ok commit=deadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
        assert_eq!(scrub_tool_output(output), output);
    }

    #[test]
    fn scrubber_preserves_memory_note_ids() {
        // Test a variety of note IDs to ensure none are redacted.
        // UUID v4 values have high entropy by design, and the regex
        // [A-Za-z0-9+/_-]{24,512} matches the full "note-{uuid}" string
        // (42 chars, hyphens included in the character class).
        let note_ids = [
            "note-f67a40ad-16d5-476a-a8a4-eb85e8817705",
            "note-a1b2c3d4-e5f6-4a7b-8c9d-0e1f2a3b4c5d",
            "note-12345678-abcd-4ef0-9876-fedcba987654",
            "note-aaaaaaaa-bbbb-4ccc-dddd-eeeeeeeeeeee",
            "note-0f1e2d3c-4b5a-4697-8899-aabbccddeeff",
            "note-deadbeef-cafe-4bad-babe-f00ddeadbeef",
        ];
        for note_id in note_ids {
            let output = format!(
                r#"[
  {{
    "note_id": "{}",
    "text": "The user's name is Shaan.",
    "score": 0.95,
    "source": "user_memory"
  }}
]"#,
                note_id
            );
            let scrubbed = scrub_tool_output(&output);
            assert!(
                scrubbed.contains(note_id),
                "note_id `{note_id}` should NOT be redacted but got:\n{scrubbed}"
            );
        }
    }

    #[test]
    fn scrubber_still_redacts_actual_secrets_that_look_like_note_ids() {
        // A high-entropy token that does NOT match the note-{uuid} pattern
        // should still be redacted.
        let token = "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8S9t0U1v2";
        let scrubbed = scrub_tool_output(format!("key={token}").as_str());
        assert_eq!(scrubbed, "key=[REDACTED]");
    }

    #[test]
    fn path_scrubbing_replaces_host_paths_with_virtual_paths() {
        let mappings = vec![
            PathScrubMapping {
                host_prefix: "/Users/alice/.oxydra/workspaces/bob/shared".to_owned(),
                virtual_path: "/shared".to_owned(),
            },
            PathScrubMapping {
                host_prefix: "/Users/alice/.oxydra/workspaces/bob/tmp".to_owned(),
                virtual_path: "/tmp".to_owned(),
            },
            PathScrubMapping {
                host_prefix: "/Users/alice/.oxydra/workspaces/bob/vault".to_owned(),
                virtual_path: "/vault".to_owned(),
            },
        ];
        let input = "path `/Users/alice/.oxydra/workspaces/bob/shared/foo.txt` is readable";
        let scrubbed = scrub_host_paths(input, &mappings);
        assert_eq!(scrubbed, "path `/shared/foo.txt` is readable");
    }

    #[test]
    fn path_scrubbing_applies_longest_match_first() {
        let mappings = vec![
            PathScrubMapping {
                host_prefix: "/home/user/workspace".to_owned(),
                virtual_path: "/workspace".to_owned(),
            },
            PathScrubMapping {
                host_prefix: "/home/user/workspace/shared".to_owned(),
                virtual_path: "/shared".to_owned(),
            },
        ];
        // The more-specific path should be replaced first, not the shorter one.
        let input = "allowed roots [/home/user/workspace/shared, /home/user/workspace/tmp]";
        let scrubbed = scrub_host_paths(input, &mappings);
        assert_eq!(scrubbed, "allowed roots [/shared, /workspace/tmp]");
    }

    #[test]
    fn path_scrubbing_is_noop_for_empty_mappings() {
        let input = "path `/some/host/path` is blocked";
        assert_eq!(scrub_host_paths(input, &[]), input);
    }

    #[test]
    fn path_scrubbing_is_noop_for_empty_output() {
        let mappings = vec![PathScrubMapping {
            host_prefix: "/host".to_owned(),
            virtual_path: "/virtual".to_owned(),
        }];
        assert_eq!(scrub_host_paths("", &mappings), "");
    }

    #[test]
    fn translate_file_read_virtual_path_to_host_path() {
        let mappings = vec![
            PathScrubMapping {
                host_prefix: "/Users/alice/.oxydra/workspaces/bob/shared".to_owned(),
                virtual_path: "/shared".to_owned(),
            },
            PathScrubMapping {
                host_prefix: "/Users/alice/.oxydra/workspaces/bob/tmp".to_owned(),
                virtual_path: "/tmp".to_owned(),
            },
            PathScrubMapping {
                host_prefix: "/Users/alice/.oxydra/workspaces/bob/vault".to_owned(),
                virtual_path: "/vault".to_owned(),
            },
        ];

        let args = serde_json::json!({ "path": "/shared/notes.txt" });
        let translated = translate_tool_arg_paths("file_read", &args, &mappings);
        assert_eq!(
            translated["path"],
            "/Users/alice/.oxydra/workspaces/bob/shared/notes.txt"
        );
    }

    #[test]
    fn translate_file_list_virtual_root_directory() {
        let mappings = vec![PathScrubMapping {
            host_prefix: "/host/workspace/shared".to_owned(),
            virtual_path: "/shared".to_owned(),
        }];

        let args = serde_json::json!({ "path": "/shared" });
        let translated = translate_tool_arg_paths("file_list", &args, &mappings);
        assert_eq!(translated["path"], "/host/workspace/shared");
    }

    #[test]
    fn translate_vault_copyto_both_paths() {
        let mappings = vec![
            PathScrubMapping {
                host_prefix: "/host/ws/vault".to_owned(),
                virtual_path: "/vault".to_owned(),
            },
            PathScrubMapping {
                host_prefix: "/host/ws/shared".to_owned(),
                virtual_path: "/shared".to_owned(),
            },
        ];

        let args = serde_json::json!({
            "source_path": "/vault/data.csv",
            "destination_path": "/shared/data.csv"
        });
        let translated = translate_tool_arg_paths("vault_copyto", &args, &mappings);
        assert_eq!(translated["source_path"], "/host/ws/vault/data.csv");
        assert_eq!(translated["destination_path"], "/host/ws/shared/data.csv");
    }

    #[test]
    fn translate_shell_command_virtual_paths() {
        let mappings = vec![
            PathScrubMapping {
                host_prefix: "/host/ws/shared".to_owned(),
                virtual_path: "/shared".to_owned(),
            },
            PathScrubMapping {
                host_prefix: "/host/ws/tmp".to_owned(),
                virtual_path: "/tmp".to_owned(),
            },
        ];

        let args = serde_json::json!({ "command": "ls /shared/subdir" });
        let translated = translate_tool_arg_paths("shell_exec", &args, &mappings);
        assert_eq!(translated["command"], "ls /host/ws/shared/subdir");
    }

    #[test]
    fn translate_skips_empty_virtual_path_mappings() {
        let mappings = vec![
            PathScrubMapping {
                host_prefix: "/host/workspace".to_owned(),
                virtual_path: "".to_owned(),
            },
            PathScrubMapping {
                host_prefix: "/host/workspace/shared".to_owned(),
                virtual_path: "/shared".to_owned(),
            },
        ];

        // The empty virtual_path should not match "/shared/foo.txt".
        let args = serde_json::json!({ "path": "/shared/foo.txt" });
        let translated = translate_tool_arg_paths("file_read", &args, &mappings);
        assert_eq!(translated["path"], "/host/workspace/shared/foo.txt");
    }

    #[test]
    fn translate_preserves_already_host_paths() {
        let mappings = vec![PathScrubMapping {
            host_prefix: "/host/ws/shared".to_owned(),
            virtual_path: "/shared".to_owned(),
        }];

        // If the path is already a host path, don't change it.
        let args = serde_json::json!({ "path": "/host/ws/shared/file.txt" });
        let translated = translate_tool_arg_paths("file_read", &args, &mappings);
        assert_eq!(translated["path"], "/host/ws/shared/file.txt");
    }

    #[test]
    fn translate_ignores_unknown_tool_names() {
        let mappings = vec![PathScrubMapping {
            host_prefix: "/host/ws/shared".to_owned(),
            virtual_path: "/shared".to_owned(),
        }];

        let args = serde_json::json!({ "path": "/shared/file.txt" });
        let translated = translate_tool_arg_paths("web_fetch", &args, &mappings);
        // Unknown tool — args should pass through unchanged.
        assert_eq!(translated["path"], "/shared/file.txt");
    }

    #[test]
    fn translate_respects_path_boundary() {
        let mappings = vec![PathScrubMapping {
            host_prefix: "/host/ws/tmp".to_owned(),
            virtual_path: "/tmp".to_owned(),
        }];

        // "/tmpfile.txt" should NOT match "/tmp" because "file.txt" doesn't
        // start with "/" — it's not a path boundary.
        let args = serde_json::json!({ "path": "/tmpfile.txt" });
        let translated = translate_tool_arg_paths("file_read", &args, &mappings);
        assert_eq!(translated["path"], "/tmpfile.txt");
    }
}
