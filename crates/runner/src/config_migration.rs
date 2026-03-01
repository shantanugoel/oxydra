use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use toml_edit::{DocumentMut, value};
use tracing::{info, warn};
use types::DEFAULT_RUNNER_CONFIG_VERSION;

use crate::RunnerError;

const LEGACY_RUNNER_CONFIG_VERSION: &str = "1.0.0";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigType {
    Global,
    User,
}

#[derive(Debug)]
struct ConfigMigration {
    from_version: &'static str,
    to_version: &'static str,
    transform: fn(&mut DocumentMut) -> Result<(), RunnerError>,
}

const GLOBAL_MIGRATIONS: &[ConfigMigration] = &[ConfigMigration {
    from_version: LEGACY_RUNNER_CONFIG_VERSION,
    to_version: DEFAULT_RUNNER_CONFIG_VERSION,
    transform: migrate_global_1_0_0_to_1_0_1,
}];

const USER_MIGRATIONS: &[ConfigMigration] = &[ConfigMigration {
    from_version: LEGACY_RUNNER_CONFIG_VERSION,
    to_version: DEFAULT_RUNNER_CONFIG_VERSION,
    transform: migrate_user_1_0_0_to_1_0_1,
}];

pub(crate) fn migrate_config_file_if_needed(
    path: &Path,
    config_type: ConfigType,
) -> Result<(), RunnerError> {
    let mut document = parse_document(path)?;
    let mut current_version = document
        .get("config_version")
        .and_then(|item| item.as_str())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| LEGACY_RUNNER_CONFIG_VERSION.to_owned());

    if current_version == DEFAULT_RUNNER_CONFIG_VERSION {
        return Ok(());
    }

    let migrations = match config_type {
        ConfigType::Global => GLOBAL_MIGRATIONS,
        ConfigType::User => USER_MIGRATIONS,
    };

    let Some(is_older) = is_older_version(&current_version, DEFAULT_RUNNER_CONFIG_VERSION) else {
        warn!(
            config_type = config_type.as_label(),
            path = %path.display(),
            current_version,
            target_version = DEFAULT_RUNNER_CONFIG_VERSION,
            "skipping runner config migration because config_version is malformed"
        );
        return Ok(());
    };

    if !is_older {
        info!(
            config_type = config_type.as_label(),
            path = %path.display(),
            current_version,
            target_version = DEFAULT_RUNNER_CONFIG_VERSION,
            "skipping runner config migration because file version is current or newer"
        );
        return Ok(());
    }

    let from_version = current_version.clone();
    while current_version != DEFAULT_RUNNER_CONFIG_VERSION {
        let migration = migrations
            .iter()
            .find(|migration| migration.from_version == current_version)
            .ok_or_else(|| RunnerError::ConfigMigration {
                path: path.to_path_buf(),
                message: format!(
                    "no migration registered for {} config version `{}`",
                    config_type.as_label(),
                    current_version
                ),
            })?;

        (migration.transform)(&mut document)?;
        document["config_version"] = value(migration.to_version);
        current_version = migration.to_version.to_owned();
    }

    let backup_path = backup_path_for(path);
    fs::copy(path, &backup_path).map_err(|source| RunnerError::ConfigMigrationIo {
        path: backup_path.clone(),
        operation: "backup",
        source,
    })?;

    // Write migrated content to a temporary file in the same directory, then
    // atomically rename it over the original. This avoids leaving a truncated
    // config on disk if the process is interrupted mid-write.
    let tmp_path = backup_path_for(path); // unique sibling path reused as temp
    fs::write(&tmp_path, document.to_string()).map_err(|source| {
        RunnerError::ConfigMigrationIo {
            path: tmp_path.clone(),
            operation: "write temp",
            source,
        }
    })?;
    fs::rename(&tmp_path, path).map_err(|source| RunnerError::ConfigMigrationIo {
        path: path.to_path_buf(),
        operation: "rename",
        source,
    })?;

    info!(
        config_type = config_type.as_label(),
        path = %path.display(),
        from_version,
        to_version = DEFAULT_RUNNER_CONFIG_VERSION,
        backup_path = %backup_path.display(),
        "auto-migrated runner config"
    );

    Ok(())
}

fn parse_document(path: &Path) -> Result<DocumentMut, RunnerError> {
    let raw = fs::read_to_string(path).map_err(|source| RunnerError::ReadConfig {
        path: path.to_path_buf(),
        source,
    })?;
    raw.parse::<DocumentMut>()
        .map_err(|source| RunnerError::ParseConfigDocument {
            path: path.to_path_buf(),
            source,
        })
}

fn backup_path_for(path: &Path) -> PathBuf {
    let timestamp = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_nanos(),
        Err(error) => error.duration().as_nanos(),
    };
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config".to_owned());
    let backup_file_name = format!("{file_name}.bak.{timestamp}.{}", std::process::id());
    path.with_file_name(backup_file_name)
}

fn parse_version_segments(version: &str) -> Option<Vec<u64>> {
    let trimmed = version.trim();
    if trimmed.is_empty() {
        return None;
    }

    trimmed
        .split('.')
        .map(|segment| segment.parse::<u64>().ok())
        .collect()
}

fn is_older_version(version: &str, target: &str) -> Option<bool> {
    let version_segments = parse_version_segments(version)?;
    let target_segments = parse_version_segments(target)?;

    // Compare normalized numeric segments, padding shorter versions with zeros
    // so that "1.0" and "1.0.0" are treated equivalently.
    let max_len = version_segments.len().max(target_segments.len());
    (0..max_len)
        .map(|idx| {
            (
                version_segments.get(idx).copied().unwrap_or(0),
                target_segments.get(idx).copied().unwrap_or(0),
            )
        })
        .find_map(|(version_part, target_part)| {
            if version_part < target_part {
                Some(true)
            } else if version_part > target_part {
                Some(false)
            } else {
                None
            }
        })
        .or(Some(false))
}

fn migrate_global_1_0_0_to_1_0_1(_document: &mut DocumentMut) -> Result<(), RunnerError> {
    // Patch-only bump: no schema transformation is needed.
    Ok(())
}

fn migrate_user_1_0_0_to_1_0_1(_document: &mut DocumentMut) -> Result<(), RunnerError> {
    // Patch-only bump: no schema transformation is needed.
    Ok(())
}

impl ConfigType {
    fn as_label(self) -> &'static str {
        match self {
            ConfigType::Global => "global",
            ConfigType::User => "user",
        }
    }
}
