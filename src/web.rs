//! The web UI: alert patterns, calendars and their defaults, per-event
//! choices, .ics import, and how to connect each app.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::extract::{Form, Multipart, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use maud::{DOCTYPE, Markup, PreEscaped, html};
use serde::Deserialize;

use crate::alarms::{self, Pattern, describe_all_day, describe_offset};
use crate::import::{self, Report};
use crate::store::{Calendar, Object};
use crate::{App, ical};

type St = State<Arc<App>>;

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/", get(home).fallback(crate::fallback))
        .route("/login", get(login_page).post(login))
        .route("/logout", post(logout))
        .route("/patterns", get(patterns_page).post(create_pattern))
        .route("/patterns/{id}", get(edit_pattern_page).post(save_pattern))
        .route("/patterns/{id}/delete", post(delete_pattern))
        .route("/calendars", get(calendars_page).post(create_calendar))
        .route("/calendars/{id}", post(save_calendar))
        .route("/calendars/{id}/delete", post(delete_calendar))
        .route("/events", get(events_page))
        .route("/events/choice", post(set_choice))
        .route("/import", get(import_page).post(import_upload))
        .route("/connect", get(connect_page))
}

const COOKIE: &str = "valet_session";

fn authed(app: &App, headers: &HeaderMap) -> bool {
    let want = app.config.session();
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .any(|(k, v)| k == COOKIE && bool::from(subtle::ConstantTimeEq::ct_eq(v.as_bytes(), want.as_bytes())))
}

macro_rules! guard {
    ($app:expr, $headers:expr) => {
        if !authed(&$app, &$headers) {
            return Redirect::to("/login").into_response();
        }
    };
}

const CSS: &str = r#"
:root { --bg:#fafaf9; --fg:#1c1917; --muted:#78716c; --line:#e7e5e4; --card:#fff; --accent:#2563eb; --bad:#b91c1c; --good:#15803d; }
@media (prefers-color-scheme: dark) { :root { --bg:#1c1917; --fg:#f5f5f4; --muted:#a8a29e; --line:#44403c; --card:#292524; --accent:#60a5fa; --bad:#f87171; --good:#4ade80; } }
* { box-sizing: border-box; }
body { margin:0; background:var(--bg); color:var(--fg); font:15px/1.5 system-ui, sans-serif; }
header { border-bottom:1px solid var(--line); background:var(--card); }
nav { max-width:960px; margin:0 auto; padding:10px 16px; display:flex; gap:16px; flex-wrap:wrap; align-items:center; }
nav a { color:var(--fg); text-decoration:none; } nav a.on { color:var(--accent); font-weight:600; }
nav .brand { font-weight:700; margin-right:8px; } nav form { margin-left:auto; }
main { max-width:960px; margin:0 auto; padding:16px; }
h1 { font-size:22px; margin:8px 0 16px; } h2 { font-size:17px; margin:24px 0 8px; }
.card { background:var(--card); border:1px solid var(--line); border-radius:8px; padding:12px 16px; margin:12px 0; }
.muted { color:var(--muted); } .small { font-size:13px; }
.ok { color:var(--good); } .err { color:var(--bad); }
table { width:100%; border-collapse:collapse; } td, th { text-align:left; padding:6px 8px; border-bottom:1px solid var(--line); vertical-align:top; }
input, select, button { font:inherit; padding:5px 8px; border:1px solid var(--line); border-radius:6px; background:var(--bg); color:var(--fg); max-width:100%; }
button { cursor:pointer; background:var(--accent); color:#fff; border:none; } button.plain { background:none; color:var(--bad); border:1px solid var(--line); }
form.row { display:flex; gap:8px; flex-wrap:wrap; align-items:end; } label { display:flex; flex-direction:column; gap:2px; font-size:13px; color:var(--muted); }
code { background:var(--line); padding:1px 5px; border-radius:4px; word-break:break-all; }
.scroll { overflow-x:auto; }
"#;

fn page(title: &str, on: &str, body: Markup) -> Response {
    let link = |href: &str, label: &str| {
        html! { a href=(href) class=[(on == href).then_some("on")] { (label) } }
    };
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " · valet" }
                style { (PreEscaped(CSS)) }
            }
            body {
                header { nav {
                    span.brand { "valet" }
                    (link("/", "Overview")) (link("/events", "Events")) (link("/patterns", "Patterns"))
                    (link("/calendars", "Calendars")) (link("/import", "Import")) (link("/connect", "Connect apps"))
                    form method="post" action="/logout" { button.plain type="submit" { "Log out" } }
                } }
                main { (body) }
            }
        }
    }
    .into_response()
}

