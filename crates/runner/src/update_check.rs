//! Version-check utilities for the `runner check-update` command.
//!
//! Queries the GitHub Releases API, caches the result locally (24-hour TTL),
//! and compares the current binary version against the latest stable release
//! using semantic versioning.  Network failures are always non-fatal.

use std::{
    path::PathBuf,
    time::{Duration, SystemTime},
};

use semver::Version;
use serde::{Deserialize, Serialize};
use types::RunnerGuestImages;

const GITHUB_RELEASES_URL: &str = "https://api.github.com/repos/shantanugoel/oxydra/releases";

const USER_AGENT: &str = concat!("oxydra-runner/", env!("CARGO_PKG_VERSION"));

const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// TTL for the cached check result when used by startup notifications.
const CACHE_TTL_SECS: u64 = 24 * 3600;

// ---------------------------------------------------------------------------
// Cache
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
struct UpdateCache {
    latest_tag: String,
    /// Unix timestamp (seconds) when the check was last performed.
    checked_at_secs: u64,
}

fn cache_path() -> Option<PathBuf> {
    // Prefer $XDG_CACHE_HOME, fall back to $HOME/.cache on Unix/macOS.
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(base.join("oxydra").join("update-check.json"))
}

fn load_cache() -> Option<UpdateCache> {
    let content = std::fs::read_to_string(cache_path()?).ok()?;
    serde_json::from_str(&content).ok()
}

fn save_cache(latest_tag: &str) {
    let Some(path) = cache_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let now_secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let cache = UpdateCache {
        latest_tag: latest_tag.to_owned(),
        checked_at_secs: now_secs,
    };
    if let Ok(json) = serde_json::to_string(&cache) {
        let _ = std::fs::write(path, json);
    }
}

fn cache_is_fresh(cache: &UpdateCache) -> bool {
    let now_secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    now_secs.saturating_sub(cache.checked_at_secs) < CACHE_TTL_SECS
}

// ---------------------------------------------------------------------------
// GitHub API fetch
// ---------------------------------------------------------------------------

/// Fetch the latest release tag from GitHub.  Returns `None` on any error.
fn fetch_latest_tag(include_prerelease: bool) -> Option<String> {
    let client = reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(FETCH_TIMEOUT)
        .build()
        .ok()?;

    let response = client.get(GITHUB_RELEASES_URL).send().ok()?;

    if !response.status().is_success() {
        tracing::debug!(
            status = %response.status(),
            "GitHub releases API returned non-success status"
        );
        return None;
    }

    let text = response.text().ok()?;
    let releases: Vec<serde_json::Value> = serde_json::from_str(&text).ok()?;

    releases
        .into_iter()
        .find(|r| {
            let draft = r["draft"].as_bool().unwrap_or(true);
            let prerelease = r["prerelease"].as_bool().unwrap_or(true);
            !draft && (include_prerelease || !prerelease)
        })
        .and_then(|r| r["tag_name"].as_str().map(str::to_owned))
}

/// Resolve the latest stable tag, using a cached result when `use_cache` is
/// true and the cache is still within its TTL.
///
/// Set `use_cache = false` for the explicit `check-update` command so that
/// it always queries the API live.  Set `use_cache = true` for lightweight
/// startup notifications so repeated invocations stay within GitHub's rate
/// limit.
pub fn latest_tag(use_cache: bool, include_prerelease: bool) -> Option<String> {
    if use_cache {
        if let Some(cache) = load_cache() {
            if cache_is_fresh(&cache) {
                tracing::debug!("using cached update-check result");
                return Some(cache.latest_tag);
            }
        }
    }

    let tag = fetch_latest_tag(include_prerelease)?;
    save_cache(&tag);
    Some(tag)
}

// ---------------------------------------------------------------------------
// Public check types
// ---------------------------------------------------------------------------

/// Outcome of a version comparison.
#[derive(Debug)]
pub struct UpdateCheckOutcome {
    pub current_version: String,
    pub latest_version: String,
    pub update_available: bool,
}

/// A mismatch between the binary version and a guest image tag.
#[derive(Debug)]
pub struct ImageTagMismatch {
    pub field: &'static str,
    pub image_ref: String,
    pub image_tag: String,
    pub binary_version: String,
}

// ---------------------------------------------------------------------------
// Public check functions
// ---------------------------------------------------------------------------

/// Perform a version check and return the outcome.
///
/// Returns `None` when the check cannot be completed (network error, parse
/// failure, etc.).  Callers must treat `None` as non-fatal.
pub fn run_check(use_cache: bool, include_prerelease: bool) -> Option<UpdateCheckOutcome> {
    let current_str = env!("CARGO_PKG_VERSION");
    let current = Version::parse(current_str).ok()?;

    let tag = latest_tag(use_cache, include_prerelease)?;
    let latest_str = tag.trim_start_matches('v');
    let latest = Version::parse(latest_str).ok()?;

    Some(UpdateCheckOutcome {
        current_version: current_str.to_owned(),
        latest_version: latest_str.to_owned(),
        update_available: latest > current,
    })
}

/// Compare the binary version against guest image tags in `runner.toml`.
///
/// Image refs that use `latest` or a non-semver tag are silently skipped.
/// Returns a (possibly empty) list of mismatches.
pub fn check_image_tag_mismatches(guest_images: &RunnerGuestImages) -> Vec<ImageTagMismatch> {
    let binary_str = env!("CARGO_PKG_VERSION");
    let Ok(binary_ver) = Version::parse(binary_str) else {
        return Vec::new();
    };

    let images: &[(&'static str, &str)] = &[
        ("guest_images.oxydra_vm", &guest_images.oxydra_vm),
        ("guest_images.shell_vm", &guest_images.shell_vm),
    ];

    let mut mismatches = Vec::new();
    for &(field, image_ref) in images {
        let tag = image_ref.rsplit_once(':').map_or("latest", |(_, t)| t);
        let tag_ver_str = tag.trim_start_matches('v');
        let Ok(tag_ver) = Version::parse(tag_ver_str) else {
            continue; // `latest` or non-semver tag → skip
        };
        if tag_ver != binary_ver {
            mismatches.push(ImageTagMismatch {
                field,
                image_ref: image_ref.to_owned(),
                image_tag: tag.to_owned(),
                binary_version: binary_str.to_owned(),
            });
        }
    }
    mismatches
}
