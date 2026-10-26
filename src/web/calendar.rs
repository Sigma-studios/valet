//! The calendar page: a shell the page's script fills from one JSON feed per
//! month (so the service worker can keep months for offline reading), the
//! series of one event for the form, saving and deleting, and the alarms
//! valet's Android app rings.

use std::collections::HashMap;

use axum::extract::rejection::FormRejection;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime};
use maud::{PreEscaped, html};
use serde::Deserialize;
use serde_json::json;

use super::*;
use crate::calendar::{self, Edit, Repeat};
use crate::store::{PutError, name_for_uid};

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, axum::Json(json!({ "error": "logged out" }))).into_response()
}

fn refused(e: impl std::fmt::Display) -> Response {
    (StatusCode::UNPROCESSABLE_ENTITY, axum::Json(json!({ "error": e.to_string() }))).into_response()
}

const CAL_CSS: &str = include_str!("assets/calendar.css");

pub async fn page(State(app): St, headers: HeaderMap) -> Response {
    guard!(app, headers);
    page_wide("Calendar", "/calendar", html! {
        style { (PreEscaped(CAL_CSS)) }
        div #cal {
            noscript { p { "The calendar needs JavaScript." } }
        }
        script src=(super::assets::url("calendar.js")) {}
    })
}

/// `2026-10` → its first day and the next month's.
fn month_bounds(key: &str) -> Option<(NaiveDate, NaiveDate)> {
    let first = NaiveDate::parse_from_str(&format!("{key}-01"), "%Y-%m-%d").ok()?;
    let next = if first.month() == 12 { NaiveDate::from_ymd_opt(first.year() + 1, 1, 1)? } else { NaiveDate::from_ymd_opt(first.year(), first.month() + 1, 1)? };
    Some((first, next))
}

fn secs(d: NaiveDate) -> i64 {
    d.and_time(NaiveTime::MIN).and_utc().timestamp()
}

/// The events of a month, a day more on each side (the page shows them in
/// the reader's zone), with the calendars and patterns to show them with.
pub async fn month(State(app): St, headers: HeaderMap, Path(key): Path<String>) -> Response {
    if !authed(&app, &headers) {
        return unauthorized();
    }
    let Some((first, next)) = month_bounds(&key) else { return StatusCode::NOT_FOUND.into_response() };
    let (from, to) = (secs(first) - 86400, secs(next) + 86400);
    let me = app.addresses();
    // The latest invitation of each event, for the ones not answered yet.
    let mut invites: HashMap<String, i64> = HashMap::new();
    for i in app.mail.invites() {
        let id = invites.entry(i.uid).or_insert(i.id);
        *id = (*id).max(i.id);
    }
    let events: Vec<calendar::Instance> = app
        .store
        .all_objects()
        .iter()
        .flat_map(|o| {
            calendar::instances(o, from, to, &me).into_iter().map(|mut i| {
                i.invite = i.unanswered.then(|| invites.get(&o.uid).copied()).flatten();
                i
            })
        })
        .collect();
    let calendars: Vec<_> = app.store.calendars().into_iter().map(|c| json!({ "id": c.id, "name": c.name, "color": c.color })).collect();
    let patterns: Vec<_> = app.store.patterns().into_iter().map(|p| json!({ "id": p.id, "name": p.name, "tag": p.tag, "ring": p.ring })).collect();
    axum::Json(json!({ "month": key, "at": crate::mailstore::now(), "calendars": calendars, "patterns": patterns, "events": events })).into_response()
}

#[derive(Deserialize)]
pub struct EventRef {
    calendar: String,
    name: String,
}

/// One event as the form edits it: the whole series for repeating ones.
pub async fn event(State(app): St, headers: HeaderMap, Query(q): Query<EventRef>) -> Response {
    if !authed(&app, &headers) {
        return unauthorized();
    }
    let Some(o) = app.store.object(&q.calendar, &q.name) else { return StatusCode::NOT_FOUND.into_response() };
    let Some(e) = calendar::editable(&o) else { return refused("that event can't be read") };
    let mut v = serde_json::to_value(&e).unwrap_or_default();
    v["pattern"] = json!(app.store.choice(&o.calendar, &o.uid));
    axum::Json(v).into_response()
}

#[derive(Deserialize)]
pub struct EventForm {
    /// Where the event is now (empty for a new one).
    #[serde(default)]
    from_calendar: String,
    #[serde(default)]
    name: String,
    /// Empty when there is no calendar to pick yet: one is made.
    #[serde(default)]
    calendar: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    all_day: Option<String>,
    start_date: String,
    #[serde(default)]
    start_time: String,
    end_date: String,
    #[serde(default)]
    end_time: String,
    #[serde(default)]
    tz: String,
    #[serde(default)]
    location: String,
    #[serde(default)]
    notes: String,
    #[serde(default)]
    repeat: String,
    /// A pattern's id, or empty for automatic.
    #[serde(default)]
    pattern: String,
}

fn moment(date: &str, time: &str, all_day: bool) -> anyhow::Result<NaiveDateTime> {
    let d = NaiveDate::parse_from_str(date.trim(), "%Y-%m-%d").map_err(|_| anyhow::anyhow!("{date} is not a date"))?;
    if all_day {
        return Ok(d.and_time(NaiveTime::MIN));
    }
    let t = NaiveTime::parse_from_str(time.trim(), "%H:%M").map_err(|_| anyhow::anyhow!("pick a time"))?;
    Ok(d.and_time(t))
}

fn put_error(e: PutError) -> anyhow::Error {
    match e {
        PutError::Invalid(m) => anyhow::anyhow!("not saved: {m}"),
        PutError::UidConflict(_) => anyhow::anyhow!("that calendar has this event already"),
        PutError::NoCalendar => anyhow::anyhow!("no such calendar"),
    }
}

