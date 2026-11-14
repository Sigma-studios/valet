//! The calendar as valet's own pages show it: the events of a time range,
//! repeating ones expanded (time zones, exceptions, moved occurrences), and
//! events written from the web form. Apps keep getting the stored iCalendar
//! as it is; this only reads it, or writes it the way apps do.

use std::sync::RwLock;

use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;
use serde::Serialize;

use crate::ical::{self, Component, Property};
use crate::store::Object;

/// A start or an end: a day (all-day events, end excluded) or an instant.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum When {
    Day(NaiveDate),
    At(i64),
}

impl When {
    /// Unix seconds; a day counts from its midnight UTC.
    fn secs(self) -> i64 {
        match self {
            When::Day(d) => d.and_time(NaiveTime::MIN).and_utc().timestamp(),
            When::At(t) => t,
        }
    }
}

/// One occurrence of an event, for the calendar page.
#[derive(Clone, Debug, Serialize)]
pub struct Instance {
    pub calendar: String,
    /// The event's resource (what to edit or delete).
    pub name: String,
    #[serde(skip)]
    pub uid: String,
    pub title: String,
    pub location: String,
    pub notes: String,
    #[serde(rename = "allDay")]
    pub all_day: bool,
    pub start: When,
    pub end: When,
    pub repeats: bool,
    /// A birthday's: the age it is, when its year is known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub age: Option<i64>,
    /// Called off by its organizer (shown, struck out).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub cancelled: bool,
    /// An invitation to one of my addresses I haven't answered.
    pub unanswered: bool,
    /// The invitation valet got by mail for it, to answer from its card.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invite: Option<i64>,
    /// The mail that invitation came in, to open from its card.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mail: Option<i64>,
    /// The alert pattern it follows, for its badge.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alert: Option<i64>,
    /// Why: `tag`, `chosen` or `default`.
    #[serde(rename = "alertBy", skip_serializing_if = "Option::is_none")]
    pub alert_by: Option<&'static str>,
}

/// The zone of the settings until one is picked.
pub const PARIS: Tz = Tz::Europe__Paris;

static ZONE: RwLock<Tz> = RwLock::new(PARIS);

/// The zone of the settings: every page shows times in it, and times
/// written without one are read in it.
pub fn default_zone() -> Tz {
    *ZONE.read().unwrap_or_else(|e| e.into_inner())
}

pub fn set_default_zone(tz: Tz) {
    *ZONE.write().unwrap_or_else(|e| e.into_inner()) = tz;
}

/// `Paris time`, `UTC`.
pub fn zone_label(tz: Tz) -> String {
    match tz.name().rsplit_once('/') {
        Some((_, city)) => format!("{} time", city.replace('_', " ")),
        None => tz.name().to_string(),
    }
}

/// What a time's hover says of its zone: `Paris time (Europe/Paris, CEST, UTC+02:00)`.
pub fn zone_note(t: DateTime<Tz>) -> String {
    let tz = t.timezone();
    if tz.name() == "UTC" {
        return "UTC".into();
    }
    format!("{} ({}, {}, UTC{})", zone_label(tz), tz.name(), t.format("%Z"), t.format("%:z"))
}

/// Every zone a person may pick, by name.
pub fn zone_names() -> impl Iterator<Item = &'static str> {
    chrono_tz::TZ_VARIANTS.iter().map(|z| z.name()).filter(|n| *n == "UTC" || (n.contains('/') && !n.starts_with("Etc/") && !n.starts_with("US/")))
}

/// When an event (the first VEVENT of `ics`, its series for repeating
/// ones) starts.
pub fn first_start(ics: &str) -> Option<When> {
    let cals = ical::parse(ics).ok()?;
    let (master, moved) = events(cals.first()?);
    let ev = master.or(moved.first().copied())?;
    ev.prop("DTSTART").and_then(|p| parse_value(p, &p.value)).map(when)
}

/// Windows names Outlook and Exchange use, for the zones likely to be met.
const WINDOWS: &[(&str, &str)] = &[
    ("Romance Standard Time", "Europe/Paris"),
    ("W. Europe Standard Time", "Europe/Berlin"),
    ("Central Europe Standard Time", "Europe/Budapest"),
    ("Central European Standard Time", "Europe/Warsaw"),
    ("GMT Standard Time", "Europe/London"),
    ("Greenwich Standard Time", "Atlantic/Reykjavik"),
    ("E. Europe Standard Time", "Europe/Chisinau"),
    ("FLE Standard Time", "Europe/Kyiv"),
    ("GTB Standard Time", "Europe/Bucharest"),
    ("Russian Standard Time", "Europe/Moscow"),
    ("Eastern Standard Time", "America/New_York"),
    ("Central Standard Time", "America/Chicago"),
    ("Mountain Standard Time", "America/Denver"),
    ("Pacific Standard Time", "America/Los_Angeles"),
    ("China Standard Time", "Asia/Shanghai"),
    ("Tokyo Standard Time", "Asia/Tokyo"),
    ("India Standard Time", "Asia/Kolkata"),
    ("AUS Eastern Standard Time", "Australia/Sydney"),
    ("UTC", "UTC"),
    ("Coordinated Universal Time", "UTC"),
];

/// The zone a TZID means: an IANA name, a Windows name, or a label naming
/// one (`/mozilla.org/…/Europe/Paris`, `(UTC+01:00) Brussels, Copenhagen, Madrid, Paris`).
pub fn zone(tzid: &str) -> Option<Tz> {
    let t = tzid.trim().trim_matches('"');
    if let Ok(z) = t.parse::<Tz>() {
        return Some(z);
    }
    if let Some((_, iana)) = WINDOWS.iter().find(|(w, _)| w.eq_ignore_ascii_case(t)) {
        return iana.parse().ok();
    }
    let zones = chrono_tz::TZ_VARIANTS.iter().filter(|z| z.name().contains('/'));
    zones.clone().find(|z| t.contains(z.name())).or_else(|| {
        zones.into_iter().find(|z| {
            let city = z.name().rsplit('/').next().unwrap_or("").replace('_', " ");
            city.len() > 3 && t.contains(&city)
        })
    })
    .copied()
}

/// A wall-clock time in a zone (the hour after, in a daylight-saving gap).
fn local(tz: Tz, naive: NaiveDateTime) -> DateTime<Tz> {
    tz.from_local_datetime(&naive)
        .earliest()
        .or_else(|| tz.from_local_datetime(&(naive + Duration::hours(1))).earliest())
        .unwrap_or_else(|| tz.from_utc_datetime(&naive))
}

