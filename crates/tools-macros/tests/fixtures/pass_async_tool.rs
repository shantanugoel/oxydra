use tools_macros::tool;

#[tool]
/// Read a file from disk.
async fn read_file(path: String) -> String {
    path
}

fn main() {
    let declaration = __tool_function_decl_read_file();
    let encoded = serde_json::to_value(declaration).expect("declaration should serialize");
    assert_eq!(encoded["name"], "read_file");
    assert_eq!(encoded["parameters"]["properties"]["path"]["type"], "string");
}
