//! The calendar as valet's own pages show it: the events of a time range,
//! repeating ones expanded (time zones, exceptions, moved occurrences), and
//! events written from the web form. Apps keep getting the stored iCalendar
//! as it is; this only reads it, or writes it the way apps do.

use std::collections::HashSet;
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
    pub title: String,
    pub location: String,
    pub notes: String,
    #[serde(rename = "allDay")]
    pub all_day: bool,
    pub start: When,
    pub end: When,
    pub repeats: bool,
    /// An invitation to one of my addresses I haven't answered.
    pub unanswered: bool,
    /// The invitation valet got by mail for it, to answer from its card.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invite: Option<i64>,
    /// The mail that invitation came in, to open from its card.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mail: Option<i64>,
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

fn overlaps(start: When, end: When, from: i64, to: i64) -> bool {
    let (s, e) = (start.secs(), end.secs());
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

/// Starts of a repeating event within [from, to), its first only if its rule
/// can't be read.
fn occurrences(ev: &Component, start: Start, len: i64, from: i64, to: i64) -> Vec<Start> {
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
            None => return if overlaps(when(start), shifted(start, len), from, to) { vec![start] } else { vec![] },
        }
    }
    let span = if matches!(start, Start::Day(_)) { len * 86400 } else { len };
    let utc = |t: i64| rrule::Tz::UTC.timestamp_opt(t, 0).single().unwrap_or(dt);
    let set = rrule::RRuleSet::new(dt)
        .set_rrules(rules)
        .set_rdates(dates(ev, "RDATE").into_iter().map(engine_time).collect())
        .set_exdates(dates(ev, "EXDATE").into_iter().map(engine_time).collect())
        .after(utc(from - span))
        .before(utc(to));
    set.all(5000)
        .dates
        .into_iter()
        .map(|d| match start {
            Start::Day(_) => Start::Day(d.date_naive()),
            Start::Time(t) => Start::Time(d.with_timezone(&t.timezone())),
        })
        .filter(|s| overlaps(when(*s), shifted(*s, len), from, to))
        .collect()
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

/// Occurrences of an object within [from, to), each with the VEVENT it comes from.
fn expand(cal: &Component, from: i64, to: i64) -> Vec<(When, When, &Component, bool)> {
    let (master, moved) = events(cal);
    let repeats = master.is_some_and(|m| m.prop("RRULE").is_some() || m.prop("RDATE").is_some());
    let mut out = Vec::new();
    let replaced: HashSet<i64> = moved.iter().filter_map(|e| e.prop("RECURRENCE-ID").and_then(|p| parse_value(p, &p.value))).map(|s| when(s).secs()).collect();
    if let Some(m) = master
        && let Some(start) = m.prop("DTSTART").and_then(|p| parse_value(p, &p.value))
    {
        let len = length(m, start);
        let starts = if repeats { occurrences(m, start, len, from, to) } else { vec![start] };
        for s in starts {
            if repeats && replaced.contains(&when(s).secs()) {
                continue;
            }
            let (a, b) = (when(s), shifted(s, len));
            if overlaps(a, b, from, to) {
                out.push((a, b, m, repeats));
            }
        }
    }
    for e in moved {
        let Some(start) = e.prop("DTSTART").and_then(|p| parse_value(p, &p.value)) else { continue };
        let (a, b) = (when(start), shifted(start, length(e, start)));
        if overlaps(a, b, from, to) {
            out.push((a, b, e, true));
        }
    }
    out.sort_by_key(|(a, ..)| a.secs());
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
    let (master, _) = events(cal);
    let value = |e: &Component, name: &str| e.value(name).or_else(|| master.and_then(|m| m.value(name))).map(unescape);
    let text = |e: &Component, name: &str| value(e, name).unwrap_or_default();
    expand(cal, from, to)
        .into_iter()
        .filter(|(_, _, e, _)| keep(&|name| value(e, name)))
        .map(|(start, end, e, repeats)| Instance {
            calendar: o.calendar.clone(),
            name: o.name.clone(),
            title: text(e, "SUMMARY"),
            location: text(e, "LOCATION"),
            notes: text(e, "DESCRIPTION"),
            all_day: matches!(start, When::Day(_)),
            start,
            end,
            repeats,
            unanswered: unanswered(e, master, me),
            invite: None,
            mail: None,
        })
        .collect()
}

/// The events of `objects` an invitation (`ics`) would clash with: timed
/// ones that overlap its timed occurrences (the first two months of them when
/// it repeats), busy, and not the invitation's own copy.
pub fn conflicts(ics: &str, objects: &[Object], now: i64) -> Vec<Instance> {
    let Ok(cals) = ical::parse(ics) else { return vec![] };
    let Some(cal) = cals.first() else { return vec![] };
    let uid = events(cal).0.and_then(|m| m.value("UID")).unwrap_or_default();
    let from = first_start(ics).map_or(now, |w| w.secs().max(now));
    let times: Vec<(i64, i64)> = expand(cal, from, from + 60 * 86400)
        .into_iter()
        .filter(|(_, _, e, _)| !e.value("STATUS").is_some_and(|s| s.eq_ignore_ascii_case("CANCELLED")))
        .filter_map(|(a, b, ..)| match (a, b) {
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

/// An alarm to ring (valet's Android app).
#[derive(Clone, Debug, Serialize)]
pub struct Ring {
    /// Unix seconds.
    pub at: i64,
    pub title: String,
    pub start: When,
}

/// The alarms of patterns tagged `tags` that go off within [from, to);
/// all-day events start at midnight in `tz` (the phone's).
pub fn rings(o: &Object, tags: &[String], from: i64, to: i64, tz: Tz) -> Vec<Ring> {
    let Ok(cals) = ical::parse(&o.data) else { return vec![] };
    let Some(cal) = cals.first() else { return vec![] };
    // Alerts go off up to a few weeks before or after the start.
    let reach = 30 * 86400;
    let mut out = Vec::new();
    for (start, _, e, _) in expand(cal, from - reach, to + reach) {
        let base = match start {
            When::Day(d) => local(tz, d.and_time(NaiveTime::MIN)).timestamp(),
            When::At(t) => t,
        };
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
                out.push(Ring { at, title: e.value("SUMMARY").map(unescape).unwrap_or_default(), start });
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
