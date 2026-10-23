//! What pages load besides themselves: scripts, the service worker, the app
//! manifest (installing valet as an app) and icons. URLs carry a version
//! (a hash of every file), so browsers keep files until valet changes.

use std::sync::OnceLock;

use axum::extract::{Path, Query};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};

const FILES: &[(&str, &str, &[u8])] = &[
    ("app.js", "text/javascript; charset=utf-8", include_bytes!("assets/app.js")),
    ("time.js", "text/javascript; charset=utf-8", include_bytes!("assets/time.js")),
    ("calendar.js", "text/javascript; charset=utf-8", include_bytes!("assets/calendar.js")),
    ("patterns.js", "text/javascript; charset=utf-8", include_bytes!("assets/patterns.js")),
    ("icon-32.png", "image/png", include_bytes!("assets/icon-32.png")),
    ("icon-180.png", "image/png", include_bytes!("assets/icon-180.png")),
    ("icon-192.png", "image/png", include_bytes!("assets/icon-192.png")),
    ("icon-512.png", "image/png", include_bytes!("assets/icon-512.png")),
    ("icon-maskable-512.png", "image/png", include_bytes!("assets/icon-maskable-512.png")),
];

const SERVICE_WORKER: &str = include_str!("assets/sw.js");

/// Changes whenever a file (or the service worker, the calendar's look or
/// the offline page) does.
fn version() -> &'static str {
    static V: OnceLock<String> = OnceLock::new();
    V.get_or_init(|| {
        let mut h = Sha256::new();
        for (name, _, bytes) in FILES {
            h.update(name.as_bytes());
            h.update(bytes);
        }
        h.update(SERVICE_WORKER.as_bytes());
        h.update(include_bytes!("assets/calendar.css"));
        h.update(offline_page().as_bytes());
        hex::encode(h.finalize())[..12].to_string()
    })
}

/// A file's address, versioned.
pub fn url(name: &str) -> String {
    format!("/assets/{name}?v={}", version())
}

#[derive(serde::Deserialize)]
pub struct V {
    v: Option<String>,
}

pub async fn file(Path(name): Path<String>, Query(q): Query<V>) -> Response {
    let Some((_, ctype, bytes)) = FILES.iter().find(|(n, ..)| *n == name) else { return StatusCode::NOT_FOUND.into_response() };
    let cache = if q.v.as_deref() == Some(version()) { "public, max-age=31536000, immutable" } else { "public, max-age=300" };
    ([(header::CONTENT_TYPE, *ctype), (header::CACHE_CONTROL, cache)], *bytes).into_response()
}

/// At the root, so it may serve every page.
pub async fn service_worker() -> Response {
    let mut precache: Vec<String> = FILES.iter().map(|(n, ..)| url(n)).collect();
    precache.push("/offline".into());
    let body = SERVICE_WORKER.replace("__VERSION__", version()).replace("__PRECACHE__", &serde_json::to_string(&precache).unwrap_or_default());
    ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8"), (header::CACHE_CONTROL, "no-cache")], body).into_response()
}

pub async fn manifest() -> Response {
    let icon = |name: &str, size: &str, purpose: &str| serde_json::json!({ "src": url(name), "sizes": size, "type": "image/png", "purpose": purpose });
    let m = serde_json::json!({
        "id": "/",
        "name": "valet",
        "short_name": "valet",
        "description": "Your mail and calendar",
        "start_url": "/inbox",
        "scope": "/",
        "display": "standalone",
        "background_color": "#fafaf9",
        "theme_color": "#2563eb",
        "icons": [icon("icon-192.png", "192x192", "any"), icon("icon-512.png", "512x512", "any"), icon("icon-maskable-512.png", "512x512", "maskable")],
        "shortcuts": [
            { "name": "Mail", "url": "/inbox" },
            { "name": "Calendar", "url": "/calendar" },
            { "name": "Write", "url": "/compose" }
        ]
    });
    ([(header::CONTENT_TYPE, "application/manifest+json"), (header::CACHE_CONTROL, "public, max-age=3600")], m.to_string()).into_response()
}

/// What the service worker shows for a page never opened before, offline.
pub async fn offline() -> Response {
    ([(header::CACHE_CONTROL, "no-cache")], axum::response::Html(offline_page())).into_response()
}

fn offline_page() -> String {
    maud::html! {
        (maud::DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { "Offline · valet" }
                link rel="manifest" href="/manifest.webmanifest";
                style { "body{font:15px/1.5 system-ui,sans-serif;max-width:420px;margin:15vh auto;padding:0 16px;color:#1c1917;background:#fafaf9}a{color:#2563eb}@media (prefers-color-scheme:dark){body{color:#f5f5f4;background:#1c1917}a{color:#60a5fa}}" }
            }
            body {
                h1 { "You're offline" }
                p { "This page wasn't opened on this device before, so there's no copy of it here. Pages and calendar months you've seen are still readable." }
                p { a href="/calendar" { "Calendar" } " · " a href="/inbox" { "Inbox" } " · " a href="javascript:location.reload()" { "Try again" } }
            }
        }
    }
    .into_string()
}

/// The head lines making valet installable, and the scripts every page runs
/// (time.js first: the pages' own scripts show times with it).
pub fn head() -> maud::Markup {
    maud::html! {
        meta name="valet-tz" content=(crate::calendar::default_zone().name());
        script src=(url("time.js")) {}
        link rel="manifest" href="/manifest.webmanifest";
        link rel="icon" type="image/png" href=(url("icon-32.png"));
        link rel="apple-touch-icon" href=(url("icon-180.png"));
        meta name="theme-color" content="#ffffff" media="(prefers-color-scheme: light)";
        meta name="theme-color" content="#292524" media="(prefers-color-scheme: dark)";
        meta name="mobile-web-app-capable" content="yes";
        meta name="apple-mobile-web-app-capable" content="yes";
        meta name="apple-mobile-web-app-title" content="valet";
        meta name="apple-mobile-web-app-status-bar-style" content="default";
        script src=(url("app.js")) defer {}
    }
}