/// The calendar new events go to when there is none of the user's own yet
/// (Invitations is valet's): one is made, as apps would.
fn first_calendar(app: &App) -> anyhow::Result<String> {
    if app.store.calendars().iter().any(|c| c.id != crate::invites::CALENDAR) {
        anyhow::bail!("pick a calendar");
    }
    app.store.create_calendar("calendar", "Calendar", None)?;
    Ok("calendar".into())
}

/// Saves the form: a new event, or the series of an existing one (which may
/// move to another calendar). Apps get it at their next sync.
pub async fn save(State(app): St, headers: HeaderMap, form: Result<axum::extract::Form<EventForm>, FormRejection>) -> Response {
    if !authed(&app, &headers) {
        return unauthorized();
    }
    let mut f = match form {
        Ok(axum::extract::Form(f)) => f,
        Err(e) => return refused(format!("not saved: {}", e.body_text())),
    };
    if f.calendar.is_empty() {
        match first_calendar(&app) {
            Ok(id) => f.calendar = id,
            Err(e) => return refused(e),
        }
    }
    let saved = (|| -> anyhow::Result<(String, String)> {
        let all_day = f.all_day.is_some();
        let edit = Edit {
            title: f.title.clone(),
            location: f.location.clone(),
            notes: f.notes.clone(),
            all_day,
            start: moment(&f.start_date, &f.start_time, all_day)?,
            end: moment(&f.end_date, &f.end_time, all_day)?,
            tz: calendar::zone(&f.tz).unwrap_or_else(calendar::default_zone),
            repeat: Repeat::parse(&f.repeat),
        };
        anyhow::ensure!(app.store.calendar(&f.calendar).is_some(), "no such calendar");
        let existing = if f.name.is_empty() { None } else { Some(app.store.object(&f.from_calendar, &f.name).ok_or_else(|| anyhow::anyhow!("that event is gone"))?) };
        let (uid, ics) = calendar::write(existing.as_ref().map(|o| o.data.as_str()), &edit)?;
        let name = existing.as_ref().map(|o| o.name.clone()).unwrap_or_else(|| name_for_uid(&uid));
        app.store.put_object(&f.calendar, &name, &ics, false).map_err(put_error)?;
        if let Some(o) = &existing
            && o.calendar != f.calendar
        {
            app.store.delete_object(&o.calendar, &o.name)?;
        }
        let pattern = f.pattern.parse::<i64>().ok();
        if pattern.is_some() || app.store.choice(&f.calendar, &uid).is_some() {
            app.store.set_choice(&f.calendar, &uid, pattern)?;
        }
        Ok((f.calendar.clone(), name))
    })();
    match saved {
        Ok((calendar, name)) => axum::Json(json!({ "ok": true, "calendar": calendar, "name": name })).into_response(),
        Err(e) => refused(format!("{e:#}")),
    }
}

pub async fn delete(State(app): St, headers: HeaderMap, form: Result<axum::extract::Form<EventRef>, FormRejection>) -> Response {
    if !authed(&app, &headers) {
        return unauthorized();
    }
    let Ok(axum::extract::Form(r)) = form else { return refused("not deleted: that event can't be found") };
    match app.store.delete_object(&r.calendar, &r.name) {
        Ok(true) => axum::Json(json!({ "ok": true })).into_response(),
        Ok(false) => refused("that event is gone"),
        Err(e) => refused(e),
    }
}

#[derive(Deserialize)]
pub struct AnswerForm {
    invite: i64,
    answer: String,
    #[serde(default)]
    calendar: String,
}

/// Answers an invitation from its event's card, as the Invitations page
/// does: the organizer gets the reply, the event moves to `calendar`. Says
/// how many still wait, for the bar.
pub async fn answer(State(app): St, headers: HeaderMap, form: Result<axum::extract::Form<AnswerForm>, FormRejection>) -> Response {
    if !authed(&app, &headers) {
        return unauthorized();
    }
    let Ok(axum::extract::Form(f)) = form else { return refused("not answered: that invitation can't be found") };
    if !matches!(f.answer.as_str(), "ACCEPTED" | "TENTATIVE" | "DECLINED") {
        return refused("not answered: unknown answer");
    }
    let calendar = (!f.calendar.is_empty()).then_some(f.calendar.as_str());
    match crate::invites::respond(&app, f.invite, &f.answer, calendar).await {
        Ok(()) => axum::Json(json!({ "ok": true, "waiting": waiting(&app).len() })).into_response(),
        Err(e) => refused(format!("not answered: {e:#}")),
    }
}

#[derive(Deserialize)]
pub struct AlarmsQuery {
    #[serde(default)]
    tz: String,
}

/// The alarms to ring in the coming week, for valet's Android app: alerts
/// of the patterns marked to ring. All-day events start at midnight in `tz`.
pub async fn alarms(State(app): St, headers: HeaderMap, Query(q): Query<AlarmsQuery>) -> Response {
    if !authed(&app, &headers) {
        return unauthorized();
    }
    let tags: Vec<String> = app.store.patterns().into_iter().filter(|p| p.ring).map(|p| p.tag).collect();
    let tz = calendar::zone(&q.tz).unwrap_or_else(calendar::default_zone);
    let now = crate::mailstore::now();
    let to = now + Duration::days(8).num_seconds();
    let mut rings: Vec<calendar::Ring> =
        if tags.is_empty() { vec![] } else { app.store.all_objects().iter().flat_map(|o| calendar::rings(o, &tags, now, to, tz)).collect() };
    rings.sort_by_key(|r| r.at);
    rings.truncate(100);
    axum::Json(json!({ "at": now, "alarms": rings })).into_response()
}