#[derive(Deserialize, Default)]
struct Msg {
    ok: Option<String>,
    err: Option<String>,
}

fn flash(m: &Msg) -> Markup {
    html! {
        @if let Some(ok) = &m.ok { p.ok { (ok) } }
        @if let Some(err) = &m.err { p.err { (err) } }
    }
}

fn back(path: &str, ok: Option<&str>, err: Option<&str>) -> Response {
    let enc = |s: &str| percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string();
    let q = match (ok, err) {
        (_, Some(e)) => format!("?err={}", enc(e)),
        (Some(o), None) => format!("?ok={}", enc(o)),
        _ => String::new(),
    };
    let sep = if path.contains('?') && !q.is_empty() { "&" } else { "" };
    let q = if sep == "&" { q.replacen('?', "", 1) } else { q };
    Redirect::to(&format!("{path}{sep}{q}")).into_response()
}

fn pattern_alerts(p: &Pattern) -> Markup {
    html! {
        @if p.offsets.is_empty() { span.muted { "no alerts" } }
        @for (i, m) in p.offsets.iter().enumerate() {
            @if i > 0 { ", " }
            (describe_offset(*m))
        }
    }
}

fn pattern_select(name: &str, patterns: &[Pattern], selected: Option<i64>, none_label: &str) -> Markup {
    html! {
        select name=(name) {
            option value="" selected[selected.is_none()] { (none_label) }
            @for p in patterns {
                option value=(p.id) selected[selected == Some(p.id)] { (p.name) }
            }
        }
    }
}

// Login

#[derive(Deserialize)]
struct LoginForm {
    password: String,
}

async fn login_page(Query(m): Query<Msg>) -> Response {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { "Log in · valet" }
                style { (PreEscaped(CSS)) }
            }
            body { main style="max-width:360px; margin-top:12vh" {
                h1 { "valet" }
                (flash(&m))
                form.card method="post" action="/login" {
                    label { "Password" input type="password" name="password" autofocus required; }
                    p { button type="submit" { "Log in" } }
                }
            } }
        }
    }
    .into_response()
}

async fn login(State(app): St, Form(f): Form<LoginForm>) -> Response {
    if !app.config.check_password(&app.config.user, &f.password) {
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
        return back("/login", None, Some("Wrong password."));
    }
    let cookie = format!("{COOKIE}={}; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age=31536000", app.config.session());
    ([(header::SET_COOKIE, cookie)], Redirect::to("/")).into_response()
}

async fn logout() -> Response {
    let cookie = format!("{COOKIE}=; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age=0");
    ([(header::SET_COOKIE, cookie)], Redirect::to("/login")).into_response()
}

// Overview

async fn home(State(app): St, headers: HeaderMap) -> Response {
    guard!(app, headers);
    let patterns = app.store.patterns();
    let name = |id: Option<i64>| id.and_then(|id| patterns.iter().find(|p| p.id == id)).map(|p| p.name.clone());
    let calendars = app.store.calendars();
    page("Overview", "/", html! {
        h1 { "Overview" }
        div.card {
            p { "How valet picks an event's alerts — the first that applies wins:" }
            ol {
                li { "a " code { "#tag" } " in the event's title naming one of your " a href="/patterns" { "patterns" } ";" }
                li { "the pattern chosen for that event on the " a href="/events" { "Events" } " page;" }
                li { "alerts you set yourself in a calendar app (kept as they are);" }
                li { "the calendar's default pattern (one for timed events, one for all-day events)." }
            }
            p.muted.small { "Turn off your apps' own default alerts, or valet will see them as alerts you set yourself." }
        }
        h2 { "Calendars" }
        @if calendars.is_empty() {
            p.muted { "No calendar yet. " a href="/calendars" { "Create one" } " or " a href="/import" { "import" } " your existing ones." }
        } @else {
            div.card.scroll { table {
                tr { th { "Calendar" } th { "Events" } th { "Timed events" } th { "All-day events" } }
                @for c in &calendars {
                    tr {
                        td { span style={"color:" (c.color)} { "● " } (c.name) }
                        td { (app.store.objects(&c.id).len()) }
                        td { (name(c.default_timed).unwrap_or_else(|| "no alerts".into())) }
                        td { (name(c.default_allday).unwrap_or_else(|| "no alerts".into())) }
                    }
                }
            } }
        }
        @if patterns.is_empty() {
            p.muted { "No alert pattern yet: " a href="/patterns" { "create your first one" } "." }
        }
    })
}

