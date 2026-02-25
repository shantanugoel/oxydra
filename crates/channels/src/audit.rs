use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use serde::Serialize;
use tracing::warn;

/// Structured audit entry for a rejected (unauthorized) sender message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(serde::Deserialize))]
pub struct AuditEntry {
    /// ISO 8601 timestamp of the rejection.
    pub timestamp: String,
    /// Channel type (e.g., "telegram", "discord").
    pub channel: String,
    /// Platform-specific sender ID that was rejected.
    pub sender_id: String,
    /// Brief reason for rejection.
    pub reason: String,
    /// Optional additional context (e.g., chat_id, message snippet).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}

/// Append-only audit logger for recording rejected sender events.
///
/// Writes JSON-lines to a file at `<workspace>/.oxydra/sender_audit.log`.
/// Each line is a self-contained `AuditEntry` JSON object.
///
/// Failures to write audit entries are logged via `tracing::warn` but never
/// propagated — audit logging must not break message processing.
#[derive(Debug, Clone)]
pub struct AuditLogger {
    log_path: PathBuf,
}

impl AuditLogger {
    /// Create a new audit logger targeting a specific file path.
    pub fn new(log_path: impl Into<PathBuf>) -> Self {
        Self {
            log_path: log_path.into(),
        }
    }

    /// Create an audit logger using the standard workspace location.
    ///
    /// The log file is placed at `<workspace_root>/.oxydra/sender_audit.log`.
    pub fn for_workspace(workspace_root: impl AsRef<Path>) -> Self {
        let log_path = workspace_root
            .as_ref()
            .join(".oxydra")
            .join("sender_audit.log");
        Self::new(log_path)
    }

    /// Record a rejected sender event.
    ///
    /// Writes a single JSON line to the audit log. Failures are logged
    /// via tracing but never propagated.
    pub fn log_rejected_sender(&self, entry: &AuditEntry) {
        if let Err(error) = self.write_entry(entry) {
            warn!(
                path = %self.log_path.display(),
                channel = %entry.channel,
                sender_id = %entry.sender_id,
                error = %error,
                "failed to write sender audit entry"
            );
        }
    }

    /// Returns the path to the audit log file.
    pub fn log_path(&self) -> &Path {
        &self.log_path
    }

