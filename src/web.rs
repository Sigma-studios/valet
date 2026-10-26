//! The web UI: alert patterns, calendars and their defaults, per-event
//! choices, .ics import, and how to connect each app.

use std::sync::Arc;

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
        .route("/", get(root).fallback(crate::fallback))
        .route("/settings", get(home))
        .route("/settings/zone", post(save_zone))
        .route("/invitations", get(invitations_page))
        .route("/calendar", get(calendar::page))
        .route("/calendar/month/{key}", get(calendar::month))
        .route("/calendar/event", get(calendar::event).post(calendar::save))
        .route("/calendar/event/delete", post(calendar::delete))
        .route("/calendar/invite", post(calendar::answer))
        .route("/calendar/alarms.json", get(calendar::alarms))
        .route("/assets/{name}", get(assets::file))
        .route("/sw.js", get(assets::service_worker))
        .route("/manifest.webmanifest", get(assets::manifest))
        .route("/offline", get(assets::offline))
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
        .route("/mail", get(mail_page))
        .route("/mail/status", get(mail_status))
        .route("/mail/new.json", get(push::new_mail))
        .route("/push/subscribe", post(push::subscribe))
        .route("/push/unsubscribe", post(push::unsubscribe))
        .route("/push/test", post(push::test))
        .route("/push/remove", post(push::remove))
        .route("/mail/accounts/new", get(new_account_page).post(create_account))
        .route("/mail/accounts/{id}", get(edit_account_page).post(update_account))
        .route("/mail/accounts/{id}/retire", post(retire_account))
        .route("/mail/accounts/{id}/delete", post(delete_account))
        .route("/mail/oauth/{account}/start", post(oauth_start))
        .route("/mail/oauth", get(oauth_callback))
        .route("/mail/oauth/paste", post(oauth_paste))
        .route("/invites/{id}", post(answer_invite))
        .route("/inbox", get(webmail::inbox))
        .route("/box/{id}", get(webmail::list))
        .route("/box/{id}/bulk", post(webmail::bulk))
        .route("/box/{id}/addresses", get(webmail::addresses))
        .route("/msg/{id}", get(webmail::view).post(webmail::action))
        .route("/msg/{id}/html", get(webmail::html_body))
        .route("/msg/{id}/seen", post(webmail::mark_seen))
        .route("/msg/{id}/att/{n}", get(webmail::attachment))
        .route("/msg/{id}/cid/{cid}", get(webmail::cid))
        .route("/compose", get(webmail::compose).post(webmail::send))
        .route("/compose/draft", post(webmail::save_draft))
        .route("/mail/refresh", post(webmail::refresh))
        .route("/mail/events", get(webmail::events))
        .route("/folders/{account}", get(webmail::folders_page).post(webmail::create_folder))
        .route("/folders/{account}/{id}/rename", post(webmail::rename_folder))
        .route("/folders/{account}/{id}/delete", post(webmail::delete_folder))
        .route("/tree", get(webmail::tree_page).post(webmail::create_node))
        .route("/tree/layout", post(webmail::tree_layout))
        .route("/tree/reset", post(webmail::reset_tree))
        .route("/tree/add/{folder}", post(webmail::add_to_tree))
        .route("/tree/{id}", post(webmail::node_action))
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

mod assets;
mod calendar;
mod push;
mod webmail;

/// The app, for what every page shows (the count of invitations waiting).
static APP: std::sync::OnceLock<std::sync::Weak<App>> = std::sync::OnceLock::new();

pub fn set_app(app: &Arc<App>) {
    let _ = APP.set(Arc::downgrade(app));
}

/// Invitations not answered yet, for events still to come.
fn waiting(app: &App) -> Vec<crate::mailstore::Invite> {
    let today = crate::util::today();
    app.mail.invites().into_iter().filter(|i| i.response.is_none() && i.dtstart.get(..8).is_some_and(|d| d >= today.as_str())).collect()
}

/// Where settings live; the rest of the bar is for every day.
const SETTINGS: &[(&str, &str)] = &[
    ("/settings", "Overview"),
    ("/mail", "Mail accounts"),
    ("/calendars", "Calendars"),
    ("/patterns", "Alert patterns"),
    ("/events", "Event alerts"),
    ("/import", "Import"),
    ("/connect", "Connect apps"),
];

