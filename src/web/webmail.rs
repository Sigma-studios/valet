//! Webmail: folders, message lists, reading (HTML mail in a locked-down
//! frame), actions, and writing. It works on valet's copy, like the mail apps
//! do, so what happens here reaches the providers and the apps.

use std::borrow::Cow;

use axum::body::Body;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use maud::{Markup, PreEscaped, html};
use serde::Deserialize;

use super::*;
use crate::accounts::Account;
use crate::mailstore::{Mailbox, Msg, StoreMode};

const PAGE: usize = 50;

fn parse(raw: &[u8]) -> Option<mail_parser::Message<'_>> {
    mail_parser::MessageParser::default().parse(raw)
}

fn has_flag(m: &Msg, flag: &str) -> bool {
    m.flags.iter().any(|f| f.eq_ignore_ascii_case(flag))
}

/// "Name" or the address, for a list.
fn who(a: Option<&mail_parser::Address>) -> String {
    let Some(first) = a.and_then(|a| a.first()) else { return String::new() };
    first.name().filter(|n| !n.trim().is_empty()).or(first.address()).unwrap_or("").to_string()
}

/// "Name <address>, …", in full.
fn everyone(a: Option<&mail_parser::Address>) -> String {
    a.map(|a| {
        a.iter()
            .filter_map(|x| {
                let addr = x.address()?;
                Some(match x.name().filter(|n| !n.trim().is_empty()) {
                    Some(n) => format!("{n} <{addr}>"),
                    None => addr.to_string(),
                })
            })
            .collect::<Vec<_>>()
            .join(", ")
    })
    .unwrap_or_default()
}

fn addresses_of(a: Option<&mail_parser::Address>) -> Vec<String> {
    a.map(|a| a.iter().filter_map(|x| x.address().map(str::to_string)).collect()).unwrap_or_default()
}

/// Seconds since 1970, shown in the reader's time zone by the page's script.
fn time(t: i64, long: bool) -> Markup {
    html! { time data-t=(t) data-long=[long.then_some("1")] { (crate::util::imap_datetime(t).trim_end_matches(" +0000")) " UTC" } }
}

const TIME_SCRIPT: &str = r#"
for (const el of document.querySelectorAll('time[data-t]')) {
  const d = new Date(el.dataset.t * 1000), now = new Date();
  const sameDay = d.toDateString() === now.toDateString();
  el.textContent = el.dataset.long
    ? d.toLocaleString([], { dateStyle: 'full', timeStyle: 'short' })
    : sameDay ? d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })
    : d.getFullYear() === now.getFullYear() ? d.toLocaleDateString([], { day: 'numeric', month: 'short' })
    : d.toLocaleDateString();
  el.title = d.toLocaleString();
}
"#;

/// Folders: all inboxes first, then each account's, in the accounts' order.
fn folders(app: &App) -> (Vec<Mailbox>, Vec<(Account, Vec<Mailbox>)>) {
    let boxes = app.mail.mailboxes();
    let top: Vec<Mailbox> = boxes.iter().filter(|b| b.is_all_inboxes()).cloned().collect();
    let order = |b: &Mailbox| {
        let rank = ["\\Inbox", "\\Drafts", "\\Sent", "\\Archive", "\\Junk", "\\Trash"].iter().position(|s| b.special.eq_ignore_ascii_case(s));
        let inbox = b.remote.as_deref().is_some_and(|r| r.eq_ignore_ascii_case("INBOX"));
        (if inbox { 0 } else { rank.map_or(99, |r| r + 1) }, b.name.to_lowercase())
    };
    let per = app
        .accounts()
        .into_iter()
        .map(|a| {
            let mut mine: Vec<Mailbox> = boxes.iter().filter(|b| b.account.as_deref() == Some(a.id.as_str())).cloned().collect();
            mine.sort_by_key(order);
            (a, mine)
        })
        .collect();
    (top, per)
}

/// A folder's name within its account (`Work/Projects/2026` → `Projects/2026`).
fn short_name(b: &Mailbox) -> String {
    if b.is_all_inboxes() {
        return "All inboxes".into();
    }
    let name = b.name.split_once('/').map(|(_, r)| r).unwrap_or(&b.name);
    if b.remote.as_deref().is_some_and(|r| r.eq_ignore_ascii_case("INBOX")) { "Inbox".into() } else { name.to_string() }
}

fn sidebar(app: &App, current: i64) -> Markup {
    let (top, per) = folders(app);
    let item = |b: &Mailbox| {
        let (_, unseen) = app.mail.count(b.id);
        html! {
            a.folder.on[b.id == current] href={"/box/" (b.id)} {
                span { (short_name(b)) }
                @if unseen > 0 { span.badge { (unseen) } }
            }
        }
    };
    html! {
        nav.folders {
            a.compose href="/compose" { "Write" }
            @for b in &top { (item(b)) }
            @for (a, boxes) in &per {
                div.account { span { (a.name) } a href={"/folders/" (a.id)} title="Manage folders" { "Edit" } }
                @for b in boxes { (item(b)) }
            }
        }
        // Phones: the folders as one menu.
        select.folders-menu onchange="location = this.value" {
            @for b in &top { option value={"/box/" (b.id)} selected[b.id == current] { (short_name(b)) } }
            @for (a, boxes) in &per {
                optgroup label=(a.name) {
                    @for b in boxes { option value={"/box/" (b.id)} selected[b.id == current] { (short_name(b)) } }
                }
            }
        }
    }
}

