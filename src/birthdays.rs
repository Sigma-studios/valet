//! Birthdays: yearly all-day events marked `X-VALET-BIRTHDAY` (the birth
//! year, or empty when it isn't known), kept in a Birthdays calendar whose
//! all-day default is the Birthday pattern, so every app shows them and
//! valet's Android app tells them. 29 February falls on the 28th in other
//! years.

use anyhow::{Context, Result, bail, ensure};
use chrono::{Datelike, Duration, NaiveDate, Utc};
use serde::Serialize;

use crate::calendar::{self, escape, unescape};
use crate::ical;
use crate::store::{Object, Store, name_for_uid};

pub const CALENDAR: &str = "birthdays";
const PREFIX: &str = "🎂 ";
/// The year a birthday without one is written in (a leap year, for 29 February).
const NO_YEAR: i32 = 2000;

/// A birthday, as the calendar page lists it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Birthday {
    pub calendar: String,
    /// The event's resource.
    pub name: String,
    pub person: String,
    pub month: u32,
    pub day: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub year: Option<i32>,
    /// Its next day, today included.
    pub next: NaiveDate,
    /// The age it is then, when the year is known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turns: Option<i32>,
}

/// The day a birthday falls on in `year` (28 February for the 29th, but in leap years).
pub fn on(month: u32, day: u32, year: i32) -> Option<NaiveDate> {
    NaiveDate::from_ymd_opt(year, month, day).or_else(|| (month == 2 && day == 29).then(|| NaiveDate::from_ymd_opt(year, 2, 28)).flatten())
}

/// The next time it is, from `today` (included).
pub fn next(month: u32, day: u32, today: NaiveDate) -> Option<NaiveDate> {
    on(month, day, today.year()).filter(|d| *d >= today).or_else(|| on(month, day, today.year() + 1))
}

/// Today, in the zone of the settings.
pub fn today() -> NaiveDate {
    Utc::now().with_timezone(&calendar::default_zone()).date_naive()
}

/// The birthday an object holds, if it is one.
pub fn read(o: &Object, today: NaiveDate) -> Option<Birthday> {
    let cals = ical::parse(&o.data).ok()?;
    let ev = cals.first()?.children.iter().find(|c| c.name == "VEVENT" && c.prop("RECURRENCE-ID").is_none())?;
    let year = calendar::birthday(ev)?;
    let start = ev.prop("DTSTART")?;
    let date = NaiveDate::parse_from_str(start.value.get(..8)?, "%Y%m%d").ok()?;
    let summary = unescape(ev.value("SUMMARY").unwrap_or(""));
    let person = summary.strip_prefix(PREFIX).unwrap_or(&summary).trim().to_string();
    let next = next(date.month(), date.day(), today)?;
    Some(Birthday {
        calendar: o.calendar.clone(),
        name: o.name.clone(),
        person,
        month: date.month(),
        day: date.day(),
        year,
        next,
        turns: year.map(|y| next.year() - y),
    })
}

/// Every birthday, the next first.
pub fn list(store: &Store, today: NaiveDate) -> Vec<Birthday> {
    let mut out: Vec<Birthday> = store.all_objects().iter().filter_map(|o| read(o, today)).collect();
    out.sort_by(|a, b| a.next.cmp(&b.next).then_with(|| a.person.to_lowercase().cmp(&b.person.to_lowercase())));
    out
}

/// The event of a birthday.
pub fn ics(uid: &str, person: &str, month: u32, day: u32, year: Option<i32>, stamp: &str) -> Result<String> {
    let first = NaiveDate::from_ymd_opt(year.unwrap_or(NO_YEAR), month, day).context("no such day")?;
    let rule = if (month, day) == (2, 29) { "FREQ=YEARLY;BYMONTH=2;BYMONTHDAY=-1" } else { "FREQ=YEARLY" };
    let d = |d: NaiveDate| d.format("%Y%m%d").to_string();
    let mut ev = ical::Component::new("VEVENT");
    ev.push("UID", uid);
    ev.push("DTSTAMP", stamp);
    ev.push("SUMMARY", &format!("{PREFIX}{}", escape(person)));
    let date = |name: &str, v: NaiveDate| ical::Property { name: name.into(), params: ";VALUE=DATE".into(), value: d(v) };
    ev.props.push(date("DTSTART", first));
    ev.props.push(date("DTEND", first + Duration::days(1)));
    ev.push("RRULE", rule);
    ev.push("TRANSP", "TRANSPARENT");
    ev.push("CATEGORIES", "Birthday");
    ev.push("X-VALET-BIRTHDAY", &year.map(|y| y.to_string()).unwrap_or_default());
    let mut cal = ical::Component::new("VCALENDAR");
    cal.push("VERSION", "2.0");
    cal.push("PRODID", "-//valet//EN");
    cal.children.push(ev);
    Ok(cal.to_ics())
}

/// The Birthdays calendar, made the first time with the Birthday pattern
/// (6 pm the day before and 8 am on the day, as notifications) for its
/// all-day events; what was changed since stays.
fn calendar(store: &Store) -> Result<()> {
    if store.calendar(CALENDAR).is_some() {
        return Ok(());
    }
    store.create_calendar(CALENDAR, "Birthdays", Some("#db2777"))?;
    let pattern = match store.patterns().into_iter().find(|p| p.tag == "birthday") {
        Some(p) => p.id,
        None => {
            store.save_pattern(None, "Birthday", "birthday", &[-360, 480])?;
            let p = store.patterns().into_iter().find(|p| p.tag == "birthday").context("the Birthday pattern")?;
            store.set_notify(p.id, true)?;
            p.id
        }
    };
    let mut cal = store.calendar(CALENDAR).context("the Birthdays calendar")?;
    cal.default_allday = Some(pattern);
    store.update_calendar(&cal)?;
    Ok(())
}

