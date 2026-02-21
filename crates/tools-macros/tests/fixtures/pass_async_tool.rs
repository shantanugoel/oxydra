use tools_macros::tool;

#[tool]
/// Read a file from disk.
async fn file_read(path: String) -> String {
    path
}

fn main() {
    let declaration = __tool_function_decl_file_read();
    let encoded = serde_json::to_value(declaration).expect("declaration should serialize");
    assert_eq!(encoded["name"], "file_read");
    assert_eq!(encoded["parameters"]["properties"]["path"]["type"], "string");
}