/// When day `d` starts in `tz`, in Unix seconds.
pub fn midnight(tz: Tz, d: NaiveDate) -> i64 {
    local(tz, d.and_time(NaiveTime::MIN)).timestamp()
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Start {
    Day(NaiveDate),
    Time(DateTime<Tz>),
}

/// One DATE or DATE-TIME value, read with its property's parameters.
fn parse_value(p: &Property, v: &str) -> Option<Start> {
    let v = v.trim();
    if p.param("VALUE").is_some_and(|x| x.eq_ignore_ascii_case("DATE")) || v.len() == 8 {
        return NaiveDate::parse_from_str(v, "%Y%m%d").ok().map(Start::Day);
    }
    let (s, utc) = match v.strip_suffix('Z') {
        Some(s) => (s, true),
        None => (v, false),
    };
    let naive = NaiveDateTime::parse_from_str(s, "%Y%m%dT%H%M%S").ok()?;
    let tz = if utc { Tz::UTC } else { p.param("TZID").and_then(zone).unwrap_or_else(default_zone) };
    Some(Start::Time(local(tz, naive)))
}

fn when(s: Start) -> When {
    match s {
        Start::Day(d) => When::Day(d),
        Start::Time(t) => When::At(t.timestamp()),
    }
}

/// How long an event lasts: days for all-day ones, seconds otherwise.
fn length(ev: &Component, start: Start) -> i64 {
    let end = ev.prop("DTEND").and_then(|p| parse_value(p, &p.value));
    match (start, end) {
        (Start::Day(a), Some(Start::Day(b))) => (b - a).num_days().max(1),
        (Start::Time(a), Some(Start::Time(b))) => (b - a).num_seconds().max(0),
        _ => match ev.value("DURATION").and_then(ical::parse_duration) {
            Some(d) if matches!(start, Start::Day(_)) => (d / 86400).max(1),
            Some(d) => d.max(0),
            None => i64::from(matches!(start, Start::Day(_))),
        },
    }
}

fn shifted(s: Start, len: i64) -> When {
    match s {
        Start::Day(d) => When::Day(d + Duration::days(len)),
        Start::Time(t) => When::At(t.timestamp() + len),
    }
}

/// Unix seconds of a start or end; a day counts from its midnight in `tz`
/// (all-day events are dates, which start when that day does where one is).
fn instant(w: When, tz: Tz) -> i64 {
    match w {
        When::Day(d) => local(tz, d.and_time(NaiveTime::MIN)).timestamp(),
        When::At(t) => t,
    }
}

/// Whether [start, end) overlaps [from, to), as RFC 4791's time-range has
/// it: an event without length is in the range its start is in.
fn overlaps(start: When, end: When, from: i64, to: i64, tz: Tz) -> bool {
    let (s, e) = (instant(start, tz), instant(end, tz));
    if s == e { s >= from && s < to } else { s < to && e > from }
}

/// In the repeat engine's terms (a day is its midnight UTC).
fn engine_time(s: Start) -> DateTime<rrule::Tz> {
    match s {
        Start::Day(d) => rrule::Tz::UTC.from_utc_datetime(&d.and_time(NaiveTime::MIN)),
        Start::Time(t) => t.with_timezone(&rrule::Tz::Tz(t.timezone())),
    }
}

/// Every value of the properties `name` (EXDATE, RDATE: lists, several times).
fn dates(ev: &Component, name: &str) -> Vec<Start> {
    ev.props.iter().filter(|p| p.name == name).flat_map(|p| p.value.split(',').filter_map(|v| parse_value(p, v))).collect()
}

/// Whether an EXDATE or a RECURRENCE-ID names occurrence `s`: the same
/// instant, or the same day when one is a date and the other a time (some
/// apps write a timed series' exceptions as dates).
fn same_slot(x: Start, s: Start) -> bool {
    match (x, s) {
        (Start::Day(d), Start::Time(t)) | (Start::Time(t), Start::Day(d)) => t.date_naive() == d,
        (a, b) => when(a).secs() == when(b).secs(),
    }
}

/// Starts of a repeating event within [from, to): its first, its RRULEs
/// and RDATEs, less its EXDATEs; its first only if a rule can't be read.
fn occurrences(ev: &Component, start: Start, len: i64, from: i64, to: i64, tz: Tz) -> Vec<Start> {
    let dt = engine_time(start);
    let mut rules = Vec::new();
    for p in ev.props.iter().filter(|p| p.name == "RRULE") {
        let mut value = p.value.clone();
        // A date-only UNTIL on a timed event: the end of that day, in UTC.
        if let (Start::Time(_), Some(i)) = (start, value.find("UNTIL=")) {
            let until = &value[i + 6..];
            let end = until.find(';').unwrap_or(until.len());
            if end == 8 {
                value.insert_str(i + 6 + 8, "T235959Z");
            }
        }
        match value.parse::<rrule::RRule<rrule::Unvalidated>>().ok().and_then(|r| r.validate(dt).ok()) {
            Some(r) => rules.push(r),
            None => return if overlaps(when(start), shifted(start, len), from, to, tz) { vec![start] } else { vec![] },
        }
    }
    // Days are midnights UTC to the engine, and local ones here: a day more
    // each side, then the exact test below.
    let span = if matches!(start, Start::Day(_)) { len * 86400 } else { len };
    let utc = |t: i64| rrule::Tz::UTC.timestamp_opt(t, 0).single().unwrap_or(dt);
    let set = rrule::RRuleSet::new(dt)
        .set_rrules(rules)
        .set_rdates(dates(ev, "RDATE").into_iter().map(engine_time).collect())
        .after(utc(from.saturating_sub(span + 86400)))
        .before(utc(to.saturating_add(86400)));
    let mut found: Vec<Start> = set
        .all(5000)
        .dates
        .into_iter()
        .map(|d| match start {
            Start::Day(_) => Start::Day(d.date_naive()),
            Start::Time(t) => Start::Time(d.with_timezone(&t.timezone())),
        })
        .collect();
    // The first time is always one (RFC 5545 3.8.5.2), even when the rule
    // doesn't fall on it or there are only RDATEs.
    if !found.iter().any(|s| same_slot(*s, start)) {
        found.push(start);
    }
    let exdates = dates(ev, "EXDATE");
    found.retain(|s| !exdates.iter().any(|x| same_slot(*x, *s)) && overlaps(when(*s), shifted(*s, len), from, to, tz));
    found.sort_by_key(|s| when(*s).secs());
    found.dedup_by_key(|s| when(*s).secs());
    found
}

/// An iCalendar TEXT value as written (`\,` `\;` `\n` `\\`).
pub fn unescape(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n' | 'N') => out.push('\n'),
            Some(c) => out.push(c),
            None => {}
        }
    }
    out
}

pub fn escape(v: &str) -> String {
    v.replace('\\', "\\\\").replace(';', "\\;").replace(',', "\\,").replace("\r\n", "\\n").replace('\n', "\\n")
}

/// The VEVENTs of an object's calendar: the series (no RECURRENCE-ID) and
/// the occurrences moved or changed.
fn events(cal: &Component) -> (Option<&Component>, Vec<&Component>) {
    let all: Vec<&Component> = cal.children.iter().filter(|c| c.name == "VEVENT").collect();
    let master = all.iter().find(|e| e.prop("RECURRENCE-ID").is_none()).copied();
    (master, all.into_iter().filter(|e| e.prop("RECURRENCE-ID").is_some()).collect())
}

fn is_cancelled(ev: &Component) -> bool {
    ev.value("STATUS").is_some_and(|s| s.eq_ignore_ascii_case("CANCELLED"))
}

/// One occurrence of an event, as the engine finds it.
#[derive(Clone, Debug)]
pub struct Occurrence<'a> {
    pub start: When,
    /// Excluded: the day after an all-day event's last, or the end instant.
    pub end: When,
    /// The VEVENT it comes from: the series', or the one that moved or
    /// changed this occurrence.
    pub event: &'a Component,
    /// The series' VEVENT, for what an occurrence's own leaves to it.
    pub master: Option<&'a Component>,
    /// For an occurrence of a series, the start it has there (its
    /// RECURRENCE-ID); `None` for an event that doesn't repeat.
    pub recurrence_id: Option<When>,
    /// Called off as a whole (STATUS:CANCELLED, as an organizer's
    /// cancellation leaves it). A series' occurrence cancelled on its own is
    /// a deleted one: it isn't found at all.
    pub cancelled: bool,
}

/// The one place valet finds when an event happens: the occurrences of an
/// object (a VCALENDAR) that overlap [from, to), Unix seconds, in order.
/// Repeats are expanded from DTSTART, RRULE, RDATE and EXDATE, in the
/// event's own zone (TZID, so an hour stays put across daylight-saving
/// changes); RECURRENCE-ID overrides move, change or (cancelled) delete
/// occurrences. All-day events stay dates, starting at midnight in `tz`.
pub fn occurrences_in(cal: &Component, from: i64, to: i64, tz: Tz) -> Vec<Occurrence<'_>> {
    let (master, overrides) = events(cal);
    let repeats = master.is_some_and(|m| m.prop("RRULE").is_some() || m.prop("RDATE").is_some());
    let master_cancelled = master.is_some_and(is_cancelled);
    let replaced: Vec<Start> = overrides.iter().filter_map(|e| e.prop("RECURRENCE-ID").and_then(|p| parse_value(p, &p.value))).collect();
    let mut out = Vec::new();
    if let Some(m) = master
        && let Some(start) = m.prop("DTSTART").and_then(|p| parse_value(p, &p.value))
    {
        let len = length(m, start);
        let starts = if repeats { occurrences(m, start, len, from, to, tz) } else { vec![start] };
        for s in starts {
            if repeats && replaced.iter().any(|r| same_slot(*r, s)) {
                continue;
            }
            let (a, b) = (when(s), shifted(s, len));
            if overlaps(a, b, from, to, tz) {
                out.push(Occurrence { start: a, end: b, event: m, master, recurrence_id: repeats.then_some(a), cancelled: master_cancelled });
            }
        }
    }
    for e in overrides {
        let cancelled = is_cancelled(e);
        if cancelled && master.is_some() && !master_cancelled {
            continue;
        }
        let Some(start) = e.prop("DTSTART").and_then(|p| parse_value(p, &p.value)) else { continue };
        let (a, b) = (when(start), shifted(start, length(e, start)));
        let rid = e.prop("RECURRENCE-ID").and_then(|p| parse_value(p, &p.value)).map(when);
        if overlaps(a, b, from, to, tz) {
            out.push(Occurrence { start: a, end: b, event: e, master, recurrence_id: rid.or(Some(a)), cancelled });
        }
    }
    out.sort_by_key(|o| (instant(o.start, tz), !matches!(o.start, When::Day(_))));
    out
}

