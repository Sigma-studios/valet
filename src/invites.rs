//! Meeting invitations found in incoming mail (iMIP): a REQUEST lands in the
//! calendar (the event's own if it is already there, otherwise
//! "Invitations"), a CANCEL marks it cancelled. Answering from valet's web
//! page emails the organizer an iTIP REPLY from the account it came to.

use anyhow::{Context, Result};

use crate::App;
use crate::accounts::{self, Account};
use crate::ical::{self, Component, Property, mailto};
use crate::mailstore::now;
use crate::store::name_for_uid;

pub const CALENDAR: &str = "invitations";

/// A calendar part: an `.ics`, whether an invitation or not.
pub fn is_calendar(p: &mail_parser::MessagePart) -> bool {
    use mail_parser::MimeHeaders;
    p.content_type().is_some_and(|ct| {
        let t = format!("{}/{}", ct.ctype(), ct.subtype().unwrap_or("")).to_ascii_lowercase();
        t == "text/calendar" || t == "application/ics"
    })
}

/// The text of the message's calendar parts.
pub fn calendar_texts(m: &mail_parser::Message) -> Vec<String> {
    m.parts
        .iter()
        .filter(|p| is_calendar(p))
        .filter_map(|p| match &p.body {
            mail_parser::PartType::Text(t) => Some(t.to_string()),
            mail_parser::PartType::Binary(b) | mail_parser::PartType::InlineBinary(b) => Some(String::from_utf8_lossy(b).into_owned()),
            _ => None,
        })
        .collect()
}

/// Whether the message invites to an event (a calendar part with METHOD:REQUEST,
/// not an answer, a cancellation, or an event just shared).
pub fn is_invitation(m: &mail_parser::Message) -> bool {
    calendar_texts(m).iter().any(|t| {
        ical::parse(t).ok().and_then(|mut c| c.pop()).is_some_and(|c| c.value("METHOD").is_some_and(|v| v.eq_ignore_ascii_case("REQUEST")))
    })
}

fn calendar_parts(raw: &[u8]) -> Vec<String> {
    let Some(m) = mail_parser::MessageParser::default().parse(raw) else { return vec![] };
    calendar_texts(&m)
}

fn master(cal: &Component) -> Option<&Component> {
    let mut ev = cal.children.iter().filter(|c| c.name == "VEVENT");
    let first = ev.clone().next();
    ev.find(|e| e.prop("RECURRENCE-ID").is_none()).or(first)
}

/// Handles the invitations in a message that just arrived in `acc`'s inbox.
pub fn from_mail(app: &App, acc: &Account, raw: &[u8]) {
    let message = crate::mailstore::message_id(raw);
    for text in calendar_parts(raw) {
        if let Err(e) = handle(app, acc, &text, &message) {
            tracing::info!(account = %acc.id, "invitation not handled: {e:#}");
        }
    }
}

fn handle(app: &App, acc: &Account, text: &str, message: &str) -> Result<()> {
    let mut cals = ical::parse(text)?;
    let mut cal = cals.pop().context("no VCALENDAR")?;
    let method = cal.value("METHOD").unwrap_or("").to_ascii_uppercase();
    if method != "REQUEST" && method != "CANCEL" {
        return Ok(());
    }
    let ev = master(&cal).context("no VEVENT")?;
    let uid = ev.value("UID").context("no UID")?.to_string();
    let sequence: i64 = ev.value("SEQUENCE").and_then(|s| s.parse().ok()).unwrap_or(0);
    let dtstart = ev.value("DTSTART").unwrap_or("").to_string();
    let recurring = ev.prop("RRULE").is_some();
    if !recurring && dtstart.get(..8).is_some_and(|d| d < crate::util::today().as_str()) {
        return Ok(());
    }
    // Mail can come in out of order (older mail is fetched newest first):
    // what a later version or a cancellation already said stands.
    if app.mail.invite_superseded(&acc.id, &uid, &method, sequence) {
        return Ok(());
    }
    let summary = ev.value("SUMMARY").unwrap_or("").to_string();
    let organizer = ev.value("ORGANIZER").map(mailto).unwrap_or_default();
    if !app.mail.add_invite(&acc.id, &uid, &method, sequence, &summary, &organizer, &dtstart, text, message)? {
        return Ok(());
    }
    cal.props.retain(|p| p.name != "METHOD");
    let store = &app.store;
    let existing = store.find_uid(&uid);
    if method == "CANCEL" {
        if let Some(o) = existing {
            let mut stored = ical::parse(&o.data)?.pop().context("stored event")?;
            for e in stored.children.iter_mut().filter(|c| c.name == "VEVENT") {
                e.props.retain(|p| p.name != "STATUS");
                e.push("STATUS", "CANCELLED");
            }
            store.put_object(&o.calendar, &o.name, &stored.to_ics(), false).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        }
        return Ok(());
    }
    // An update keeps the answer already given.
    if let Some(o) = &existing {
        let old = ical::parse(&o.data)?.pop().context("stored event")?;
        if let Some(old_ev) = master(&old) {
            let mine: Vec<&Property> = old_ev.props.iter().filter(|p| p.name == "ATTENDEE" && is_me(app, acc, &p.value)).collect();
            for e in cal.children.iter_mut().filter(|c| c.name == "VEVENT") {
                for p in e.props.iter_mut().filter(|p| p.name == "ATTENDEE") {
                    if let Some(m) = mine.iter().find(|m| mailto(&m.value).eq_ignore_ascii_case(&mailto(&p.value))) {
                        p.params = m.params.clone();
                    }
                }
            }
        }
    }
    let (calendar, name) = match existing {
        Some(o) => (o.calendar, o.name),
        None => {
            if store.calendar(CALENDAR).is_none() {
                store.create_calendar(CALENDAR, "Invitations", Some("#f59e0b"))?;
            }
            (CALENDAR.to_string(), name_for_uid(&uid))
        }
    };
    store.put_object(&calendar, &name, &cal.to_ics(), false).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    Ok(())
}

