//! The dashboard's production build, embedded in the binary (see `build.rs`) and served under
//! `/` (`archi/07-code-architecture.md`, "Dashboard"). `index.html` is not cached; the files of
//! `assets/` are named by their content hash and never change.

use axum::http::{Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};

use crate::error::ApiError;

mod files {
    include!(concat!(env!("OUT_DIR"), "/dashboard.rs"));
}

/// Serves a file of the build, `index.html` for `/`. Everything else is a 404.
pub(super) async fn serve(method: Method, uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if method != Method::GET && method != Method::HEAD {
        return ApiError::not_found(format!("no {method} {}", uri.path())).into_response();
    }
    let path = if path.is_empty() { "index.html" } else { path };
    let Some((_, bytes)) = files::FILES.iter().find(|(name, _)| *name == path) else {
        return ApiError::not_found(format!("nothing at {}", uri.path())).into_response();
    };
    let cache = if path.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, content_type(path)),
            (header::CACHE_CONTROL, cache),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        *bytes,
    )
        .into_response()
}

fn content_type(path: &str) -> &'static str {
    match path.rsplit_once('.').map(|(_, extension)| extension) {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("json" | "map") => "application/json",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_build_is_embedded() {
        assert!(
            super::files::FILES
                .iter()
                .any(|(name, _)| *name == "index.html")
        );
    }
}