/// The occurrences of an event that overlap [from, to) (Unix seconds); `me`,
/// my addresses, tells the invitations I haven't answered.
pub fn instances(o: &Object, from: i64, to: i64, me: &[String]) -> Vec<Instance> {
    instances_where(o, from, to, me, |_| true)
}

/// Whether `ev` (or the series it leaves its people to) invites one of `me`
/// who hasn't answered: no PARTSTAT is NEEDS-ACTION. The organizer listed
/// among the attendees isn't invited (but another of my addresses may be).
fn unanswered(ev: &Component, master: Option<&Component>, me: &[String]) -> bool {
    let people = if ev.prop("ATTENDEE").is_some() { ev } else { master.unwrap_or(ev) };
    let organizer = people.value("ORGANIZER").map(ical::mailto).unwrap_or_default();
    people.props.iter().any(|p| {
        let a = ical::mailto(&p.value);
        p.name == "ATTENDEE"
            && me.iter().any(|m| m.eq_ignore_ascii_case(&a))
            && !a.eq_ignore_ascii_case(&organizer)
            && p.param("PARTSTAT").is_none_or(|s| s.eq_ignore_ascii_case("NEEDS-ACTION"))
    })
}

/// The occurrences that overlap [from, to) and whose VEVENT (with what it
/// leaves to the series' own) passes `keep`.
fn instances_where(o: &Object, from: i64, to: i64, me: &[String], keep: impl Fn(&dyn Fn(&str) -> Option<String>) -> bool) -> Vec<Instance> {
    let Ok(cals) = ical::parse(&o.data) else { return vec![] };
    let Some(cal) = cals.first() else { return vec![] };
    let value = |occ: &Occurrence, name: &str| occ.event.value(name).or_else(|| occ.master.and_then(|m| m.value(name))).map(unescape);
    let text = |occ: &Occurrence, name: &str| value(occ, name).unwrap_or_default();
    occurrences_in(cal, from, to, default_zone())
        .into_iter()
        .filter(|occ| keep(&|name| value(occ, name)))
        .map(|occ| Instance {
            calendar: o.calendar.clone(),
            name: o.name.clone(),
            uid: o.uid.clone(),
            title: text(&occ, "SUMMARY"),
            location: text(&occ, "LOCATION"),
            notes: text(&occ, "DESCRIPTION"),
            all_day: matches!(occ.start, When::Day(_)),
            start: occ.start,
            end: occ.end,
            repeats: occ.recurrence_id.is_some(),
            age: age(occ.master.unwrap_or(occ.event), occ.start),
            cancelled: occ.cancelled,
            unanswered: unanswered(occ.event, occ.master, me),
            invite: None,
            mail: None,
            alert: None,
            alert_by: None,
        })
        .collect()
}

/// Whether an object (its iCalendar text) has an occurrence overlapping
/// [from, to): CalDAV's time-range filter.
pub fn happens(data: &str, from: i64, to: i64) -> bool {
    ical::parse(data).ok().and_then(|cals| cals.into_iter().next()).is_some_and(|cal| !occurrences_in(&cal, from, to, default_zone()).is_empty())
}

/// An object as CalDAV's `expand` returns it (RFC 4791 9.6.5): each of its
/// occurrences overlapping [from, to) a VEVENT of its own, with its
/// RECURRENCE-ID when it belongs to a series, times in UTC (all-day events
/// keep their dates), no repeat rules and no VTIMEZONE.
pub fn expanded(data: &str, from: i64, to: i64) -> Option<String> {
    let cals = ical::parse(data).ok()?;
    let cal = cals.first()?;
    let mut out = Component::new("VCALENDAR");
    out.props = cal.props.clone();
    let value = |w: When| match w {
        When::Day(d) => (";VALUE=DATE".to_string(), d.format("%Y%m%d").to_string()),
        When::At(t) => (String::new(), DateTime::<Utc>::from_timestamp(t, 0).unwrap_or_default().format("%Y%m%dT%H%M%SZ").to_string()),
    };
    for occ in occurrences_in(cal, from, to, default_zone()) {
        let mut ev = occ.event.clone();
        ev.props.retain(|p| !matches!(p.name.as_str(), "DTSTART" | "DTEND" | "DURATION" | "RECURRENCE-ID" | "RRULE" | "RDATE" | "EXDATE" | "EXRULE"));
        let at = ev.props.iter().position(|p| p.name == "UID").map_or(0, |i| i + 1);
        let mut times = vec![("DTSTART", occ.start), ("DTEND", occ.end)];
        if let Some(rid) = occ.recurrence_id {
            times.push(("RECURRENCE-ID", rid));
        }
        for (i, (name, w)) in times.into_iter().enumerate() {
            let (params, value) = value(w);
            ev.props.insert(at + i, Property { name: name.into(), params, value });
        }
        out.children.push(ev);
    }
    Some(out.to_ics())
}

/// The events of `objects` an invitation (`ics`) would clash with: timed
/// ones that overlap its timed occurrences (the first two months of them when
/// it repeats), busy, and not the invitation's own copy.
pub fn conflicts(ics: &str, objects: &[Object], now: i64) -> Vec<Instance> {
    let Ok(cals) = ical::parse(ics) else { return vec![] };
    let Some(cal) = cals.first() else { return vec![] };
    let uid = events(cal).0.and_then(|m| m.value("UID")).unwrap_or_default();
    let from = first_start(ics).map_or(now, |w| w.secs().max(now));
    let times: Vec<(i64, i64)> = occurrences_in(cal, from, from + 60 * 86400, default_zone())
        .into_iter()
        .filter(|occ| !occ.cancelled)
        .filter_map(|occ| match (occ.start, occ.end) {
            (When::At(a), When::At(b)) => Some((a, b)),
            _ => None,
        })
        .collect();
    let (Some(lo), Some(hi)) = (times.iter().map(|t| t.0).min(), times.iter().map(|t| t.1).max()) else { return vec![] };
    let busy = |v: &dyn Fn(&str) -> Option<String>| {
        !v("STATUS").is_some_and(|s| s.eq_ignore_ascii_case("CANCELLED")) && !v("TRANSP").is_some_and(|s| s.eq_ignore_ascii_case("TRANSPARENT"))
    };
    let mut out: Vec<Instance> = objects
        .iter()
        .filter(|o| o.uid != uid)
        .flat_map(|o| instances_where(o, lo, hi, &[], busy))
        .filter(|i| match (i.start, i.end) {
            (When::At(a), When::At(b)) => times.iter().any(|&(s, f)| a < f && s < b),
            _ => false,
        })
        .collect();
    out.sort_by_key(|i| i.start.secs());
    out
}

/// An alarm to ring, or a notification to show (valet's Android app).
#[derive(Clone, Debug, Serialize)]
pub struct Ring {
    /// Unix seconds.
    pub at: i64,
    pub title: String,
    pub start: When,
    /// The pattern it comes from.
    #[serde(skip)]
    pub tag: String,
    /// A notification rather than an alarm.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub notify: bool,
}

/// What an event marked as a birthday (`X-VALET-BIRTHDAY`, valet's
/// birthdays) says of the year: `Some(None)` when it isn't known.
pub fn birthday(ev: &Component) -> Option<Option<i32>> {
    ev.value("X-VALET-BIRTHDAY").map(|v| v.trim().parse().ok())
}