/// Checks a birthday typed in: a name, a day of the year, a birth year
/// from 1900 to `today` if any.
pub fn check(person: &str, month: u32, day: u32, year: Option<i32>, today: NaiveDate) -> Result<()> {
    ensure!(!person.trim().is_empty(), "a name is needed");
    ensure!(NaiveDate::from_ymd_opt(NO_YEAR, month, day).is_some(), "there is no such day");
    if let Some(y) = year {
        let Some(born) = NaiveDate::from_ymd_opt(y, month, day) else { bail!("{y} had no 29 February") };
        ensure!(y >= 1900 && born <= today, "born when? {y} can't be");
    }
    Ok(())
}

/// Saves a birthday: a new one (`existing` None) in the Birthdays calendar,
/// or one already there, where it is. Returns where it is.
pub fn save(store: &Store, existing: Option<(&str, &str)>, person: &str, month: u32, day: u32, year: Option<i32>, today: NaiveDate) -> Result<(String, String)> {
    check(person, month, day, year, today)?;
    let stamp = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let (calendar, name, uid) = match existing {
        Some((c, n)) => {
            let o = store.object(c, n).context("that birthday is gone")?;
            (o.calendar, o.name, o.uid)
        }
        None => {
            calendar(store)?;
            let uid = format!("{}@valet", uuid::Uuid::new_v4());
            (CALENDAR.to_string(), name_for_uid(&uid), uid)
        }
    };
    let text = ics(&uid, person.trim(), month, day, year, &stamp)?;
    store.put_object(&calendar, &name, &text, false).map_err(|e| anyhow::anyhow!("not saved: {e:?}"))?;
    Ok((calendar, name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ymd(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn next_ones_and_29_february() {
        let today = ymd(2026, 11, 14);
        assert_eq!(next(11, 14, today), Some(ymd(2026, 11, 14)), "today is the next");
        assert_eq!(next(4, 12, today), Some(ymd(2027, 4, 12)));
        assert_eq!(next(2, 29, today), Some(ymd(2027, 2, 28)));
        assert_eq!(next(2, 29, ymd(2027, 11, 1)), Some(ymd(2028, 2, 29)));
        assert!(check("Ann", 2, 29, Some(1991), today).is_err() && check("Ann", 2, 29, Some(1992), today).is_ok());
        assert!(check("Ann", 4, 31, None, today).is_err() && check(" ", 4, 1, None, today).is_err());
        assert!(check("Ann", 12, 1, Some(2026), today).is_err(), "not born yet");
    }

    #[test]
    fn saved_as_yearly_events_told_by_the_android_app() {
        let s = Store::memory();
        let today = ymd(2026, 11, 14);
        let (cal, name) = save(&s, None, "Ann, my sister", 4, 12, Some(1990), today).unwrap();
        save(&s, None, "Bob", 2, 29, None, today).unwrap();
        save(&s, None, "Zoé", 11, 14, Some(2000), today).unwrap();
        // The calendar and its pattern, made once.
        let c = s.calendar(CALENDAR).unwrap();
        let p = s.patterns().into_iter().find(|p| p.tag == "birthday").unwrap();
        assert!(c.name == "Birthdays" && c.default_allday == Some(p.id) && p.notify && !p.ring && p.offsets == [-360, 480]);
        let o = s.object(&cal, &name).unwrap();
        assert!(o.data.contains("DTSTART;VALUE=DATE:19900412") && o.data.contains("RRULE:FREQ=YEARLY\r\n") && o.data.contains("SUMMARY:🎂 Ann\\, my sister"));
        assert!(o.data.contains("TRIGGER:-PT6H") && o.data.contains("TRIGGER:PT8H"), "the pattern's alerts: {}", o.data);

        let all = list(&s, today);
        let rows: Vec<(&str, NaiveDate, Option<i32>)> = all.iter().map(|b| (b.person.as_str(), b.next, b.turns)).collect();
        assert_eq!(rows, [("Zoé", ymd(2026, 11, 14), Some(26)), ("Bob", ymd(2027, 2, 28), None), ("Ann, my sister", ymd(2027, 4, 12), Some(37))]);

        // The calendar page and the agenda say the age; the app's notifications too.
        let i = calendar::instances(&o, 1_807_400_000, 1_807_600_000, &[]);
        assert_eq!((i.len(), i[0].age), (1, Some(37)), "12 April 2027");
        let rings = calendar::rings(&o, &["birthday".into()], 1_807_400_000, 1_807_600_000, chrono_tz::Tz::Europe__Paris);
        assert_eq!(rings.iter().map(|r| r.title.as_str()).collect::<Vec<_>>(), ["🎂 Ann, my sister turns 37", "🎂 Ann, my sister turns 37"]);

        // Edited where it is: the year forgotten, the day changed; the pattern stays as changed.
        s.save_pattern(Some(p.id), "Birthday", "birthday", &[480]).unwrap();
        save(&s, Some((&cal, &name)), "Ann", 4, 13, None, today).unwrap();
        let b = read(&s.object(&cal, &name).unwrap(), today).unwrap();
        assert_eq!((b.person.as_str(), b.day, b.year, b.turns), ("Ann", 13, None, None));
        assert!(!s.object(&cal, &name).unwrap().data.contains("-PT6H"));
        assert_eq!(s.calendars().len(), 1);
    }
}
