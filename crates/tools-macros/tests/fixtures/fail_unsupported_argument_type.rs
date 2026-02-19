use tools_macros::tool;

#[tool]
async fn read_file(path: std::path::PathBuf) -> String {
    path.display().to_string()
}

fn main() {}