/// The age a birthday's occurrence starting `start` is, when its year is known.
pub fn age(ev: &Component, start: When) -> Option<i64> {
    let year = birthday(ev)??;
    let on = match start {
        When::Day(d) => d.year(),
        When::At(t) => DateTime::<Utc>::from_timestamp(t, 0)?.year(),
    };
    Some(i64::from(on - year)).filter(|a| *a >= 0)
}

/// The alarms of patterns tagged `tags` that go off within [from, to);
/// all-day events start at midnight in `tz` (the phone's).
pub fn rings(o: &Object, tags: &[String], from: i64, to: i64, tz: Tz) -> Vec<Ring> {
    let Ok(cals) = ical::parse(&o.data) else { return vec![] };
    let Some(cal) = cals.first() else { return vec![] };
    // Alerts go off up to a few weeks before or after the start.
    let reach = 30 * 86400;
    let mut out = Vec::new();
    for occ in occurrences_in(cal, from - reach, to + reach, tz) {
        let (start, e) = (occ.start, occ.event);
        let base = instant(start, tz);
        for a in e.children.iter().filter(|c| c.name == "VALARM") {
            if !a.value("X-VALET").is_some_and(|t| tags.iter().any(|x| x.eq_ignore_ascii_case(t))) {
                continue;
            }
            let Some(t) = a.prop("TRIGGER") else { continue };
            if t.param("VALUE").is_some_and(|v| v.eq_ignore_ascii_case("DATE-TIME")) || t.param("RELATED").is_some_and(|v| v.eq_ignore_ascii_case("END")) {
                continue;
            }
            let Some(offset) = ical::parse_duration(&t.value) else { continue };
            let at = base + offset;
            if at >= from && at < to {
                let mut title = e.value("SUMMARY").or_else(|| occ.master.and_then(|m| m.value("SUMMARY"))).map(unescape).unwrap_or_default();
                if let Some(n) = age(occ.master.unwrap_or(e), start) {
                    title = format!("{title} turns {n}");
                }
                out.push(Ring { at, title, start, tag: a.value("X-VALET").unwrap_or("").to_string(), notify: false });
            }
        }
    }
    out
}

