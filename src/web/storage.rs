//! What valet's data takes: in all (each account, the rest), and in each
//! account by kind (text, attachments, Spam and Trash); an alert by email
//! when it nears a size set here.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Form, Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Redirect, Response};
use maud::{Markup, PreEscaped, html};
use serde::Deserialize;

use super::{Msg, St, authed, back, flash, megabytes, page, plural};
use crate::App;
use crate::accounts::Account;
use crate::mailstore::{Usage, now};

/// The alert goes once the data reaches this share of the size set, and can
/// go again once it is back under the second.
const NEAR: f64 = 0.9;
const BACK_UNDER: f64 = 0.8;

/// The categorical palette (light, then dark), validated for adjacent
/// segments; each account keeps its slot, by its place in the list. Past
/// eight, accounts fold into the gray "other".
const CSS: &str = r#"
.viz { --s1:#2a78d6; --s2:#eb6834; --s3:#1baf7a; --s4:#eda100; --s5:#e87ba4; --s6:#008300; --s7:#6250d6; --s8:#e34948; --other:#a8a29e; --track:var(--line); }
@media (prefers-color-scheme: dark) {
  .viz { --s1:#3987e5; --s2:#d95926; --s3:#199e70; --s4:#c98500; --s5:#d55181; --s6:#008300; --s7:#9085e9; --s8:#e66767; --other:#78716c; }
}
.viz .bar { display:flex; gap:2px; height:14px; margin:10px 0 8px; }
.viz .bar span { min-width:2px; }
.viz .bar span:last-child { border-radius:0 4px 4px 0; }
.viz .bar span.free { background:var(--track); }
.viz .legend { display:flex; flex-wrap:wrap; gap:4px 16px; margin:0; padding:0; list-style:none; font-size:13px; color:var(--muted); }
.viz .legend li { display:flex; align-items:center; gap:6px; }
.viz .legend i { width:10px; height:10px; border-radius:3px; flex:none; }
.viz .legend b { color:var(--fg); font-weight:600; }
.viz .total { font-size:20px; font-weight:600; } .viz .total small { font-size:13px; font-weight:400; color:var(--muted); }
"#;

/// A segment of a bar: what it is, its size, its color (a CSS value).
struct Part {
    label: String,
    bytes: u64,
    color: String,
}

/// A horizontal bar of `parts` (side by side, each its size), over `scale`
/// when larger (the alert's size: the rest shows as free track); then its
/// legend, which says every size.
fn bar(parts: &[Part], scale: u64, what: &str) -> Markup {
    let used: u64 = parts.iter().map(|p| p.bytes).sum();
    let free = scale.saturating_sub(used);
    let share = |n: u64| if used.max(scale) == 0 { 0.0 } else { n as f64 * 100.0 / used.max(scale) as f64 };
    let said: Vec<String> = parts.iter().filter(|p| p.bytes > 0).map(|p| format!("{} {}", p.label, megabytes(p.bytes))).collect();
    html! {
        div.bar role="img" aria-label={(what) ": " (said.join(", "))} {
            @for p in parts.iter().filter(|p| p.bytes > 0) {
                span style={"flex:" (p.bytes) " 1 0; background:" (p.color)} title={(p.label) ": " (megabytes(p.bytes)) " (" (format!("{:.0}", share(p.bytes))) "%)"} {}
            }
            @if free > 0 { span.free style={"flex:" (free) " 1 0"} title={(megabytes(free)) " left before the alert"} {} }
        }
        ul.legend {
            @for p in parts {
                li { i style={"background:" (p.color)} {} (p.label) " " b { (megabytes(p.bytes)) } }
            }
        }
    }
}

fn slot(i: usize) -> String {
    if i < 8 { format!("var(--s{})", i + 1) } else { "var(--other)".into() }
}

/// What valet's data takes in all: each account, and the rest (calendars,
/// indexes, what is shared).
struct Totals {
    accounts: Vec<(Account, Usage)>,
    /// mail.db and valet.db.
    all: u64,
}

/// Each account's usage, worked out at most every minute (it reads every
/// message's row, and the Mail page asks every few seconds).
pub fn usage(app: &App) -> std::collections::HashMap<String, Usage> {
    type Known = Option<(i64, std::collections::HashMap<String, Usage>)>;
    static KNOWN: std::sync::Mutex<Known> = std::sync::Mutex::new(None);
    let t = now();
    if let Some((at, u)) = KNOWN.lock().unwrap().as_ref()
        && t - at < 60
    {
        return u.clone();
    }
    let u = app.mail.usage();
    *KNOWN.lock().unwrap() = Some((t, u.clone()));
    u
}

fn totals(app: &App) -> Totals {
    let by = usage(app);
    let accounts: Vec<(Account, Usage)> = app.accounts().into_iter().map(|a| { let u = by.get(&a.id).copied().unwrap_or_default(); (a, u) }).collect();
    Totals { accounts, all: app.mail.db_bytes() + app.store.db_bytes() }
}

impl Totals {
    /// Each account (the first eight), then the rest: calendars, indexes,
    /// what accounts share, the accounts past the eighth.
    fn parts(&self) -> Vec<Part> {
        let mut parts: Vec<Part> =
            self.accounts.iter().take(8).enumerate().map(|(i, (a, u))| Part { label: a.name.clone(), bytes: u.total(), color: slot(i) }).collect();
        let shown: u64 = parts.iter().map(|p| p.bytes).sum();
        parts.push(Part { label: "Calendars, indexes and other".into(), bytes: self.all.saturating_sub(shown), color: "var(--other)".into() });
        parts
    }
}

/// The alert's size (bytes), and the account it's sent from (to its own address).
fn alert(app: &App) -> (Option<u64>, Option<Account>) {
    let limit = app.store.setting("storage_alert_bytes").and_then(|v| v.parse::<u64>().ok()).filter(|&n| n > 0);
    let from = app.store.setting("storage_alert_account").and_then(|id| app.account(&id)).filter(|a| a.retired.is_none());
    (limit, from)
}

/// The whole, for the Overview page and the Storage page.
pub fn overview(app: &App) -> Markup {
    let t = totals(app);
    let (limit, _) = alert(app);
    let near = limit.is_some_and(|l| t.all as f64 >= l as f64 * NEAR);
    html! {
        style { (PreEscaped(CSS)) }
        div.card.viz {
            div.total { (megabytes(t.all)) small { " used by valet"
                @if let Some(l) = limit { " · the alert is at " (megabytes(l)) } } }
            @if near { p.err.small style="margin:4px 0 0" { "⚠ Near the alert's size." } }
            (bar(&t.parts(), limit.unwrap_or(0), "What valet's data takes"))
        }
    }
}

pub async fn storage_page(State(app): St, headers: HeaderMap, Query(m): Query<Msg>) -> Response {
    if !authed(&app, &headers) {
        return Redirect::to("/login").into_response();
    }
    let t = totals(&app);
    let (limit, from) = alert(&app);
    let senders: Vec<Account> = app.accounts().into_iter().filter(|a| a.retired.is_none()).collect();
    let gb = limit.map(|l| format!("{}", (l as f64 / 1e9 * 100.0).round() / 100.0)).unwrap_or_default();
    page("Storage", "/storage", html! {
        h1 { "Storage" }
        (flash(&m))
        (overview(&app))
        h2 { "Alert" }
        form.card.row method="post" action="/storage" {
            label { "When valet's data reaches (GB; empty: no alert)" input name="gb" type="number" min="0" step="0.1" value=(gb) style="width:10em"; }
            label { "Email me from" select name="account" {
                @for a in &senders { option value=(a.id) selected[from.as_ref().is_some_and(|f| f.id == a.id)] { (a.name) " (" (a.address) ")" } }
            } }
            button type="submit" { "Save" }
            @if limit.is_some() && from.is_some() { button.plain type="submit" formaction="/storage/test" { "Send a test" } }
            p.muted.small style="flex-basis:100%; margin:0" {
                "An email goes to that account's own address once the data reaches " (format!("{:.0}", NEAR * 100.0)) "% of the size, "
                "checked every hour; again only after it went back under " (format!("{:.0}", BACK_UNDER * 100.0)) "%. "
                "To make room: keep less of an account (its page, What valet keeps), or empty its Spam and Trash."
            }
        }
        h2 { "Each account" }
        @if t.accounts.is_empty() { p.muted { "No mail account yet." } }
        @for (a, u) in &t.accounts {
            div.card.viz {
                div style="display:flex; justify-content:space-between; gap:8px; flex-wrap:wrap; align-items:baseline" {
                    b { (a.name) " " span.muted.small { (a.address) } }
                    span.small { b { (megabytes(u.total())) } }
                }
                (bar(&[
                    Part { label: "Text and formatting".into(), bytes: u.text, color: "var(--s1)".into() },
                    Part { label: "Attachments".into(), bytes: u.attachments, color: "var(--s2)".into() },
                    Part { label: "Spam and Trash".into(), bytes: u.bins, color: "var(--other)".into() },
                ], 0, &a.name))
                p.small.muted style="margin:8px 0 0" {
                    (plural(u.messages, "message")) ", " (plural(u.attachment_count, "attachment"))
                    @if u.removed > 0 { ", " (plural(u.removed, "attachment")) " not kept" }
                    @if let Some(d) = &a.mail_since { " · mail since " (d) }
                    @if let Some(d) = &a.attachments_since { " · attachments since " (d) }
                    " · " a href={"/mail/accounts/" (a.id)} { "What it keeps" }
                }
            }
        }
        p.muted.small { "Sizes are as kept, compressed. A message in several folders counts once; in Spam and Trash when all its copies are there." }
    })
}

#[derive(Deserialize)]
pub struct AlertForm {
    #[serde(default)]
    gb: String,
    #[serde(default)]
    account: String,
}

pub async fn save_alert(State(app): St, headers: HeaderMap, Form(f): Form<AlertForm>) -> Response {
    if !authed(&app, &headers) {
        return Redirect::to("/login").into_response();
    }
    let gb = f.gb.trim().replace(',', ".");
    let bytes = if gb.is_empty() {
        0
    } else {
        match gb.parse::<f64>() {
            Ok(g) if g > 0.0 && g.is_finite() => (g * 1e9) as u64,
            _ => return back("/storage", None, Some(&format!("{} isn't a size in GB.", f.gb.trim()))),
        }
    };
    if bytes > 0 && app.account(&f.account).is_none() {
        return back("/storage", None, Some("Pick the account the alert is sent from."));
    }
    let saved = app
        .store
        .set_setting("storage_alert_bytes", &bytes.to_string())
        .and_then(|_| app.store.set_setting("storage_alert_account", &f.account))
        .and_then(|_| app.store.set_setting("storage_alerted", ""));
    if let Err(e) = saved {
        return back("/storage", None, Some(&format!("Not saved: {e}")));
    }
    if bytes == 0 {
        return back("/storage", Some("No storage alert."), None);
    }
    // Already near: told now, not in an hour.
    let app2 = app.clone();
    tokio::spawn(async move { check(&app2).await });
    back("/storage", Some(&format!("Saved: an email when valet's data nears {}.", megabytes(bytes))), None)
}

pub async fn test_alert(State(app): St, headers: HeaderMap) -> Response {
    if !authed(&app, &headers) {
        return Redirect::to("/login").into_response();
    }
    let (Some(limit), Some(from)) = alert(&app) else { return back("/storage", None, Some("Save an alert first.")) };
    match send(&app, &from, limit, true).await {
        Ok(()) => back("/storage", Some(&format!("Test sent to {}.", from.address)), None),
        Err(e) => back("/storage", None, Some(&format!("Not sent: {e:#}"))),
    }
}

/// Emails what valet's data takes, by account, to `from`'s own address.
async fn send(app: &App, from: &Account, limit: u64, test: bool) -> anyhow::Result<()> {
    let t = totals(app);
    let subject = format!("{}valet's data takes {} (alert at {})", if test { "Test: " } else { "" }, megabytes(t.all), megabytes(limit));
    let mut body = format!(
        "valet's data takes {} of the {} set for this alert ({:.0}%).\r\n\r\n",
        megabytes(t.all),
        megabytes(limit),
        t.all as f64 * 100.0 / limit as f64
    );
    for (a, u) in &t.accounts {
        body.push_str(&format!(
            "- {}: {} (attachments {}, Spam and Trash {})\r\n",
            a.name,
            megabytes(u.total()),
            megabytes(u.attachments),
            megabytes(u.bins)
        ));
    }
    body.push_str("\r\nTo make room: keep less of an account (its page on valet, What valet keeps), or empty its Spam and Trash. Settings, Storage, has the details.\r\n");
    let message = format!(
        "From: valet <{addr}>\r\nTo: {addr}\r\nSubject: {subject}\r\nDate: {date}\r\nMessage-ID: <{mid}@valet>\r\nMIME-Version: 1.0\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\r\n{body}",
        addr = from.address,
        subject = crate::smtpd::encode_header(&subject),
        date = crate::util::rfc2822(now()),
        mid = uuid::Uuid::new_v4().simple(),
    );
    crate::accounts::send(from, &app.mail, &from.address, std::slice::from_ref(&from.address), message.as_bytes()).await
}

/// Sends the alert if the data nears its size and it hasn't gone since it
/// was last under; re-arms it once back under.
async fn check(app: &App) {
    let (Some(limit), Some(from)) = alert(app) else { return };
    let all = app.mail.db_bytes() + app.store.db_bytes();
    let sent = app.store.setting("storage_alerted").is_some_and(|v| v == "1");
    if all as f64 >= limit as f64 * NEAR {
        if sent {
            return;
        }
        match send(app, &from, limit, false).await {
            Ok(()) => {
                tracing::info!("storage alert sent: {} of {}", megabytes(all), megabytes(limit));
                let _ = app.store.set_setting("storage_alerted", "1");
            }
            Err(e) => tracing::warn!("storage alert not sent: {e:#}"),
        }
    } else if sent && (all as f64) < limit as f64 * BACK_UNDER {
        let _ = app.store.set_setting("storage_alerted", "");
    }
}

/// Every hour, from a few minutes after starting.
pub fn watch(app: Arc<App>) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(300)).await;
        loop {
            check(&app).await;
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    });
}
