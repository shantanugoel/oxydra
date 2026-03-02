use rust_embed::Embed;

use axum::{
    extract::Path,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};

#[derive(Embed)]
#[folder = "static/"]
struct StaticAssets;

/// Serve embedded static files. Falls back to `index.html` for SPA routing.
pub async fn serve_static(Path(path): Path<String>) -> Response {
    serve_file(&path)
}

/// Serve the SPA shell at root.
pub async fn serve_index() -> Response {
    serve_file("index.html")
}

fn serve_file(path: &str) -> Response {
    match StaticAssets::get(path) {
        Some(file) => {
            let mime = mime_guess::from_path(path)
                .first_or_octet_stream()
                .to_string();
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, mime),
                    (
                        header::CACHE_CONTROL,
                        "no-cache, must-revalidate".to_owned(),
                    ),
                ],
                file.data.to_vec(),
            )
                .into_response()
        }
        None => {
            // SPA fallback: serve index.html for any unknown path.
            match StaticAssets::get("index.html") {
                Some(file) => (
                    StatusCode::OK,
                    [
                        (header::CONTENT_TYPE, "text/html; charset=utf-8".to_owned()),
                        (
                            header::CACHE_CONTROL,
                            "no-cache, must-revalidate".to_owned(),
                        ),
                    ],
                    file.data.to_vec(),
                )
                    .into_response(),
                None => StatusCode::NOT_FOUND.into_response(),
            }
        }
    }
}