const CSS: &str = r#"
:root { color-scheme:light dark; --bg:#fafaf9; --fg:#1c1917; --muted:#78716c; --line:#e7e5e4; --card:#fff; --accent:#2563eb; --bad:#b91c1c; --good:#15803d; }
@media (prefers-color-scheme: dark) { :root { --bg:#1c1917; --fg:#f5f5f4; --muted:#a8a29e; --line:#44403c; --card:#292524; --accent:#60a5fa; --bad:#f87171; --good:#4ade80; } }
* { box-sizing: border-box; } [hidden] { display:none !important; }
body { margin:0; background:var(--bg); color:var(--fg); font:15px/1.5 system-ui, sans-serif; }
header { border-bottom:1px solid var(--line); background:var(--card); }
/* The bar is the same on every page, so its links never move. */
header nav { max-width:2000px; margin:0 auto; padding:10px 16px; display:flex; gap:16px; flex-wrap:wrap; align-items:center; }
a { color:var(--accent); text-decoration:none; } a:hover { text-decoration:underline; }
header nav a { color:var(--fg); } header nav a.on { color:var(--accent); text-decoration:underline 2px; text-underline-offset:6px; }
header nav .brand { font-weight:700; margin-right:8px; } header nav form { margin-left:auto; }
main { max-width:960px; margin:0 auto; padding:16px; } body.wide main { max-width:2000px; }
/* Wider than a phone, the window is the app: under the bar, the page's
   own part scrolls (its scrollbar at the window's edge), never the window,
   so the bar stays whole and in place whether a page is long or not. */
@media (min-width: 721px) {
  html, body { height:100%; }
  body:has(> header) { display:flex; flex-direction:column; overflow:hidden; }
  body > header, body > .subnav { flex:none; }
  /* .wide too: the plain max-width (body.wide main) would otherwise win and,
     with this padding, leave the content almost no room on large screens. */
  body:has(> header) > main, body.wide:has(> header) > main { max-width:none; }
  body:has(> header) > main { flex:1; min-height:0; width:100%; overflow-y:auto;
    padding-inline:max(16px, calc((100% - 960px) / 2 + 16px)); scrollbar-width:thin; }
  body.wide:has(> header) > main { padding-inline:max(16px, calc((100% - 2000px) / 2 + 16px)); }
}
h1 { font-size:22px; margin:8px 0 16px; } h2 { font-size:17px; margin:24px 0 8px; }
.card { background:var(--card); border:1px solid var(--line); border-radius:8px; padding:12px 16px; margin:12px 0; }
.card.retired { background:none; border-style:dashed; } .card.retired h2 { color:var(--muted); }
.pill { font-size:11px; font-weight:600; border:1px solid var(--line); border-radius:9px; padding:1px 7px; color:var(--muted); vertical-align:2px; }
.muted { color:var(--muted); } .small { font-size:13px; }
.ok { color:var(--good); } .err { color:var(--bad); }
table { width:100%; border-collapse:collapse; } td, th { text-align:left; padding:6px 8px; border-bottom:1px solid var(--line); vertical-align:top; }
input, select, button { font:inherit; padding:5px 8px; border:1px solid var(--line); border-radius:6px; background:var(--bg); color:var(--fg); max-width:100%; }
button { cursor:pointer; background:var(--accent); color:#fff; border:none; } button.plain { background:none; color:var(--bad); border:1px solid var(--line); }
form.row { display:flex; gap:8px; flex-wrap:wrap; align-items:end; } label { display:flex; flex-direction:column; gap:2px; font-size:13px; color:var(--muted); }
code { background:var(--line); padding:1px 5px; border-radius:4px; word-break:break-all; }
.scroll { overflow-x:auto; }
.menu { position:relative; } .menu summary { list-style:none; cursor:pointer; padding:0 10px; border:1px solid var(--line); border-radius:6px; font-size:18px; line-height:28px; user-select:none; }
.menu summary::-webkit-details-marker { display:none; }
.menu > div { position:absolute; right:0; top:calc(100% + 4px); z-index:10; min-width:220px; display:flex; flex-direction:column; background:var(--card); border:1px solid var(--line); border-radius:8px; padding:4px; box-shadow:0 6px 20px rgb(0 0 0 / .18); }
.menu > div a, .menu > div button { display:block; width:100%; text-align:left; padding:7px 10px; border:none; border-radius:6px; background:none; color:var(--fg); }
.menu > div a:hover, .menu > div button:hover { background:var(--line); text-decoration:none; }
.menu > div button.danger { color:var(--bad); }
header nav .count { background:var(--bad); color:#fff; border-radius:9px; font-size:11px; font-weight:700; padding:0 6px; margin-left:4px; vertical-align:2px; }
.subnav { border-bottom:1px solid var(--line); background:var(--bg); }
.subnav div { max-width:2000px; margin:0 auto; padding:6px 16px; display:flex; gap:14px; flex-wrap:wrap; font-size:14px; }
.subnav a { color:var(--muted); } .subnav a.on { color:var(--fg); text-decoration:underline 2px; text-underline-offset:5px; }
.subnav form { display:none; margin-left:auto; }
@media (max-width: 720px) { header nav { gap:14px; } header nav form { display:none; } .subnav form { display:block; } }
.offline-banner { background:var(--card); border:1px solid var(--line); border-left:3px solid var(--bad); border-radius:6px; padding:6px 10px; font-size:13px; margin-bottom:10px; }
.alert-rows { display:flex; flex-direction:column; gap:6px; }
.alert-row { display:flex; gap:6px; align-items:center; flex-wrap:wrap; font-size:14px; }
.alert-add { display:flex; gap:6px; flex-wrap:wrap; }
.alerts-box { display:flex; flex-direction:column; gap:6px; flex-basis:100%; }
time[title], [data-zone] { cursor:help; }
"#;

fn page(title: &str, on: &str, body: Markup) -> Response {
    page_with(title, on, false, body)
}

/// A page using the whole window's width (webmail).
fn page_wide(title: &str, on: &str, body: Markup) -> Response {
    page_with(title, on, true, body)
}

fn page_with(title: &str, on: &str, wide: bool, body: Markup) -> Response {
    let in_settings = SETTINGS.iter().any(|(h, _)| *h == on);
    let link = |href: &str, label: &str, here: bool| {
        html! { a href=(href) class=[here.then_some("on")] { (label) } }
    };
    let waiting = APP.get().and_then(std::sync::Weak::upgrade).map(|a| waiting(&a).len()).unwrap_or(0);
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " · valet" }
                style { (PreEscaped(CSS)) }
                (assets::head())
            }
            body.wide[wide] {
                header { nav {
                    span.brand { "valet" }
                    (link("/inbox", "Mail", on == "/inbox")) (link("/calendar", "Calendar", on == "/calendar"))
                    a href="/invitations" class=[(on == "/invitations").then_some("on")] {
                        "Invitations" @if waiting > 0 { span.count title="Waiting for an answer" { (waiting) } }
                    }
                    (link("/settings", "Settings", in_settings))
                    form method="post" action="/logout" { button.plain type="submit" { "Log out" } }
                } }
                @if in_settings {
                    div.subnav { div {
                        @for (href, label) in SETTINGS { (link(href, label, on == *href)) }
                        form method="post" action="/logout" { button.plain type="submit" { "Log out" } }
                    } }
                }
                main { (body) }
            }
        }
    }
    .into_response()
}

/// The everyday page first: mail.
async fn root(State(app): St, headers: HeaderMap) -> Response {
    guard!(app, headers);
    Redirect::to("/inbox").into_response()
}

#[derive(Deserialize, Default)]
struct Msg {
    ok: Option<String>,
    err: Option<String>,
    /// Mail page: the account whose Microsoft login to show again.
    relogin: Option<String>,
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
                (assets::head())
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
    ([(header::SET_COOKIE, cookie)], Redirect::to("/inbox")).into_response()
}

async fn logout() -> Response {
    let cookie = format!("{COOKIE}=; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age=0");
    ([(header::SET_COOKIE, cookie)], Redirect::to("/login")).into_response()
}

// Overview

async fn home(State(app): St, headers: HeaderMap, Query(m): Query<Msg>) -> Response {
    guard!(app, headers);
    let patterns = app.store.patterns();
    let name = |id: Option<i64>| id.and_then(|id| patterns.iter().find(|p| p.id == id)).map(|p| p.name.clone());
    let calendars = app.store.calendars();
    let zone = crate::calendar::default_zone();
    page("Overview", "/settings", html! {
        h1 { "Overview" }
        (flash(&m))
        h2 { "Time zone" }
        form.card.row method="post" action="/settings/zone" {
            label { "Times are shown in" input #zone name="zone" list="zones" required value=(zone.name()) size="24" autocomplete="off"; }
            datalist #zones { @for z in crate::calendar::zone_names() { option value=(z) {} } }
            button type="submit" { "Save" }
            button.plain type="button" #zone-here hidden { "Use this device's" }
            p.muted.small style="flex-basis:100%; margin:0" {
                "Every page shows times in it: hover a time to see its zone. "
                "It's also the zone of the rare event times written without one."
            }
        }
        script { (PreEscaped(r#"{
  const here = Intl.DateTimeFormat().resolvedOptions().timeZone, b = document.getElementById('zone-here'), z = document.getElementById('zone');
  if (here && here !== z.value) { b.hidden = false; b.textContent = `Use this device's (${here})`; b.onclick = () => { z.value = here; z.form.requestSubmit(); }; }
}"#)) }
        h2 { "Alerts" }
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
        (push::section(&app))
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

#[derive(Deserialize)]
struct ZoneForm {
    zone: String,
}

async fn save_zone(State(app): St, headers: HeaderMap, Form(f): Form<ZoneForm>) -> Response {
    guard!(app, headers);
    let Some(tz) = f.zone.trim().parse::<chrono_tz::Tz>().ok() else {
        return back("/settings", None, Some(&format!("{} is not a time zone: pick one in the list (like Europe/Paris).", f.zone.trim())));
    };
    if let Err(e) = app.store.set_setting("tz", tz.name()) {
        return back("/settings", None, Some(&format!("Not saved: {e}")));
    }
    crate::calendar::set_default_zone(tz);
    back("/settings", Some(&format!("Times are shown in {} now.", crate::calendar::zone_label(tz))), None)
}

// Patterns

#[derive(Deserialize)]
struct PatternForm {
    name: String,
    tag: String,
    alerts: String,
    #[serde(default)]
    ring: Option<String>,
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
        div.alerts-box data-alerts=(serde_json::to_string(&p.map(|p| p.offsets.clone()).unwrap_or_else(|| vec![-10])).unwrap_or_default()) {
            label { "Alerts" input name="alerts" value=[p.map(spec_of)] placeholder="12m, 2m" size="24"; }
        }
        label style="flex-direction:row; align-items:center; gap:6px" {
            input type="checkbox" name="ring" value="1" checked[p.is_some_and(|p| p.ring)];
            "Ring like an alarm (valet's Android app)"
        }
    }
}

const SPEC_HELP: &str = "Alerts before or after the start are for events at a time; alerts at a time of day are for all-day events \
    (which start at midnight: 9:00 the same day, 18:00 the day before). A pattern with no alert keeps events silent (#silent).";

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
                        td { (pattern_alerts(p)) @if p.ring { br; span.small { "⏰ rings on the Android app" } } }
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
        script src=(assets::url("patterns.js")) {}
    })
}

async fn create_pattern(State(app): St, headers: HeaderMap, Form(f): Form<PatternForm>) -> Response {
    guard!(app, headers);
    let (name, tag, offsets) = match check_pattern(&f) {
        Ok(v) => v,
        Err(e) => return back("/patterns", None, Some(&e)),
    };
    match app.store.save_pattern(None, &name, &tag, &offsets) {
        Ok(true) => {
            if let Some(p) = app.store.patterns().into_iter().find(|p| p.tag == tag) {
                let _ = app.store.set_ring(p.id, f.ring.is_some());
            }
            back("/patterns", Some(&format!("Created {name}.")), None)
        }
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
        script src=(assets::url("patterns.js")) {}
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
        Ok(true) => {
            let _ = app.store.set_ring(id, f.ring.is_some());
            back("/patterns", Some(&format!("Saved {name}.")), None)
        }
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

fn today() -> String {
    crate::util::today()
}

/// A moment as every page shows it: in the settings' zone, said on hover
/// (where the device is elsewhere, the page's script adds its own time).
pub fn at(t: i64) -> Markup {
    use chrono::TimeZone;
    let Some(d) = crate::calendar::default_zone().timestamp_opt(t, 0).single() else { return html! {} };
    html! { time datetime=(d.to_rfc3339()) title=(crate::calendar::zone_note(d)) { (d.format("%d/%m/%Y %H:%M")) } }
}

/// From a start to an end: `23/10/2026 10:00–10:30`, the end in full when on another day.
fn during(start: crate::calendar::When, end: crate::calendar::When) -> Markup {
    use crate::calendar::When;
    let (When::At(a), When::At(b)) = (start, end) else { return html! {} };
    let tz = crate::calendar::default_zone();
    let day = |t: i64| chrono::DateTime::from_timestamp(t, 0).map(|d| d.with_timezone(&tz));
    match (day(a), day(b)) {
        (Some(x), Some(y)) if x.date_naive() == y.date_naive() => html! { (at(a)) "–" (y.format("%H:%M")) },
        _ => html! { (at(a)) " – " (at(b)) },
    }
}

/// When an event starts (`23/10/2026 10:00`, or `23/10/2026, all day`).
fn starts(ics: &str) -> Markup {
    match crate::calendar::first_start(ics) {
        Some(crate::calendar::When::At(t)) => at(t),
        Some(crate::calendar::When::Day(d)) => html! { (d.format("%d/%m/%Y")) ", all day" },
        None => html! { span.muted { "no date" } },
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
        (flash(&Msg { ok: q.ok.clone(), err: q.err.clone(), ..Default::default() }))
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
                        td.small {
                            @match crate::calendar::describe(o).filter(|_| o.recurring) {
                                // A series keeps the hour of its own zone: said when it isn't the usual one.
                                Some((rule, first, Some(t))) => {
                                    span title=(crate::calendar::zone_note(t)) {
                                        (rule)
                                        @if t.timezone() != crate::calendar::default_zone() { " (" (crate::calendar::zone_label(t.timezone())) ")" }
                                    }
                                    br; span.muted title=(crate::calendar::zone_note(t)) { "first: " (first) }
                                }
                                Some((rule, first, None)) => { (rule) br; span.muted { "first: " (first) } }
                                None => { (starts(&o.data)) }
                            }
                        }
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
        h1 { "valet as an app" }
        p { "This site installs like an app: its own window and icon, and pages and calendar months you've seen stay readable offline." }
        div.card {
            h2 { "Phone" }
            ul {
                li { b { "Android: " } "valet's Android app (from the " code { "android/" } " folder of valet's code) is this site plus alarms that ring, for the "
                    a href="/patterns" { "patterns" } " marked so. At first launch, give it " code { (base) } ". "
                    "Without it: Chrome → ⋮ → " i { "Add to home screen" } " → " i { "Install" } "." }
                li { b { "iPhone: " } "Safari → Share → " i { "Add to Home Screen" } "." }
            }
            p #in-app hidden { "You're in valet's Android app. " button.plain type="button" onclick="ValetApp.changeServer()" style="color:var(--fg)" { "Change valet's address" } }
            script { (PreEscaped("if (window.ValetApp) document.getElementById('in-app').hidden = false;")) }
        }
        div.card {
            h2 { "Computer" }
            ul {
                li { b { "Chrome, Edge, Brave: " } "the install icon at the right of the address bar (or ⋮ → " i { "Install valet" } ")." }
                li { b { "Safari on a Mac: " } "File → " i { "Add to Dock" } "." }
            }
        }

        h1 { "Calendar apps" }
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

        h1 { "Mail apps" }
        p { "One account in each app, for all your mailboxes:" }
        div.card.scroll { table {
            tr { th {} th { "Server" } th { "Port" } th { "Security" } }
            tr { td { "Incoming (IMAP)" } td { code { (host) } } td { "993" } td { "SSL/TLS" } }
            tr { td { "Outgoing (SMTP)" } td { code { (host) } } td { "465" } td { "SSL/TLS" } }
        } }
        p { "User name " code { (user) } ", valet's password (normal password authentication)." }
        p { "Then add one " b { "identity" } " (Thunderbird: account settings → Manage identities; Apple Mail: the account's Email Addresses field, comma-separated) per address, so replies go out from the address they were sent to:" }
        ul { @for a in &app.accounts() { li { code { (a.address) } " (" (a.name) ")" @for al in &a.aliases { ", " code { (al) } } } } }
        p.muted.small { "Your folders come first (INBOX, Sent, Spam…, each gathering the accounts' folders: arrange them on the webmail's Edit folders page); "
            "each account's own folders are under Accounts. "
            "Running valet without façade in front (on your own machine): IMAP on port 1143 and SMTP on 1587, with no encryption." }
    })
}

// Mail

/// An account's sync status and counts, in an element the Mail page refreshes.
fn account_status(app: &App, a: &crate::accounts::Account, boxes: &[crate::mailstore::Mailbox]) -> Markup {
    html! { div id={"status-" (a.id)} { (account_status_inner(app, a, boxes)) } }
}

fn account_status_inner(app: &App, a: &crate::accounts::Account, boxes: &[crate::mailstore::Mailbox]) -> Markup {
    let (ops, op_error) = app.mail.pending_ops(&a.id);
    let folders: Vec<_> = boxes.iter().filter(|b| b.account.as_deref() == Some(a.id.as_str())).collect();
    let count: u32 = folders.iter().map(|b| app.mail.count(b.id).0).sum();
    if let Some(t) = a.retired {
        return html! {
            p.muted { "Not synced since " (at(t)) ": " (folders.len()) " folders, " (count) " messages kept here." }
            p.small.muted { "Its mail reads as before, here and in your mail apps. valet no longer logs in to it or sends from it: "
                "what you change in its mail (read, flagged, moved, deleted) stays in valet." }
        };
    }
    html! {
        @match app.mail.status(&a.id) {
            Some((true, msg, t)) => p.ok { "● " (msg) span.muted.small { " · " (at(t)) } },
            Some((false, msg, t)) => p.err { "● " (msg) span.muted.small { " · " (at(t)) } },
            None => p.muted { "Connecting…" },
        }
        p.small.muted { (folders.len()) " folders, " (count) " messages kept here"
            @if ops > 0 { " · " (ops) " changes waiting to reach the provider" } }
        @if let Some(e) = op_error { p.small.err { "Last refused change: " (e) } }
    }
}

/// Every account's status, for the Mail page to refresh itself.
async fn mail_status(State(app): St, headers: HeaderMap) -> Response {
    if !authed(&app, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let boxes = app.mail.mailboxes();
    let all: serde_json::Map<String, serde_json::Value> =
        app.accounts().iter().map(|a| (a.id.clone(), account_status_inner(&app, a, &boxes).into_string().into())).collect();
    axum::Json(all).into_response()
}

/// A Microsoft account whose login ends on an address to paste back here.
fn pastes(a: &crate::accounts::Account) -> bool {
    a.oauth.as_ref().is_some_and(|o| o.redirect_uri.is_some())
}

async fn mail_page(State(app): St, headers: HeaderMap, Query(m): Query<Msg>) -> Response {
    guard!(app, headers);
    let boxes = app.mail.mailboxes();
    let accounts = app.accounts();
    // The Microsoft login shows until the account is connected, or when asked for again.
    let login_shown = |a: &crate::accounts::Account| {
        a.oauth.is_some() && a.retired.is_none() && (app.mail.token(&a.id).is_none() || m.relogin.as_deref() == Some(a.id.as_str()))
    };
    page("Accounts", "/mail", html! {
        h1 { "Mail accounts" }
        (flash(&m))
        @if accounts.is_empty() {
            p.muted { "No account yet: add your mailboxes below. valet logs in to each, keeps a copy, and serves them all to your mail apps as one account." }
        }
        @for a in &accounts {
            div.card.retired[a.retired.is_some()] {
                div style="display:flex; justify-content:space-between; align-items:start; gap:8px" {
                    h2 style="margin-top:0" { (a.name) " " span.muted.small { (a.address) } @if a.retired.is_some() { " " span.pill { "Retired" } } }
                    details.menu {
                        summary aria-label="Actions" { "⋯" }
                        div {
                            a href={"/mail/accounts/" (a.id)} { "Edit" }
                            @if a.oauth.is_some() && a.retired.is_none() && !login_shown(a) {
                                a href={"/mail?relogin=" (a.id)} { "Log in to Microsoft again" }
                            }
                            form method="post" action={"/mail/accounts/" (a.id) "/retire"}
                                onsubmit=[a.retired.is_none().then_some("return confirm('Stop syncing this account? Its mail stays here and in your mail apps, readable as before; valet stops logging in to it and sending from it, and changes to its mail stay in valet. You can sync it again later.')")] {
                                @if a.retired.is_some() {
                                    button type="submit" name="action" value="resume" title="Log in to it again and sync it" { "Sync again" }
                                } @else {
                                    button type="submit" name="action" value="retire" title="For an address that is out of service" { "Stop syncing, keep the mail" }
                                }
                            }
                            form method="post" action={"/mail/accounts/" (a.id) "/delete"}
                                onsubmit="return confirm('Remove this account from valet? Its mail stays at the provider; the copy kept here goes.')" {
                                button.danger type="submit" { "Remove" }
                            }
                        }
                    }
                }
                (account_status(&app, a, &boxes))
                @if login_shown(a) {
                    form method="post" action={"/mail/oauth/" (a.id) "/start"} target="_blank" style="margin-top:8px" {
                        button type="submit" { "Log in to Microsoft" }
                    }
                    @if pastes(a) {
                        form method="post" action="/mail/oauth/paste" style="margin-top:12px" {
                            ol.small style="margin:0 0 8px; padding-left:20px" {
                                li { "Log in to Microsoft (it opens in a new tab)." }
                                li { "You end on a page that doesn't load, at " code { "https://localhost/?code=…" } ": that's expected. Copy its whole address from the address bar." }
                                li { "Paste it here within a few minutes, and press Finish." }
                            }
                            div style="display:flex; gap:8px" {
                                input name="url" required placeholder="https://localhost/?code=…" style="flex:1";
                                button type="submit" { "Finish" }
                            }
                        }
                    }
                }
            }
        }
        p { "Add an account: "
            @for (i, p) in crate::accounts::PRESETS.iter().enumerate() {
                @if i > 0 { " · " }
                a href={"/mail/accounts/new?preset=" (p.key)} { (p.label) }
            }
            " · " a href="/mail/accounts/new" { "other (IMAP and SMTP)" }
        }
        // Status lines follow the sync without reloading (forms being typed in stay as they are).
        script { (PreEscaped(r#"
setInterval(async () => {
  if (document.hidden) return;
  try {
    const r = await fetch('/mail/status');
    if (!r.ok) return;
    for (const [id, html] of Object.entries(await r.json())) {
      const el = document.getElementById('status-' + id);
      if (el && el.innerHTML !== html) el.innerHTML = html;
    }
  } catch (_) {}
}, 3000);
"#)) }
    })
}

/// Invitations: events not in a calendar yet, waiting for an answer.
async fn invitations_page(State(app): St, headers: HeaderMap, Query(m): Query<Msg>) -> Response {
    guard!(app, headers);
    let waiting = waiting(&app);
    let others: Vec<crate::mailstore::Invite> = app.mail.invites().into_iter().filter(|i| !waiting.iter().any(|w| w.id == i.id)).collect();
    let all_calendars = app.store.calendars();
    let calendars: Vec<&Calendar> = all_calendars.iter().filter(|c| c.id != crate::invites::CALENDAR).collect();
    let calendar_name = |id: &str| all_calendars.iter().find(|c| c.id == id).map_or(id.to_string(), |c| c.name.clone());
    let objects = if waiting.is_empty() { vec![] } else { app.store.all_objects() };
    let now = crate::mailstore::now();
    let card = |i: &crate::mailstore::Invite| html! {
        div.card {
            div style="display:flex; justify-content:space-between; gap:12px; flex-wrap:wrap" {
                div {
                    b { (i.summary) }
                    div.small.muted { (starts(&i.ics)) " · from " (i.organizer) }
                    // Before answering: what it would clash with.
                    @if i.response.is_none() {
                        @let clashes = crate::calendar::conflicts(&i.ics, &objects, now);
                        @if !clashes.is_empty() {
                            div.small.err {
                                "Overlaps "
                                @for (n, e) in clashes.iter().take(5).enumerate() {
                                    @if n > 0 { ", " }
                                    b { (e.title) } " " (during(e.start, e.end)) " (" (calendar_name(&e.calendar)) ")"
                                }
                                @if clashes.len() > 5 { " and " (clashes.len() - 5) " more" }
                            }
                        }
                    }
                }
                @match i.response.as_deref() {
                    Some("IGNORED") => span.small.muted { "ignored" },
                    Some(r) => span.small.ok { "answered: " (r.to_lowercase()) },
                    None => {},
                }
            }
            form.row method="post" action={"/invites/" (i.id)} style="margin-top:10px" {
                label { "Put it in" select name="calendar" {
                    @for c in &calendars { option value=(c.id) { (c.name) } }
                    option value="" { "Invitations" }
                } }
                button type="submit" name="answer" value="ACCEPTED" { "Accept" }
                button type="submit" name="answer" value="TENTATIVE" { "Maybe" }
                button.plain type="submit" name="answer" value="DECLINED" { "Decline" }
                button.plain type="submit" name="answer" value="IGNORED" title="Dismiss it without answering: the organizer isn't told" { "Ignore" }
            }
        }
    };
    page("Invitations", "/invitations", html! {
        h1 { "Invitations" }
        (flash(&m))
        @if waiting.is_empty() {
            p.muted { "Nothing waiting: invitations arriving in your inboxes show here until you answer them." }
        } @else {
            @for i in &waiting { (card(i)) }
            p.muted.small { "Answering emails the organizer from the account the invitation came to, and puts the event in the calendar you pick. Accepting from a mail app works too. "
                "Ignore dismisses it without telling the organizer." }
        }
        @if !others.is_empty() {
            details style="margin-top:20px" {
                summary { "Answered or past (" (others.len()) ")" }
                @for i in &others { (card(i)) }
            }
        }
    })
}

fn redirect_uri(app: &App, account: &crate::accounts::Account, headers: &HeaderMap) -> String {
    let _ = app;
    account.oauth.as_ref().and_then(|o| o.redirect_uri.clone()).unwrap_or_else(|| {
        let host = headers.get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("localhost");
        format!("https://{host}/mail/oauth")
    })
}

async fn oauth_start(State(app): St, headers: HeaderMap, Path(account): Path<String>) -> Response {
    guard!(app, headers);
    let Some(acc) = &app.account(&account) else { return StatusCode::NOT_FOUND.into_response() };
    let redirect = redirect_uri(&app, acc, &headers);
    let state = uuid::Uuid::new_v4().simple().to_string();
    match crate::accounts::authorize_url(acc, &redirect, &state) {
        Ok((verifier, url)) => {
            app.oauth_pending.lock().unwrap().insert(state, (acc.id.clone(), verifier, redirect));
            Redirect::to(&url).into_response()
        }
        Err(e) => back("/mail", None, Some(&e.to_string())),
    }
}

async fn finish_oauth(app: &Arc<App>, code: &str, state: &str) -> Response {
    let pending = app.oauth_pending.lock().unwrap().remove(state);
    let Some((account, verifier, redirect)) = pending else {
        return back("/mail", None, Some("That login expired: start again."));
    };
    let Some(acc) = &app.account(&account) else { return back("/mail", None, Some("Unknown account.")) };
    match crate::accounts::exchange_code(acc, &app.mail, code, &verifier, &redirect).await {
        Ok(()) => {
            crate::sync::restart(app, &acc.id);
            back("/mail", Some(&format!("{} is connected.", acc.name)), None)
        }
        Err(e) => back("/mail", None, Some(&format!("{e:#}"))),
    }
}

#[derive(Deserialize)]
struct OAuthQuery {
    code: Option<String>,
    state: Option<String>,
    error_description: Option<String>,
}

async fn oauth_callback(State(app): St, headers: HeaderMap, Query(q): Query<OAuthQuery>) -> Response {
    guard!(app, headers);
    match (q.code, q.state) {
        (Some(code), Some(state)) => finish_oauth(&app, &code, &state).await,
        _ => back("/mail", None, Some(q.error_description.as_deref().unwrap_or("The login did not finish."))),
    }
}

#[derive(Deserialize)]
struct PasteForm {
    url: String,
}

async fn oauth_paste(State(app): St, headers: HeaderMap, Form(f): Form<PasteForm>) -> Response {
    guard!(app, headers);
    let query = f.url.split_once('?').map(|(_, q)| q).unwrap_or(&f.url);
    let mut code = None;
    let mut state = None;
    for (k, v) in form_urlencoded_pairs(query) {
        match k.as_str() {
            "code" => code = Some(v),
            "state" => state = Some(v),
            _ => {}
        }
    }
    match (code, state) {
        (Some(c), Some(s)) => finish_oauth(&app, &c, &s).await,
        _ => back("/mail", None, Some("That address has no code= and state= in it.")),
    }
}

fn form_urlencoded_pairs(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| {
            let dec = |s: &str| percent_encoding::percent_decode_str(&s.replace('+', " ")).decode_utf8_lossy().into_owned();
            (dec(k), dec(v))
        })
        .collect()
}

// Mail accounts

use crate::accounts::{Account, OAuth, PRESETS, Preset, preset};

fn blank_account(p: Option<&Preset>) -> Account {
    Account {
        id: String::new(),
        name: p.map(|p| p.label.split(' ').next().unwrap_or(p.label).to_string()).unwrap_or_default(),
        address: String::new(),
        aliases: Vec::new(),
        imap: p.map(|p| p.imap.to_string()).unwrap_or_default(),
        smtp: p.map(|p| p.smtp.to_string()).unwrap_or_default(),
        user: None,
        password: None,
        oauth: p.filter(|p| p.microsoft).map(|_| OAuth {
            client_id: crate::accounts::THUNDERBIRD_CLIENT.into(),
            client_secret: None,
            tenant: "common".into(),
            redirect_uri: Some("https://localhost".into()),
            scopes: "offline_access https://outlook.office.com/IMAP.AccessAsUser.All https://outlook.office.com/SMTP.Send".into(),
        }),
        history_days: 90,
        skip: Vec::new(),
        tls: true,
        retired: None,
    }
}

/// The add or edit form; `existing` is false for a new account.
fn account_view(draft: &Account, existing: bool, preset_key: &str, err: Option<&str>) -> Response {
    let p = preset(preset_key);
    let known_servers = p.is_some_and(|p| draft.imap == p.imap && draft.smtp == p.smtp);
    let title = if existing { draft.name.clone() } else { format!("Add {}", p.map(|p| p.label).unwrap_or("an account")) };
    let action = if existing { format!("/mail/accounts/{}", draft.id) } else { "/mail/accounts/new".into() };
    let join = |v: &[String]| v.join(", ");
    page(&title, "/mail", html! {
        p.small { a href="/mail" { "← Mail" } }
        h1 { (title) }
        @if let Some(e) = err { p.err { (e) } }
        form.card method="post" action=(action) style="display:flex; flex-direction:column; gap:10px; max-width:520px" {
            input type="hidden" name="preset" value=(preset_key);
            label { "Name (the folder mail apps show for it)" input name="name" required value=(draft.name) placeholder="Work"; }
            label { "Email address" input name="address" type="email" required value=(draft.address) placeholder="me@example.com"; }
            label { "Other addresses it sends as (comma-separated)" input name="aliases" value=(join(&draft.aliases)); }
            @if draft.oauth.is_none() {
                label {
                    @if existing { "Password (leave empty to keep it)" } @else { "Password" }
                    input name="password" type="password" autocomplete="new-password" required[!existing];
                }
            }
            // Links open beside the form, which keeps what's typed.
            div.small.muted {
                "Where to find it:"
                ul style="margin:2px 0 0; padding-left:18px" {
                    @for h in p.map_or(crate::accounts::OTHER_HELP, |p| p.help) {
                        li { (PreEscaped(h.replace("<a ", r#"<a target="_blank" rel="noopener" "#))) }
                    }
                }
            }
            details open[!known_servers] {
                summary.small { "Servers and sync" }
                div style="display:flex; flex-direction:column; gap:10px; margin-top:8px" {
                    label { "IMAP server (host:port, SSL/TLS)" input name="imap" required value=(draft.imap) placeholder="imap.example.com:993"; }
                    label { "SMTP server (host:port; 465 is SSL/TLS, others STARTTLS)" input name="smtp" required value=(draft.smtp) placeholder="smtp.example.com:465"; }
                    label { "Login, if it isn't the address" input name="user" value=(draft.user.clone().unwrap_or_default()); }
                    label { "Days of mail to fetch on the first sync" input name="history_days" type="number" min="1" value=(draft.history_days); }
                    label { "Provider folders to leave out (comma-separated)" input name="skip" value=(join(&draft.skip)); }
                }
            }
            div { button type="submit" { @if existing { "Save" } @else { "Add" } } }
            p.small.muted { @if draft.oauth.is_some() { "Microsoft accounts log in with Microsoft, from the Mail page, once saved." }
                @else { "valet logs in to both servers before saving, so a wrong password shows up here." } }
        }
        @if existing {
            form method="post" action={"/mail/accounts/" (draft.id) "/delete"}
                onsubmit="return confirm('Remove this account from valet? Its mail stays at the provider; the copy kept here goes.')" {
                button.plain type="submit" { "Remove account" }
            }
        }
    })
}

#[derive(Deserialize)]
struct PresetQuery {
    #[serde(default)]
    preset: String,
}

async fn new_account_page(State(app): St, headers: HeaderMap, Query(q): Query<PresetQuery>) -> Response {
    guard!(app, headers);
    account_view(&blank_account(preset(&q.preset)), false, &q.preset, None)
}

async fn edit_account_page(State(app): St, headers: HeaderMap, Path(id): Path<String>) -> Response {
    guard!(app, headers);
    let Some(acc) = app.account(&id) else { return StatusCode::NOT_FOUND.into_response() };
    let key = PRESETS.iter().find(|p| p.imap == acc.imap).map(|p| p.key).unwrap_or("");
    account_view(&acc, true, key, None)
}

#[derive(Deserialize)]
struct AccountForm {
    #[serde(default)]
    preset: String,
    name: String,
    address: String,
    #[serde(default)]
    aliases: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    imap: String,
    #[serde(default)]
    smtp: String,
    #[serde(default)]
    user: String,
    #[serde(default)]
    history_days: String,
    #[serde(default)]
    skip: String,
}

fn list(s: &str) -> Vec<String> {
    s.split(',').map(str::trim).filter(|x| !x.is_empty()).map(str::to_string).collect()
}

/// The account the form describes, over `base` (the saved account or a blank one).
fn apply_form(base: &Account, f: &AccountForm) -> Account {
    let mut a = base.clone();
    a.name = f.name.trim().to_string();
    a.address = f.address.trim().to_string();
    a.aliases = list(&f.aliases);
    if !f.password.is_empty() && a.oauth.is_none() {
        // Google shows app passwords in groups of four letters.
        a.password = Some(if f.preset == "gmail" { f.password.split_whitespace().collect() } else { f.password.clone() });
    }
    a.imap = f.imap.trim().to_string();
    a.smtp = f.smtp.trim().to_string();
    a.user = Some(f.user.trim().to_string()).filter(|u| !u.is_empty());
    a.history_days = f.history_days.trim().parse().unwrap_or(a.history_days).max(1);
    a.skip = list(&f.skip);
    a
}

/// Checks, tries the login when it matters, saves, and restarts the sync.
async fn save_account_form(app: &Arc<App>, base: Account, existing: bool, f: AccountForm) -> Response {
    let mut acc = apply_form(&base, &f);
    if !existing {
        let stem = slug(&acc.name);
        acc.id = stem.clone();
        let mut n = 2;
        while app.account(&acc.id).is_some() {
            acc.id = format!("{stem}-{n}");
            n += 1;
        }
    }
    let shown = |e: &str| {
        let mut draft = acc.clone();
        draft.password = None;
        account_view(&draft, existing, &f.preset, Some(e))
    };
    if let Err(e) = acc.check() {
        return shown(&e.to_string());
    }
    let login_changed =
        !existing || acc.password != base.password || acc.imap != base.imap || acc.smtp != base.smtp || acc.login() != base.login();
    if login_changed && acc.oauth.is_none() {
        let test = tokio::time::timeout(std::time::Duration::from_secs(40), crate::accounts::test_login(&acc, &app.mail)).await;
        match test {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return shown(&format!("Login failed, nothing saved: {e:#}")),
            Err(_) => return shown("The servers didn't answer within 40 seconds, nothing saved."),
        }
    }
    if let Err(e) = app.save_account(acc.clone()) {
        return shown(&format!("{e:#}"));
    }
    let msg = if acc.retired.is_some() {
        format!("Saved {}.", acc.name)
    } else if acc.oauth.is_some() && app.mail.token(&acc.id).is_none() {
        format!("Saved {}: now log in to Microsoft.", acc.name)
    } else {
        format!("Saved {}: syncing.", acc.name)
    };
    back("/mail", Some(&msg), None)
}

async fn create_account(State(app): St, headers: HeaderMap, Form(f): Form<AccountForm>) -> Response {
    guard!(app, headers);
    let base = blank_account(preset(&f.preset));
    save_account_form(&app, base, false, f).await
}

async fn update_account(State(app): St, headers: HeaderMap, Path(id): Path<String>, Form(f): Form<AccountForm>) -> Response {
    guard!(app, headers);
    let Some(base) = app.account(&id) else { return StatusCode::NOT_FOUND.into_response() };
    save_account_form(&app, base, true, f).await
}

#[derive(Deserialize)]
struct RetireForm {
    /// `retire`, or `resume`.
    action: String,
}

async fn retire_account(State(app): St, headers: HeaderMap, Path(id): Path<String>, Form(f): Form<RetireForm>) -> Response {
    guard!(app, headers);
    let retire = f.action != "resume";
    match app.retire_account(&id, retire) {
        Ok(acc) if retire => back("/mail", Some(&format!("{} no longer syncs: its mail stays here.", acc.name)), None),
        Ok(acc) => back("/mail", Some(&format!("Syncing {} again.", acc.name)), None),
        Err(e) => back("/mail", None, Some(&format!("{e:#}"))),
    }
}

async fn delete_account(State(app): St, headers: HeaderMap, Path(id): Path<String>) -> Response {
    guard!(app, headers);
    let Some(acc) = app.account(&id) else { return StatusCode::NOT_FOUND.into_response() };
    match app.delete_account(&id) {
        Ok(()) => back("/mail", Some(&format!("Removed {}.", acc.name)), None),
        Err(e) => back("/mail", None, Some(&format!("{e:#}"))),
    }
}

#[derive(Deserialize)]
struct AnswerForm {
    answer: String,
    #[serde(default)]
    calendar: String,
}

async fn answer_invite(State(app): St, headers: HeaderMap, Path(id): Path<i64>, Form(f): Form<AnswerForm>) -> Response {
    guard!(app, headers);
    if f.answer == "IGNORED" {
        return match crate::invites::ignore(&app, id) {
            Ok(()) => back("/invitations", Some("Ignored: the organizer isn't told."), None),
            Err(e) => back("/invitations", None, Some(&format!("{e:#}"))),
        };
    }
    if !matches!(f.answer.as_str(), "ACCEPTED" | "TENTATIVE" | "DECLINED") {
        return back("/invitations", None, Some("Unknown answer."));
    }
    let calendar = (!f.calendar.is_empty()).then_some(f.calendar.as_str());
    match crate::invites::respond(&app, id, &f.answer, calendar).await {
        Ok(()) => back("/invitations", Some("Answered: the organizer gets your reply by email."), None),
        Err(e) => back("/invitations", None, Some(&format!("{e:#}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gmail_app_passwords_lose_their_spaces() {
        let form = |preset: &str| AccountForm {
            preset: preset.into(),
            name: "G".into(),
            address: "a@gmail.com".into(),
            aliases: String::new(),
            password: "abcd efgh ijkl mnop".into(),
            imap: "imap.gmail.com:993".into(),
            smtp: "smtp.gmail.com:465".into(),
            user: String::new(),
            history_days: String::new(),
            skip: String::new(),
        };
        let base = blank_account(preset("gmail"));
        assert_eq!(apply_form(&base, &form("gmail")).password.as_deref(), Some("abcdefghijklmnop"));
        assert_eq!(apply_form(&base, &form("")).password.as_deref(), Some("abcd efgh ijkl mnop"), "others' passwords stay as typed");
    }

    #[test]
    fn helpers() {
        assert_eq!(slug("Work (IONOS)"), "work-ionos");
        assert_eq!(slug("!!"), "calendar");
        let ics = |start: &str| format!("BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:u\r\nDTSTART{start}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n");
        assert_eq!(
            starts(&ics(":20261023T100000Z")).into_string(),
            r#"<time datetime="2026-10-23T12:00:00+02:00" title="Paris time (Europe/Paris, CEST, UTC+02:00)">23/10/2026 12:00</time>"#
        );
        assert!(starts(&ics(";TZID=America/New_York:20261023T100000")).into_string().contains("23/10/2026 16:00"));
        assert_eq!(starts(&ics(";VALUE=DATE:20261023")).into_string(), "23/10/2026, all day");
        assert_eq!(today().len(), 8);
        let p = Pattern { id: 1, name: "x".into(), tag: "x".into(), offsets: vec![-1620, -12, 540], ring: false };
        assert_eq!(spec_of(&p), "1d 3h, 12m, 09:00");
        assert_eq!(alarms::parse_spec(&spec_of(&p)).unwrap(), vec![-1620, -12, 540]);
    }
}