    fn write_entry(&self, entry: &AuditEntry) -> Result<(), AuditWriteError> {
        // Ensure the parent directory exists.
        if let Some(parent) = self.log_path.parent() {
            fs::create_dir_all(parent).map_err(|source| AuditWriteError::CreateDir {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        let mut line =
            serde_json::to_string(entry).map_err(|source| AuditWriteError::Serialize {
                source,
            })?;
        line.push('\n');

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
            .map_err(|source| AuditWriteError::OpenFile {
                path: self.log_path.clone(),
                source,
            })?;

        file.write_all(line.as_bytes())
            .map_err(|source| AuditWriteError::Write {
                path: self.log_path.clone(),
                source,
            })?;

        Ok(())
    }
}

/// Returns the current time as an ISO 8601 string (UTC).
///
/// Uses `SystemTime` to avoid external chrono dependency. Format: `YYYY-MM-DDTHH:MM:SSZ`.
pub fn now_iso8601() -> String {
    // Use humantime for simple ISO 8601 formatting from SystemTime.
    // Fallback to Unix epoch seconds if formatting fails.
    let now = std::time::SystemTime::now();
    match now.duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => {
            let secs = duration.as_secs();
            // Simple UTC formatting without external deps.
            // Seconds since epoch → broken-down date/time.
            let days = secs / 86400;
            let time_of_day = secs % 86400;
            let hours = time_of_day / 3600;
            let minutes = (time_of_day % 3600) / 60;
            let seconds = time_of_day % 60;

            // Convert days since epoch to year/month/day.
            let (year, month, day) = days_to_ymd(days);
            format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}Z")
        }
        Err(_) => "1970-01-01T00:00:00Z".to_owned(),
    }
}

/// Convert days since Unix epoch to (year, month, day).
fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    // Algorithm from Howard Hinnant's date algorithms.
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[derive(Debug)]
enum AuditWriteError {
    CreateDir {
        path: PathBuf,
        source: std::io::Error,
    },
    Serialize {
        source: serde_json::Error,
    },
    OpenFile {
        path: PathBuf,
        source: std::io::Error,
    },
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl std::fmt::Display for AuditWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CreateDir { path, source } => {
                write!(f, "create dir `{}`: {source}", path.display())
            }
            Self::Serialize { source } => write!(f, "serialize entry: {source}"),
            Self::OpenFile { path, source } => {
                write!(f, "open file `{}`: {source}", path.display())
            }
            Self::Write { path, source } => {
                write!(f, "write to `{}`: {source}", path.display())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{env, fs, path::PathBuf, time::SystemTime};

    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should be monotonic")
            .as_nanos();
        let path = env::temp_dir().join(format!(
            "oxydra-audit-test-{label}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("temp dir should be creatable");
        path
    }

    #[test]
    fn audit_logger_creates_log_file_and_writes_entry() {
        let root = temp_dir("write-entry");
        let logger = AuditLogger::for_workspace(&root);

        let entry = AuditEntry {
            timestamp: "2026-02-25T12:00:00Z".to_owned(),
            channel: "telegram".to_owned(),
            sender_id: "99999999".to_owned(),
            reason: "sender not in authorized list".to_owned(),
            context: Some("chat_id=12345".to_owned()),
        };

        logger.log_rejected_sender(&entry);

        let content = fs::read_to_string(logger.log_path()).expect("audit log should exist");
        let parsed: AuditEntry =
            serde_json::from_str(content.trim()).expect("should be valid JSON");
        assert_eq!(parsed.channel, "telegram");
        assert_eq!(parsed.sender_id, "99999999");
        assert_eq!(parsed.context, Some("chat_id=12345".to_owned()));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn audit_logger_appends_multiple_entries() {
        let root = temp_dir("append");
        let logger = AuditLogger::for_workspace(&root);

        for i in 0..3 {
            let entry = AuditEntry {
                timestamp: format!("2026-02-25T12:0{i}:00Z"),
                channel: "telegram".to_owned(),
                sender_id: format!("sender-{i}"),
                reason: "unauthorized".to_owned(),
                context: None,
            };
            logger.log_rejected_sender(&entry);
        }

        let content = fs::read_to_string(logger.log_path()).expect("audit log should exist");
        let lines: Vec<&str> = content.trim().lines().collect();
        assert_eq!(lines.len(), 3);

        for (i, line) in lines.iter().enumerate() {
            let parsed: AuditEntry =
                serde_json::from_str(line).expect("each line should be valid JSON");
            assert_eq!(parsed.sender_id, format!("sender-{i}"));
        }

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn audit_logger_creates_parent_directories() {
        let root = temp_dir("mkdirs");
        // Remove the .oxydra dir if it was auto-created by for_workspace
        let oxydra_dir = root.join(".oxydra");
        let _ = fs::remove_dir_all(&oxydra_dir);

        let logger = AuditLogger::for_workspace(&root);
        assert!(!oxydra_dir.exists());

        let entry = AuditEntry {
            timestamp: "2026-02-25T12:00:00Z".to_owned(),
            channel: "discord".to_owned(),
            sender_id: "abc123".to_owned(),
            reason: "not in allowlist".to_owned(),
            context: None,
        };
        logger.log_rejected_sender(&entry);

        assert!(logger.log_path().exists());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn now_iso8601_returns_valid_format() {
        let ts = now_iso8601();
        // Should match YYYY-MM-DDTHH:MM:SSZ
        assert!(ts.ends_with('Z'), "timestamp should end with Z: {ts}");
        assert_eq!(ts.len(), 20, "timestamp should be 20 chars: {ts}");
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[7..8], "-");
        assert_eq!(&ts[10..11], "T");
        assert_eq!(&ts[13..14], ":");
        assert_eq!(&ts[16..17], ":");
    }
}