fn ordinal(n: i64) -> String {
    let suffix = match (n % 10, n % 100) {
        (1, x) if x != 11 => "st",
        (2, x) if x != 12 => "nd",
        (3, x) if x != 13 => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

fn and_list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

const WEEKDAYS: &[(&str, &str)] =
    &[("MO", "Monday"), ("TU", "Tuesday"), ("WE", "Wednesday"), ("TH", "Thursday"), ("FR", "Friday"), ("SA", "Saturday"), ("SU", "Sunday")];

/// A repeat rule said for people: `every week on Tuesday and Thursday at
/// 10:00, until 31/12/2026`.
fn describe_rule(rule: &str, start: Start) -> String {
    let parts: Vec<(String, String)> = rule
        .split(';')
        .filter_map(|p| p.split_once('='))
        .map(|(k, v)| (k.to_ascii_uppercase(), v.to_ascii_uppercase()))
        .collect();
    let get = |k: &str| parts.iter().find(|(x, _)| x == k).map(|(_, v)| v.as_str());
    let n = get("INTERVAL").and_then(|v| v.parse::<i64>().ok()).filter(|&n| n > 1);
    let unit = match get("FREQ") {
        Some("DAILY") => "day",
        Some("WEEKLY") => "week",
        Some("MONTHLY") => "month",
        Some("YEARLY") => "year",
        _ => return "repeats".into(),
    };
    let mut out = match n {
        Some(n) => format!("every {n} {unit}s"),
        None => format!("every {unit}"),
    };
    let date = match start {
        Start::Day(d) => d,
        Start::Time(t) => t.date_naive(),
    };
    let days: Vec<&str> = get("BYDAY").map(|v| v.split(',').collect()).unwrap_or_default();
    let day_name = |code: &str| WEEKDAYS.iter().find(|(c, _)| code.ends_with(c)).map(|(_, n)| n.to_string());
    if !days.is_empty() {
        let plain: Option<Vec<String>> = days.iter().map(|d| (d.len() == 2).then(|| day_name(d)).flatten()).collect();
        match plain {
            Some(names) if unit == "day" && names.len() == 5 && !names.iter().any(|n| n.starts_with('S')) && n.is_none() => out = "every weekday".into(),
            Some(names) => out.push_str(&format!(" on {}", and_list(&names))),
            None => {
                let named: Vec<String> = days
                    .iter()
                    .filter_map(|d| {
                        let (num, code) = d.split_at(d.len().saturating_sub(2));
                        let name = day_name(code)?;
                        Some(match num.parse::<i64>() {
                            Ok(-1) => format!("the last {name}"),
                            Ok(k) if k > 0 => format!("the {} {name}", ordinal(k)),
                            _ => name,
                        })
                    })
                    .collect();
                out.push_str(&format!(" on {}", and_list(&named)));
            }
        }
    } else if let Some(md) = get("BYMONTHDAY") {
        let named: Vec<String> = md.split(',').filter_map(|d| d.parse::<i64>().ok()).map(|d| if d == -1 { "the last day".into() } else { format!("the {}", ordinal(d)) }).collect();
        out.push_str(&format!(" on {}", and_list(&named)));
    } else {
        match unit {
            "week" => out.push_str(&format!(" on {}", date.format("%A"))),
            "month" => out.push_str(&format!(" on the {}", ordinal(i64::from(date.day())))),
            "year" => out.push_str(&format!(" on {}", date.format("%-d %B"))),
            _ => {}
        }
    }
    if let Start::Time(t) = start {
        out.push_str(&format!(" at {}", t.format("%H:%M")));
    }
    if let Some(c) = get("COUNT") {
        out.push_str(&format!(", {c} times"));
    } else if let Some(u) = get("UNTIL").and_then(|u| NaiveDate::parse_from_str(u.get(..8)?, "%Y%m%d").ok()) {
        out.push_str(&format!(", until {}", u.format("%d/%m/%Y")));
    }
    out
}

/// For a repeating event: how it repeats, its first time (`23/10/2026
/// 10:00`), and the zone both are in (for timed events: their own).
pub fn describe(o: &Object) -> Option<(String, String, Option<DateTime<Tz>>)> {
    let cals = ical::parse(&o.data).ok()?;
    let (master, _) = events(cals.first()?);
    let m = master?;
    let start = match m.prop("DTSTART").and_then(|p| parse_value(p, &p.value))? {
        // Said in UTC, it's said for no one: in the settings' zone instead.
        Start::Time(t) if t.timezone() == Tz::UTC => Start::Time(t.with_timezone(&default_zone())),
        s => s,
    };
    let first = match start {
        Start::Day(d) => d.format("%d/%m/%Y").to_string(),
        Start::Time(t) => t.format("%d/%m/%Y %H:%M").to_string(),
    };
    let rules: Vec<String> = m.props.iter().filter(|p| p.name == "RRULE").map(|p| describe_rule(&p.value, start)).collect();
    let mut text = match rules.as_slice() {
        [] if m.prop("RDATE").is_some() => "on several dates".to_string(),
        [] => return None,
        r => r.join("; "),
    };
    if let Some(c) = text.get(..1) {
        text = c.to_uppercase() + &text[1..];
    }
    let zoned = match start {
        Start::Day(_) => None,
        Start::Time(t) => Some(t),
    };
    Some((text, first, zoned))
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Repeat {
    None,
    Daily,
    Weekly,
    Monthly,
    Yearly,
    /// A rule the form can't show: kept as it is.
    Custom,
}

impl Repeat {
    pub fn parse(s: &str) -> Repeat {
        match s {
            "daily" => Repeat::Daily,
            "weekly" => Repeat::Weekly,
            "monthly" => Repeat::Monthly,
            "yearly" => Repeat::Yearly,
            "custom" => Repeat::Custom,
            _ => Repeat::None,
        }
    }

    fn of(ev: &Component) -> Repeat {
        let rules: Vec<&str> = ev.props.iter().filter(|p| p.name == "RRULE").map(|p| p.value.as_str()).collect();
        if rules.is_empty() && ev.prop("RDATE").is_none() {
            return Repeat::None;
        }
        let simple = |r: &str| {
            let parts: Vec<&str> = r.split(';').filter(|p| !p.eq_ignore_ascii_case("INTERVAL=1") && !p.to_ascii_uppercase().starts_with("WKST=")).collect();
            match parts.as_slice() {
                [f] => Some(f.to_ascii_uppercase()),
                _ => None,
            }
        };
        match (rules.as_slice(), ev.prop("RDATE")) {
            ([r], None) => match simple(r).as_deref() {
                Some("FREQ=DAILY") => Repeat::Daily,
                Some("FREQ=WEEKLY") => Repeat::Weekly,
                Some("FREQ=MONTHLY") => Repeat::Monthly,
                Some("FREQ=YEARLY") => Repeat::Yearly,
                _ => Repeat::Custom,
            },
            _ => Repeat::Custom,
        }
    }

    fn rule(self) -> Option<&'static str> {
        Some(match self {
            Repeat::Daily => "FREQ=DAILY",
            Repeat::Weekly => "FREQ=WEEKLY",
            Repeat::Monthly => "FREQ=MONTHLY",
            Repeat::Yearly => "FREQ=YEARLY",
            Repeat::None | Repeat::Custom => return None,
        })
    }
}

/// An event as the web form edits it (the whole series, for repeating ones).
#[derive(Clone, Debug, Serialize)]
pub struct Editable {
    pub calendar: String,
    pub name: String,
    pub title: String,
    pub location: String,
    pub notes: String,
    #[serde(rename = "allDay")]
    pub all_day: bool,
    pub start: When,
    /// Excluded, like the iCalendar end.
    pub end: When,
    pub repeat: Repeat,
    /// Invitations to and from others: their time is the organizer's.
    pub meeting: bool,
}

pub fn editable(o: &Object) -> Option<Editable> {
    let cals = ical::parse(&o.data).ok()?;
    let (master, moved) = events(cals.first()?);
    let ev = master.or(moved.first().copied())?;
    let start = ev.prop("DTSTART").and_then(|p| parse_value(p, &p.value))?;
    let text = |name: &str| ev.value(name).map(unescape).unwrap_or_default();
    Some(Editable {
        calendar: o.calendar.clone(),
        name: o.name.clone(),
        title: text("SUMMARY"),
        location: text("LOCATION"),
        notes: text("DESCRIPTION"),
        all_day: matches!(start, Start::Day(_)),
        start: when(start),
        end: shifted(start, length(ev, start)),
        repeat: Repeat::of(ev),
        meeting: ev.prop("ORGANIZER").is_some() || ev.prop("ATTENDEE").is_some(),
    })
}

/// What the form sends.
#[derive(Clone, Debug)]
pub struct Edit {
    pub title: String,
    pub location: String,
    pub notes: String,
    pub all_day: bool,
    pub start: NaiveDateTime,
    /// For all-day events, the last day (included).
    pub end: NaiveDateTime,
    /// The zone the times were typed in.
    pub tz: Tz,
    pub repeat: Repeat,
}

fn set(ev: &mut Component, name: &str, params: &str, value: &str) {
    ev.props.retain(|p| p.name != name);
    ev.props.push(Property { name: name.into(), params: params.into(), value: value.into() });
}

fn set_text(ev: &mut Component, name: &str, value: &str) {
    ev.props.retain(|p| p.name != name);
    if !value.trim().is_empty() {
        ev.props.push(Property { name: name.into(), params: String::new(), value: escape(value.trim()) });
    }
}

fn stamp(t: DateTime<Utc>) -> String {
    t.format("%Y%m%dT%H%M%SZ").to_string()
}

/// Writes the form into the event (`existing`, an iCalendar object) or a new
/// one. Returns (UID, iCalendar text). The rest of an existing event
/// (attendees, alarms, moved occurrences…) stays as it was.
pub fn write(existing: Option<&str>, e: &Edit) -> Result<(String, String)> {
    let title = e.title.trim();
    ensure!(!title.is_empty(), "the event needs a title");
    if e.all_day {
        ensure!(e.end.date() >= e.start.date(), "it ends before it starts");
    } else {
        ensure!(e.end >= e.start, "it ends before it starts");
    }
    let now = Utc::now();
    let mut cal = match existing {
        Some(text) => {
            let mut cals = ical::parse(text)?;
            ensure!(cals.len() == 1, "that event can't be read");
            cals.remove(0)
        }
        None => {
            let mut cal = Component::new("VCALENDAR");
            cal.push("VERSION", "2.0");
            cal.push("PRODID", "-//valet//EN");
            let mut ev = Component::new("VEVENT");
            ev.push("UID", &format!("{}@valet", uuid::Uuid::new_v4()));
            ev.push("CREATED", &stamp(now));
            cal.children.push(ev);
            cal
        }
    };
    let i = cal
        .children
        .iter()
        .position(|c| c.name == "VEVENT" && c.prop("RECURRENCE-ID").is_none())
        .or_else(|| cal.children.iter().position(|c| c.name == "VEVENT"))
        .context("that event can't be read")?;
    let ev = &mut cal.children[i];
    let uid = ev.value("UID").context("that event has no UID")?.to_string();
    set_text(ev, "SUMMARY", title);
    set_text(ev, "LOCATION", &e.location);
    set_text(ev, "DESCRIPTION", &e.notes);
    ev.props.retain(|p| p.name != "DURATION");
    let repeat = match e.repeat {
        Repeat::Custom if Repeat::of(ev) == Repeat::Custom => Repeat::Custom,
        Repeat::Custom => bail!("pick how it repeats"),
        r => r,
    };
    if e.all_day {
        set(ev, "DTSTART", ";VALUE=DATE", &e.start.format("%Y%m%d").to_string());
        set(ev, "DTEND", ";VALUE=DATE", &(e.end.date() + Duration::days(1)).format("%Y%m%d").to_string());
    } else if repeat == Repeat::None {
        // Once: in UTC, which every app reads the same.
        let utc = |t: NaiveDateTime| stamp(local(e.tz, t).with_timezone(&Utc));
        set(ev, "DTSTART", "", &utc(e.start));
        set(ev, "DTEND", "", &utc(e.end));
    } else {
        // Repeating: in its zone, so it keeps its hour across daylight saving.
        let params = format!(";TZID={}", e.tz.name());
        set(ev, "DTSTART", &params, &e.start.format("%Y%m%dT%H%M%S").to_string());
        set(ev, "DTEND", &params, &e.end.format("%Y%m%dT%H%M%S").to_string());
    }
    if repeat != Repeat::Custom {
        ev.props.retain(|p| p.name != "RRULE" && p.name != "RDATE" && p.name != "EXDATE");
        if let Some(rule) = repeat.rule() {
            ev.push("RRULE", rule);
        }
    }
    let sequence = ev.value("SEQUENCE").and_then(|s| s.parse::<i64>().ok());
    if existing.is_some() {
        set(ev, "SEQUENCE", "", &(sequence.unwrap_or(0) + 1).to_string());
    }
    set(ev, "DTSTAMP", "", &stamp(now));
    set(ev, "LAST-MODIFIED", "", &stamp(now));
    Ok((uid, cal.to_ics()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(data: &str) -> Object {
        Object {
            calendar: "c".into(),
            name: "e.ics".into(),
            uid: "u".into(),
            data: if data.contains('\r') { data.to_string() } else { data.replace('\n', "\r\n") },
            etag: String::new(),
            rev: 0,
            summary: String::new(),
            dtstart: String::new(),
            recurring: false,
        }
    }

    fn at(s: &str) -> i64 {
        DateTime::parse_from_rfc3339(s).unwrap().timestamp()
    }

    fn ev(body: &str) -> Object {
        object(&format!("BEGIN:VCALENDAR\nVERSION:2.0\nBEGIN:VEVENT\nUID:u\n{body}\nEND:VEVENT\nEND:VCALENDAR\n"))
    }

    #[test]
    fn single_events_in_utc_zones_and_days() {
        let o = ev("SUMMARY:Lunch\\, at Bob's\nDTSTART;TZID=Europe/Paris:20261023T120000\nDTEND;TZID=Europe/Paris:20261023T133000");
        let i = instances(&o, at("2026-10-01T00:00:00Z"), at("2026-11-01T00:00:00Z"), &[]);
        assert_eq!(i.len(), 1);
        assert_eq!((i[0].start, i[0].end), (When::At(at("2026-10-23T10:00:00Z")), When::At(at("2026-10-23T11:30:00Z"))));
        assert_eq!(i[0].title, "Lunch, at Bob's");
        assert!(instances(&o, at("2026-10-24T00:00:00Z"), at("2026-11-01T00:00:00Z"), &[]).is_empty());

        let day = ev("SUMMARY:Trip\nDTSTART;VALUE=DATE:20261030\nDTEND;VALUE=DATE:20261102");
        let i = instances(&day, at("2026-11-01T00:00:00Z"), at("2026-12-01T00:00:00Z"), &[]);
        assert_eq!((i[0].start, i[0].end, i[0].all_day), (When::Day(NaiveDate::from_ymd_opt(2026, 10, 30).unwrap()), When::Day(NaiveDate::from_ymd_opt(2026, 11, 2).unwrap()), true));

        let outlook = ev("DTSTART;TZID=\"Romance Standard Time\":20260715T090000\nDURATION:PT1H");
        let i = instances(&outlook, at("2026-07-01T00:00:00Z"), at("2026-08-01T00:00:00Z"), &[]);
        assert_eq!((i[0].start, i[0].end), (When::At(at("2026-07-15T07:00:00Z")), When::At(at("2026-07-15T08:00:00Z"))));
        assert_eq!(zone("/mozilla.org/20050126_1/Europe/Paris"), Some(Tz::Europe__Paris));
        assert_eq!(zone("(UTC+01:00) Brussels, Copenhagen, Madrid, Paris"), Some(Tz::Europe__Brussels));
    }

    #[test]
    fn repeating_events_keep_their_hour_skip_exceptions_and_follow_moves() {
        let o = object(
            "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:u\nSUMMARY:Standup\nDTSTART;TZID=Europe/Paris:20261019T093000\nDTEND;TZID=Europe/Paris:20261019T094500\n\
             RRULE:FREQ=DAILY;COUNT=10\nEXDATE;TZID=Europe/Paris:20261021T093000\nEND:VEVENT\n\
             BEGIN:VEVENT\nUID:u\nRECURRENCE-ID;TZID=Europe/Paris:20261022T093000\nSUMMARY:Standup (late)\nDTSTART;TZID=Europe/Paris:20261022T110000\n\
             DTEND;TZID=Europe/Paris:20261022T111500\nEND:VEVENT\nEND:VCALENDAR\n",
        );
        let i = instances(&o, at("2026-10-19T00:00:00Z"), at("2026-10-27T00:00:00Z"), &[]);
        let starts: Vec<When> = i.iter().map(|x| x.start).collect();
        // Daylight saving ends on the 25th: 9:30 in Paris is 7:30 UTC before, 8:30 after.
        assert_eq!(starts[0], When::At(at("2026-10-19T07:30:00Z")));
        assert!(!starts.contains(&When::At(at("2026-10-21T07:30:00Z"))), "the exception is skipped");
        assert!(!starts.contains(&When::At(at("2026-10-22T07:30:00Z"))), "the moved one is not at its old time");
        assert!(i.iter().any(|x| x.title == "Standup (late)" && x.start == When::At(at("2026-10-22T09:00:00Z"))));
        assert!(starts.contains(&When::At(at("2026-10-26T08:30:00Z"))));
        assert_eq!(i.len(), 7, "19, 20, 22 (moved), 23, 24, 25, 26");
        assert!(i.iter().all(|x| x.repeats));

        // A yearly birthday, years after it began; an unreadable rule shows its first time only.
        let birthday = ev("SUMMARY:Ann\nDTSTART;VALUE=DATE:19900412\nRRULE:FREQ=YEARLY");
        let i = instances(&birthday, at("2027-04-01T00:00:00Z"), at("2027-05-01T00:00:00Z"), &[]);
        assert_eq!(i[0].start, When::Day(NaiveDate::from_ymd_opt(2027, 4, 12).unwrap()));
        let until = ev("DTSTART:20261001T080000Z\nRRULE:FREQ=WEEKLY;UNTIL=20261015");
        assert_eq!(instances(&until, at("2026-10-01T00:00:00Z"), at("2026-11-01T00:00:00Z"), &[]).len(), 3);
        let bad = ev("DTSTART:20261001T080000Z\nRRULE:FREQ=SOMETIMES");
        assert_eq!(instances(&bad, at("2026-10-01T00:00:00Z"), at("2026-11-01T00:00:00Z"), &[]).len(), 1);
    }

    /// The starts found in [from, to), in RFC 3339, UTC.
    fn starts(o: &Object, from: &str, to: &str) -> Vec<String> {
        let cal = &ical::parse(&o.data).unwrap()[0];
        occurrences_in(cal, at(from), at(to), Tz::Europe__Paris)
            .into_iter()
            .map(|occ| match occ.start {
                When::Day(d) => d.to_string(),
                When::At(t) => DateTime::from_timestamp(t, 0).unwrap().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            })
            .collect()
    }

    #[test]
    fn weekly_event_with_one_moved_and_one_deleted_across_daylight_saving() {
        // Mondays at 9:00 in Paris from 5 October; the 19th moved to Tuesday
        // 14:00, 2 November cancelled on its own, 9 November an EXDATE.
        // Daylight saving ends on 25 October: 9:00 is 7:00 UTC, then 8:00.
        let o = object(
            "BEGIN:VCALENDAR\nBEGIN:VTIMEZONE\nTZID:Europe/Paris\nEND:VTIMEZONE\nBEGIN:VEVENT\nUID:u\nSUMMARY:Standup\n\
             DTSTART;TZID=Europe/Paris:20261005T090000\nDTEND;TZID=Europe/Paris:20261005T091500\nRRULE:FREQ=WEEKLY;COUNT=8\n\
             EXDATE;TZID=Europe/Paris:20261109T090000\nEND:VEVENT\n\
             BEGIN:VEVENT\nUID:u\nRECURRENCE-ID;TZID=Europe/Paris:20261019T090000\nSUMMARY:Standup (moved)\n\
             DTSTART;TZID=Europe/Paris:20261020T140000\nDTEND;TZID=Europe/Paris:20261020T141500\nEND:VEVENT\n\
             BEGIN:VEVENT\nUID:u\nRECURRENCE-ID:20261102T080000Z\nSUMMARY:Standup\nSTATUS:CANCELLED\n\
             DTSTART;TZID=Europe/Paris:20261102T090000\nEND:VEVENT\nEND:VCALENDAR\n",
        );
        assert_eq!(
            starts(&o, "2026-10-01T00:00:00Z", "2027-01-01T00:00:00Z"),
            ["2026-10-05T07:00:00Z", "2026-10-12T07:00:00Z", "2026-10-20T12:00:00Z", "2026-10-26T08:00:00Z", "2026-11-16T08:00:00Z", "2026-11-23T08:00:00Z"],
        );
        let cal = &ical::parse(&o.data).unwrap()[0];
        let found = occurrences_in(cal, at("2026-10-19T00:00:00Z"), at("2026-10-27T00:00:00Z"), Tz::Europe__Paris);
        let moved = &found[0];
        assert_eq!(moved.event.value("SUMMARY"), Some("Standup (moved)"));
        assert_eq!(moved.recurrence_id, Some(When::At(at("2026-10-19T07:00:00Z"))), "where it was in the series");
        assert_eq!(moved.end, When::At(at("2026-10-20T12:15:00Z")));
        assert_eq!(found[1].recurrence_id, Some(found[1].start));
        assert!(found.iter().all(|o| !o.cancelled));
        // The moved one is found from where it went, not from where it was.
        assert_eq!(starts(&o, "2026-10-20T00:00:00Z", "2026-10-21T00:00:00Z"), ["2026-10-20T12:00:00Z"]);
        assert!(starts(&o, "2026-10-19T00:00:00Z", "2026-10-20T00:00:00Z").is_empty());
    }

    #[test]
    fn hours_stay_put_in_their_own_zone() {
        // New York leaves daylight saving on 1 November, a week after Paris.
        let o = ev("DTSTART;TZID=America/New_York:20261023T090000\nDURATION:PT1H\nRRULE:FREQ=WEEKLY;BYDAY=FR,MO");
        assert_eq!(
            starts(&o, "2026-10-23T00:00:00Z", "2026-11-03T00:00:00Z"),
            ["2026-10-23T13:00:00Z", "2026-10-26T13:00:00Z", "2026-10-30T13:00:00Z", "2026-11-02T14:00:00Z"],
        );
        // In the hour that doesn't exist (2:30 on 29 March in Paris), the hour after.
        let gap = ev("DTSTART;TZID=Europe/Paris:20260328T023000\nRRULE:FREQ=DAILY;COUNT=3");
        assert_eq!(starts(&gap, "2026-03-27T00:00:00Z", "2026-04-01T00:00:00Z"), ["2026-03-28T01:30:00Z", "2026-03-29T01:30:00Z", "2026-03-30T00:30:00Z"]);
    }

    #[test]
    fn multi_day_all_day_events_stay_dates() {
        // Friday to Sunday: DTEND is the Monday, excluded.
        let o = ev("SUMMARY:Weekend away\nDTSTART;VALUE=DATE:20261113\nDTEND;VALUE=DATE:20261116");
        let day = |d: u32| NaiveDate::from_ymd_opt(2026, 11, d).unwrap();
        let paris = |d: u32| local(Tz::Europe__Paris, day(d).and_time(NaiveTime::MIN)).timestamp();
        let cal = &ical::parse(&o.data).unwrap()[0];
        let on = |d: u32| occurrences_in(cal, paris(d), paris(d + 1), Tz::Europe__Paris);
        let found = on(15);
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].start, found[0].end), (When::Day(day(13)), When::Day(day(16))));
        assert!(on(13).len() == 1 && on(14).len() == 1);
        // Days are Paris days: not the Monday, though its first hour (UTC) is still Sunday.
        assert!(on(16).is_empty() && on(12).is_empty());
        // Repeating, each time as long; and one with no DTEND lasts its day.
        let yearly = ev("DTSTART;VALUE=DATE:20251113\nDTEND;VALUE=DATE:20251116\nRRULE:FREQ=YEARLY");
        assert_eq!(starts(&yearly, "2026-11-15T12:00:00Z", "2026-11-15T13:00:00Z"), ["2026-11-13"]);
        let one = ev("DTSTART;VALUE=DATE:20261113");
        assert_eq!(starts(&one, "2026-11-12T23:00:00Z", "2026-11-13T23:00:00Z"), ["2026-11-13"]);
        assert!(starts(&one, "2026-11-13T23:00:00Z", "2026-11-14T23:00:00Z").is_empty());
    }

    #[test]
    fn rdates_and_cancellations() {
        // RDATEs add to the first time; EXDATEs written as dates take a timed occurrence's day.
        let o = ev("DTSTART:20261102T100000Z\nRDATE:20261104T100000Z,20261106T150000Z\nRRULE:FREQ=WEEKLY;COUNT=2\nEXDATE;VALUE=DATE:20261109");
        assert_eq!(starts(&o, "2026-11-01T00:00:00Z", "2026-12-01T00:00:00Z"), ["2026-11-02T10:00:00Z", "2026-11-04T10:00:00Z", "2026-11-06T15:00:00Z"]);
        // An event called off as a whole is still found, said cancelled.
        let off = object(
            "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:u\nSTATUS:CANCELLED\nDTSTART:20261102T100000Z\nRRULE:FREQ=DAILY;COUNT=2\nEND:VEVENT\n\
             BEGIN:VEVENT\nUID:u\nSTATUS:CANCELLED\nRECURRENCE-ID:20261103T100000Z\nDTSTART:20261103T110000Z\nEND:VEVENT\nEND:VCALENDAR\n",
        );
        let i = instances(&off, at("2026-11-01T00:00:00Z"), at("2026-11-05T00:00:00Z"), &[]);
        assert_eq!(i.len(), 2);
        assert!(i.iter().all(|x| x.cancelled));
    }

    #[test]
    fn expanded_for_caldav() {
        let o = object(
            "BEGIN:VCALENDAR\nVERSION:2.0\nBEGIN:VTIMEZONE\nTZID:Europe/Paris\nEND:VTIMEZONE\nBEGIN:VEVENT\nUID:u\nSUMMARY:Standup\n\
             DTSTART;TZID=Europe/Paris:20261026T090000\nDURATION:PT15M\nRRULE:FREQ=DAILY;COUNT=3\nEND:VEVENT\n\
             BEGIN:VEVENT\nUID:u\nRECURRENCE-ID;TZID=Europe/Paris:20261027T090000\nSUMMARY:Late\n\
             DTSTART;TZID=Europe/Paris:20261027T110000\nDTEND;TZID=Europe/Paris:20261027T111500\nEND:VEVENT\nEND:VCALENDAR\n",
        );
        let text = expanded(&o.data, at("2026-10-26T00:00:00Z"), at("2026-10-28T00:00:00Z")).unwrap();
        let cal = &ical::parse(&text).unwrap()[0];
        assert_eq!(cal.value("VERSION"), Some("2.0"));
        assert!(cal.children.iter().all(|c| c.name == "VEVENT" && c.prop("RRULE").is_none() && c.prop("DURATION").is_none()));
        let rows: Vec<(&str, &str, &str, &str)> = cal.children.iter().map(|e| (e.value("SUMMARY").unwrap(), e.value("DTSTART").unwrap(), e.value("DTEND").unwrap(), e.value("RECURRENCE-ID").unwrap())).collect();
        assert_eq!(rows, [("Standup", "20261026T080000Z", "20261026T081500Z", "20261026T080000Z"), ("Late", "20261027T100000Z", "20261027T101500Z", "20261027T080000Z")]);
        let day = expanded(&ev("DTSTART;VALUE=DATE:20261113\nDTEND;VALUE=DATE:20261116").data, at("2026-11-14T00:00:00Z"), at("2026-11-15T00:00:00Z")).unwrap();
        assert!(day.contains("DTSTART;VALUE=DATE:20261113\r\nDTEND;VALUE=DATE:20261116\r\n") && !day.contains("RECURRENCE-ID"));
    }

    #[test]
    fn writing_new_and_existing_events() {
        let t = |s: &str| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap();
        let mut e = Edit {
            title: "Dentist; bring card".into(),
            location: "".into(),
            notes: "line 1\nline 2".into(),
            all_day: false,
            start: t("2026-11-03 10:00"),
            end: t("2026-11-03 10:30"),
            tz: Tz::Europe__Paris,
            repeat: Repeat::None,
        };
        let (uid, text) = write(None, &e).unwrap();
        assert!(text.contains("DTSTART:20261103T090000Z") && text.contains("SUMMARY:Dentist\\; bring card") && text.contains("DESCRIPTION:line 1\\nline 2"));
        let o = object(&text);
        let ed = editable(&o).unwrap();
        assert_eq!((ed.title.as_str(), ed.notes.as_str(), ed.repeat), ("Dentist; bring card", "line 1\nline 2", Repeat::None));

        // Repeating: written in its zone; edits keep what the form doesn't know.
        e.repeat = Repeat::Weekly;
        let with_extra = text.replace("END:VEVENT", "ATTENDEE:mailto:x@y.z\r\nEND:VEVENT");
        let (uid2, text2) = write(Some(&with_extra), &e).unwrap();
        assert_eq!(uid, uid2);
        assert!(text2.contains("DTSTART;TZID=Europe/Paris:20261103T100000") && text2.contains("RRULE:FREQ=WEEKLY"));
        assert!(text2.contains("ATTENDEE:mailto:x@y.z") && text2.contains("SEQUENCE:1"));
        assert!(editable(&object(&text2)).unwrap().meeting);

        let custom = text2.replace("RRULE:FREQ=WEEKLY", "RRULE:FREQ=WEEKLY;BYDAY=MO,TH");
        assert_eq!(editable(&object(&custom)).unwrap().repeat, Repeat::Custom);
        e.repeat = Repeat::Custom;
        assert!(write(Some(&custom), &e).unwrap().1.contains("BYDAY=MO,TH"), "kept");
        assert!(write(None, &e).is_err());

        e.all_day = true;
        e.repeat = Repeat::None;
        e.end = t("2026-11-04 00:00");
        let (_, text) = write(None, &e).unwrap();
        assert!(text.contains("DTSTART;VALUE=DATE:20261103") && text.contains("DTEND;VALUE=DATE:20261105"));
        e.title = " ".into();
        assert!(write(None, &e).is_err());
    }

    #[test]
    fn zones_said_on_hover() {
        assert_eq!(zone_label(Tz::Europe__Paris), "Paris time");
        assert_eq!(zone_label(Tz::America__New_York), "New York time");
        assert_eq!(zone_note(Tz::Europe__Paris.with_ymd_and_hms(2026, 12, 1, 9, 0, 0).unwrap()), "Paris time (Europe/Paris, CET, UTC+01:00)");
        assert_eq!(zone_note(Tz::UTC.with_ymd_and_hms(2026, 12, 1, 9, 0, 0).unwrap()), "UTC");
        assert!(zone_names().any(|z| z == "Europe/Paris") && zone_names().any(|z| z == "UTC") && !zone_names().any(|z| z.starts_with("Etc/")));
        let ics = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:u\nDTSTART;TZID=Europe/Paris:20261023T100000\nEND:VEVENT\nEND:VCALENDAR\n";
        assert_eq!(first_start(ics), Some(When::At(at("2026-10-23T08:00:00Z"))));
    }

    #[test]
    fn repeat_rules_said_for_people() {
        let say = |body: &str| describe(&ev(body)).unwrap();
        assert_eq!(say("DTSTART;TZID=Europe/Paris:20261020T100000\nRRULE:FREQ=WEEKLY"), ("Every week on Tuesday at 10:00".into(), "20/10/2026 10:00".into(), Some(Tz::Europe__Paris.with_ymd_and_hms(2026, 10, 20, 10, 0, 0).unwrap())));
        assert_eq!(say("DTSTART:20261020T080000Z\nRRULE:FREQ=WEEKLY;INTERVAL=2;BYDAY=MO,TH;UNTIL=20261231T000000Z").0, "Every 2 weeks on Monday and Thursday at 10:00, until 31/12/2026", "UTC is said in Paris time");
        assert_eq!(say("DTSTART:20261020T080000Z\nRRULE:FREQ=DAILY;BYDAY=MO,TU,WE,TH,FR").0, "Every weekday at 10:00");
        assert_eq!(say("DTSTART:20261020T080000Z\nRRULE:FREQ=MONTHLY;BYDAY=-1FR;COUNT=6").0, "Every month on the last Friday at 10:00, 6 times");
        assert_eq!(say("DTSTART:20261023T080000Z\nRRULE:FREQ=MONTHLY").0, "Every month on the 23rd at 10:00");
        assert_eq!(say("DTSTART;VALUE=DATE:19900412\nRRULE:FREQ=YEARLY"), ("Every year on 12 April".into(), "12/04/1990".into(), None));
        assert!(describe(&ev("DTSTART:20261020T080000Z")).is_none());
    }

    #[test]
    fn unanswered_invitations_are_told() {
        let me = ["me@b.c".to_string(), "alias@b.c".to_string()];
        let when = "DTSTART:20261027T100000Z\nDTEND:20261027T110000Z\nORGANIZER:mailto:o@x.fr";
        let unanswered = |people: &str| {
            let o = ev(&format!("{when}\n{people}"));
            instances(&o, at("2026-10-27T00:00:00Z"), at("2026-10-28T00:00:00Z"), &me)[0].unanswered
        };
        assert!(unanswered("ATTENDEE;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:me@b.c"));
        assert!(unanswered("ATTENDEE;CN=Me:MAILTO:Alias@b.c"), "no PARTSTAT is NEEDS-ACTION; any of my addresses");
        assert!(!unanswered("ATTENDEE;PARTSTAT=ACCEPTED:mailto:me@b.c"));
        assert!(!unanswered("ATTENDEE;PARTSTAT=TENTATIVE:mailto:me@b.c"));
        assert!(!unanswered("ATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:someone@x.fr"), "others' answers aren't mine");
        assert!(!unanswered(""), "my own events");
        let mine = |people: &str| {
            let o = ev(&format!("DTSTART:20261027T100000Z\nORGANIZER:mailto:me@b.c\n{people}"));
            instances(&o, at("2026-10-27T00:00:00Z"), at("2026-10-28T00:00:00Z"), &me)[0].unanswered
        };
        assert!(!mine("ATTENDEE:mailto:me@b.c"), "nor the organizer in the list");
        assert!(mine("ATTENDEE:mailto:me@b.c\nATTENDEE:mailto:alias@b.c"), "but another of my addresses, invited");
        // A moved occurrence without its own people is the series'.
        let series = object(
            "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:u\nDTSTART:20261026T100000Z\nRRULE:FREQ=DAILY;COUNT=3\nORGANIZER:mailto:o@x.fr\n\
             ATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:me@b.c\nEND:VEVENT\nBEGIN:VEVENT\nUID:u\nRECURRENCE-ID:20261027T100000Z\n\
             DTSTART:20261027T120000Z\nEND:VEVENT\nEND:VCALENDAR\n",
        );
        let i = instances(&series, at("2026-10-26T00:00:00Z"), at("2026-10-29T00:00:00Z"), &me);
        assert!(i.len() == 3 && i.iter().all(|x| x.unanswered));
    }

    #[test]
    fn invitations_clash_with_busy_timed_events() {
        let now = at("2026-10-24T00:00:00Z");
        let invite = |body: &str| format!("BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:inv\r\nSUMMARY:Review\r\n{body}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n");
        let mut copy = ev("SUMMARY:Review\nDTSTART:20261027T100000Z\nDTEND:20261027T110000Z");
        copy.uid = "inv".into();
        let cal = [
            copy,
            ev("SUMMARY:Standup\nDTSTART:20261027T103000Z\nDTEND:20261027T104500Z"),
            ev("SUMMARY:Before\nDTSTART:20261027T090000Z\nDTEND:20261027T100000Z"),
            ev("SUMMARY:Free\nTRANSP:TRANSPARENT\nDTSTART:20261027T100000Z\nDTEND:20261027T110000Z"),
            ev("SUMMARY:Off\nSTATUS:CANCELLED\nDTSTART:20261027T100000Z\nDTEND:20261027T110000Z"),
            ev("SUMMARY:Holiday\nDTSTART;VALUE=DATE:20261027"),
            ev("SUMMARY:Gym\nDTSTART:20261020T100000Z\nDTEND:20261020T113000Z\nRRULE:FREQ=WEEKLY"),
        ];
        let titles = |ics: &str| conflicts(ics, &cal, now).into_iter().map(|i| i.title).collect::<Vec<_>>();
        // Back to back isn't a clash; the invitation's own copy, free, cancelled and all-day events aren't either.
        assert_eq!(titles(&invite("DTSTART:20261027T100000Z\nDTEND:20261027T110000Z")), ["Gym", "Standup"]);
        assert!(titles(&invite("DTSTART:20261027T120000Z\nDURATION:PT1H")).is_empty());
        assert!(titles(&invite("DTSTART;VALUE=DATE:20261027")).is_empty());
        // Repeating: every clash of its first occurrences; a past start counts from now.
        let weekly = titles(&invite("DTSTART:20261013T110000Z\nDTEND:20261013T120000Z\nRRULE:FREQ=WEEKLY;COUNT=4"));
        assert_eq!(weekly, ["Gym", "Gym"], "the 27th and 3 November, not the 13th or 20th");
    }

    #[test]
    fn rings_for_tagged_alarms_only() {
        let o = ev("SUMMARY:Flight\nDTSTART:20261110T060000Z\nBEGIN:VALARM\nTRIGGER:-PT2H\nX-VALET:wake\nEND:VALARM\nBEGIN:VALARM\nTRIGGER:-P1D\nX-VALET:other\nEND:VALARM");
        let r = rings(&o, &["wake".into()], at("2026-11-09T00:00:00Z"), at("2026-11-11T00:00:00Z"), Tz::UTC);
        assert_eq!(r.len(), 1);
        assert_eq!((r[0].at, r[0].title.as_str()), (at("2026-11-10T04:00:00Z"), "Flight"));
        let day = ev("SUMMARY:Exam\nDTSTART;VALUE=DATE:20261112\nBEGIN:VALARM\nTRIGGER:PT7H\nX-VALET:wake\nEND:VALARM");
        let r = rings(&day, &["wake".into()], at("2026-11-11T00:00:00Z"), at("2026-11-13T00:00:00Z"), Tz::Europe__Paris);
        assert_eq!(r[0].at, at("2026-11-12T06:00:00Z"), "7:00 in Paris");
    }
}
