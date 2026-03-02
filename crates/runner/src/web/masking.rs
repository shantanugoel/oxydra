use serde_json::Value;

const MASKED_VALUE: &str = "********";

/// Secret field paths that should be masked in API responses.
/// Paths use dot-separated segments. A `*` segment matches any key at that level.
const SECRET_PATHS: &[&[&str]] = &[
    &["providers", "registry", "*", "api_key"],
    &["memory", "auth_token"],
    &["web", "auth_token"],
    &["credential_refs", "*"],
];

/// Mask secrets in a JSON value by replacing known secret field paths with asterisks.
pub fn mask_secrets(value: &mut Value) {
    for path in SECRET_PATHS {
        mask_at_path(value, path, 0);
    }
}

fn mask_at_path(value: &mut Value, path: &[&str], depth: usize) {
    if depth >= path.len() {
        return;
    }

    let segment = path[depth];
    let is_last = depth == path.len() - 1;

    let Some(obj) = value.as_object_mut() else {
        return;
    };

    if segment == "*" {
        // Wildcard: apply to all keys at this level.
        let keys: Vec<String> = obj.keys().cloned().collect();
        for key in keys {
            if is_last {
                if let Some(field) = obj.get_mut(&key)
                    && field.is_string()
                    && !field.as_str().unwrap_or_default().is_empty()
                {
                    *field = Value::String(MASKED_VALUE.to_owned());
                }
            } else if let Some(child) = obj.get_mut(&key) {
                mask_at_path(child, path, depth + 1);
            }
        }
    } else if is_last {
        if let Some(field) = obj.get_mut(segment)
            && field.is_string()
            && !field.as_str().unwrap_or_default().is_empty()
        {
            *field = Value::String(MASKED_VALUE.to_owned());
        }
    } else if let Some(child) = obj.get_mut(segment) {
        mask_at_path(child, path, depth + 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn masks_provider_api_keys() {
        let mut val = json!({
            "providers": {
                "registry": {
                    "openai": { "api_key": "sk-secret-123" },
                    "anthropic": { "api_key": "sk-ant-456" }
                }
            }
        });
        mask_secrets(&mut val);
        assert_eq!(
            val["providers"]["registry"]["openai"]["api_key"],
            MASKED_VALUE
        );
        assert_eq!(
            val["providers"]["registry"]["anthropic"]["api_key"],
            MASKED_VALUE
        );
    }

    #[test]
    fn masks_memory_auth_token() {
        let mut val = json!({ "memory": { "auth_token": "secret-token" } });
        mask_secrets(&mut val);
        assert_eq!(val["memory"]["auth_token"], MASKED_VALUE);
    }

    #[test]
    fn masks_web_auth_token() {
        let mut val = json!({ "web": { "auth_token": "my-secret" } });
        mask_secrets(&mut val);
        assert_eq!(val["web"]["auth_token"], MASKED_VALUE);
    }

    #[test]
    fn masks_credential_refs() {
        let mut val = json!({ "credential_refs": { "gh_token": "ghp_xxx", "aws_key": "AKIA..." } });
        mask_secrets(&mut val);
        assert_eq!(val["credential_refs"]["gh_token"], MASKED_VALUE);
        assert_eq!(val["credential_refs"]["aws_key"], MASKED_VALUE);
    }

    #[test]
    fn does_not_mask_non_secret_fields() {
        let mut val = json!({
            "runtime": { "max_turns": 50 },
            "memory": { "enabled": true }
        });
        let original = val.clone();
        mask_secrets(&mut val);
        assert_eq!(val, original);
    }

    #[test]
    fn does_not_mask_empty_secrets() {
        let mut val = json!({ "memory": { "auth_token": "" } });
        mask_secrets(&mut val);
        // Empty strings are left as-is (nothing to mask).
        assert_eq!(val["memory"]["auth_token"], "");
    }

    #[test]
    fn handles_missing_paths_gracefully() {
        let mut val = json!({ "other": "value" });
        // Should not panic.
        mask_secrets(&mut val);
        assert_eq!(val["other"], "value");
    }
}