const MAIL_CSS: &str = r#"
.mail { display:grid; grid-template-columns: 210px 1fr; gap:20px; align-items:start; }
.folders { display:flex; flex-direction:column; gap:1px; position:sticky; top:12px; }
.folders a.folder { display:flex; justify-content:space-between; gap:8px; padding:5px 10px; border-radius:6px; color:var(--fg); }
.folders a.folder:hover { background:var(--line); text-decoration:none; }
.folders a.folder.on { background:var(--accent); color:#fff; }
.folders .account { display:flex; justify-content:space-between; margin:14px 10px 4px; font-size:12px; font-weight:600; text-transform:uppercase; letter-spacing:.04em; color:var(--muted); }
.folders .account a { text-transform:none; letter-spacing:0; font-weight:400; color:var(--muted); }
.badge { font-size:12px; font-weight:600; }
a.compose { display:block; text-align:center; background:var(--accent); color:#fff; border-radius:6px; padding:7px 10px; margin-bottom:10px; font-weight:600; }
a.compose:hover { text-decoration:none; filter:brightness(1.1); }
.folders-menu { display:none; width:100%; margin-bottom:10px; }
.list { background:var(--card); border:1px solid var(--line); border-radius:8px; overflow:hidden; }
.list a.row { display:grid; grid-template-columns: 14px minmax(110px, 200px) 1fr auto; gap:10px; align-items:baseline; padding:8px 12px; border-bottom:1px solid var(--line); color:var(--fg); }
.list a.row:last-child { border-bottom:none; }
.list a.row:hover { background:var(--bg); text-decoration:none; }
.list a.row.unread .from, .list a.row.unread .subject { font-weight:650; }
.list .from, .list .line { overflow:hidden; text-overflow:ellipsis; white-space:nowrap; }
.list .preview { color:var(--muted); }
.list .when { color:var(--muted); font-size:13px; white-space:nowrap; }
.list .star { color:#eab308; font-size:12px; }
.tag { font-size:11px; border:1px solid var(--line); border-radius:4px; padding:0 4px; color:var(--muted); margin-right:4px; }
.toolbar { display:flex; gap:8px; flex-wrap:wrap; align-items:center; margin-bottom:12px; }
.toolbar form { display:inline; margin:0; }
.toolbar input[type=search] { flex:1; min-width:160px; }
.toolbar a.button, .toolbar button { background:var(--card); color:var(--fg); border:1px solid var(--line); border-radius:6px; padding:5px 10px; }
.toolbar a.button:hover, .toolbar button:hover { background:var(--line); text-decoration:none; }
.toolbar button.danger { color:var(--bad); }
.headers { margin-bottom:12px; } .headers h1 { margin:0 0 8px; }
.headers .meta { color:var(--muted); font-size:13px; }
.reader iframe { width:100%; border:0; background:#fff; border-radius:6px; min-height:200px; display:block; }
.reader pre { white-space:pre-wrap; word-wrap:break-word; font:inherit; margin:0; }
.atts { display:flex; flex-wrap:wrap; gap:8px; margin-top:12px; }
.atts a { border:1px solid var(--line); border-radius:6px; padding:4px 10px; background:var(--card); }
.notice { background:var(--card); border:1px solid var(--line); border-radius:6px; padding:6px 10px; font-size:13px; margin-bottom:8px; }
.pager { display:flex; justify-content:space-between; margin-top:10px; }
form.write { display:flex; flex-direction:column; gap:10px; }
form.write textarea { min-height:320px; font:inherit; padding:8px; border:1px solid var(--line); border-radius:6px; background:var(--bg); color:var(--fg); resize:vertical; }
@media (max-width: 720px) {
  .mail { display:block; } nav.folders { display:none; } .folders-menu { display:block; }
  .list a.row { grid-template-columns: 1fr auto; } .list .star { display:none; }
  .list .line { grid-column: 1 / -1; }
}
"#;

fn mail_page(title: &str, current: i64, app: &App, body: Markup) -> Response {
    page_wide(
        title,
        "/inbox",
        html! {
            style { (PreEscaped(MAIL_CSS)) }
            div.mail {
                (sidebar(app, current))
                div { (body) }
            }
            script { (PreEscaped(TIME_SCRIPT)) }
        },
    )
}

pub async fn inbox(State(app): St, headers: HeaderMap) -> Response {
    guard!(app, headers);
    Redirect::to(&format!("/box/{}", app.mail.all_inboxes())).into_response()
}

#[derive(Deserialize, Default)]
pub struct ListQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    page: usize,
    ok: Option<String>,
    err: Option<String>,
    images: Option<String>,
}

fn flash_of(q: &ListQuery) -> Markup {
    flash(&super::Msg { ok: q.ok.clone(), err: q.err.clone(), ..Default::default() })
}

/// Whether the message contains every word of the search.
fn matches(raw: &[u8], words: &[String]) -> bool {
    let Some(m) = parse(raw) else { return false };
    let mut hay = String::new();
    for part in [m.subject().unwrap_or(""), &everyone(m.from()), &everyone(m.to()), &everyone(m.cc())] {
        hay.push_str(part);
        hay.push('\n');
    }
    if let Some(t) = m.body_text(0) {
        hay.push_str(&t);
    }
    let hay = hay.to_lowercase();
    words.iter().all(|w| hay.contains(w.as_str()))
}

pub async fn list(State(app): St, headers: HeaderMap, Path(id): Path<i64>, Query(q): Query<ListQuery>) -> Response {
    guard!(app, headers);
    let Some(mbox) = app.mail.mailbox(id) else { return StatusCode::NOT_FOUND.into_response() };
    let mut msgs = app.mail.messages(id);
    msgs.sort_by_key(|m| std::cmp::Reverse((m.date, m.id)));
    let words: Vec<String> = q.q.split_whitespace().map(str::to_lowercase).collect();
    if !words.is_empty() {
        msgs.retain(|m| app.mail.blob(&m.blob).is_some_and(|raw| matches(&raw, &words)));
    }
    let total = msgs.len();
    let start = (q.page * PAGE).min(total);
    let shown = &msgs[start..(start + PAGE).min(total)];
    let names: std::collections::HashMap<String, String> = app.accounts().into_iter().map(|a| (a.id, a.name)).collect();
    let account_of = |m: &Msg| {
        let src = app.mail.source(m.clone());
        app.mail.mailbox(src.mailbox).and_then(|b| b.account).and_then(|a| names.get(&a).cloned())
    };
    let title = short_name(&mbox);
    let drafts = mbox.special.eq_ignore_ascii_case("\\Drafts");
    let link = |page: usize| {
        let mut l = format!("/box/{id}?page={page}");
        if !q.q.is_empty() {
            l.push_str("&q=");
            l.push_str(&percent_encoding::utf8_percent_encode(&q.q, percent_encoding::NON_ALPHANUMERIC).to_string());
        }
        l
    };
    mail_page(&title, id, &app, html! {
        (flash_of(&q))
        form.toolbar method="get" action={"/box/" (id)} {
            input type="search" name="q" value=(q.q) placeholder={"Search " (title)};
            @if !q.q.is_empty() { a.button href={"/box/" (id)} { "Clear" } }
        }
        @if shown.is_empty() {
            p.muted { @if q.q.is_empty() { "Nothing here." } @else { "No message matches." } }
        } @else {
            div.list {
                @for m in shown {
                    @let raw = app.mail.blob(&m.blob).unwrap_or_default();
                    @let parsed = parse(&raw);
                    @let subject = parsed.as_ref().and_then(|p| p.subject().map(str::to_string)).filter(|s| !s.trim().is_empty()).unwrap_or_else(|| "(no subject)".into());
                    @let from = parsed.as_ref().map(|p| if mbox.special.eq_ignore_ascii_case("\\Sent") { format!("To: {}", who(p.to())) } else { who(p.from()) }).unwrap_or_default();
                    @let preview: String = parsed.as_ref().and_then(|p| p.body_text(0).map(|t| t.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(140).collect())).unwrap_or_default();
                    a.row.unread[!has_flag(m, "\\Seen")] href=(if drafts { format!("/compose?draft={}", m.id) } else { format!("/msg/{}", m.id) }) {
                        span.star { @if has_flag(m, "\\Flagged") { "★" } }
                        span.from { (from) }
                        span.line {
                            @if mbox.is_all_inboxes() { @if let Some(a) = account_of(m) { span.tag { (a) } } }
                            span.subject { (subject) }
                            @if !preview.is_empty() { span.preview { " — " (preview) } }
                        }
                        span.when { (time(m.date, false)) }
                    }
                }
            }
            div.pager.small.muted {
                span { @if q.page > 0 { a href=(link(q.page - 1)) { "← Newer" } } }
                span { (start + 1) "–" (start + shown.len()) " of " (total) }
                span { @if start + PAGE < total { a href=(link(q.page + 1)) { "Older →" } } }
            }
        }
    })
}

/// The message row as listed, its source row (an account folder's), and that folder.
fn locate(app: &App, id: i64) -> Option<(Msg, Msg, Mailbox)> {
    let row = app.mail.message(id)?;
    let src = app.mail.source(row.clone());
    let folder = app.mail.mailbox(src.mailbox)?;
    Some((row, src, folder))
}

pub async fn view(State(app): St, headers: HeaderMap, Path(id): Path<i64>, Query(q): Query<ListQuery>) -> Response {
    guard!(app, headers);
    let Some((row, src, folder)) = locate(&app, id) else { return StatusCode::NOT_FOUND.into_response() };
    if !has_flag(&src, "\\Seen") {
        let _ = app.mail.store(&[src.id], &StoreMode::Add, &["\\Seen".to_string()]);
    }
    let raw = app.mail.blob(&src.blob).unwrap_or_default();
    let Some(m) = parse(&raw) else { return (StatusCode::INTERNAL_SERVER_ERROR, "This message can't be read.").into_response() };
    let subject = m.subject().filter(|s| !s.trim().is_empty()).unwrap_or("(no subject)").to_string();
    // mail-parser turns a text-only message into HTML too: only real HTML goes in the frame.
    let html_body = m.html_body.first().and_then(|&i| m.part(i)).is_some_and(|p| p.is_text_html());
    let remote = html_body && m.body_html(0).is_some_and(|h| has_remote(&h));
    let images = q.images.is_some();
    let back = row.mailbox;
    let account_folders: Vec<Mailbox> = app.mail.mailboxes().into_iter().filter(|b| b.account == folder.account && b.id != folder.id).collect();
    let attachments: Vec<(usize, String, usize)> = m
        .attachments()
        .enumerate()
        .filter(|(_, a)| !is_inline_image(a) || !html_body)
        .map(|(i, a)| {
            use mail_parser::MimeHeaders;
            (i, a.attachment_name().unwrap_or("attachment").to_string(), a.contents().len())
        })
        .collect();
    let flagged = has_flag(&src, "\\Flagged");
    mail_page(&subject, back, &app, html! {
        (flash_of(&q))
        div.toolbar {
            a.button href={"/box/" (back)} { "← Back" }
            @if is_draft(&src, &folder) { a.button href={"/compose?draft=" (id)} { "Continue writing" } }
            a.button href={"/compose?reply=" (id)} { "Reply" }
            a.button href={"/compose?all=" (id)} { "Reply all" }
            a.button href={"/compose?forward=" (id)} { "Forward" }
            form method="post" action={"/msg/" (id)} { input type="hidden" name="back" value=(back);
                button name="action" value=(if flagged { "unflag" } else { "flag" }) { @if flagged { "★ Unflag" } @else { "☆ Flag" } } }
            form method="post" action={"/msg/" (id)} { input type="hidden" name="back" value=(back);
                button name="action" value="unseen" { "Mark unread" } }
            @if !account_folders.is_empty() {
                form method="post" action={"/msg/" (id)} {
                    input type="hidden" name="back" value=(back);
                    input type="hidden" name="action" value="move";
                    select name="to" onchange="this.form.submit()" {
                        option value="" { "Move to…" }
                        @for b in &account_folders { option value=(b.id) { (short_name(b)) } }
                    }
                }
            }
            form method="post" action={"/msg/" (id)} { input type="hidden" name="back" value=(back);
                button.danger name="action" value="delete" { "Delete" } }
        }
        div.card.reader {
            div.headers {
                h1 { (subject) }
                div { b { (everyone(m.from())) } }
                div.meta { "To " (everyone(m.to()))
                    @let cc = everyone(m.cc()); @if !cc.is_empty() { br; "Cc " (cc) } }
                div.meta { (time(src.date, true)) " · " (folder.name) }
            }
            @if html_body {
                @if remote && !images {
                    div.notice { "Remote images are blocked, so the sender can't see you opened this. "
                        a href={"/msg/" (id) "?images=1"} { "Show them" } }
                }
                iframe src=(format!("/msg/{id}/html{}", if images { "?images=1" } else { "" }))
                    sandbox="allow-same-origin allow-popups allow-popups-to-escape-sandbox"
                    referrerpolicy="no-referrer"
                    onload="const d=this.contentDocument; if (d) this.style.height = (d.documentElement.scrollHeight + 4) + 'px'" {}
            } @else {
                pre { (m.body_text(0).unwrap_or(Cow::Borrowed(""))) }
            }
            @if !attachments.is_empty() {
                div.atts {
                    @for (i, name, size) in &attachments {
                        a href={"/msg/" (id) "/att/" (i)} { "📎 " (name) span.muted.small { " " (human_size(*size)) } }
                    }
                }
            }
        }
    })
}

fn human_size(n: usize) -> String {
    match n {
        n if n < 1024 => format!("{n} B"),
        n if n < 1024 * 1024 => format!("{} KB", n / 1024),
        n => format!("{:.1} MB", n as f64 / 1024.0 / 1024.0),
    }
}

fn is_inline_image(p: &mail_parser::MessagePart) -> bool {
    use mail_parser::MimeHeaders;
    p.content_id().is_some() && p.content_type().is_some_and(|c| c.ctype().eq_ignore_ascii_case("image"))
}

fn has_remote(html: &str) -> bool {
    let h = html.to_ascii_lowercase();
    ["src=\"http", "src='http", "src=http", "url(http", "url('http", "url(\"http", "background=\"http"].iter().any(|p| h.contains(p))
}

#[derive(Deserialize)]
pub struct HtmlQuery {
    #[serde(default)]
    images: Option<String>,
}

/// The HTML body, for the reader's frame: no scripts, no forms, no plugins,
/// nothing fetched from elsewhere unless images were asked for, links open
/// in a new tab.
pub async fn html_body(State(app): St, headers: HeaderMap, Path(id): Path<i64>, Query(q): Query<HtmlQuery>) -> Response {
    guard!(app, headers);
    let Some((_, src, _)) = locate(&app, id) else { return StatusCode::NOT_FOUND.into_response() };
    let raw = app.mail.blob(&src.blob).unwrap_or_default();
    let Some(m) = parse(&raw) else { return StatusCode::NOT_FOUND.into_response() };
    let body = m.body_html(0).unwrap_or_default();
    let body = cid_links(&body, id);
    let remote = if q.images.is_some() { " https: http:" } else { "" };
    let csp = format!(
        "sandbox allow-same-origin allow-popups allow-popups-to-escape-sandbox; default-src 'none'; script-src 'none'; \
         img-src 'self' data:{remote}; style-src 'unsafe-inline'{remote}; font-src data:{remote}; media-src 'none'; frame-src 'none'; form-action 'none'; base-uri 'self'"
    );
    let doc = format!(
        "<!doctype html><meta charset=\"utf-8\"><base target=\"_blank\"><style>html{{color-scheme:light}}body{{margin:12px;font:14px/1.45 system-ui,sans-serif;color:#111;background:#fff;overflow-wrap:anywhere}}img{{max-width:100%;height:auto}}</style>{body}"
    );
    Response::builder()
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header("Content-Security-Policy", csp)
        .header("X-Content-Type-Options", "nosniff")
        .header("Referrer-Policy", "no-referrer")
        .header(header::CACHE_CONTROL, "private, max-age=3600")
        .body(Body::from(doc))
        .unwrap()
}

/// `cid:` references (images sent inside the message) point to valet.
fn cid_links(html: &str, id: i64) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(i) = rest.to_ascii_lowercase().find("cid:") {
        let before = rest[..i].chars().last();
        out.push_str(&rest[..i]);
        if matches!(before, Some('"' | '\'' | '=' | '(')) {
            out.push_str(&format!("/msg/{id}/cid/"));
        } else {
            out.push_str(&rest[i..i + 4]);
        }
        rest = &rest[i + 4..];
    }
    out.push_str(rest);
    out
}

fn part_response(p: &mail_parser::MessagePart, download: bool) -> Response {
    use mail_parser::MimeHeaders;
    let ctype = p
        .content_type()
        .map(|c| format!("{}/{}", c.ctype(), c.subtype().unwrap_or("octet-stream")))
        .unwrap_or_else(|| "application/octet-stream".into());
    let name = p.attachment_name().unwrap_or("attachment").replace(['"', '\\', '\r', '\n'], "_");
    let mut b = Response::builder()
        .header(header::CONTENT_TYPE, if download { "application/octet-stream".to_string() } else { ctype })
        .header("X-Content-Type-Options", "nosniff")
        .header("Content-Security-Policy", "sandbox; default-src 'none'");
    if download {
        let enc = percent_encoding::utf8_percent_encode(&name, percent_encoding::NON_ALPHANUMERIC);
        b = b.header(header::CONTENT_DISPOSITION, format!("attachment; filename=\"{name}\"; filename*=UTF-8''{enc}"));
    }
    b.body(Body::from(p.contents().to_vec())).unwrap()
}

pub async fn attachment(State(app): St, headers: HeaderMap, Path((id, n)): Path<(i64, usize)>) -> Response {
    guard!(app, headers);
    let Some((_, src, _)) = locate(&app, id) else { return StatusCode::NOT_FOUND.into_response() };
    let raw = app.mail.blob(&src.blob).unwrap_or_default();
    let Some(m) = parse(&raw) else { return StatusCode::NOT_FOUND.into_response() };
    match m.attachments().nth(n) {
        Some(p) => part_response(p, true),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

pub async fn cid(State(app): St, headers: HeaderMap, Path((id, cid)): Path<(i64, String)>) -> Response {
    guard!(app, headers);
    use mail_parser::MimeHeaders;
    let Some((_, src, _)) = locate(&app, id) else { return StatusCode::NOT_FOUND.into_response() };
    let raw = app.mail.blob(&src.blob).unwrap_or_default();
    let Some(m) = parse(&raw) else { return StatusCode::NOT_FOUND.into_response() };
    let want = cid.trim_matches(['<', '>']);
    let found = m.parts.iter().find(|p| p.content_id().is_some_and(|c| c.trim_matches(['<', '>']).eq_ignore_ascii_case(want)));
    match found {
        Some(p) if is_inline_image(p) => part_response(p, false),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

#[derive(Deserialize)]
pub struct ActionForm {
    action: String,
    #[serde(default)]
    back: Option<i64>,
    #[serde(default)]
    to: Option<String>,
}

pub async fn action(State(app): St, headers: HeaderMap, Path(id): Path<i64>, axum::extract::Form(f): axum::extract::Form<ActionForm>) -> Response {
    guard!(app, headers);
    let Some((row, src, folder)) = locate(&app, id) else { return StatusCode::NOT_FOUND.into_response() };
    let list = format!("/box/{}", f.back.unwrap_or(row.mailbox));
    let here = format!("/msg/{id}");
    let flag = |mode: StoreMode, f: &str| app.mail.store(&[src.id], &mode, &[f.to_string()]).map(|_| ());
    let done = match f.action.as_str() {
        "seen" => flag(StoreMode::Add, "\\Seen").map(|_| (here.clone(), None)),
        "unseen" => flag(StoreMode::Remove, "\\Seen").map(|_| (list.clone(), Some("Marked unread."))),
        "flag" => flag(StoreMode::Add, "\\Flagged").map(|_| (here.clone(), None)),
        "unflag" => flag(StoreMode::Remove, "\\Flagged").map(|_| (here.clone(), None)),
        "move" => {
            let target = f.to.as_deref().and_then(|t| t.parse::<i64>().ok()).and_then(|t| app.mail.mailbox(t));
            match target {
                Some(t) if t.account == folder.account => {
                    app.mail.copy(&[src.id], t.id, true).map(|_| (list.clone(), Some("Moved.")))
                }
                _ => return back(&here, None, Some("Pick a folder of the same account.")),
            }
        }
        "delete" => {
            // To the account's Trash, like mail apps do; from the Trash itself, for good.
            let trash = app.mail.mailboxes().into_iter().find(|b| b.account == folder.account && b.special.eq_ignore_ascii_case("\\Trash"));
            match trash {
                Some(t) if t.id != folder.id => app.mail.copy(&[src.id], t.id, true).map(|_| (list.clone(), Some("Moved to the Trash."))),
                _ => app.mail.expunge(&[src.id]).map(|_| (list.clone(), Some("Deleted."))),
            }
        }
        _ => return back(&here, None, Some("Unknown action.")),
    };
    if let Some(acc) = &folder.account {
        app.mail.waker(acc).notify_one();
    }
    match done {
        Ok((to, ok)) => back(&to, ok, None),
        Err(e) => back(&here, None, Some(&format!("{e:#}"))),
    }
}

// Folders

pub async fn folders_page(State(app): St, headers: HeaderMap, Path(account): Path<String>, Query(q): Query<ListQuery>) -> Response {
    guard!(app, headers);
    let Some(acc) = app.account(&account) else { return StatusCode::NOT_FOUND.into_response() };
    let (_, per) = folders(&app);
    let boxes = per.into_iter().find(|(a, _)| a.id == acc.id).map(|(_, b)| b).unwrap_or_default();
    let inbox = app.mail.all_inboxes();
    mail_page(&format!("{} folders", acc.name), inbox, &app, html! {
        (flash_of(&q))
        h1 { (acc.name) " folders" }
        div.card {
            @for b in &boxes {
                @let fixed = b.special.is_empty() && !b.remote.as_deref().is_some_and(|r| r.eq_ignore_ascii_case("INBOX"));
                div style="display:flex; gap:8px; align-items:center; flex-wrap:wrap; padding:6px 0; border-bottom:1px solid var(--line)" {
                    @if fixed {
                        form.row method="post" action={"/folders/" (acc.id) "/" (b.id) "/rename"} style="flex:1" {
                            input name="name" value=(b.name.rsplit('/').next().unwrap_or(&b.name)) aria-label="Name" style="flex:1; min-width:140px";
                            button type="submit" { "Rename" }
                        }
                        form method="post" action={"/folders/" (acc.id) "/" (b.id) "/delete"}
                            onsubmit="return confirm('Delete this folder and the mail in it, at the provider too?')" {
                            button.plain type="submit" { "Delete" }
                        }
                        span.muted.small style="flex-basis:100%" { (short_name(b)) " · " (app.mail.count(b.id).0) " messages" }
                    } @else {
                        span style="flex:1" { (short_name(b)) }
                        span.muted.small { "the provider's own folder" }
                    }
                }
            }
        }
        h2 { "New folder" }
        form.card.row method="post" action={"/folders/" (acc.id)} {
            label { "Name" input name="name" required; }
            label { "Inside" select name="parent" {
                option value="" { "(top level)" }
                @for b in &boxes { option value=(b.id) { (short_name(b)) } }
            } }
            button type="submit" { "Create" }
        }
        p.muted.small { "Folders are made, renamed and deleted at the provider too, so they match in every app. Mail apps can do the same." }
    })
}

#[derive(Deserialize)]
pub struct FolderForm {
    #[serde(default)]
    name: String,
    #[serde(default)]
    parent: String,
}

pub async fn create_folder(State(app): St, headers: HeaderMap, Path(account): Path<String>, axum::extract::Form(f): axum::extract::Form<FolderForm>) -> Response {
    guard!(app, headers);
    let here = format!("/folders/{account}");
    match crate::folders::create(&app, &account, f.parent.parse().ok(), &f.name).await {
        Ok(_) => back(&here, Some(&format!("Created {}.", f.name.trim())), None),
        Err(e) => back(&here, None, Some(&format!("{e:#}"))),
    }
}

pub async fn rename_folder(State(app): St, headers: HeaderMap, Path((account, id)): Path<(String, i64)>, axum::extract::Form(f): axum::extract::Form<FolderForm>) -> Response {
    guard!(app, headers);
    let here = format!("/folders/{account}");
    match crate::folders::rename(&app, id, &f.name).await {
        Ok(()) => back(&here, Some("Renamed."), None),
        Err(e) => back(&here, None, Some(&format!("{e:#}"))),
    }
}

pub async fn delete_folder(State(app): St, headers: HeaderMap, Path((account, id)): Path<(String, i64)>) -> Response {
    guard!(app, headers);
    let here = format!("/folders/{account}");
    match crate::folders::delete(&app, id).await {
        Ok(()) => back(&here, Some("Deleted."), None),
        Err(e) => back(&here, None, Some(&format!("{e:#}"))),
    }
}

// Writing

#[derive(Deserialize, Default)]
pub struct ComposeQuery {
    reply: Option<i64>,
    all: Option<i64>,
    forward: Option<i64>,
    draft: Option<i64>,
}

/// What the form holds: a new message, a reply, a forward, or a draft.
#[derive(Default)]
struct Draft {
    from: String,
    to: String,
    cc: String,
    bcc: String,
    subject: String,
    /// The text, as HTML (sanitized when it comes from the form).
    html: String,
    in_reply_to: String,
    references: String,
    /// The message whose attachments go along (the forwarded one, or the
    /// draft last saved), minus those in `drop`.
    carry: Option<i64>,
    drop: Vec<usize>,
    /// The saved draft this replaces.
    draft_id: Option<i64>,
    replying: Option<i64>,
}

fn prefixed(prefix: &str, subject: &str) -> String {
    let s = subject.trim();
    if s.to_lowercase().starts_with(&prefix.to_lowercase()) { s.to_string() } else { format!("{prefix} {s}") }
}

/// What a message reads as, in HTML fit to quote or edit: its HTML part
/// cleaned up, or its text.
fn quotable_html(m: &mail_parser::Message) -> String {
    let real_html = m.html_body.first().and_then(|&i| m.part(i)).is_some_and(|p| p.is_text_html());
    match (real_html, m.body_html(0)) {
        (true, Some(h)) => clean(&h),
        _ => mail_parser::decoders::html::text_to_html(&m.body_text(0).unwrap_or_default()),
    }
}

/// HTML written in the editor (or pasted into it), reduced to what mail needs.
fn clean(html: &str) -> String {
    ammonia::Builder::default()
        .add_tags(["font", "center"])
        .add_generic_attributes(["align"])
        .url_schemes(["http", "https", "mailto", "tel"].into_iter().collect())
        .clean(html)
        .to_string()
}

fn escape(s: &str) -> String {
    maud::html! { (s) }.into_string()
}

/// The address of mine the message was sent to, for answering from it.
fn my_address(app: &App, m: &mail_parser::Message, account: Option<&str>) -> Option<String> {
    let accounts = app.accounts();
    let mut to: Vec<String> = addresses_of(m.to());
    to.extend(addresses_of(m.cc()));
    to.extend(m.header_values("Delivered-To").filter_map(|v| v.as_text().map(str::to_string)));
    for addr in &to {
        if accounts.iter().any(|a| a.sends_as(addr)) {
            return Some(addr.to_lowercase());
        }
    }
    account.and_then(|id| accounts.iter().find(|a| a.id == id)).map(|a| a.address.clone())
}

fn is_draft(m: &Msg, folder: &Mailbox) -> bool {
    folder.special.eq_ignore_ascii_case("\\Drafts") || has_flag(m, "\\Draft")
}

fn header_ids(v: &mail_parser::HeaderValue) -> Vec<String> {
    match v {
        mail_parser::HeaderValue::Text(t) => vec![format!("<{t}>")],
        mail_parser::HeaderValue::TextList(l) => l.iter().map(|t| format!("<{t}>")).collect(),
        _ => vec![],
    }
}

fn draft_for(app: &App, q: &ComposeQuery) -> Draft {
    let (id, mode) = match (q.reply, q.all, q.forward, q.draft) {
        (Some(id), ..) => (id, "reply"),
        (_, Some(id), ..) => (id, "all"),
        (_, _, Some(id), _) => (id, "forward"),
        (.., Some(id)) => (id, "draft"),
        _ => return Draft { html: "<p><br></p>".into(), ..Default::default() },
    };
    let Some((_, src, folder)) = locate(app, id) else { return Draft::default() };
    let raw = app.mail.blob(&src.blob).unwrap_or_default();
    let Some(m) = parse(&raw) else { return Draft::default() };
    let subject = m.subject().unwrap_or("");
    if mode == "draft" {
        return Draft {
            from: addresses_of(m.from()).into_iter().next().unwrap_or_default(),
            to: everyone(m.to()),
            cc: everyone(m.cc()),
            bcc: everyone(m.bcc()),
            subject: subject.to_string(),
            html: quotable_html(&m),
            in_reply_to: header_ids(m.in_reply_to()).join(" "),
            references: header_ids(m.references()).join(" "),
            carry: (m.attachment_count() > 0).then_some(id),
            draft_id: is_draft(&src, &folder).then_some(id),
            ..Default::default()
        };
    }
    let from = my_address(app, &m, folder.account.as_deref()).unwrap_or_default();
    let when = m.date().map(|d| d.to_rfc822()).unwrap_or_default();
    let mut d = Draft { from: from.clone(), ..Default::default() };
    if mode == "forward" {
        d.subject = prefixed("Fwd:", subject);
        d.html = format!(
            "<p><br></p><p>---------- Forwarded message ----------<br>From: {}<br>Date: {}<br>Subject: {}<br>To: {}</p>{}",
            escape(&everyone(m.from())),
            escape(&when),
            escape(subject),
            escape(&everyone(m.to())),
            quotable_html(&m)
        );
        d.carry = (m.attachment_count() > 0).then_some(id);
        return d;
    }
    d.to = if m.reply_to().is_some() { everyone(m.reply_to()) } else { everyone(m.from()) };
    if mode == "all" {
        let mine = |a: &str| a.eq_ignore_ascii_case(&from) || app.accounts().iter().any(|x| x.sends_as(a));
        let mut others: Vec<String> = Vec::new();
        for x in [m.to(), m.cc()].into_iter().flatten().flat_map(|l| l.iter()) {
            let Some(addr) = x.address() else { continue };
            if mine(addr) || d.to.to_lowercase().contains(&addr.to_lowercase()) {
                continue;
            }
            others.push(match x.name().filter(|n| !n.trim().is_empty()) {
                Some(n) => format!("{n} <{addr}>"),
                None => addr.to_string(),
            });
        }
        d.cc = others.join(", ");
    }
    d.subject = prefixed("Re:", subject);
    d.html = format!(
        "<p><br></p><p>On {}, {} wrote:</p><blockquote>{}</blockquote>",
        escape(&when),
        escape(&who(m.from())),
        quotable_html(&m)
    );
    if let Some(mid) = m.message_id() {
        d.in_reply_to = format!("<{mid}>");
        d.references = header_ids(m.references()).into_iter().chain(std::iter::once(d.in_reply_to.clone())).collect::<Vec<_>>().join(" ");
    }
    d.replying = Some(id);
    d
}

/// The names of the attachments a draft carries along.
fn carried(app: &App, d: &Draft) -> Vec<(usize, String)> {
    use mail_parser::MimeHeaders;
    let Some((_, src, _)) = d.carry.and_then(|c| locate(app, c)) else { return vec![] };
    let raw = app.mail.blob(&src.blob).unwrap_or_default();
    let Some(m) = parse(&raw) else { return vec![] };
    m.attachments().enumerate().filter(|(i, _)| !d.drop.contains(i)).map(|(i, a)| (i, a.attachment_name().unwrap_or("attachment").to_string())).collect()
}

const EDITOR_CSS: &str = r#"
.editor-bar { display:flex; gap:4px; flex-wrap:wrap; border:1px solid var(--line); border-bottom:none; border-radius:6px 6px 0 0; padding:4px; background:var(--bg); }
.editor-bar button { background:none; color:var(--fg); border:1px solid transparent; border-radius:4px; padding:3px 9px; min-width:32px; }
.editor-bar button:hover { background:var(--line); }
.editor { min-height:320px; padding:10px 12px; border:1px solid var(--line); border-radius:0 0 6px 6px; background:var(--bg); color:var(--fg); overflow-wrap:anywhere; }
.editor:focus { outline:2px solid var(--accent); outline-offset:-1px; }
.editor blockquote { margin:0 0 0 4px; padding-left:10px; border-left:3px solid var(--line); color:var(--muted); }
.editor img { max-width:100%; }
.write-actions { display:flex; gap:10px; align-items:center; flex-wrap:wrap; }
.write-actions .status { color:var(--muted); font-size:13px; }
.carried label { display:inline-flex; flex-direction:row; align-items:center; gap:4px; margin-right:12px; color:var(--fg); }
"#;

const EDITOR_SCRIPT: &str = r#"
(() => {
  const form = document.querySelector('form.write'), ed = form.querySelector('.editor');
  const status = form.querySelector('.status');
  let dirty = false, saving = null, sending = false;
  for (const b of form.querySelectorAll('.editor-bar button')) {
    b.addEventListener('mousedown', e => e.preventDefault());
    b.addEventListener('click', () => {
      const cmd = b.dataset.cmd;
      if (cmd === 'createLink') {
        const url = prompt('Link to');
        if (url) document.execCommand('createLink', false, /^[a-z]+:/i.test(url) ? url : 'https://' + url);
      } else if (cmd === 'formatBlock') {
        document.execCommand('formatBlock', false, b.dataset.arg);
      } else {
        document.execCommand(cmd, false, null);
      }
      ed.focus(); dirty = true;
    });
  }
  const sync = () => { form.elements.html.value = ed.innerHTML; };
  form.addEventListener('input', () => { dirty = true; });
  form.addEventListener('change', () => { dirty = true; });
  const carried = form.querySelector('.carried');
  const show = names => {
    carried.innerHTML = '';
    names.forEach(([i, n]) => {
      const l = document.createElement('label');
      const c = document.createElement('input');
      c.type = 'checkbox'; c.name = 'drop'; c.value = i; c.title = 'Leave out';
      l.append('📎 ' + n + ' ', c, ' leave out');
      carried.append(l);
    });
  };
  async function save() {
    if (!dirty || saving || sending) return saving;
    dirty = false; sync();
    const data = new FormData(form);
    const hadFiles = data.getAll('files').some(f => f.size);
    saving = (async () => {
      try {
        const r = await fetch('/compose/draft', { method: 'POST', body: data });
        const j = await r.json();
        if (!r.ok) throw new Error(j.error || 'not saved');
        form.elements.draft_id.value = j.draft;
        form.elements.carry.value = j.draft;
        if (hadFiles) form.elements.files.value = '';
        show(j.attachments);
        status.textContent = 'Draft saved ' + new Date().toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
      } catch (e) {
        dirty = true;
        status.textContent = 'Draft not saved: ' + e.message;
      } finally {
        saving = null;
      }
    })();
    return saving;
  }
  setInterval(save, 8000);
  form.querySelector('[data-save]').addEventListener('click', () => { dirty = true; save(); });
  form.addEventListener('submit', async e => {
    sync();
    if (e.submitter && e.submitter.name === 'discard') { sending = true; return; }
    sending = true;
    if (saving) { e.preventDefault(); await saving; form.submit(); }
  });
  window.addEventListener('pagehide', () => {
    if (dirty && !sending) { sync(); navigator.sendBeacon('/compose/draft', new FormData(form)); }
  });
})();
"#;

fn compose_view(app: &App, d: &Draft, err: Option<&str>) -> Response {
    let accounts = app.accounts();
    let senders: Vec<(String, String)> = accounts
        .iter()
        .flat_map(|a| std::iter::once(&a.address).chain(&a.aliases).map(move |addr| (addr.clone(), a.name.clone())))
        .collect();
    let inbox = app.mail.all_inboxes();
    let tool = |cmd: &str, arg: &str, label: Markup, title: &str| {
        html! { button type="button" data-cmd=(cmd) data-arg=(arg) title=(title) { (label) } }
    };
    mail_page("Write", inbox, app, html! {
        style { (PreEscaped(EDITOR_CSS)) }
        @if let Some(e) = err { p.err { (e) } }
        @if senders.is_empty() {
            p.muted { "Add a mail account first, on the " a href="/mail" { "Accounts" } " page." }
        } @else {
            form.card.write method="post" action="/compose" enctype="multipart/form-data" {
                label { "From" select name="from" {
                    @for (addr, name) in &senders { option value=(addr) selected[addr.eq_ignore_ascii_case(&d.from)] { (addr) " (" (name) ")" } }
                } }
                label { "To" input name="to" value=(d.to) placeholder="name@example.com, …" autofocus[d.to.is_empty()]; }
                label { "Cc" input name="cc" value=(d.cc); }
                details open[!d.bcc.is_empty()] { summary.small { "Bcc" } label { input name="bcc" value=(d.bcc); } }
                label { "Subject" input name="subject" value=(d.subject); }
                div {
                    div.editor-bar {
                        (tool("bold", "", html! { b { "B" } }, "Bold"))
                        (tool("italic", "", html! { i { "I" } }, "Italic"))
                        (tool("underline", "", html! { u { "U" } }, "Underline"))
                        (tool("strikeThrough", "", html! { s { "S" } }, "Strikethrough"))
                        (tool("createLink", "", html! { "Link" }, "Link"))
                        (tool("insertUnorderedList", "", html! { "• List" }, "Bulleted list"))
                        (tool("insertOrderedList", "", html! { "1. List" }, "Numbered list"))
                        (tool("formatBlock", "blockquote", html! { "Quote" }, "Quote"))
                        (tool("formatBlock", "h3", html! { "Heading" }, "Heading"))
                        (tool("formatBlock", "p", html! { "Text" }, "Normal text"))
                        (tool("removeFormat", "", html! { "Clear" }, "Clear formatting"))
                    }
                    div.editor contenteditable="true" autofocus[!d.to.is_empty()] { (PreEscaped(&d.html)) }
                }
                div.carried.small {
                    @for (i, name) in carried(app, d) {
                        label { "📎 " (name) " " input type="checkbox" name="drop" value=(i) title="Leave out"; " leave out" }
                    }
                }
                label { "Attach files" input type="file" name="files" multiple; }
                input type="hidden" name="html";
                input type="hidden" name="in_reply_to" value=(d.in_reply_to);
                input type="hidden" name="references" value=(d.references);
                input type="hidden" name="carry" value=(d.carry.map(|c| c.to_string()).unwrap_or_default());
                input type="hidden" name="draft_id" value=(d.draft_id.map(|c| c.to_string()).unwrap_or_default());
                input type="hidden" name="replying" value=(d.replying.map(|c| c.to_string()).unwrap_or_default());
                div.write-actions {
                    button type="submit" name="send" value="1" { "Send" }
                    button.plain type="button" data-save style="color:var(--fg)" { "Save draft" }
                    button.plain type="submit" name="discard" value="1" formnovalidate
                        onclick="return confirm('Discard this message?')" { "Discard" }
                    span.status { @if d.draft_id.is_some() { "Draft" } }
                }
            }
            script { (PreEscaped(EDITOR_SCRIPT)) }
        }
    })
}

pub async fn compose(State(app): St, headers: HeaderMap, Query(q): Query<ComposeQuery>) -> Response {
    guard!(app, headers);
    compose_view(&app, &draft_for(&app, &q), None)
}

/// Whether the provider puts mail sent through it in Sent by itself (then
/// valet doesn't, or it would be there twice).
fn saves_sent(acc: &Account) -> bool {
    let smtp = acc.smtp.to_ascii_lowercase();
    smtp.contains("gmail.com") || smtp.contains("office365.com") || smtp.contains("outlook.com")
}

type Files = Vec<(String, String, Vec<u8>)>;

/// The compose form, and the buttons pressed.
async fn read_form(mut form: Multipart) -> (Draft, Files, std::collections::HashMap<String, String>) {
    let mut fields: std::collections::HashMap<String, String> = Default::default();
    let mut drop = Vec::new();
    let mut files: Files = Vec::new();
    while let Ok(Some(field)) = form.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        if name == "files" {
            let filename = field.file_name().unwrap_or("").to_string();
            let ctype = field.content_type().unwrap_or("application/octet-stream").to_string();
            let Ok(data) = field.bytes().await else { continue };
            if !filename.is_empty() && !data.is_empty() {
                files.push((filename, ctype, data.to_vec()));
            }
        } else if let Ok(v) = field.text().await {
            if name == "drop" {
                drop.extend(v.parse::<usize>().ok());
            } else {
                fields.insert(name, v);
            }
        }
    }
    let get = |k: &str| fields.get(k).cloned().unwrap_or_default();
    let draft = Draft {
        from: get("from"),
        to: get("to"),
        cc: get("cc"),
        bcc: get("bcc"),
        subject: get("subject"),
        html: clean(&get("html")),
        in_reply_to: get("in_reply_to"),
        references: get("references"),
        carry: get("carry").parse().ok(),
        drop,
        draft_id: get("draft_id").parse().ok(),
        replying: get("replying").parse().ok(),
    };
    (draft, files, fields)
}

pub async fn send(State(app): St, headers: HeaderMap, form: Multipart) -> Response {
    guard!(app, headers);
    let (draft, files, fields) = read_form(form).await;
    let inbox = format!("/box/{}", app.mail.all_inboxes());
    if fields.contains_key("discard") {
        if let Some(d) = draft.draft_id {
            let _ = forget_draft(&app, d);
        }
        return back(&inbox, Some("Discarded."), None);
    }
    match deliver(&app, &draft, files).await {
        Ok(name) => {
            if let Some(r) = draft.replying.and_then(|r| locate(&app, r)) {
                let _ = app.mail.store(&[r.1.id], &StoreMode::Add, &["\\Answered".to_string()]);
            }
            if let Some(d) = draft.draft_id {
                let _ = forget_draft(&app, d);
            }
            back(&inbox, Some(&format!("Sent from {name}.")), None)
        }
        Err(e) => compose_view(&app, &draft, Some(&format!("Not sent: {e:#}"))),
    }
}

fn forget_draft(app: &App, id: i64) -> anyhow::Result<()> {
    let Some((row, src, folder)) = locate(app, id) else { return Ok(()) };
    if is_draft(&src, &folder) {
        app.mail.expunge(&[row.id])?;
    }
    Ok(())
}

/// Saves the form as a draft in the From account's Drafts folder, replacing
/// the one saved before. Answers the new draft's id and its attachments.
pub async fn save_draft(State(app): St, headers: HeaderMap, form: Multipart) -> Response {
    if !super::authed(&app, &headers) {
        return (StatusCode::UNAUTHORIZED, axum::Json(serde_json::json!({ "error": "logged out" }))).into_response();
    }
    let (draft, files, _) = read_form(form).await;
    let saved = async {
        use anyhow::Context;
        let acc = app.accounts().into_iter().find(|a| a.sends_as(&draft.from)).context("pick an address to send from")?;
        let message = build(&app, &draft, files, true)?;
        let folder = match app.mail.mailboxes().into_iter().find(|b| b.account.as_deref() == Some(acc.id.as_str()) && b.special.eq_ignore_ascii_case("\\Drafts")) {
            Some(b) => b.id,
            None => match app.mail.mailboxes().into_iter().find(|b| b.account.as_deref() == Some(acc.id.as_str()) && short_name(b) == "Drafts") {
                Some(b) => b.id,
                None => crate::folders::create(&app, &acc.id, None, "Drafts").await?,
            },
        };
        let id = app.mail.append_row(folder, &message.formatted(), &["\\Draft".into(), "\\Seen".into()], crate::mailstore::now())?;
        if let Some(old) = draft.draft_id.filter(|&o| o != id) {
            forget_draft(&app, old)?;
        }
        app.mail.waker(&acc.id).notify_one();
        anyhow::Ok(id)
    }
    .await;
    match saved {
        Ok(id) => {
            let names: Vec<(usize, String)> = carried(&app, &Draft { carry: Some(id), ..Default::default() });
            axum::Json(serde_json::json!({ "draft": id, "attachments": names })).into_response()
        }
        Err(e) => (StatusCode::UNPROCESSABLE_ENTITY, axum::Json(serde_json::json!({ "error": format!("{e:#}") }))).into_response(),
    }
}

/// The message the form describes. A draft may have no recipient yet and
/// keeps its Bcc.
fn build(app: &App, d: &Draft, mut files: Files, draft: bool) -> anyhow::Result<lettre::Message> {
    use anyhow::Context;
    use lettre::message::header::ContentType;
    use lettre::message::{Attachment, Mailboxes, MultiPart};
    let parse_list = |s: &str| -> anyhow::Result<Mailboxes> {
        let s = s.trim().trim_end_matches(',');
        if s.is_empty() { Ok(Mailboxes::new()) } else { s.parse::<Mailboxes>().map_err(|e| anyhow::anyhow!("{s}: {e}")) }
    };
    let (to, cc, bcc) = (parse_list(&d.to)?, parse_list(&d.cc)?, parse_list(&d.bcc)?);
    let from: lettre::message::Mailbox = d.from.parse().context("the From address")?;
    let mut b = lettre::Message::builder().from(from.clone()).subject(d.subject.clone()).date_now();
    for m in to.iter() {
        b = b.to(m.clone());
    }
    for m in cc.iter() {
        b = b.cc(m.clone());
    }
    for m in bcc.iter() {
        b = b.bcc(m.clone());
    }
    if draft {
        b = b.keep_bcc();
        if to.iter().chain(cc.iter()).chain(bcc.iter()).next().is_none() {
            b = b.envelope(lettre::address::Envelope::new(Some(from.email.clone()), vec![from.email.clone()])?);
        }
    } else {
        anyhow::ensure!(to.iter().chain(cc.iter()).chain(bcc.iter()).next().is_some(), "who is it for?");
    }
    if !d.in_reply_to.is_empty() {
        b = b.in_reply_to(d.in_reply_to.clone()).references(d.references.clone());
    }
    let domain = d.from.rsplit_once('@').map(|(_, h)| h.to_string()).unwrap_or_else(|| "valet".into());
    b = b.message_id(Some(format!("<{}@{domain}>", uuid::Uuid::new_v4().simple())));
    let mut all: Files = Vec::new();
    if let Some((_, src, _)) = d.carry.and_then(|c| locate(app, c)) {
        let raw = app.mail.blob(&src.blob).unwrap_or_default();
        if let Some(m) = parse(&raw) {
            use mail_parser::MimeHeaders;
            for (i, a) in m.attachments().enumerate() {
                if d.drop.contains(&i) {
                    continue;
                }
                let ct = a.content_type().map(|c| format!("{}/{}", c.ctype(), c.subtype().unwrap_or("octet-stream"))).unwrap_or_default();
                all.push((a.attachment_name().unwrap_or("attachment").to_string(), ct, a.contents().to_vec()));
            }
        }
    }
    all.append(&mut files);
    let text = mail_parser::decoders::html::html_to_text(&d.html);
    let body = MultiPart::alternative_plain_html(text.trim_end().to_string(), format!("<!doctype html><html><body>{}</body></html>", d.html));
    let message = if all.is_empty() {
        b.multipart(body)?
    } else {
        let mut mp = MultiPart::mixed().multipart(body);
        for (name, ct, data) in all {
            let ct = ContentType::parse(&ct).unwrap_or(ContentType::parse("application/octet-stream").unwrap());
            mp = mp.singlepart(Attachment::new(name).body(data, ct));
        }
        b.multipart(mp)?
    };
    Ok(message)
}

/// Sends through the account the message is from, and keeps a copy in that
/// account's Sent folder. Returns the account's name.
async fn deliver(app: &App, d: &Draft, files: Files) -> anyhow::Result<String> {
    use anyhow::Context;
    let acc = app.accounts().into_iter().find(|a| a.sends_as(&d.from)).context("pick an address to send from")?;
    let message = build(app, d, files, false)?;
    let recipients: Vec<String> = message.envelope().to().iter().map(|a| a.to_string()).collect();
    let raw = crate::smtpd::strip_bcc(&message.formatted());
    crate::accounts::send(&acc, &app.mail, &d.from, &recipients, &raw).await?;
    if !saves_sent(&acc) {
        let sent = app.mail.mailboxes().into_iter().find(|b| b.account.as_deref() == Some(acc.id.as_str()) && b.special.eq_ignore_ascii_case("\\Sent"));
        if let Some(sent) = sent {
            app.mail.append(sent.id, &raw, &["\\Seen".to_string()], crate::mailstore::now())?;
            app.mail.waker(&acc.id).notify_one();
        }
    }
    Ok(acc.name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(cid_links(r#"<img src="cid:abc@x"> cid: text"#, 7), r#"<img src="/msg/7/cid/abc@x"> cid: text"#);
        assert!(has_remote(r#"<img SRC="https://t.example/p.gif">"#) && !has_remote("<p>http://x</p>"));
        assert_eq!(prefixed("Re:", "re: hi"), "re: hi");
        assert_eq!(prefixed("Re:", "hi"), "Re: hi");
        assert_eq!(human_size(2048), "2 KB");
    }

    #[test]
    fn reads_names_and_search() {
        let raw = b"From: \"Ann B\" <ann@x.fr>\r\nTo: me@y.fr, Bob <bob@z.fr>\r\nSubject: =?utf-8?q?R=C3=A9union?=\r\n\r\nOrdre du jour\r\n";
        let m = parse(raw).unwrap();
        assert_eq!(who(m.from()), "Ann B");
        assert_eq!(everyone(m.to()), "me@y.fr, Bob <bob@z.fr>");
        assert!(matches(raw, &["réunion".into(), "jour".into()]));
        assert!(!matches(raw, &["absent".into()]));
    }
}
