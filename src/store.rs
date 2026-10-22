//! SQLite storage: calendars, their events (one iCalendar object per resource,
//! as CalDAV wants), alert patterns and per-event choices.
//!
//! Every change takes the next value of one global revision counter; an object
//! and its calendar remember the revision of their last change, and deletions
//! leave a tombstone, which is all that sync tokens need.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

use crate::alarms::{Pattern, Rules, Source};
use crate::ical::{self, Component};

pub struct Store {
    conn: Mutex<Connection>,
}

#[derive(Clone, Debug)]
pub struct Calendar {
    pub id: String,
    pub name: String,
    pub color: String,
    pub description: String,
    pub ord: i64,
    pub default_timed: Option<i64>,
    pub default_allday: Option<i64>,
    pub rev: i64,
}

#[derive(Clone, Debug)]
pub struct Object {
    pub calendar: String,
    pub name: String,
    pub uid: String,
    pub data: String,
    pub etag: String,
    pub rev: i64,
    pub summary: String,
    pub dtstart: String,
    pub recurring: bool,
}

#[derive(Debug)]
pub enum PutError {
    Invalid(String),
    /// Another resource of the calendar already holds this UID.
    UidConflict(String),
    NoCalendar,
}

pub struct Put {
    pub etag: String,
    pub created: bool,
    /// The stored data differs from what was sent (alarms were rewritten).
    pub rewritten: bool,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS calendars (
    id TEXT PRIMARY KEY, name TEXT NOT NULL, color TEXT NOT NULL DEFAULT '#3b82f6',
    description TEXT NOT NULL DEFAULT '', ord INTEGER NOT NULL DEFAULT 0,
    default_timed INTEGER, default_allday INTEGER, rev INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS objects (
    calendar TEXT NOT NULL, name TEXT NOT NULL, uid TEXT NOT NULL, data TEXT NOT NULL,
    etag TEXT NOT NULL, rev INTEGER NOT NULL, summary TEXT NOT NULL, dtstart TEXT NOT NULL,
    recurring INTEGER NOT NULL, PRIMARY KEY (calendar, name));
CREATE INDEX IF NOT EXISTS objects_uid ON objects (calendar, uid);
CREATE TABLE IF NOT EXISTS tombstones (
    calendar TEXT NOT NULL, name TEXT NOT NULL, rev INTEGER NOT NULL, PRIMARY KEY (calendar, name));
CREATE TABLE IF NOT EXISTS patterns (
    id INTEGER PRIMARY KEY, name TEXT NOT NULL, tag TEXT NOT NULL UNIQUE, offsets TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS choices (
    calendar TEXT NOT NULL, uid TEXT NOT NULL, pattern INTEGER NOT NULL, PRIMARY KEY (calendar, uid));
";

const CAL_COLS: &str = "id, name, color, description, ord, default_timed, default_allday, rev";
const OBJ_COLS: &str = "calendar, name, uid, data, etag, rev, summary, dtstart, recurring";

fn cal_row(r: &rusqlite::Row) -> rusqlite::Result<Calendar> {
    Ok(Calendar {
        id: r.get(0)?,
        name: r.get(1)?,
        color: r.get(2)?,
        description: r.get(3)?,
        ord: r.get(4)?,
        default_timed: r.get(5)?,
        default_allday: r.get(6)?,
        rev: r.get(7)?,
    })
}

fn obj_row(r: &rusqlite::Row) -> rusqlite::Result<Object> {
    Ok(Object {
        calendar: r.get(0)?,
        name: r.get(1)?,
        uid: r.get(2)?,
        data: r.get(3)?,
        etag: r.get(4)?,
        rev: r.get(5)?,
        summary: r.get(6)?,
        dtstart: r.get(7)?,
        recurring: r.get(8)?,
    })
}

pub fn etag(data: &str) -> String {
    format!("\"{}\"", &hex::encode(Sha256::digest(data.as_bytes()))[..32])
}

/// A resource name for an imported event, stable across re-imports.
pub fn name_for_uid(uid: &str) -> String {
    format!("{}.ics", &hex::encode(Sha256::digest(uid.as_bytes()))[..32])
}

/// The checked contents of one CalDAV resource.
struct Parsed {
    cal: Component,
    uid: String,
}

fn parse_object(text: &str) -> Result<Parsed, PutError> {
    let mut cals = ical::parse(text).map_err(|e| PutError::Invalid(e.to_string()))?;
    if cals.len() != 1 || cals[0].name != "VCALENDAR" {
        return Err(PutError::Invalid("expected exactly one VCALENDAR".into()));
    }
    let cal = cals.remove(0);
    let mut uid = None;
    for c in cal.children.iter().filter(|c| c.name != "VTIMEZONE") {
        if c.name != "VEVENT" {
            return Err(PutError::Invalid(format!("only events are stored here, not {}", c.name)));
        }
        let u = c.value("UID").ok_or_else(|| PutError::Invalid("VEVENT without UID".into()))?;
        if uid.get_or_insert(u) != &u {
            return Err(PutError::Invalid("one resource must hold a single UID".into()));
        }
    }
    let uid = uid.ok_or_else(|| PutError::Invalid("no VEVENT".into()))?.to_string();
    Ok(Parsed { cal, uid })
}

/// The event to show for an object: the one without RECURRENCE-ID if any.
fn master(cal: &Component) -> Option<&Component> {
    let mut events = cal.children.iter().filter(|c| c.name == "VEVENT");
    let first = events.clone().next();
    events.find(|e| e.prop("RECURRENCE-ID").is_none()).or(first)
}

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Store { conn: Mutex::new(conn) })
    }

