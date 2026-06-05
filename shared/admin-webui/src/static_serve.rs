// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Static `/ui` bundle serving.
//!
//! The HTML/CSS/JS bundle is embedded at compile time via
//! [`include_dir!`] so a bare binary serves the UI with no package
//! assets installed. An operator can override the bundle on disk by
//! pointing `http.webui.asset_dir` at a directory; a file present
//! there wins, a file missing there falls back to embedded. The set of
//! servable files is whatever `assets/` holds at build time.
//!
//! Path safety: a requested sub-path is normalized by [`safe_key`],
//! which rejects any empty / `.` / `..` segment, so neither the disk
//! lookup nor the embedded lookup can escape the asset root.

use std::path::Path;

use axum::{
    Router,
    extract::{Path as AxumPath, State},
    http::{StatusCode, header},
    response::{IntoResponse, Redirect, Response},
    routing::get,
};
use include_dir::{Dir, include_dir};
use shared_admin_auth::AuthState;

use crate::WebuiConfig;

/// The HTML/CSS/JS bundle, embedded at compile time. The fallback when
/// no on-disk `asset_dir` override is configured (or a requested file
/// is missing from it).
static EMBEDDED: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/assets");

/// Router state for the static handlers: the optional on-disk asset
/// directory. An empty path means embedded-only.
#[derive(Clone)]
struct StaticState {
    asset_dir: std::path::PathBuf,
}

/// Mount the static `/ui` bundle, gated by the web-admin password.
///
/// - `GET /ui`         -> 308 redirect to `/ui/`
/// - `GET /ui/`        -> `index.html`
/// - `GET /ui/{path}`  -> the named asset (disk override, else embedded)
pub fn static_router(cfg: &WebuiConfig, auth: AuthState) -> Router {
    let state = StaticState {
        asset_dir: cfg.asset_dir.clone(),
    };
    Router::new()
        .route("/ui", get(|| async { Redirect::permanent("/ui/") }))
        .route("/ui/", get(serve_index))
        .route("/ui/*path", get(serve_path))
        .route_layer(axum::middleware::from_fn_with_state(
            auth,
            shared_admin_auth::require_admin_password,
        ))
        .with_state(state)
}

async fn serve_index(State(st): State<StaticState>) -> Response {
    serve(&st.asset_dir, "index.html").await
}

async fn serve_path(State(st): State<StaticState>, AxumPath(path): AxumPath<String>) -> Response {
    match safe_key(&path) {
        Some(key) => serve(&st.asset_dir, &key).await,
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Normalize a wildcard sub-path into a safe relative key, rejecting
/// any traversal / absolute / hidden segment. Returns `None` for
/// anything that could escape the asset root.
fn safe_key(raw: &str) -> Option<String> {
    let trimmed = raw.trim_start_matches('/');
    if trimmed.is_empty() {
        return Some("index.html".to_string());
    }
    if trimmed.contains('\0') {
        return None;
    }
    for seg in trimmed.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." {
            return None;
        }
    }
    Some(trimmed.to_string())
}

/// Serve one already-validated asset key: on-disk override first (when
/// an `asset_dir` is configured and holds the file), embedded fallback
/// otherwise, 404 if neither has it.
async fn serve(asset_dir: &Path, key: &str) -> Response {
    let ctype = content_type(key);
    if !asset_dir.as_os_str().is_empty() {
        // `key` is traversal-checked, so the join stays inside the dir.
        if let Ok(bytes) = tokio::fs::read(asset_dir.join(key)).await {
            return ([(header::CONTENT_TYPE, ctype)], bytes).into_response();
        }
    }
    if let Some(file) = EMBEDDED.get_file(key) {
        return ([(header::CONTENT_TYPE, ctype)], file.contents().to_vec()).into_response();
    }
    StatusCode::NOT_FOUND.into_response()
}

fn content_type(key: &str) -> &'static str {
    match key.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("png") => "image/png",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_key_maps_root_to_index() {
        assert_eq!(safe_key("").as_deref(), Some("index.html"));
        assert_eq!(safe_key("/").as_deref(), Some("index.html"));
    }

    #[test]
    fn safe_key_accepts_plain_names() {
        assert_eq!(safe_key("app.css").as_deref(), Some("app.css"));
        assert_eq!(safe_key("/app.js").as_deref(), Some("app.js"));
        assert_eq!(safe_key("sub/dir/x.png").as_deref(), Some("sub/dir/x.png"));
    }

    #[test]
    fn safe_key_rejects_traversal() {
        assert_eq!(safe_key("../etc/passwd"), None);
        assert_eq!(safe_key("a/../../b"), None);
        assert_eq!(safe_key("./x"), None);
        assert_eq!(safe_key("a//b"), None);
        assert_eq!(safe_key("x\0y"), None);
    }

    #[test]
    fn content_type_by_extension() {
        assert_eq!(content_type("index.html"), "text/html; charset=utf-8");
        assert_eq!(content_type("app.css"), "text/css; charset=utf-8");
        assert_eq!(content_type("app.js"), "text/javascript; charset=utf-8");
        assert_eq!(content_type("blob"), "application/octet-stream");
    }

    #[tokio::test]
    async fn embedded_index_is_served_without_asset_dir() {
        let resp = serve(Path::new(""), "index.html").await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn unknown_key_is_404() {
        let resp = serve(Path::new(""), "does-not-exist.txt").await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn disk_override_wins_over_embedded() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("app.css"), b":root{--x:1}").unwrap();
        let resp = serve(dir.path(), "app.css").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 64).await.unwrap();
        assert_eq!(&body[..], b":root{--x:1}");
    }

    #[tokio::test]
    async fn missing_on_disk_falls_back_to_embedded() {
        // asset_dir set but does not hold index.html -> embedded served.
        let dir = tempfile::tempdir().unwrap();
        let resp = serve(dir.path(), "index.html").await;
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