/// Once: finds the mail the invitations recorded before valet kept it came
/// in, a stored message with a calendar part of the same event and version.
pub fn find_mails(app: &App) {
    if !app.mail.first_time("invite_mails") || !app.mail.invites_without_mail() {
        return;
    }
    let mut found = 0;
    for (account, message, blob) in app.mail.account_rows() {
        let Some(raw) = app.mail.blob(&blob) else { continue };
        if !raw.windows(8).any(|w| w.eq_ignore_ascii_case(b"calendar")) && !raw.windows(4).any(|w| w.eq_ignore_ascii_case(b"/ics")) {
            continue;
        }
        for text in calendar_parts(&raw) {
            let Some(cal) = ical::parse(&text).ok().and_then(|mut c| c.pop()) else { continue };
            let method = cal.value("METHOD").unwrap_or("").to_ascii_uppercase();
            let Some(ev) = master(&cal) else { continue };
            let Some(uid) = ev.value("UID") else { continue };
            let sequence: i64 = ev.value("SEQUENCE").and_then(|s| s.parse().ok()).unwrap_or(0);
            found += app.mail.set_invite_mail(&account, uid, &method, sequence, &message).unwrap_or(0);
        }
    }
    tracing::info!("found the mail of {found} past invitations");
}

fn is_me(app: &App, acc: &Account, value: &str) -> bool {
    let a = mailto(value);
    acc.sends_as(&a) || app.addresses().iter().any(|x| x.eq_ignore_ascii_case(&a))
}

