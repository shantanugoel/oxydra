use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};

use thiserror::Error;
use types::{CapsOverrideEntry, CapsOverrides, CatalogProvider, ModelCatalog};

const MODELS_DEV_API_URL: &str = "https://models.dev/api.json";

/// Default set of providers to include when filtering the models.dev catalog.
pub const DEFAULT_FILTER_PROVIDERS: &[&str] = &["openai", "anthropic", "google"];

#[derive(Debug, Error)]
pub enum CatalogError {
    #[error("failed to fetch models.dev catalog: {0}")]
    Fetch(String),
    #[error("failed to parse catalog JSON: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("catalog file I/O error: {0}")]
    Io(#[from] io::Error),
}

impl From<reqwest::Error> for CatalogError {
    fn from(err: reqwest::Error) -> Self {
        Self::Fetch(err.to_string())
    }
}

/// Fetch the full models.dev API catalog as raw JSON.
pub fn fetch_models_dev_json() -> Result<String, CatalogError> {
    let body = reqwest::blocking::get(MODELS_DEV_API_URL)?.text()?;
    Ok(body)
}

/// Filter a raw models.dev JSON response to only include the specified providers.
///
/// The raw JSON is expected to be a provider-keyed map matching the
/// `BTreeMap<String, CatalogProvider>` layout.
pub fn filter_catalog(raw_json: &str, providers: &[&str]) -> Result<ModelCatalog, CatalogError> {
    let full: BTreeMap<String, CatalogProvider> = serde_json::from_str(raw_json)?;
    let filtered: BTreeMap<String, CatalogProvider> = full
        .into_iter()
        .filter(|(id, _)| providers.contains(&id.as_str()))
        .collect();
    Ok(ModelCatalog::new(filtered))
}

/// Produce a deterministic, pretty-printed JSON string for a catalog.
///
/// BTreeMap ordering ensures key-sorted output. The trailing newline matches
/// the convention used by the committed snapshot file.
pub fn canonicalize_json(catalog: &ModelCatalog) -> Result<String, CatalogError> {
    let json = serde_json::to_string_pretty(catalog)?;
    Ok(format!("{json}\n"))
}

/// Load capability overrides from a JSON file on disk.
///
/// Returns default overrides if the file does not exist.
pub fn load_overrides(path: &Path) -> Result<CapsOverrides, CatalogError> {
    if path.exists() {
        let content = fs::read_to_string(path)?;
        Ok(serde_json::from_str(&content)?)
    } else {
        Ok(CapsOverrides::default())
    }
}

/// Merge capability overrides, preserving all existing entries and ensuring
/// every configured provider has a `provider_defaults` entry.
pub fn merge_overrides(existing: &CapsOverrides, providers: &[&str]) -> CapsOverrides {
    let mut merged = existing.clone();

    for provider in providers {
        merged
            .provider_defaults
            .entry((*provider).to_string())
            .or_insert_with(|| CapsOverrideEntry {
                supports_streaming: Some(true),
                ..CapsOverrideEntry::default()
            });
    }

    merged
}

/// Write a canonical JSON string to the specified path.
pub fn write_canonical_catalog(canonical_json: &str, path: &Path) -> Result<(), CatalogError> {
    fs::write(path, canonical_json)?;
    Ok(())
}

/// Write capability overrides to a JSON file.
pub fn write_overrides(overrides: &CapsOverrides, path: &Path) -> Result<(), CatalogError> {
    let json = serde_json::to_string_pretty(overrides)?;
    fs::write(path, format!("{json}\n"))?;
    Ok(())
}

/// Run the full catalog fetch pipeline: fetch, filter, merge, canonicalize, write.
pub fn run_fetch(providers: &[&str]) -> Result<(), CatalogError> {
    let catalog_path = PathBuf::from(ModelCatalog::pinned_snapshot_path());
    let overrides_path = PathBuf::from(ModelCatalog::pinned_overrides_path());

    eprintln!("Fetching models.dev catalog...");
    let raw = fetch_models_dev_json()?;

    eprintln!("Filtering to providers: {}", providers.join(", "));
    let catalog = filter_catalog(&raw, providers)?;

    let canonical = canonicalize_json(&catalog)?;
    write_canonical_catalog(&canonical, &catalog_path)?;
    eprintln!("Wrote pinned catalog: {}", catalog_path.display());

    let existing_overrides = load_overrides(&overrides_path)?;
    let merged_overrides = merge_overrides(&existing_overrides, providers);
    write_overrides(&merged_overrides, &overrides_path)?;
    eprintln!("Wrote caps overrides: {}", overrides_path.display());

    show_catalog_summary(&catalog);
    Ok(())
}

/// Verify that a freshly fetched + filtered + canonicalized catalog matches
/// the committed pinned snapshot. Returns `true` if they are identical.
pub fn run_verify(providers: &[&str]) -> Result<bool, CatalogError> {
    eprintln!("Fetching models.dev catalog for verification...");
    let raw = fetch_models_dev_json()?;
    let catalog = filter_catalog(&raw, providers)?;
    let fresh_json = canonicalize_json(&catalog)?;

    let committed_json = ModelCatalog::pinned_snapshot_json();

    if fresh_json == committed_json {
        eprintln!("Pinned catalog is up to date.");
        Ok(true)
    } else {
        eprintln!("Pinned catalog has drifted from models.dev upstream.");

        if let Ok(committed_catalog) = ModelCatalog::from_snapshot_str(committed_json) {
            for provider_id in providers {
                let committed_count = committed_catalog
                    .providers
                    .get(*provider_id)
                    .map(|p| p.models.len())
                    .unwrap_or(0);
                let fresh_count = catalog
                    .providers
                    .get(*provider_id)
                    .map(|p| p.models.len())
                    .unwrap_or(0);
                if committed_count != fresh_count {
                    eprintln!("  {provider_id}: {committed_count} -> {fresh_count} models");
                }
            }
        }

        eprintln!("Run `oxydra catalog fetch` to update the snapshot.");
        Ok(false)
    }
}

/// Pretty-print a summary of the current pinned catalog.
pub fn run_show() -> Result<(), CatalogError> {
    let committed = ModelCatalog::pinned_snapshot_json();
    let catalog = ModelCatalog::from_snapshot_str(committed)
        .map_err(|e| CatalogError::Io(io::Error::new(io::ErrorKind::InvalidData, e.to_string())))?;
    show_catalog_summary(&catalog);
    Ok(())
}

fn show_catalog_summary(catalog: &ModelCatalog) {
    println!("Pinned Model Catalog");
    println!("====================");
    let mut total = 0;
    for (provider_id, provider) in &catalog.providers {
        let count = provider.models.len();
        total += count;
        println!("\n{provider_id} ({count} models):");
        for model_id in provider.models.keys() {
            println!("  - {model_id}");
        }
    }
    println!(
        "\nTotal: {total} models across {} providers",
        catalog.providers.len()
    );
}

/// Compare a freshly canonicalized snapshot against committed content.
///
/// This is the pure (no-network) verification primitive used by tests and
/// by `run_verify` after fetching.
pub fn verify_snapshot_bytes(fresh_canonical: &str, committed: &str) -> bool {
    fresh_canonical == committed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    /// Minimal models.dev-shaped JSON with 4 providers; used by filter and
    /// determinism tests without network access.
    fn mock_models_dev_json() -> String {
        serde_json::to_string_pretty(&serde_json::json!({
            "openai": {
                "id": "openai",
                "name": "OpenAI",
                "env": ["OPENAI_API_KEY"],
                "api": "https://api.openai.com/v1",
                "models": {
                    "gpt-4o": {
                        "id": "gpt-4o",
                        "name": "GPT-4o",
                        "tool_call": true,
                        "limit": { "context": 128000, "output": 16384 }
                    }
                }
            },
            "anthropic": {
                "id": "anthropic",
                "name": "Anthropic",
                "env": ["ANTHROPIC_API_KEY"],
                "models": {
                    "claude-3-5-sonnet-latest": {
                        "id": "claude-3-5-sonnet-latest",
                        "name": "Claude 3.5 Sonnet",
                        "tool_call": true,
                        "limit": { "context": 200000, "output": 8192 }
                    }
                }
            },
            "google": {
                "id": "google",
                "name": "Google",
                "env": ["GEMINI_API_KEY"],
                "models": {
                    "gemini-2.0-flash": {
                        "id": "gemini-2.0-flash",
                        "name": "Gemini 2.0 Flash",
                        "tool_call": true,
                        "limit": { "context": 1048576, "output": 8192 }
                    }
                }
            },
            "mistral": {
                "id": "mistral",
                "name": "Mistral AI",
                "env": ["MISTRAL_API_KEY"],
                "models": {
                    "mistral-large-latest": {
                        "id": "mistral-large-latest",
                        "name": "Mistral Large",
                        "limit": { "context": 128000, "output": 8192 }
                    }
                }
            }
        }))
        .expect("mock JSON should serialize")
    }

    fn temp_dir(label: &str) -> PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be monotonic")
            .as_nanos();
        let mut path = env::temp_dir();
        path.push(format!(
            "oxydra-catalog-{label}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("temp dir should be creatable");
        path
    }

    #[test]
    fn regenerate_snapshot_is_deterministic() {
        let raw = mock_models_dev_json();
        let providers = &["openai", "anthropic", "google"];

        let catalog1 = filter_catalog(&raw, providers).expect("first filter should succeed");
        let json1 = canonicalize_json(&catalog1).expect("first canonicalize should succeed");

        let catalog2 = filter_catalog(&raw, providers).expect("second filter should succeed");
        let json2 = canonicalize_json(&catalog2).expect("second canonicalize should succeed");

        assert_eq!(
            json1, json2,
            "two runs from the same source data must produce byte-identical output"
        );
    }

    #[test]
    fn verify_detects_drift() {
        let raw = mock_models_dev_json();
        let providers = &["openai", "anthropic", "google"];

        let catalog = filter_catalog(&raw, providers).expect("filter should succeed");
        let canonical = canonicalize_json(&catalog).expect("canonicalize should succeed");

        // Identical comparison should pass
        assert!(
            verify_snapshot_bytes(&canonical, &canonical),
            "identical snapshots should verify successfully"
        );

        // Modify one field and re-check
        let mut drifted_catalog = catalog;
        if let Some(openai) = drifted_catalog.providers.get_mut("openai")
            && let Some(model) = openai.models.get_mut("gpt-4o")
        {
            model.name = "GPT-4o-MODIFIED".to_string();
        }
        let drifted_json =
            canonicalize_json(&drifted_catalog).expect("drifted canonicalize should succeed");
        assert!(
            !verify_snapshot_bytes(&drifted_json, &canonical),
            "drifted snapshot should fail verification"
        );
    }

    #[test]
    fn filter_excludes_non_configured_providers() {
        let raw = mock_models_dev_json();

        // Filter to only openai and anthropic (exclude google and mistral)
        let catalog =
            filter_catalog(&raw, &["openai", "anthropic"]).expect("filter should succeed");
        assert_eq!(
            catalog.providers.len(),
            2,
            "should have exactly 2 providers"
        );
        assert!(
            catalog.providers.contains_key("openai"),
            "openai should be included"
        );
        assert!(
            catalog.providers.contains_key("anthropic"),
            "anthropic should be included"
        );
        assert!(
            !catalog.providers.contains_key("google"),
            "google should be excluded"
        );
        assert!(
            !catalog.providers.contains_key("mistral"),
            "mistral should be excluded"
        );
    }

    #[test]
    fn filter_to_default_providers_excludes_mistral() {
        let raw = mock_models_dev_json();
        let catalog =
            filter_catalog(&raw, DEFAULT_FILTER_PROVIDERS).expect("filter should succeed");

        assert_eq!(
            catalog.providers.len(),
            3,
            "default filter should yield openai + anthropic + google"
        );
        assert!(
            !catalog.providers.contains_key("mistral"),
            "mistral should not be in default filter output"
        );
    }

    #[test]
    fn overlay_preserves_existing_overrides() {
        let root = temp_dir("overlay-preserve");
        let overrides_path = root.join("oxydra_caps_overrides.json");

        // Write an overrides file with a custom model-specific entry
        let existing = CapsOverrides {
            overrides: {
                let mut map = BTreeMap::new();
                map.insert(
                    "anthropic/claude-3-5-sonnet-latest".to_string(),
                    CapsOverrideEntry {
                        supports_streaming: Some(true),
                        supports_tools: Some(false),
                        ..CapsOverrideEntry::default()
                    },
                );
                map
            },
            provider_defaults: {
                let mut map = BTreeMap::new();
                map.insert(
                    "openai".to_string(),
                    CapsOverrideEntry {
                        supports_streaming: Some(true),
                        ..CapsOverrideEntry::default()
                    },
                );
                map
            },
        };
        write_overrides(&existing, &overrides_path)
            .expect("writing initial overrides should succeed");

        // Load and merge with a provider set that includes a new provider
        let loaded = load_overrides(&overrides_path).expect("loading overrides should succeed");
        let merged = merge_overrides(&loaded, &["openai", "anthropic", "google"]);

        // Existing model-specific override is preserved
        assert_eq!(
            merged
                .overrides
                .get("anthropic/claude-3-5-sonnet-latest")
                .and_then(|e| e.supports_tools),
            Some(false),
            "existing model-specific override should be preserved"
        );

        // Existing provider default is preserved
        assert_eq!(
            merged
                .provider_defaults
                .get("openai")
                .and_then(|e| e.supports_streaming),
            Some(true),
            "existing openai provider default should be preserved"
        );

        // New providers get default entries
        assert!(
            merged.provider_defaults.contains_key("anthropic"),
            "anthropic should have a provider_defaults entry after merge"
        );
        assert!(
            merged.provider_defaults.contains_key("google"),
            "google should have a provider_defaults entry after merge"
        );
        assert_eq!(
            merged
                .provider_defaults
                .get("google")
                .and_then(|e| e.supports_streaming),
            Some(true),
            "new google provider default should have supports_streaming=true"
        );

        // Write merged and re-read to verify round-trip
        write_overrides(&merged, &overrides_path).expect("writing merged overrides should succeed");
        let reloaded = load_overrides(&overrides_path).expect("reloading overrides should succeed");
        assert_eq!(
            reloaded, merged,
            "overrides should round-trip through write and load"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_overrides_returns_default_when_file_missing() {
        let path = PathBuf::from("/tmp/nonexistent-oxydra-overrides-12345.json");
        let overrides = load_overrides(&path).expect("missing file should return defaults");
        assert!(
            overrides.overrides.is_empty(),
            "default overrides should have no model entries"
        );
        assert!(
            overrides.provider_defaults.is_empty(),
            "default overrides should have no provider defaults"
        );
    }

    #[test]
    fn canonicalize_json_ends_with_newline() {
        let raw = mock_models_dev_json();
        let catalog = filter_catalog(&raw, &["openai"]).expect("filter should succeed");
        let canonical = canonicalize_json(&catalog).expect("canonicalize should succeed");
        assert!(
            canonical.ends_with('\n'),
            "canonical JSON should end with a newline"
        );
    }

    #[test]
    fn filter_catalog_with_empty_provider_list_yields_empty_catalog() {
        let raw = mock_models_dev_json();
        let catalog = filter_catalog(&raw, &[]).expect("filter should succeed");
        assert!(
            catalog.providers.is_empty(),
            "filtering with empty provider list should yield empty catalog"
        );
    }

    #[test]
    fn show_does_not_panic_on_committed_snapshot() {
        // Verify the compiled-in snapshot is parseable and show logic works
        run_show().expect("run_show on committed snapshot should succeed");
    }
}
