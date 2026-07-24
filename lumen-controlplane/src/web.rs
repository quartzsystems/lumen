//! Serves the static lumen-webui export (Next.js `output: "export"`).
//!
//! ServeDir resolves /login → login/index.html etc.; unknown paths get the
//! export's 404 page. The webui dir may legitimately be absent during
//! API-only development — the API works regardless, so just warn.

use std::path::Path;

use axum::Router;
use tower_http::services::{ServeDir, ServeFile};

pub fn router(webui_dir: &Path) -> Router {
    if !webui_dir.join("index.html").is_file() {
        tracing::warn!(
            "webui export not found at {} — serving API only",
            webui_dir.display()
        );
    }
    let serve =
        ServeDir::new(webui_dir).not_found_service(ServeFile::new(webui_dir.join("404.html")));
    Router::new().fallback_service(serve)
}
