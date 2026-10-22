//! One-time import of .ics exports (Google, Outlook, IONOS…): every event
//! becomes its own resource, with the time zones it refers to.

use std::collections::{BTreeMap, HashMap};

use crate::ical::{self, Component};
use crate::store::{PutError, Store, name_for_uid};

#[derive(Debug, Default)]
pub struct Report {
    pub created: usize,
    pub updated: usize,
    /// Component kinds left out (VTODO, VJOURNAL…), with their count.
    pub skipped: BTreeMap<String, usize>,
    pub errors: Vec<String>,
}

/// Every TZID parameter in `c` and its children.
fn tzids(c: &Component, out: &mut Vec<String>) {
    for p in &c.props {
        if let Some(tz) = p.param("TZID") {
            out.push(tz.to_string());
        }
    }
    for child in &c.children {
        tzids(child, out);
    }
}

/// Imports `text` into `calendar`. Events already there (same UID) are
/// replaced. `fresh_alerts` drops the alerts the export came with, so the
/// calendar's patterns apply.
pub fn import(store: &Store, calendar: &str, text: &str, fresh_alerts: bool, report: &mut Report) {
    let cals = match ical::parse(text) {
        Ok(c) => c,
        Err(e) => {
            report.errors.push(format!("not a valid .ics file: {e}"));
            return;
        }
    };
    let mut zones: HashMap<String, Component> = HashMap::new();
    let mut events: Vec<(String, Vec<Component>)> = Vec::new();
    for cal in cals.into_iter().filter(|c| c.name == "VCALENDAR") {
        for c in cal.children {
            match c.name.as_str() {
                "VTIMEZONE" => {
                    if let Some(id) = c.value("TZID") {
                        zones.insert(id.to_string(), c);
                    }
                }
                "VEVENT" => {
                    let uid = c.value("UID").map(str::to_string).unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                    let mut c = c;
                    if c.prop("UID").is_none() {
                        c.push("UID", &uid);
                    }
                    match events.iter_mut().find(|(u, _)| *u == uid) {
                        Some((_, list)) => list.push(c),
                        None => events.push((uid, vec![c])),
                    }
                }
                other => *report.skipped.entry(other.to_string()).or_default() += 1,
            }
        }
    }
    for (uid, list) in events {
        let mut out = Component::new("VCALENDAR");
        out.push("VERSION", "2.0");
        out.push("PRODID", "-//valet//import//EN");
        let mut needed = Vec::new();
        for e in &list {
            tzids(e, &mut needed);
        }
        needed.sort();
        needed.dedup();
        out.children.extend(needed.iter().filter_map(|tz| zones.get(tz).cloned()));
        out.children.extend(list);
        let existing = store.object_by_uid(calendar, &uid);
        let name = existing.map(|o| o.name).unwrap_or_else(|| name_for_uid(&uid));
        match store.put_object(calendar, &name, &out.to_ics(), fresh_alerts) {
            Ok(put) if put.created => report.created += 1,
            Ok(_) => report.updated += 1,
            Err(PutError::Invalid(why)) => report.errors.push(format!("{uid}: {why}")),
            Err(e) => report.errors.push(format!("{uid}: {e:?}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXPORT: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:Google\r\n\
BEGIN:VTIMEZONE\r\nTZID:Europe/Paris\r\nBEGIN:STANDARD\r\nDTSTART:19701025T030000\r\nTZOFFSETFROM:+0200\r\nTZOFFSETTO:+0100\r\nEND:STANDARD\r\nEND:VTIMEZONE\r\n\
BEGIN:VTIMEZONE\r\nTZID:America/New_York\r\nBEGIN:STANDARD\r\nDTSTART:19701101T020000\r\nTZOFFSETFROM:-0400\r\nTZOFFSETTO:-0500\r\nEND:STANDARD\r\nEND:VTIMEZONE\r\n\
BEGIN:VEVENT\r\nUID:weekly@google.com\r\nDTSTART;TZID=Europe/Paris:20261005T090000\r\nRRULE:FREQ=WEEKLY\r\nSUMMARY:Weekly\r\n\
BEGIN:VALARM\r\nACTION:DISPLAY\r\nTRIGGER:-PT30M\r\nEND:VALARM\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nUID:weekly@google.com\r\nRECURRENCE-ID;TZID=Europe/Paris:20261012T090000\r\nDTSTART;TZID=Europe/Paris:20261012T100000\r\nSUMMARY:Weekly (moved)\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nUID:once\r\nDTSTART;VALUE=DATE:20261101\r\nSUMMARY:Holiday\r\nEND:VEVENT\r\n\
BEGIN:VTODO\r\nUID:t\r\nSUMMARY:Task\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";

    #[test]
    fn splits_by_uid_with_their_zones() {
        let store = Store::memory();
        store.create_calendar("c", "C", None).unwrap();
        let mut report = Report::default();
        import(&store, "c", EXPORT, true, &mut report);
        assert_eq!((report.created, report.updated), (2, 0), "{report:?}");
        assert_eq!(report.skipped.get("VTODO"), Some(&1));
        let weekly = store.object_by_uid("c", "weekly@google.com").unwrap();
        assert!(weekly.data.contains("TZID:Europe/Paris"));
        assert!(!weekly.data.contains("America/New_York"));
        assert!(weekly.data.contains("Weekly (moved)"));
        assert!(!weekly.data.contains("VALARM"), "fresh alerts drop the exported ones");
        assert!(weekly.recurring);
        assert_eq!(weekly.summary, "Weekly");

        let mut again = Report::default();
        import(&store, "c", EXPORT, false, &mut again);
        assert_eq!((again.created, again.updated), (0, 2));
        assert!(store.object_by_uid("c", "weekly@google.com").unwrap().data.contains("TRIGGER:-PT30M"));
    }
}
