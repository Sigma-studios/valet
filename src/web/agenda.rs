//! What's on, for something outside valet (a desktop bar): the occurrences
//! of a few days, read with a device's token, and the settings page that
//! makes and revokes those tokens.

use chrono::{Duration, NaiveDate, TimeZone};
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use serde::Serialize;
use serde_json::json;

use super::*;
use crate::calendar::{self, When};

/// What a calendar's id or an event's name escape in the page's address.
const PART: &AsciiSet = &CONTROLS.add(b' ').add(b'"').add(b'#').add(b'%').add(b'/').add(b'<').add(b'>').add(b'?').add(b'`').add(b'{').add(b'}');

/// The most days one request covers: two months.
pub const MAX_DAYS: i64 = 62;

/// A device's `Authorization: Bearer <token>`, still valid.
fn bearer(app: &App, headers: &HeaderMap) -> bool {
    let Some(value) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) else { return false };
    let Some((scheme, token)) = value.trim().split_once(' ') else { return false };
    scheme.eq_ignore_ascii_case("bearer") && app.store.use_token(token, crate::mailstore::now())
}

#[derive(Deserialize)]
pub struct AgendaQuery {
    from: Option<String>,
    days: Option<String>,
}

/// One occurrence, as the agenda gives it.
#[derive(Serialize, Debug, PartialEq)]
pub struct Item {
    /// The event's UID: every occurrence of a series shares it.
    pub id: String,
    pub title: String,
    /// `2026-11-14T09:00:00+01:00` in the settings' zone; for all-day
    /// events a date, the end excluded (the day after the last).
    pub start: String,
    pub end: String,
    #[serde(rename = "allDay")]
    pub all_day: bool,
    pub calendar: String,
    pub color: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    /// Opens the event on valet's calendar page (the PWA).
    pub url: String,
    /// An invitation not answered yet.
    pub pending: bool,
}

fn moment(w: When, tz: chrono_tz::Tz) -> String {
    match w {
        When::Day(d) => d.format("%Y-%m-%d").to_string(),
        When::At(t) => tz.timestamp_opt(t, 0).single().map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Secs, false)).unwrap_or_default(),
    }
}

/// The occurrences overlapping `days` days from `from` (midnight to
/// midnight in `tz`), by start; events called off aren't on.
pub fn items(app: &App, from: NaiveDate, days: i64, tz: chrono_tz::Tz, base: &str) -> Vec<Item> {
    let (start, end) = (calendar::midnight(tz, from), calendar::midnight(tz, from + Duration::days(days)));
    let calendars = app.store.calendars();
    let me = app.addresses();
    let enc = |s: &str| utf8_percent_encode(s, PART).to_string();
    let mut found: Vec<(i64, Item)> = app
        .store
        .all_objects()
        .iter()
        .flat_map(|o| calendar::instances(o, start, end, &me))
        .filter(|i| !i.cancelled)
        .map(|i| {
            let cal = calendars.iter().find(|c| c.id == i.calendar);
            let (at, day) = match i.start {
                When::Day(d) => (calendar::midnight(tz, d), d),
                When::At(t) => (t, tz.timestamp_opt(t, 0).single().map_or(from, |d| d.date_naive())),
            };
            let item = Item {
                id: i.uid.clone(),
                title: i.title.clone(),
                start: moment(i.start, tz),
                end: moment(i.end, tz),
                all_day: i.all_day,
                calendar: cal.map_or_else(|| i.calendar.clone(), |c| c.name.clone()),
                color: cal.map_or_else(String::new, |c| c.color.clone()),
                location: Some(i.location.clone()).filter(|l| !l.trim().is_empty()),
                url: format!("{base}/calendar#day/{}/{}/{}", day.format("%Y-%m-%d"), enc(&i.calendar), enc(&i.name)),
                pending: i.unanswered,
            };
            (at, item)
        })
        .collect();
    found.sort_by(|(a, x), (b, y)| a.cmp(b).then(y.all_day.cmp(&x.all_day)).then_with(|| x.title.cmp(&y.title)));
    found.into_iter().map(|(_, i)| i).collect()
}

/// The site's address as the request reached it.
fn base_url(headers: &HeaderMap) -> String {
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("localhost");
    let local = host.starts_with("localhost") || host.starts_with("127.") || host.starts_with("[::1]");
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|h| h.to_str().ok())
        .map(|p| p.split(',').next().unwrap_or("").trim().to_string())
        .filter(|p| p == "http" || p == "https")
        .unwrap_or_else(|| if local { "http".into() } else { "https".into() });
    format!("{scheme}://{host}")
}

