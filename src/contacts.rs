//! The people mail was sent to or received from, for completing addresses
//! as they are typed in the composer. Kept in memory: read once from every
//! kept message's headers (in the background, at start), then topped up
//! with what arrived since, at each lookup.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::App;

#[derive(Default)]
struct Person {
    name: String,
    address: String,
    /// Messages of ours it was among the recipients of.
    sent: u32,
    /// Messages it sent us.
    received: u32,
    /// Messages to us it was also a recipient of.
    copied: u32,
    /// The newest of them (seconds since 1970).
    last: i64,
    /// When `name` was given.
    named_at: i64,
}

#[derive(Default)]
struct Book {
    /// The last kept message read (blobs' rowid).
    after: i64,
    people: HashMap<String, Person>,
}

static BOOK: Mutex<Option<Book>> = Mutex::new(None);
/// Someone is reading new messages into the book.
static READING: AtomicBool = AtomicBool::new(false);

fn book() -> std::sync::MutexGuard<'static, Option<Book>> {
    BOOK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Reads the messages kept since the last time into the book, unless that
/// is already under way. The first time reads them all, a batch at a time,
/// so lookups meanwhile get what is read so far.
pub fn update(app: &App) {
    if READING.swap(true, Ordering::AcqRel) {
        return;
    }
    let started = std::time::Instant::now();
    let mine: Vec<String> = app.addresses().iter().map(|a| a.to_lowercase()).collect();
    let mut after = book().as_ref().map_or(0, |b| b.after);
    let mut n = 0;
    loop {
        let batch = app.mail.heads_after(after, 500);
        let Some(&(last, _)) = batch.last() else { break };
        let mut b = book();
        let b = b.get_or_insert_with(Book::default);
        for (_, raw) in &batch {
            read(b, raw, &mine);
        }
        b.after = last;
        after = last;
        n += batch.len();
    }
    if n > 1000 {
        tracing::info!("addresses: {n} messages read in {:.0?}", started.elapsed());
    }
    READING.store(false, Ordering::Release);
}

/// Notes who a message is from and to.
fn read(b: &mut Book, raw: &[u8], mine: &[String]) {
    let Some(m) = mail_parser::MessageParser::default().parse_headers(raw) else { return };
    let date = m.date().map_or(0, |d| d.to_timestamp());
    let people = |a: Option<&mail_parser::Address>| -> Vec<(String, String)> {
        a.map(|a| {
            a.iter()
                .filter_map(|x| {
                    let address = x.address()?.trim().to_string();
                    address.contains('@').then(|| (x.name().unwrap_or("").trim().trim_matches('"').trim().to_string(), address))
                })
                .collect()
        })
        .unwrap_or_default()
    };
    let from = people(m.from());
    let mut to = people(m.to());
    to.extend(people(m.cc()));
    to.extend(people(m.bcc()));
    let ours = from.iter().any(|(_, a)| mine.contains(&a.to_lowercase()));
    let mut note = |(name, address): &(String, String), how: fn(&mut Person)| {
        let key = address.to_lowercase();
        if mine.contains(&key) {
            return;
        }
        let p = b.people.entry(key).or_insert_with(|| Person { address: address.clone(), ..Default::default() });
        how(p);
        p.last = p.last.max(date);
        // the newest name given; a name over none
        if !name.is_empty() && !name.eq_ignore_ascii_case(address) && (p.name.is_empty() || date >= p.named_at) {
            p.name = name.clone();
            p.named_at = date;
        }
    };
    if ours {
        for p in &to {
            note(p, |p| p.sent += 1);
        }
    } else {
        for p in &from {
            note(p, |p| p.received += 1);
        }
        for p in &to {
            note(p, |p| p.copied += 1);
        }
    }
}

/// Addresses that only send: suggested once written to.
fn automated(address: &str) -> bool {
    let local = address.split('@').next().unwrap_or("").to_lowercase();
    ["noreply", "no-reply", "no_reply", "donotreply", "do-not-reply", "do_not_reply", "mailer-daemon", "postmaster", "bounce", "notification", "newsletter"]
        .iter()
        .any(|w| local.contains(w))
}

