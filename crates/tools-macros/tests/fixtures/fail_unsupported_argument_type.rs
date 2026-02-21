use tools_macros::tool;

#[tool]
async fn file_read(path: std::path::PathBuf) -> String {
    path.display().to_string()
}

fn main() {}
