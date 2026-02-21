use std::{
    collections::{BTreeMap, BTreeSet},
    sync::OnceLock,
};

use regex::{Captures, Regex, RegexSet};

const REDACTION_MARKER: &str = "[REDACTED]";
const MIN_ENTROPY_TOKEN_LENGTH: usize = 24;
const MAX_ENTROPY_TOKEN_LENGTH: usize = 512;
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

fn looks_like_secret_token(token: &str) -> bool {
    if token.len() < MIN_ENTROPY_TOKEN_LENGTH || token.len() > MAX_ENTROPY_TOKEN_LENGTH {
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
    use super::scrub_tool_output;

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
}
