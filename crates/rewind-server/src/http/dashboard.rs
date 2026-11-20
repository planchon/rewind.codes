//! The dashboard's production build, embedded in the binary (see `build.rs`) and served at the
//! root (`archi/07-code-architecture.md`, "Dashboard"). The dashboard's routes are paths
//! (`/runs/{id}`): a route that is no file of the build answers `index.html`, which reads the
//! route from the URL. `index.html` is not cached; the files of `assets/` are named by their
//! content hash and never change.

use axum::http::{Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};

use crate::error::ApiError;

mod files {
    include!(concat!(env!("OUT_DIR"), "/dashboard.rs"));
}

/// The first segments of the server's own paths: nothing under them is the dashboard's.
const SERVER_ROOTS: [&str; 7] = [
    "v1",
    "auth",
    "oauth",
    ".well-known",
    "mcp",
    "hooks",
    "assets",
];

/// Serves a file of the build, `index.html` for `/` and for a route of the dashboard.
/// Everything else is a 404.
pub(super) async fn serve(method: Method, uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if method != Method::GET && method != Method::HEAD {
        return ApiError::not_found(format!("no {method} {}", uri.path())).into_response();
    }
    let path = if path.is_empty() || (is_route(path) && !is_file(path)) {
        "index.html"
    } else {
        path
    };
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

fn is_file(path: &str) -> bool {
    files::FILES.iter().any(|(name, _)| *name == path)
}

/// Whether `path` (without its leading `/`) may be a route of the dashboard: not under a root
/// of the server, and not a file name at the root (`favicon.ico`). A deeper segment may hold a
/// dot: a namespace's name does (`/namespaces/team.alpha`).
fn is_route(path: &str) -> bool {
    let first = path.split('/').next().unwrap_or_default();
    !SERVER_ROOTS.contains(&first) && (path.contains('/') || !first.contains('.'))
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
    use rewind_api::routes;

    use super::*;

    #[test]
    fn the_build_is_embedded() {
        assert!(is_file("index.html"));
    }

    #[test]
    fn routes_are_paths_outside_the_server_s_roots() {
        for route in [
            "overview",
            "runs/0193a4f0/trace",
            "namespaces/alice",
            "namespaces/team.alpha",
            "connect/authorize",
            "runs/abc/",
        ] {
            assert!(is_route(route), "{route}");
        }
        for path in [
            "v1/nope",
            "v1",
            "auth/whatever",
            "oauth/x",
            ".well-known/x",
            "mcp",
            "hooks/x",
            "assets/missing.js",
            "favicon.ico",
            "robots.txt",
        ] {
            assert!(!is_route(path), "{path}");
        }
    }

    /// Every path the router serves itself starts with one of the roots.
    #[test]
    fn the_roots_cover_every_path_of_the_server() {
        for (_, path) in routes::OPERATIONS.iter().chain([&("POST", routes::MCP)]) {
            let path = path.trim_start_matches('/');
            assert!(!is_route(path), "{path}");
        }
    }
}