/// `GET /api/agenda?from=YYYY-MM-DD&days=N`: from today, one day, unless
/// asked; at most two months.
pub async fn agenda(State(app): St, headers: HeaderMap, Query(q): Query<AgendaQuery>) -> Response {
    if !bearer(&app, &headers) && !authed(&app, &headers) {
        return (StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE, "Bearer realm=\"valet\"")], axum::Json(json!({ "error": "a device token is needed" }))).into_response();
    }
    let bad = |e: String| (StatusCode::BAD_REQUEST, axum::Json(json!({ "error": e }))).into_response();
    let tz = calendar::default_zone();
    let from = match q.from.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => chrono::Utc::now().with_timezone(&tz).date_naive(),
        Some(s) => match NaiveDate::parse_from_str(s, "%Y-%m-%d") {
            Ok(d) => d,
            Err(_) => return bad(format!("from: {s} is not a YYYY-MM-DD date")),
        },
    };
    let days = match q.days.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => 1,
        Some(s) => match s.parse::<i64>() {
            Ok(n) if n >= 1 => n.min(MAX_DAYS),
            _ => return bad(format!("days: {s} is not a number of days (1 to {MAX_DAYS})")),
        },
    };
    let events = items(&app, from, days, tz, &base_url(&headers));
    (
        [(header::CACHE_CONTROL, "no-store")],
        axum::Json(json!({ "from": from.format("%Y-%m-%d").to_string(), "days": days, "zone": tz.name(), "events": events })),
    )
        .into_response()
}

// The settings page

#[derive(Deserialize)]
pub struct TokenForm {
    name: String,
}

