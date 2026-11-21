use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "ui/"]
struct Assets;

/// Serve the embedded single-page app; unknown paths fall back to index.html.
pub async fn static_handler(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if path.starts_with("api/") {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    let path = if path.is_empty() { "index.html" } else { path };
    match Assets::get(path).or_else(|| Assets::get("index.html")) {
        Some(file) => {
            let mime = if Assets::get(path).is_some() {
                mime_guess::from_path(path).first_or_octet_stream().to_string()
            } else {
                "text/html".to_string()
            };
            ([(header::CONTENT_TYPE, mime), (header::CACHE_CONTROL, "no-cache".into())], file.data).into_response()
        }
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}