// Patterns

#[derive(Deserialize)]
struct PatternForm {
    name: String,
    tag: String,
    alerts: String,
}

fn spec_of(p: &Pattern) -> String {
    p.offsets
        .iter()
        .map(|&m| {
            if m > 0 {
                let (d, rest) = (m / 1440, m % 1440);
                if d == 0 { format!("{:02}:{:02}", rest / 60, rest % 60) } else { format!("+{m}m") }
            } else if m == 0 {
                "0m".into()
            } else {
                let m = -m;
                let mut s = String::new();
                for (n, u) in [(m / 1440, "d"), (m % 1440 / 60, "h"), (m % 60, "m")] {
                    if n > 0 {
                        if !s.is_empty() {
                            s.push(' ');
                        }
                        s.push_str(&format!("{n}{u}"));
                    }
                }
                s
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn pattern_fields(p: Option<&Pattern>) -> Markup {
    html! {
        label { "Name" input name="name" required value=[p.map(|p| &p.name)] placeholder="Meeting"; }
        label { "Tag (for titles)" input name="tag" required value=[p.map(|p| &p.tag)] placeholder="meeting" pattern="[A-Za-z0-9_\\-]+"; }
        label { "Alerts" input name="alerts" value=[p.map(spec_of)] placeholder="12m, 2m" size="24"; }
    }
}

const SPEC_HELP: &str = "Alerts, separated by commas: 12m, 2h, 1d, 1w or combinations like 1d 3h are before the start; \
    +30m is after it. For all-day events (which start at midnight) use a clock time: 9:00 is 9 am that day, 1d 18:00 is 6 pm the day before. \
    Leave empty for a pattern with no alerts (e.g. #silent).";

fn check_pattern(f: &PatternForm) -> Result<(String, String, Vec<i64>), String> {
    let name = f.name.trim().to_string();
    let tag = f.tag.trim().trim_start_matches('#').to_lowercase();
    if name.is_empty() {
        return Err("A pattern needs a name.".into());
    }
    if !alarms::valid_tag(&tag) {
        return Err("A tag is one word: letters, digits, - or _.".into());
    }
    Ok((name, tag, alarms::parse_spec(&f.alerts)?))
}

async fn patterns_page(State(app): St, headers: HeaderMap, Query(m): Query<Msg>) -> Response {
    guard!(app, headers);
    let patterns = app.store.patterns();
    page("Patterns", "/patterns", html! {
        h1 { "Alert patterns" }
        (flash(&m))
        @if !patterns.is_empty() {
            div.card.scroll { table {
                tr { th { "Pattern" } th { "Tag" } th { "Alerts" } th { "On all-day events" } th {} }
                @for p in &patterns {
                    tr {
                        td { a href={"/patterns/" (p.id)} { (p.name) } }
                        td { code { "#" (p.tag) } }
                        td { (pattern_alerts(p)) }
                        td.muted.small { @for (i, m) in p.offsets.iter().enumerate() { @if i > 0 { ", " } (describe_all_day(*m)) } }
                        td { form method="post" action={"/patterns/" (p.id) "/delete"} onsubmit="return confirm('Delete this pattern?')" {
                            button.plain type="submit" { "Delete" }
                        } }
                    }
                }
            } }
        }
        h2 { "New pattern" }
        form.card.row method="post" action="/patterns" {
            (pattern_fields(None))
            button type="submit" { "Create" }
        }
        p.muted.small { (SPEC_HELP) }
    })
}

async fn create_pattern(State(app): St, headers: HeaderMap, Form(f): Form<PatternForm>) -> Response {
    guard!(app, headers);
    let (name, tag, offsets) = match check_pattern(&f) {
        Ok(v) => v,
        Err(e) => return back("/patterns", None, Some(&e)),
    };
    match app.store.save_pattern(None, &name, &tag, &offsets) {
        Ok(true) => back("/patterns", Some(&format!("Created {name}.")), None),
        Ok(false) => back("/patterns", None, Some(&format!("Another pattern already uses #{tag}."))),
        Err(e) => back("/patterns", None, Some(&e.to_string())),
    }
}

async fn edit_pattern_page(State(app): St, headers: HeaderMap, Path(id): Path<i64>, Query(m): Query<Msg>) -> Response {
    guard!(app, headers);
    let patterns = app.store.patterns();
    let Some(p) = patterns.iter().find(|p| p.id == id) else { return StatusCode::NOT_FOUND.into_response() };
    page(&p.name, "/patterns", html! {
        h1 { "Edit " (p.name) }
        (flash(&m))
        form.card.row method="post" action={"/patterns/" (p.id)} {
            (pattern_fields(Some(p)))
            button type="submit" { "Save" }
        }
        p.muted.small { (SPEC_HELP) " Saving updates the alerts of every event using this pattern, on all your devices." }
    })
}

async fn save_pattern(State(app): St, headers: HeaderMap, Path(id): Path<i64>, Form(f): Form<PatternForm>) -> Response {
    guard!(app, headers);
    let here = format!("/patterns/{id}");
    let (name, tag, offsets) = match check_pattern(&f) {
        Ok(v) => v,
        Err(e) => return back(&here, None, Some(&e)),
    };
    match app.store.save_pattern(Some(id), &name, &tag, &offsets) {
        Ok(true) => back("/patterns", Some(&format!("Saved {name}.")), None),
        Ok(false) => back(&here, None, Some(&format!("Another pattern already uses #{tag}."))),
        Err(e) => back(&here, None, Some(&e.to_string())),
    }
}

async fn delete_pattern(State(app): St, headers: HeaderMap, Path(id): Path<i64>) -> Response {
    guard!(app, headers);
    match app.store.delete_pattern(id) {
        Ok(()) => back("/patterns", Some("Deleted."), None),
        Err(e) => back("/patterns", None, Some(&e.to_string())),
    }
}

// Calendars

#[derive(Deserialize)]
struct NewCalendar {
    name: String,
}

#[derive(Deserialize)]
struct CalendarForm {
    name: String,
    color: String,
    default_timed: String,
    default_allday: String,
}

fn slug(name: &str) -> String {
    let s: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if s.is_empty() { "calendar".into() } else { s }
}

/// A new calendar named `name`, with a free id derived from it.
fn new_calendar(app: &App, name: &str) -> anyhow::Result<String> {
    let base = slug(name);
    for i in 1.. {
        let id = if i == 1 { base.clone() } else { format!("{base}-{i}") };
        if app.store.create_calendar(&id, name, None)? {
            return Ok(id);
        }
    }
    unreachable!()
}

async fn calendars_page(State(app): St, headers: HeaderMap, Query(m): Query<Msg>) -> Response {
    guard!(app, headers);
    let patterns = app.store.patterns();
    let calendars = app.store.calendars();
    page("Calendars", "/calendars", html! {
        h1 { "Calendars" }
        (flash(&m))
        @for c in &calendars {
            div.card {
                form.row method="post" action={"/calendars/" (c.id)} {
                    label { "Name" input name="name" required value=(c.name); }
                    label { "Color" input type="color" name="color" value=(c.color.get(..7).unwrap_or("#3b82f6")); }
                    label { "Timed events" (pattern_select("default_timed", &patterns, c.default_timed, "No alerts")) }
                    label { "All-day events" (pattern_select("default_allday", &patterns, c.default_allday, "No alerts")) }
                    button type="submit" { "Save" }
                }
                form method="post" action={"/calendars/" (c.id) "/delete"} style="margin-top:8px"
                    onsubmit="return confirm('Delete this calendar and all its events, on every device?')" {
                    span.muted.small { (app.store.objects(&c.id).len()) " events · " }
                    button.plain type="submit" { "Delete" }
                }
            }
        }
        h2 { "New calendar" }
        form.card.row method="post" action="/calendars" {
            label { "Name" input name="name" required placeholder="Work"; }
            button type="submit" { "Create" }
        }
        p.muted.small { "Calendars made in your apps appear here too. Defaults apply to events without a tag, a choice or alerts of their own." }
    })
}

async fn create_calendar(State(app): St, headers: HeaderMap, Form(f): Form<NewCalendar>) -> Response {
    guard!(app, headers);
    let name = f.name.trim();
    if name.is_empty() {
        return back("/calendars", None, Some("A calendar needs a name."));
    }
    match new_calendar(&app, name) {
        Ok(_) => back("/calendars", Some(&format!("Created {name}.")), None),
        Err(e) => back("/calendars", None, Some(&e.to_string())),
    }
}

fn pattern_id(s: &str) -> Option<i64> {
    s.parse().ok()
}

async fn save_calendar(State(app): St, headers: HeaderMap, Path(id): Path<String>, Form(f): Form<CalendarForm>) -> Response {
    guard!(app, headers);
    let Some(mut cal) = app.store.calendar(&id) else { return StatusCode::NOT_FOUND.into_response() };
    if !f.name.trim().is_empty() {
        cal.name = f.name.trim().to_string();
    }
    if f.color.starts_with('#') {
        cal.color = f.color.clone();
    }
    cal.default_timed = pattern_id(&f.default_timed);
    cal.default_allday = pattern_id(&f.default_allday);
    match app.store.update_calendar(&cal) {
        Ok(()) => back("/calendars", Some(&format!("Saved {}.", cal.name)), None),
        Err(e) => back("/calendars", None, Some(&e.to_string())),
    }
}

async fn delete_calendar(State(app): St, headers: HeaderMap, Path(id): Path<String>) -> Response {
    guard!(app, headers);
    match app.store.delete_calendar(&id) {
        Ok(()) => back("/calendars", Some("Deleted."), None),
        Err(e) => back("/calendars", None, Some(&e.to_string())),
    }
}

// Events

#[derive(Deserialize, Default)]
struct EventsQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    all: Option<String>,
    ok: Option<String>,
    err: Option<String>,
}

#[derive(Deserialize)]
struct ChoiceForm {
    calendar: String,
    uid: String,
    pattern: String,
    back: String,
}

/// Today as `YYYYMMDD` (UTC).
fn today() -> String {
    let days = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() / 86400).unwrap_or(0) as i64;
    // Howard Hinnant's civil_from_days.
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}{m:02}{d:02}")
}

/// `2026-10-23 10:00` (or `2026-10-23, all day`) from a DTSTART value.
fn when(dtstart: &str) -> String {
    let b = dtstart.as_bytes();
    if b.len() < 8 || !b[..8].iter().all(u8::is_ascii_digit) {
        return dtstart.to_string();
    }
    let date = format!("{}-{}-{}", &dtstart[..4], &dtstart[4..6], &dtstart[6..8]);
    if dtstart.len() >= 13 && b[8] == b'T' {
        let utc = if dtstart.ends_with('Z') { " UTC" } else { "" };
        format!("{date} {}:{}{utc}", &dtstart[9..11], &dtstart[11..13])
    } else {
        format!("{date}, all day")
    }
}

/// The alerts an event has right now, described.
fn current_alerts(o: &Object) -> String {
    let Ok(cals) = ical::parse(&o.data) else { return String::new() };
    let Some(ev) = cals.first().and_then(|c| c.children.iter().find(|e| e.name == "VEVENT" && e.prop("RECURRENCE-ID").is_none()))
    else {
        return String::new();
    };
    let list: Vec<String> = ev
        .children
        .iter()
        .filter(|a| a.name == "VALARM")
        .map(|a| match a.value("TRIGGER").and_then(ical::parse_duration) {
            Some(s) => describe_offset(s / 60),
            None => "at a fixed time".into(),
        })
        .collect();
    if list.is_empty() { "none".into() } else { list.join(", ") }
}

async fn events_page(State(app): St, headers: HeaderMap, Query(q): Query<EventsQuery>) -> Response {
    guard!(app, headers);
    let patterns = app.store.patterns();
    let calendars = app.store.calendars();
    let today = today();
    let needle = q.q.trim().to_lowercase();
    let all = q.all.is_some();
    let events: Vec<Object> = app
        .store
        .all_objects()
        .into_iter()
        .filter(|o| all || o.recurring || o.dtstart.get(..8).is_some_and(|d| d >= today.as_str()))
        .filter(|o| needle.is_empty() || o.summary.to_lowercase().contains(&needle))
        .take(500)
        .collect();
    let back_to = format!(
        "/events?q={}{}",
        percent_encoding::utf8_percent_encode(&q.q, percent_encoding::NON_ALPHANUMERIC),
        if all { "&all=1" } else { "" }
    );
    let cal_name = |id: &str| calendars.iter().find(|c| c.id == id).map(|c| c.name.clone()).unwrap_or_default();
    page("Events", "/events", html! {
        h1 { "Events" }
        (flash(&Msg { ok: q.ok.clone(), err: q.err.clone() }))
        form.row method="get" action="/events" {
            label { "Search titles" input name="q" value=(q.q); }
            label style="flex-direction:row; align-items:center; gap:6px" { input type="checkbox" name="all" value="1" checked[all]; "Past events too" }
            button type="submit" { "Show" }
        }
        @if events.is_empty() {
            p.muted { "No event to show." }
        } @else {
            div.card.scroll { table {
                tr { th { "When" } th { "Event" } th { "Alerts now" } th { "Pattern for this event" } }
                @for o in &events {
                    tr {
                        td.small { (when(&o.dtstart)) @if o.recurring { br; span.muted { "repeats" } } }
                        td { (o.summary) br; span.muted.small { (cal_name(&o.calendar)) } }
                        td.small { (current_alerts(o)) br; span.muted { "from: " (app.store.source(o).describe()) } }
                        td {
                            form.row method="post" action="/events/choice" {
                                input type="hidden" name="calendar" value=(o.calendar);
                                input type="hidden" name="uid" value=(o.uid);
                                input type="hidden" name="back" value=(back_to);
                                (pattern_select("pattern", &patterns, app.store.choice(&o.calendar, &o.uid), "Automatic"))
                                button type="submit" { "Set" }
                            }
                        }
                    }
                }
            } }
        }
        p.muted.small { "A #tag in the title still wins over the pattern chosen here." }
    })
}

async fn set_choice(State(app): St, headers: HeaderMap, Form(f): Form<ChoiceForm>) -> Response {
    guard!(app, headers);
    let to = if f.back.starts_with("/events") { f.back.clone() } else { "/events".into() };
    match app.store.set_choice(&f.calendar, &f.uid, pattern_id(&f.pattern)) {
        Ok(()) => back(&to, Some("Saved: your devices pick it up at their next sync."), None),
        Err(e) => back(&to, None, Some(&e.to_string())),
    }
}

// Import

async fn import_page(State(app): St, headers: HeaderMap) -> Response {
    guard!(app, headers);
    import_view(&app, None)
}

fn import_view(app: &App, report: Option<(&str, &Report)>) -> Response {
    let calendars: Vec<Calendar> = app.store.calendars();
    page("Import", "/import", html! {
        h1 { "Import calendars" }
        @if let Some((name, r)) = report {
            div.card {
                p.ok { "Imported into " (name) ": " (r.created) " new events, " (r.updated) " updated." }
                @for (kind, n) in &r.skipped { p.muted.small { "Left out " (n) " " (kind) " (only events are kept)." } }
                @if !r.errors.is_empty() {
                    p.err { (r.errors.len()) " could not be imported:" }
                    ul.small { @for e in r.errors.iter().take(50) { li { (e) } } }
                }
            }
        }
        form.card method="post" action="/import" enctype="multipart/form-data" {
            p { label { ".ics files" input type="file" name="file" accept=".ics,text/calendar" multiple required; } }
            p { label { "Into"
                select name="calendar" {
                    option value="" { "A new calendar named after the file" }
                    @for c in &calendars { option value=(c.id) { (c.name) } }
                }
            } }
            p { label style="flex-direction:row; align-items:center; gap:6px" {
                input type="checkbox" name="fresh" value="1" checked;
                "Replace the alerts the export came with by my patterns"
            } }
            button type="submit" { "Import" }
        }
        div.card.small {
            p { b { "Getting the files" } }
            ul {
                li { b { "Google Calendar" } ": calendar.google.com → Settings → Import & export → Export. You get a .zip with one .ics per calendar: unzip it first." }
                li { b { "Outlook (Epitech)" } ": outlook.office.com → Settings → Calendar → Shared calendars → Publish a calendar, then download the ICS link; or export from the Outlook desktop app." }
                li { b { "IONOS" } ": in IONOS webmail's calendar, open the calendar's menu → Export." }
            }
            p.muted { "Importing the same file again updates those events instead of duplicating them." }
        }
    })
}

async fn import_upload(State(app): St, headers: HeaderMap, mut form: Multipart) -> Response {
    guard!(app, headers);
    let mut files: Vec<(String, String)> = Vec::new();
    let mut calendar = String::new();
    let mut fresh = false;
    while let Ok(Some(field)) = form.next_field().await {
        match field.name() {
            Some("file") => {
                let name = field.file_name().unwrap_or("Imported").to_string();
                match field.bytes().await {
                    Ok(b) => files.push((name, String::from_utf8_lossy(&b).into_owned())),
                    Err(e) => return back("/import", None, Some(&e.to_string())),
                }
            }
            Some("calendar") => calendar = field.text().await.unwrap_or_default(),
            Some("fresh") => fresh = true,
            _ => {}
        }
    }
    if files.is_empty() {
        return back("/import", None, Some("Choose at least one file."));
    }
    let mut report = Report::default();
    let mut names = Vec::new();
    for (file, text) in &files {
        let id = if calendar.is_empty() {
            let name = file.trim_end_matches(".ics");
            let name = name.split('_').next().filter(|n| !n.is_empty()).unwrap_or(name);
            match new_calendar(&app, name) {
                Ok(id) => id,
                Err(e) => return back("/import", None, Some(&e.to_string())),
            }
        } else {
            calendar.clone()
        };
        if let Some(c) = app.store.calendar(&id) {
            names.push(c.name);
        }
        import::import(&app.store, &id, text, fresh, &mut report);
    }
    names.dedup();
    import_view(&app, Some((&names.join(", "), &report)))
}

// Connect

async fn connect_page(State(app): St, headers: HeaderMap) -> Response {
    guard!(app, headers);
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("mail.sigma-dev.eu");
    let base = format!("https://{host}");
    let user = &app.config.user;
    page("Connect apps", "/connect", html! {
        h1 { "Connect your apps" }
        p { "Every app uses user name " code { (user) } " and valet's password. After connecting, "
            b { "turn off the app's own default alerts" } ", so valet's patterns apply." }
        div.card {
            h2 { "Android: DAVx⁵" }
            ol {
                li { "Install DAVx⁵ (F-Droid or Play Store), tap +, choose " i { "Login with URL and user name" } "." }
                li { "Base URL " code { (base) "/" } ", your user name and password." }
                li { "Tick the calendars to sync. They appear in any calendar app (Google Calendar, Fossify, Etar…)." }
                li { "In Android settings, set DAVx⁵'s battery use to " i { "Unrestricted" } "." }
            }
        }
        div.card {
            h2 { "macOS: Calendar" }
            ol {
                li { "Calendar → Settings → Accounts → + → " i { "Other CalDAV Account" } "." }
                li { "Account type " i { "Manual" } ", user name, password, server address " code { (host) } "." }
                li { "Then Calendar → Settings → Alerts: set the defaults to " i { "None" } "." }
            }
        }
        div.card {
            h2 { "Linux: Thunderbird" }
            ol {
                li { "Calendar → New Calendar → " i { "On the Network" } "." }
                li { "User name, and location " code { (base) "/dav/" } "; Thunderbird finds the calendars." }
                li { "Settings → Calendar → Reminders: no default reminder for new events." }
            }
        }
        div.card {
            h2 { "Linux: GNOME Calendar / Evolution" }
            ol {
                li { "Evolution: File → New → Calendar, type " i { "CalDAV" } ", URL " code { (base) "/dav/" } ", then " i { "Find Calendars" } "." }
                li { "GNOME Calendar shows the calendars Evolution knows about." }
            }
        }
        p.muted.small { "Thunderbird and DAVx⁵ can also find everything from " code { (base) "/.well-known/caldav" } "." }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(slug("Work (IONOS)"), "work-ionos");
        assert_eq!(slug("!!"), "calendar");
        assert_eq!(when("20261023T100000Z"), "2026-10-23 10:00 UTC");
        assert_eq!(when("20261023T100000"), "2026-10-23 10:00");
        assert_eq!(when("20261023"), "2026-10-23, all day");
        assert_eq!(today().len(), 8);
        let p = Pattern { id: 1, name: "x".into(), tag: "x".into(), offsets: vec![-1620, -12, 540] };
        assert_eq!(spec_of(&p), "1d 3h, 12m, 09:00");
        assert_eq!(alarms::parse_spec(&spec_of(&p)).unwrap(), vec![-1620, -12, 540]);
    }
}
