use std::path::Path;

use types::{MemoryConfig, MemoryError};

use crate::errors::initialization_error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConnectionStrategy {
    Local { db_path: String },
    Remote { url: String, auth_token: String },
}

impl ConnectionStrategy {
    pub(crate) fn from_config(config: &MemoryConfig) -> Result<Option<Self>, MemoryError> {
        if !config.enabled {
            return Ok(None);
        }

        if let Some(url) = config
            .remote_url
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            let auth_token = config
                .auth_token
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    initialization_error(format!(
                        "remote memory mode requires auth_token when remote_url is set ({url})"
                    ))
                })?;
            return Ok(Some(Self::Remote {
                url: url.to_owned(),
                auth_token: auth_token.to_owned(),
            }));
        }

        let db_path = config.db_path.trim();
        if db_path.is_empty() {
            return Err(initialization_error(
                "local memory mode requires a non-empty db_path".to_owned(),
            ));
        }

        Ok(Some(Self::Local {
            db_path: db_path.to_owned(),
        }))
    }
}

pub(crate) fn ensure_local_parent_directory(db_path: &str) -> Result<(), MemoryError> {
    let path = Path::new(db_path);
    let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return Ok(());
    };
    std::fs::create_dir_all(parent).map_err(|error| {
        initialization_error(format!(
            "failed to prepare local memory directory `{}`: {error}",
            parent.display()
        ))
    })?;
    Ok(())
}