/// Lowercase without accents, for matching what is typed.
fn fold(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| match c {
            'à' | 'â' | 'ä' | 'á' | 'ã' | 'å' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'î' | 'ï' | 'í' | 'ì' => 'i',
            'ô' | 'ö' | 'ó' | 'ò' | 'õ' => 'o',
            'û' | 'ü' | 'ú' | 'ù' => 'u',
            'ç' => 'c',
            'ñ' => 'n',
            'ÿ' => 'y',
            c => c,
        })
        .collect()
}

/// The people matching what is typed (the start of a word of their name or
/// address, or of their whole name or address), the most written to first.
pub fn suggest(app: &App, typed: &str, limit: usize) -> Vec<(String, String)> {
    update(app);
    book().as_ref().map_or_else(Vec::new, |b| pick(b, typed, limit))
}

fn pick(b: &Book, typed: &str, limit: usize) -> Vec<(String, String)> {
    let typed = fold(typed.trim());
    if typed.is_empty() {
        return Vec::new();
    }
    let matches = |p: &Person| {
        let (name, address) = (fold(&p.name), fold(&p.address));
        name.starts_with(&typed)
            || address.starts_with(&typed)
            || name.split(|c: char| !c.is_alphanumeric()).chain(address.split(|c: char| !c.is_alphanumeric())).any(|w| w.starts_with(&typed))
            || name.contains(&format!(" {typed}"))
    };
    let mut found: Vec<&Person> = b.people.values().filter(|p| (p.sent > 0 || !automated(&p.address)) && matches(p)).collect();
    let score = |p: &Person| p.sent as u64 * 8 + p.received as u64 * 2 + p.copied as u64;
    found.sort_by(|a, b| {
        (b.sent > 0).cmp(&(a.sent > 0)).then(score(b).cmp(&score(a))).then(b.last.cmp(&a.last)).then(a.address.cmp(&b.address))
    });
    found.into_iter().take(limit).map(|p| (p.name.clone(), p.address.clone())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mail(from: &str, to: &str, cc: &str, date: &str) -> Vec<u8> {
        format!("From: {from}\r\nTo: {to}\r\nCc: {cc}\r\nDate: {date}\r\nSubject: s\r\n\r\nbody\r\n").into_bytes()
    }

    fn book(mails: &[Vec<u8>]) -> Book {
        let mut b = Book::default();
        let mine = vec!["me@valet.test".to_string()];
        for m in mails {
            read(&mut b, m, &mine);
        }
        b
    }

    fn find(b: &Book, typed: &str) -> Vec<String> {
        pick(b, typed, 8).into_iter().map(|(_, a)| a).collect()
    }

    #[test]
    fn who_mail_was_sent_to_and_received_from() {
        let b = book(&[
            mail("Me <me@valet.test>", "\"Héloïse Martin\" <heloise@ex.fr>, bob@ex.fr", "", "Mon, 3 Nov 2025 10:00:00 +0100"),
            mail("Me <me@valet.test>", "Héloïse <heloise@ex.fr>", "", "Tue, 4 Nov 2025 10:00:00 +0100"),
            mail("Bob B <bob@ex.fr>", "me@valet.test", "Carol <carol@ex.fr>", "Wed, 5 Nov 2025 10:00:00 +0100"),
            mail("Shop <noreply@shop.fr>", "me@valet.test", "", "Wed, 5 Nov 2025 11:00:00 +0100"),
        ]);
        let h = &b.people["heloise@ex.fr"];
        assert_eq!((h.sent, h.received, h.name.as_str()), (2, 0, "Héloïse"), "the newest name");
        let bob = &b.people["bob@ex.fr"];
        assert_eq!((bob.sent, bob.received, bob.name.as_str()), (1, 1, "Bob B"));
        assert_eq!(b.people["carol@ex.fr"].copied, 1);
        assert!(!b.people.contains_key("me@valet.test"), "our own address is not suggested");
        assert!(automated("noreply@shop.fr") && !automated("bob@ex.fr"));
        assert_eq!(find(&b, "ex"), ["heloise@ex.fr", "bob@ex.fr", "carol@ex.fr"], "the most written to first");
        assert_eq!(find(&b, "bob b"), ["bob@ex.fr"], "by the whole name");
        assert_eq!(find(&b, "car"), ["carol@ex.fr"], "someone only copied");
        assert_eq!(find(&b, "helo"), ["heloise@ex.fr"], "accents aside");
        assert!(find(&b, "shop").is_empty(), "only sends: left out");
    }
}

