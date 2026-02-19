use serde_json::json;
use tools_macros::tool;

#[tool]
/// Read UTF-8 text from a file.
/// Returns full file content.
async fn read_file(path: String) -> String {
    path
}

#[tool]
/// Execute a shell command with retry controls.
async fn bash(command: &str, retries: u32, dry_run: bool, timeout_secs: f64) -> String {
    format!("{command}:{retries}:{dry_run}:{timeout_secs}")
}

#[test]
fn generated_schema_for_read_file_is_deterministic() {
    std::mem::drop(read_file("placeholder".to_owned()));
    let encoded =
        serde_json::to_value(__tool_function_decl_read_file()).expect("schema should serialize");
    assert_eq!(
        encoded,
        json!({
            "name": "read_file",
            "description": "Read UTF-8 text from a file.\nReturns full file content.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string"
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }
        })
    );
}

#[test]
fn generated_schema_maps_supported_rust_types() {
    std::mem::drop(bash("echo hi", 2, false, 1.5));
    let encoded =
        serde_json::to_value(__tool_function_decl_bash()).expect("schema should serialize");
    assert_eq!(
        encoded,
        json!({
            "name": "bash",
            "description": "Execute a shell command with retry controls.",
            "parameters": {
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "retries": { "type": "integer" },
                    "dry_run": { "type": "boolean" },
                    "timeout_secs": { "type": "number" }
                },
                "required": ["command", "retries", "dry_run", "timeout_secs"],
                "additionalProperties": false
            }
        })
    );
}