/// Answers invitation `id` (`ACCEPTED`, `TENTATIVE`, `DECLINED`): emails the
/// organizer, updates the event, and moves it to `calendar` when given.
pub async fn respond(app: &App, id: i64, partstat: &str, calendar: Option<&str>) -> Result<()> {
    let inv = app.mail.invite(id).context("no such invitation")?;
    let acc = &app.account(&inv.account).context("account gone")?;
    let cal = ical::parse(&inv.ics)?.pop().context("bad invitation")?;
    let ev = master(&cal).context("no VEVENT")?;
    let me = ev
        .props
        .iter()
        .find(|p| p.name == "ATTENDEE" && is_me(app, acc, &p.value))
        .map(|p| mailto(&p.value))
        .unwrap_or_else(|| acc.address.clone());

    let mut reply = Component::new("VCALENDAR");
    reply.push("VERSION", "2.0");
    reply.push("PRODID", "-//valet//EN");
    reply.push("METHOD", "REPLY");
    let mut r = Component::new("VEVENT");
    for name in ["UID", "SEQUENCE", "DTSTART", "DTEND", "DURATION", "SUMMARY", "ORGANIZER", "RECURRENCE-ID"] {
        if let Some(p) = ev.prop(name) {
            r.props.push(p.clone());
        }
    }
    r.push("DTSTAMP", &stamp(now()));
    r.props.push(Property { name: "ATTENDEE".into(), params: format!(";PARTSTAT={partstat}"), value: format!("mailto:{me}") });
    reply.children.push(r);
    let ics = reply.to_ics();

    let verb = match partstat {
        "ACCEPTED" => "Accepted",
        "TENTATIVE" => "Tentative",
        _ => "Declined",
    };
    let boundary = format!("valet-{}", uuid::Uuid::new_v4().simple());
    let message = format!(
        "From: {me}\r\nTo: {org}\r\nSubject: {verb}: {subject}\r\nDate: {date}\r\nMessage-ID: <{mid}@valet>\r\nMIME-Version: 1.0\r\n\
         Content-Type: multipart/alternative; boundary=\"{boundary}\"\r\n\r\n\
         --{boundary}\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{verb}: {summary}\r\n\
         --{boundary}\r\nContent-Type: text/calendar; charset=utf-8; method=REPLY\r\n\r\n{ics}\
         --{boundary}--\r\n",
        org = inv.organizer,
        subject = crate::smtpd::encode_header(&inv.summary),
        summary = inv.summary,
        date = crate::util::rfc2822(now()),
        mid = uuid::Uuid::new_v4().simple(),
    );
    if !inv.organizer.is_empty() {
        accounts::send(acc, &app.mail, &me, std::slice::from_ref(&inv.organizer), message.as_bytes()).await?;
    }

    // The calendar copy.
    if let Some(o) = app.store.find_uid(&inv.uid) {
        if partstat == "DECLINED" {
            app.store.delete_object(&o.calendar, &o.name)?;
        } else {
            let mut stored = ical::parse(&o.data)?.pop().context("stored event")?;
            for e in stored.children.iter_mut().filter(|c| c.name == "VEVENT") {
                for p in e.props.iter_mut().filter(|p| p.name == "ATTENDEE" && mailto(&p.value).eq_ignore_ascii_case(&me)) {
                    let others: String = p
                        .params
                        .split(';')
                        .filter(|s| !s.is_empty() && !s.to_ascii_uppercase().starts_with("PARTSTAT=") && !s.to_ascii_uppercase().starts_with("RSVP="))
                        .map(|s| format!(";{s}"))
                        .collect();
                    p.params = format!("{others};PARTSTAT={partstat}");
                }
            }
            let target = calendar.filter(|c| *c != o.calendar && app.store.calendar(c).is_some());
            match target {
                Some(t) => {
                    app.store.put_object(t, &o.name, &stored.to_ics(), false).map_err(|e| anyhow::anyhow!("{e:?}"))?;
                    app.store.delete_object(&o.calendar, &o.name)?;
                }
                None => {
                    app.store.put_object(&o.calendar, &o.name, &stored.to_ics(), false).map_err(|e| anyhow::anyhow!("{e:?}"))?;
                }
            }
        }
    }
    app.mail.set_invite_response(id, partstat)?;
    Ok(())
}

/// Leaves the invitation unanswered for good: the organizer isn't told, and
/// the event leaves the Invitations calendar (not one it was put in). It
/// can still be answered later.
pub fn ignore(app: &App, id: i64) -> Result<()> {
    let inv = app.mail.invite(id).context("no such invitation")?;
    if let Some(o) = app.store.find_uid(&inv.uid).filter(|o| o.calendar == CALENDAR) {
        app.store.delete_object(&o.calendar, &o.name)?;
    }
    app.mail.set_invite_response(id, "IGNORED")
}

/// `20261022T140300Z`.
pub fn stamp(unix: i64) -> String {
    let (y, m, d) = crate::util::civil(unix.div_euclid(86400));
    let s = unix.rem_euclid(86400);
    format!("{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z", s / 3600, s % 3600 / 60, s % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_calendar_parts() {
        let raw = b"From: o@x.fr\r\nTo: me@b.c\r\nSubject: Invitation\r\nMIME-Version: 1.0\r\nContent-Type: multipart/alternative; boundary=\"b\"\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nhi\r\n--b\r\nContent-Type: text/calendar; method=REQUEST; charset=utf-8\r\n\r\nBEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:inv1\r\nDTSTART:20991023T100000Z\r\nSUMMARY:Sync\r\nORGANIZER:mailto:o@x.fr\r\nATTENDEE;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:me@b.c\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n--b--\r\n";
        let parts = calendar_parts(raw);
        assert_eq!(parts.len(), 1);
        assert!(parts[0].contains("UID:inv1"));
        assert_eq!(stamp(20748 * 86400 + 3661), "20261022T010101Z");
    }
}
