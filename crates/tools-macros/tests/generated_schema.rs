use serde_json::json;
use tools_macros::tool;

#[tool]
/// Read UTF-8 text from a file.
/// Returns full file content.
async fn file_read(path: String) -> String {
    path
}

#[tool]
/// Execute a shell command with retry controls.
async fn shell_exec(command: &str, retries: u32, dry_run: bool, timeout_secs: f64) -> String {
    format!("{command}:{retries}:{dry_run}:{timeout_secs}")
}

#[test]
fn generated_schema_for_file_read_is_deterministic() {
    std::mem::drop(file_read("placeholder".to_owned()));
    let encoded =
        serde_json::to_value(__tool_function_decl_file_read()).expect("schema should serialize");
    assert_eq!(
        encoded,
        json!({
            "name": "file_read",
            "description": "Read UTF-8 text from a file.\nReturns full file content.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string"
                    }
                },
                "required": ["path"]
            }
        })
    );
}

#[test]
fn generated_schema_maps_supported_rust_types() {
    std::mem::drop(shell_exec("echo hi", 2, false, 1.5));
    let encoded =
        serde_json::to_value(__tool_function_decl_shell_exec()).expect("schema should serialize");
    assert_eq!(
        encoded,
        json!({
            "name": "shell_exec",
            "description": "Execute a shell command with retry controls.",
            "parameters": {
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "retries": { "type": "integer" },
                    "dry_run": { "type": "boolean" },
                    "timeout_secs": { "type": "number" }
                },
                "required": ["command", "retries", "dry_run", "timeout_secs"]
            }
        })
    );
}