    pub fn memory() -> Store {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        Store { conn: Mutex::new(conn) }
    }

    fn db(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn next_rev(c: &Connection) -> rusqlite::Result<i64> {
        c.query_row(
            "INSERT INTO meta (key, value) VALUES ('rev', 1)
             ON CONFLICT (key) DO UPDATE SET value = value + 1 RETURNING value",
            [],
            |r| r.get(0),
        )
    }

    fn touch(c: &Connection, calendar: &str) -> rusqlite::Result<i64> {
        let rev = Self::next_rev(c)?;
        c.execute("UPDATE calendars SET rev = ?1 WHERE id = ?2", params![rev, calendar])?;
        Ok(rev)
    }

    // Calendars

    pub fn calendars(&self) -> Vec<Calendar> {
        let c = self.db();
        let mut st = c.prepare(&format!("SELECT {CAL_COLS} FROM calendars ORDER BY ord, name")).unwrap();
        st.query_map([], cal_row).unwrap().filter_map(Result::ok).collect()
    }

    pub fn calendar(&self, id: &str) -> Option<Calendar> {
        let c = self.db();
        c.query_row(&format!("SELECT {CAL_COLS} FROM calendars WHERE id = ?1"), [id], cal_row).optional().unwrap()
    }

    /// False if the id is taken.
    pub fn create_calendar(&self, id: &str, name: &str, color: Option<&str>) -> Result<bool> {
        let c = self.db();
        let rev = Self::next_rev(&c)?;
        let n = c.execute(
            "INSERT OR IGNORE INTO calendars (id, name, color, ord, rev)
             VALUES (?1, ?2, COALESCE(?3, '#3b82f6'), (SELECT COUNT(*) FROM calendars), ?4)",
            params![id, name, color, rev],
        )?;
        Ok(n == 1)
    }

    /// Saves `cal`'s editable fields; alarms are recomputed when defaults change.
    pub fn update_calendar(&self, cal: &Calendar) -> Result<()> {
        let before = self.calendar(&cal.id).context("no such calendar")?;
        {
            let c = self.db();
            let rev = Self::next_rev(&c)?;
            c.execute(
                "UPDATE calendars SET name = ?2, color = ?3, description = ?4, ord = ?5,
                 default_timed = ?6, default_allday = ?7, rev = ?8 WHERE id = ?1",
                params![cal.id, cal.name, cal.color, cal.description, cal.ord, cal.default_timed, cal.default_allday, rev],
            )?;
        }
        if (before.default_timed, before.default_allday) != (cal.default_timed, cal.default_allday) {
            self.reapply(Some(&cal.id))?;
        }
        Ok(())
    }

    pub fn delete_calendar(&self, id: &str) -> Result<()> {
        let mut c = self.db();
        let tx = c.transaction()?;
        tx.execute("DELETE FROM objects WHERE calendar = ?1", [id])?;
        tx.execute("DELETE FROM tombstones WHERE calendar = ?1", [id])?;
        tx.execute("DELETE FROM choices WHERE calendar = ?1", [id])?;
        tx.execute("DELETE FROM calendars WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(())
    }

    // Objects

    pub fn objects(&self, calendar: &str) -> Vec<Object> {
        let c = self.db();
        let mut st = c.prepare(&format!("SELECT {OBJ_COLS} FROM objects WHERE calendar = ?1 ORDER BY name")).unwrap();
        st.query_map([calendar], obj_row).unwrap().filter_map(Result::ok).collect()
    }

    pub fn all_objects(&self) -> Vec<Object> {
        let c = self.db();
        let mut st = c.prepare(&format!("SELECT {OBJ_COLS} FROM objects ORDER BY dtstart")).unwrap();
        st.query_map([], obj_row).unwrap().filter_map(Result::ok).collect()
    }

    pub fn object(&self, calendar: &str, name: &str) -> Option<Object> {
        let c = self.db();
        c.query_row(&format!("SELECT {OBJ_COLS} FROM objects WHERE calendar = ?1 AND name = ?2"), [calendar, name], obj_row)
            .optional()
            .unwrap()
    }

    pub fn object_by_uid(&self, calendar: &str, uid: &str) -> Option<Object> {
        let c = self.db();
        c.query_row(&format!("SELECT {OBJ_COLS} FROM objects WHERE calendar = ?1 AND uid = ?2"), [calendar, uid], obj_row)
            .optional()
            .unwrap()
    }

    /// Objects changed and names deleted after revision `since`.
    pub fn changes(&self, calendar: &str, since: i64) -> (Vec<Object>, Vec<String>) {
        let c = self.db();
        let mut st =
            c.prepare(&format!("SELECT {OBJ_COLS} FROM objects WHERE calendar = ?1 AND rev > ?2 ORDER BY rev")).unwrap();
        let changed = st.query_map(params![calendar, since], obj_row).unwrap().filter_map(Result::ok).collect();
        let mut st = c.prepare("SELECT name FROM tombstones WHERE calendar = ?1 AND rev > ?2 ORDER BY rev").unwrap();
        let deleted = st.query_map(params![calendar, since], |r| r.get(0)).unwrap().filter_map(Result::ok).collect();
        (changed, deleted)
    }

    fn rules<'a>(c: &Connection, patterns: &'a [Pattern], calendar: &str, uid: &str) -> rusqlite::Result<Rules<'a>> {
        let (default_timed, default_allday) = c
            .query_row("SELECT default_timed, default_allday FROM calendars WHERE id = ?1", [calendar], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?
            .unwrap_or((None, None));
        let choice = c
            .query_row("SELECT pattern FROM choices WHERE calendar = ?1 AND uid = ?2", [calendar, uid], |r| r.get(0))
            .optional()?;
        Ok(Rules { patterns, default_timed, default_allday, choice })
    }

    /// Stores `text` as resource `name`, after giving it the alarms the rules
    /// call for. `strip_alarms` first drops the alarms it came with.
    pub fn put_object(&self, calendar: &str, name: &str, text: &str, strip_alarms: bool) -> Result<Put, PutError> {
        let Parsed { mut cal, uid } = parse_object(text)?;
        if strip_alarms {
            for ev in cal.children.iter_mut() {
                ev.children.retain(|a| a.name != "VALARM");
            }
        }
        let patterns = self.patterns();
        let c = self.db();
        let exists: bool = c.query_row("SELECT COUNT(*) FROM calendars WHERE id = ?1", [calendar], |r| r.get(0)).unwrap();
        if !exists {
            return Err(PutError::NoCalendar);
        }
        let holder: Option<String> = c
            .query_row("SELECT name FROM objects WHERE calendar = ?1 AND uid = ?2", [calendar, &uid], |r| r.get(0))
            .optional()
            .unwrap();
        if let Some(holder) = holder.filter(|h| h != name) {
            return Err(PutError::UidConflict(holder));
        }
        let rules = Self::rules(&c, &patterns, calendar, &uid).unwrap();
        let rewritten = rules.apply(&mut cal) || strip_alarms;
        let data = if rewritten { cal.to_ics() } else { text.to_string() };
        let tag = etag(&data);
        let ev = master(&cal);
        let summary = ev.and_then(|e| e.value("SUMMARY")).unwrap_or("").to_string();
        let dtstart = ev.and_then(|e| e.value("DTSTART")).unwrap_or("").to_string();
        let recurring = ev.is_some_and(|e| e.prop("RRULE").is_some() || e.prop("RDATE").is_some());
        let rev = Self::touch(&c, calendar).unwrap();
        let created = c
            .execute(
                &format!("INSERT INTO objects ({OBJ_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT (calendar, name) DO NOTHING"),
                params![calendar, name, uid, data, tag, rev, summary, dtstart, recurring],
            )
            .unwrap()
            == 1;
        if !created {
            c.execute(
                "UPDATE objects SET uid = ?3, data = ?4, etag = ?5, rev = ?6, summary = ?7, dtstart = ?8, recurring = ?9
                 WHERE calendar = ?1 AND name = ?2",
                params![calendar, name, uid, data, tag, rev, summary, dtstart, recurring],
            )
            .unwrap();
        }
        c.execute("DELETE FROM tombstones WHERE calendar = ?1 AND name = ?2", [calendar, name]).unwrap();
        Ok(Put { etag: tag, created, rewritten })
    }

    pub fn delete_object(&self, calendar: &str, name: &str) -> Result<bool> {
        let c = self.db();
        let uid: Option<String> = c
            .query_row("SELECT uid FROM objects WHERE calendar = ?1 AND name = ?2", [calendar, name], |r| r.get(0))
            .optional()?;
        let Some(uid) = uid else { return Ok(false) };
        let rev = Self::touch(&c, calendar)?;
        c.execute("DELETE FROM objects WHERE calendar = ?1 AND name = ?2", [calendar, name])?;
        c.execute("DELETE FROM choices WHERE calendar = ?1 AND uid = ?2", [calendar, &uid])?;
        c.execute(
            "INSERT INTO tombstones (calendar, name, rev) VALUES (?1, ?2, ?3)
             ON CONFLICT (calendar, name) DO UPDATE SET rev = ?3",
            params![calendar, name, rev],
        )?;
        Ok(true)
    }

    /// Recomputes alarms of every object (of one calendar, or all), saving
    /// those that change under a new revision so apps pick them up.
    pub fn reapply(&self, calendar: Option<&str>) -> Result<usize> {
        let patterns = self.patterns();
        let objects = match calendar {
            Some(id) => self.objects(id),
            None => self.all_objects(),
        };
        let c = self.db();
        let mut changed = 0;
        for o in objects {
            let Ok(Parsed { mut cal, .. }) = parse_object(&o.data) else { continue };
            if !Self::rules(&c, &patterns, &o.calendar, &o.uid)?.apply(&mut cal) {
                continue;
            }
            let data = cal.to_ics();
            let rev = Self::touch(&c, &o.calendar)?;
            c.execute(
                "UPDATE objects SET data = ?3, etag = ?4, rev = ?5 WHERE calendar = ?1 AND name = ?2",
                params![o.calendar, o.name, data, etag(&data), rev],
            )?;
            changed += 1;
        }
        Ok(changed)
    }

    /// Which rule gives `o` its alarms.
    pub fn source(&self, o: &Object) -> Source {
        let patterns = self.patterns();
        let Ok(Parsed { cal, .. }) = parse_object(&o.data) else { return Source::Nothing };
        let c = self.db();
        Self::rules(&c, &patterns, &o.calendar, &o.uid).map(|r| r.source(&cal)).unwrap_or(Source::Nothing)
    }

    // Patterns and choices

    pub fn patterns(&self) -> Vec<Pattern> {
        let c = self.db();
        let mut st = c.prepare("SELECT id, name, tag, offsets FROM patterns ORDER BY name").unwrap();
        st.query_map([], |r| {
            let offsets: String = r.get(3)?;
            Ok(Pattern { id: r.get(0)?, name: r.get(1)?, tag: r.get(2)?, offsets: serde_json::from_str(&offsets).unwrap_or_default() })
        })
        .unwrap()
        .filter_map(Result::ok)
        .collect()
    }

    /// Creates (`id` None) or updates a pattern; false if its tag is taken.
    pub fn save_pattern(&self, id: Option<i64>, name: &str, tag: &str, offsets: &[i64]) -> Result<bool> {
        let json = serde_json::to_string(offsets)?;
        let result = {
            let c = self.db();
            match id {
                None => c.execute("INSERT INTO patterns (name, tag, offsets) VALUES (?1, ?2, ?3)", params![name, tag, json]),
                Some(id) => c.execute(
                    "UPDATE patterns SET name = ?2, tag = ?3, offsets = ?4 WHERE id = ?1",
                    params![id, name, tag, json],
                ),
            }
        };
        match result {
            Ok(_) => {
                self.reapply(None)?;
                Ok(true)
            }
            Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == rusqlite::ErrorCode::ConstraintViolation => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    pub fn delete_pattern(&self, id: i64) -> Result<()> {
        {
            let c = self.db();
            c.execute("DELETE FROM patterns WHERE id = ?1", [id])?;
            c.execute("DELETE FROM choices WHERE pattern = ?1", [id])?;
            c.execute("UPDATE calendars SET default_timed = NULL WHERE default_timed = ?1", [id])?;
            c.execute("UPDATE calendars SET default_allday = NULL WHERE default_allday = ?1", [id])?;
        }
        self.reapply(None)?;
        Ok(())
    }

    pub fn choice(&self, calendar: &str, uid: &str) -> Option<i64> {
        let c = self.db();
        c.query_row("SELECT pattern FROM choices WHERE calendar = ?1 AND uid = ?2", [calendar, uid], |r| r.get(0))
            .optional()
            .unwrap()
    }

    /// `None` goes back to automatic.
    pub fn set_choice(&self, calendar: &str, uid: &str, pattern: Option<i64>) -> Result<()> {
        {
            let c = self.db();
            match pattern {
                Some(p) => c.execute(
                    "INSERT INTO choices (calendar, uid, pattern) VALUES (?1, ?2, ?3)
                     ON CONFLICT (calendar, uid) DO UPDATE SET pattern = ?3",
                    params![calendar, uid, p],
                )?,
                None => c.execute("DELETE FROM choices WHERE calendar = ?1 AND uid = ?2", [calendar, uid])?,
            };
        }
        let Some(o) = self.object_by_uid(calendar, uid) else { return Ok(()) };
        let patterns = self.patterns();
        let Ok(Parsed { mut cal, .. }) = parse_object(&o.data) else { return Ok(()) };
        let c = self.db();
        if pattern.is_none() {
            // Back to automatic: valet's alarms from the choice must not be
            // mistaken for manual ones, so start from none of valet's.
            for ev in cal.children.iter_mut() {
                ev.children.retain(|a| a.name != "VALARM" || a.prop("X-VALET").is_none());
            }
        }
        Self::rules(&c, &patterns, calendar, uid)?.apply(&mut cal);
        let data = cal.to_ics();
        if data != o.data {
            let rev = Self::touch(&c, calendar)?;
            c.execute(
                "UPDATE objects SET data = ?3, etag = ?4, rev = ?5 WHERE calendar = ?1 AND name = ?2",
                params![calendar, o.name, data, etag(&data), rev],
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EVENT: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:e1\r\nDTSTART:20261023T080000Z\r\nSUMMARY:Standup\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    fn store() -> Store {
        let s = Store::memory();
        s.create_calendar("work", "Work", None).unwrap();
        s
    }

    #[test]
    fn put_without_patterns_keeps_data() {
        let s = store();
        let put = s.put_object("work", "e1.ics", EVENT, false).unwrap();
        assert!(put.created && !put.rewritten);
        assert_eq!(s.object("work", "e1.ics").unwrap().data, EVENT);
        assert!(matches!(s.put_object("work", "other.ics", EVENT, false), Err(PutError::UidConflict(_))));
        assert!(matches!(s.put_object("nope", "e1.ics", EVENT, false), Err(PutError::NoCalendar)));
    }

    #[test]
    fn defaults_and_choices_rewrite_and_bump_revisions() {
        let s = store();
        s.put_object("work", "e1.ics", EVENT, false).unwrap();
        let rev0 = s.calendar("work").unwrap().rev;

        assert!(s.save_pattern(None, "Meeting", "meeting", &[-12, -2]).unwrap());
        assert!(!s.save_pattern(None, "Other", "meeting", &[-5]).unwrap(), "tags are unique");
        let meeting = s.patterns()[0].id;
        let mut cal = s.calendar("work").unwrap();
        cal.default_timed = Some(meeting);
        s.update_calendar(&cal).unwrap();

        let o = s.object("work", "e1.ics").unwrap();
        assert!(o.data.contains("TRIGGER:-PT12M") && o.data.contains("TRIGGER:-PT2M"));
        assert!(o.rev > rev0);
        let (changed, deleted) = s.changes("work", rev0);
        assert_eq!((changed.len(), deleted.len()), (1, 0));
        assert_eq!(s.source(&o), Source::Default("Meeting".into()));

        assert!(s.save_pattern(None, "Silent", "silent", &[]).unwrap());
        let silent = s.patterns().iter().find(|p| p.tag == "silent").unwrap().id;
        s.set_choice("work", "e1", Some(silent)).unwrap();
        assert!(!s.object("work", "e1.ics").unwrap().data.contains("VALARM"));
        s.set_choice("work", "e1", None).unwrap();
        assert!(s.object("work", "e1.ics").unwrap().data.contains("TRIGGER:-PT12M"));

        let rev1 = s.calendar("work").unwrap().rev;
        assert!(s.delete_object("work", "e1.ics").unwrap());
        assert_eq!(s.changes("work", rev1).1, ["e1.ics"]);
    }

    #[test]
    fn rejects_bad_objects() {
        let s = store();
        let todo = "BEGIN:VCALENDAR\nBEGIN:VTODO\nUID:t\nEND:VTODO\nEND:VCALENDAR\n";
        assert!(matches!(s.put_object("work", "t.ics", todo, false), Err(PutError::Invalid(_))));
        let two = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:a\nEND:VEVENT\nBEGIN:VEVENT\nUID:b\nEND:VEVENT\nEND:VCALENDAR\n";
        assert!(matches!(s.put_object("work", "t.ics", two, false), Err(PutError::Invalid(_))));
    }
}
