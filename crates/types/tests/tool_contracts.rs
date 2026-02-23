use serde_json::json;
use types::{FunctionDecl, SafetyTier};

#[test]
fn function_decl_serializes_to_expected_json_shape() {
    let schema = FunctionDecl::new(
        "file_read",
        Some("Read UTF-8 text from a file".to_owned()),
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute file path"
                }
            },
            "required": ["path"]
        }),
    );

    let encoded = serde_json::to_value(schema).expect("function schema should serialize");
    assert_eq!(
        encoded,
        json!({
            "name": "file_read",
            "description": "Read UTF-8 text from a file",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute file path"
                    }
                },
                "required": ["path"]
            }
        })
    );
}

#[test]
fn function_decl_supports_full_json_schema_keywords() {
    // Verify that enum, min/max, minLength/maxLength, default all serialize correctly.
    let schema = FunctionDecl::new(
        "web_search",
        Some("Search the web".to_owned()),
        json!({
            "type": "object",
            "required": ["query"],
            "properties": {
                "query":     { "type": "string", "minLength": 1 },
                "count":     { "type": "integer", "minimum": 1, "maximum": 10, "default": 5 },
                "freshness": { "type": "string", "enum": ["day", "week", "month", "year"] }
            }
        }),
    );

    let encoded = serde_json::to_value(&schema).expect("schema should serialize");
    assert_eq!(
        encoded["parameters"]["properties"]["freshness"]["enum"][0],
        "day"
    );
    assert_eq!(encoded["parameters"]["properties"]["count"]["minimum"], 1);
    assert_eq!(encoded["parameters"]["properties"]["count"]["maximum"], 10);
    assert_eq!(encoded["parameters"]["properties"]["query"]["minLength"], 1);
}

#[test]
fn safety_tier_uses_snake_case_serde_labels() {
    let encoded = serde_json::to_string(&SafetyTier::ReadOnly).expect("tier should serialize");
    assert_eq!(encoded, "\"read_only\"");
}
