use std::collections::BTreeMap;

use serde_json::json;
use types::{FunctionDecl, JsonSchema, JsonSchemaType, SafetyTier};

#[test]
fn function_decl_serializes_to_expected_json_shape() {
    let mut properties = BTreeMap::new();
    properties.insert(
        "path".to_owned(),
        JsonSchema::new(JsonSchemaType::String).with_description("Absolute file path"),
    );

    let schema = FunctionDecl::new(
        "file_read",
        Some("Read UTF-8 text from a file".to_owned()),
        JsonSchema::object(properties, vec!["path".to_owned()]),
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
fn safety_tier_uses_snake_case_serde_labels() {
    let encoded = serde_json::to_string(&SafetyTier::ReadOnly).expect("tier should serialize");
    assert_eq!(encoded, "\"read_only\"");
}