fn tokens_page(app: &App, m: &Msg, made: Option<(&str, &str)>, host: &str) -> Response {
    let tokens = app.store.tokens();
    page("Agenda tokens", "/tokens", html! {
        h1 { "Agenda tokens" }
        (flash(m))
        p {
            "A token lets one device read what's on — " code { "GET /api/agenda" } " — without valet's password: "
            "a desktop bar, say. It reads nothing else and changes nothing; the password still logs in everywhere else."
        }
        @if let Some((name, token)) = made {
            div.card style="border-left:3px solid var(--good)" {
                p { b { "The token for " (name) ":" } }
                p { code #token { (token) } " " button type="button" onclick="navigator.clipboard.writeText(document.getElementById('token').textContent).then(() => this.textContent = 'Copied')" { "Copy" } }
                p.small { "Copy it now: valet keeps only a fingerprint of it, so this page can't show it again." }
                p.small.muted { "For instance:" br; code { "curl -H 'Authorization: Bearer " (token) "' '" (host) "/api/agenda?from=" (chrono::Utc::now().with_timezone(&calendar::default_zone()).format("%Y-%m-%d")) "&days=7'" } }
            }
        }
        form.card.row method="post" action="/tokens" {
            label { "Device" input name="name" required maxlength="80" placeholder="Laptop's bar" autocomplete="off"; }
            button type="submit" { "Make a token" }
        }
        @if tokens.is_empty() {
            p.muted { "No token yet." }
        } @else {
            div.card.scroll { table {
                tr { th { "Device" } th { "Made" } th { "Last read the agenda" } th {} }
                @for t in &tokens {
                    tr {
                        td { (t.name) }
                        td { (at(t.created)) }
                        td { @if let Some(u) = t.used { (at(u)) } @else { span.muted { "never" } } }
                        td { form method="post" action={"/tokens/" (t.id) "/revoke"} onsubmit="return confirm('Revoke this token? Its device can no longer read the agenda.')" {
                            button.plain type="submit" { "Revoke" }
                        } }
                    }
                }
            } }
        }
    })
}

pub async fn tokens(State(app): St, headers: HeaderMap, Query(m): Query<Msg>) -> Response {
    guard!(app, headers);
    tokens_page(&app, &m, None, &base_url(&headers))
}

pub async fn create_token(State(app): St, headers: HeaderMap, Form(f): Form<TokenForm>) -> Response {
    guard!(app, headers);
    let name = f.name.trim();
    if name.is_empty() {
        return back("/tokens", None, Some("Name the device the token is for."));
    }
    match app.store.create_token(name, crate::mailstore::now()) {
        Ok(token) => tokens_page(&app, &Msg::default(), Some((name, &token)), &base_url(&headers)),
        Err(e) => back("/tokens", None, Some(&format!("No token made: {e}"))),
    }
}

pub async fn revoke_token(State(app): St, headers: HeaderMap, Path(id): Path<i64>) -> Response {
    guard!(app, headers);
    match app.store.revoke_token(id) {
        Ok(true) => back("/tokens", Some("Revoked: that device can no longer read the agenda."), None),
        Ok(false) => back("/tokens", None, Some("That token was revoked already.")),
        Err(e) => back("/tokens", None, Some(&e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WEEKLY: &str = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:weekly\r\nSUMMARY:Standup\r\nLOCATION:Room 2\r\n\
        DTSTART;TZID=Europe/Paris:20261019T090000\r\nDTEND;TZID=Europe/Paris:20261019T091500\r\nRRULE:FREQ=WEEKLY\r\nEND:VEVENT\r\n\
        BEGIN:VEVENT\r\nUID:weekly\r\nRECURRENCE-ID;TZID=Europe/Paris:20261019T090000\r\nSUMMARY:Standup (moved)\r\n\
        DTSTART;TZID=Europe/Paris:20261020T140000\r\nDTEND;TZID=Europe/Paris:20261020T141500\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    const AWAY: &str = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:away\r\nSUMMARY:Away\r\n\
        DTSTART;VALUE=DATE:20261026\r\nDTEND;VALUE=DATE:20261028\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    const INVITE: &str = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:review\r\nSUMMARY:Review\r\nORGANIZER:mailto:boss@x.fr\r\n\
        ATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:me@b.c\r\nDTSTART:20261026T080000Z\r\nDTEND:20261026T090000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    const OFF: &str = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:off\r\nSUMMARY:Off\r\nSTATUS:CANCELLED\r\n\
        DTSTART:20261021T080000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    fn app() -> Arc<App> {
        let store = crate::store::Store::memory();
        store.create_calendar("work", "Work", Some("#8ab4ff")).unwrap();
        store.create_calendar("invitations", "Invitations", Some("#f59e0b")).unwrap();
        store.put_object("work", "weekly.ics", WEEKLY, false).unwrap();
        store.put_object("work", "away.ics", AWAY, false).unwrap();
        store.put_object("work", "off.ics", OFF, false).unwrap();
        store.put_object("invitations", "review.ics", INVITE, false).unwrap();
        let config = crate::Config { user: "me".into(), password: "a long test password".into(), addresses: vec!["me@b.c".into()] };
        Arc::new(App::new(store, config, crate::mailstore::MailStore::memory()))
    }

    async fn get(app: &Arc<App>, token: Option<&str>, query: &str) -> (StatusCode, serde_json::Value) {
        let mut headers = HeaderMap::new();
        if let Some(t) = token {
            headers.insert(header::AUTHORIZATION, format!("Bearer {t}").parse().unwrap());
        }
        let q: AgendaQuery = serde_urlencoded_query(query);
        let res = agenda(State(app.clone()), headers, Query(q)).await;
        let status = res.status();
        let body = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    fn serde_urlencoded_query(q: &str) -> AgendaQuery {
        let get = |k: &str| q.split('&').filter_map(|kv| kv.split_once('=')).find(|(a, _)| *a == k).map(|(_, v)| v.to_string());
        AgendaQuery { from: get("from"), days: get("days") }
    }

    #[tokio::test]
    async fn agenda_with_a_device_token() {
        let app = app();
        assert_eq!(get(&app, None, "from=2026-10-19&days=14").await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(get(&app, Some("valet_nope"), "from=2026-10-19&days=14").await.0, StatusCode::UNAUTHORIZED);
        let token = app.store.create_token("bar", 1).unwrap();

        let (status, body) = get(&app, Some(&token), "from=2026-10-19&days=9").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!((body["from"].as_str(), body["days"].as_i64(), body["zone"].as_str()), (Some("2026-10-19"), Some(9), Some("Europe/Paris")));
        let rows: Vec<(&str, &str, &str, bool, bool)> = body["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| (e["title"].as_str().unwrap(), e["start"].as_str().unwrap(), e["end"].as_str().unwrap(), e["allDay"].as_bool().unwrap(), e["pending"].as_bool().unwrap()))
            .collect();
        // By start: the moved standup, the Monday after daylight saving (all
        // day first, then the hours), the cancelled one left out.
        assert_eq!(
            rows,
            [
                ("Standup (moved)", "2026-10-20T14:00:00+02:00", "2026-10-20T14:15:00+02:00", false, false),
                ("Away", "2026-10-26", "2026-10-28", true, false),
                ("Review", "2026-10-26T09:00:00+01:00", "2026-10-26T10:00:00+01:00", false, true),
                ("Standup", "2026-10-26T09:00:00+01:00", "2026-10-26T09:15:00+01:00", false, false),
            ]
        );
        let first = &body["events"][0];
        assert_eq!(first["id"], "weekly");
        assert_eq!(first["calendar"], "Work");
        assert_eq!(first["color"], "#8ab4ff");
        assert_eq!(first["location"], "Room 2", "the series' own, for a moved occurrence");
        assert_eq!(first["url"], "http://localhost/calendar#day/2026-10-20/work/weekly.ics");
        assert_eq!(body["events"][2]["calendar"], "Invitations");
        assert!(body["events"][1].get("location").is_none());
        assert!(app.store.tokens()[0].used.is_some(), "its use is noted");

        // Days are Paris days: the 28th has nothing, the trip ended the day before.
        let (_, body) = get(&app, Some(&token), "from=2026-10-28&days=1").await;
        assert!(body["events"].as_array().unwrap().is_empty());
        let (_, body) = get(&app, Some(&token), "from=2026-10-19&days=500").await;
        assert_eq!(body["days"], MAX_DAYS);
        assert_eq!(get(&app, Some(&token), "from=19/10/2026").await.0, StatusCode::BAD_REQUEST);
        assert_eq!(get(&app, Some(&token), "days=0").await.0, StatusCode::BAD_REQUEST);

        app.store.revoke_token(app.store.tokens()[0].id).unwrap();
        assert_eq!(get(&app, Some(&token), "from=2026-10-19").await.0, StatusCode::UNAUTHORIZED);
    }
}
