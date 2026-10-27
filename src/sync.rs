//! One task per account: replays what mail apps changed here, mirrors each
//! provider folder (new messages, flag changes, deletions), then waits in
//! IDLE on the inbox until the provider or valet has something new. With
//! CONDSTORE, a folder that hasn't changed since the last pass costs a SELECT,
//! and one that has, only its changed flags.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_imap::types::{Flag, NameAttribute};
use futures::TryStreamExt;

use crate::App;
use crate::accounts::{self, Account, Imap};
use crate::mailstore::{Op, Pass, account_path, now};
use crate::{invites, mutf7, notify, util};

pub fn start(app: Arc<App>) {
    for acc in app.accounts() {
        restart(&app, &acc.id);
    }
}

/// (Re)starts the account's sync task, with its current settings; stops it
/// for a retired account.
pub fn restart(app: &Arc<App>, id: &str) {
    let mut syncs = app.syncs.lock().unwrap();
    if let Some(old) = syncs.remove(id) {
        old.abort();
    }
    let Some(acc) = app.account(id).filter(|a| a.retired.is_none()) else { return };
    let app = app.clone();
    let task = tokio::spawn(async move {
        let mut backoff = 15;
        loop {
            // A session only ends in an error; one that got through a pass
            // was a working connection, and the wait starts over.
            let finished = app.mail.passes(&acc.id).finished;
            let Err(e) = session(&app, &acc).await;
            if app.mail.passes(&acc.id).finished > finished {
                backoff = 15;
            }
            let msg = format!("{e:#}");
            tracing::warn!(account = %acc.id, "sync: {msg}");
            app.mail.set_status(&acc.id, false, &msg);
            app.mail.pass(&acc.id, Pass::Fail);
            let waker = app.mail.waker(&acc.id);
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(backoff)) => {}
                _ = waker.notified() => {}
            }
            backoff = (backoff * 2).min(600);
        }
    });
    syncs.insert(id.to_string(), task.abort_handle());
}

struct Folder {
    remote: String,
    mailbox: i64,
}

/// A folder as its SELECT showed it, at the end of a pass that synced it.
#[derive(Clone, Copy, PartialEq)]
struct Seen {
    exists: u32,
    uid_next: Option<u32>,
    modseq: Option<u64>,
}

fn flag_text(f: &Flag) -> Option<String> {
    Some(match f {
        Flag::Seen => "\\Seen".into(),
        Flag::Answered => "\\Answered".into(),
        Flag::Flagged => "\\Flagged".into(),
        Flag::Deleted => "\\Deleted".into(),
        Flag::Draft => "\\Draft".into(),
        Flag::Custom(c) if !c.starts_with('\\') => c.to_string(),
        _ => return None,
    })
}

/// The flag list for an IMAP command, without flags valet can't set.
fn flag_list(flags: &[String]) -> String {
    let f: Vec<&str> = flags.iter().map(String::as_str).filter(|f| !f.eq_ignore_ascii_case("\\Recent")).collect();
    format!("({})", f.join(" "))
}

async fn session(app: &Arc<App>, acc: &Account) -> Result<std::convert::Infallible> {
    let mut s = accounts::imap(acc, &app.mail).await?;
    tracing::info!(account = %acc.id, "logged in");
    let caps = s.capabilities().await?;
    let has_move = caps.has_str("MOVE");
    let has_uidplus = caps.has_str("UIDPLUS");
    let has_idle = caps.has_str("IDLE");
    let condstore = caps.has_str("CONDSTORE");
    let waker = app.mail.waker(&acc.id);
    // Per provider folder, this session's last look at it: a session's first
    // pass goes through every folder's flags.
    let mut seen: HashMap<String, Seen> = HashMap::new();
    // At most one extra pass in a row for the inbox before IDLE, whatever
    // the provider says.
    let mut again = false;
    // Older mail fetched in this session.
    let mut older_total = 0;
    loop {
        let number = app.mail.pass(&acc.id, Pass::Start);
        let hurry = app.mail.hurried(&acc.id);
        replay(app, acc, &mut s, has_move, has_uidplus).await?;
        let mut folders = folders(app, acc, &mut s).await?;
        // The inbox, then the folders asked for (Refresh, on the folder shown).
        folders.sort_by_key(|f| (f.remote != "INBOX", !hurry.contains(&f.remote)));
        let ahead = folders.iter().take_while(|f| f.remote == "INBOX" || hurry.contains(&f.remote)).count();
        let mut fetched = 0;
        let mut back = Older::default();
        let first = app.mail.status(&acc.id).is_none_or(|(ok, _, _)| !ok);
        for (i, f) in folders.iter().enumerate() {
            if i == ahead {
                app.mail.pass(&acc.id, Pass::Ahead(number));
            }
            if first {
                let name = mutf7::decode(&f.remote);
                app.mail.set_status(&acc.id, true, &format!("connected, fetching mail: folder {} of {} ({name}), {fetched} messages so far", i + 1, folders.len()));
            }
            let before = seen.remove(&f.remote);
            let (added, now) = sync_folder(app, acc, &mut s, f, condstore, before).await?;
            fetched += added;
            if let Some(now) = now {
                seen.insert(f.remote.clone(), now);
                let o = older(app, acc, &mut s, f).await?;
                back.fetched += o.fetched;
                back.more |= o.more;
                back.paused |= o.paused;
            }
        }
        older_total += back.fetched;
        if ahead == folders.len() {
            app.mail.pass(&acc.id, Pass::Ahead(number));
        }
        let older_note = if back.more {
            format!("; fetching older mail, {older_total} so far")
        } else if back.paused {
            format!("; older mail continues tomorrow (the provider's daily download limit), {older_total} fetched")
        } else {
            String::new()
        };
        app.mail.set_status(
            &acc.id,
            true,
            &format!("{} folders in sync{}{older_note}", folders.len(), if fetched > 0 { format!(", {fetched} new") } else { String::new() }),
        );
        app.mail.pass(&acc.id, Pass::Finish(number));
        // Ops queued while syncing go out before waiting; older mail left
        // comes in the next pass.
        if !app.mail.ops(&acc.id).is_empty() || back.more {
            continue;
        }
        if !has_idle {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(120)) => {}
                _ = waker.notified() => {}
            }
            continue;
        }
        // What came into the inbox since its turn in this pass (IDLE only
        // tells of what comes after it starts) calls for another pass.
        let inbox = select(&mut s, "INBOX", condstore).await?;
        if !again && seen.get("INBOX").is_some_and(|f| *f != seen_of(&inbox)) {
            again = true;
            continue;
        }
        again = false;
        let mut idle = s.idle();
        idle.init().await?;
        {
            let (wait, stop) = idle.wait_with_timeout(Duration::from_secs(9 * 60));
            tokio::pin!(wait);
            let mut stop = Some(stop);
            tokio::select! {
                r = &mut wait => { r?; }
                _ = waker.notified() => {
                    stop.take();
                    wait.await?;
                }
            }
            drop(stop);
        }
        s = idle.done().await?;
    }
}

/// Lists the provider's folders and makes sure each has a local mailbox. A
/// new account's folders go in the tree as the default tree has them.
async fn folders(app: &App, acc: &Account, s: &mut Imap) -> Result<Vec<Folder>> {
    let names: Vec<_> = s.list(Some(""), Some("*")).await?.try_collect().await?;
    let fresh = !app.mail.mailboxes().iter().any(|m| m.account.as_deref() == Some(acc.id.as_str()));
    let mut out = Vec::new();
    let mut keep = Vec::new();
    for n in &names {
        let attrs = n.attributes();
        let has = |a: &NameAttribute| attrs.iter().any(|x| x == a);
        if has(&NameAttribute::NoSelect) || has(&NameAttribute::All) || has(&NameAttribute::Flagged) {
            continue;
        }
        if attrs.iter().any(|a| matches!(a, NameAttribute::Extension(e) if e.eq_ignore_ascii_case("\\Important") || e.eq_ignore_ascii_case("\\NonExistent"))) {
            continue;
        }
        let remote = if n.name().eq_ignore_ascii_case("INBOX") { "INBOX".to_string() } else { n.name().to_string() };
        let delim = n.delimiter().unwrap_or("/");
        let decoded = mutf7::decode(&remote);
        if acc.skip.iter().any(|s| s == &decoded) {
            continue;
        }
        let shown = if remote == "INBOX" {
            "Inbox".to_string()
        } else {
            let d = decoded.strip_prefix("[Gmail]").map(|r| r.trim_start_matches(delim)).unwrap_or(&decoded);
            d.replace(delim, "/")
        };
        let special = [
            (NameAttribute::Sent, "\\Sent"),
            (NameAttribute::Drafts, "\\Drafts"),
            (NameAttribute::Trash, "\\Trash"),
            (NameAttribute::Junk, "\\Junk"),
            (NameAttribute::Archive, "\\Archive"),
        ]
        .iter()
        .find(|(a, _)| has(a))
        .map(|(_, s)| *s)
        .unwrap_or("");
        let mailbox = app.mail.remote_mailbox(&acc.id, &remote, &account_path(&acc.name, &shown), special)?;
        keep.push(remote.clone());
        out.push(Folder { remote, mailbox });
    }
    app.mail.prune_mailboxes(&acc.id, &keep)?;
    if fresh {
        app.mail.place_account(&acc.id)?;
    }
    // The inbox first: new mail shows up before the rest catches up.
    out.sort_by_key(|f| f.remote != "INBOX");
    Ok(out)
}

async fn select(s: &mut Imap, remote: &str, condstore: bool) -> async_imap::error::Result<async_imap::types::Mailbox> {
    if condstore { s.select_condstore(remote).await } else { s.select(remote).await }
}

fn seen_of(mb: &async_imap::types::Mailbox) -> Seen {
    Seen { exists: mb.exists, uid_next: mb.uid_next, modseq: mb.highest_modseq }
}

/// Brings one folder in line with the provider; returns how many new
/// messages, and the folder as seen (when synced). `before`: as seen at the
/// session's last pass.
async fn sync_folder(app: &Arc<App>, acc: &Account, s: &mut Imap, f: &Folder, condstore: bool, mut before: Option<Seen>) -> Result<(usize, Option<Seen>)> {
    let mb = match select(s, &f.remote, condstore).await {
        Ok(mb) => mb,
        // Deleted since it was listed: the next round forgets it.
        Err(async_imap::error::Error::No(_)) => return Ok((0, None)),
        Err(e) => return Err(e.into()),
    };
    let validity = mb.uid_validity.unwrap_or(0);
    let (known, mut last) = app.mail.remote_state(f.mailbox);
    if known.is_some_and(|v| v != validity) {
        app.mail.reset_mailbox(f.mailbox)?;
        last = 0;
        before = None;
    }
    // A first fetch brings old mail: nothing to tell of. (A folder empty
    // until now has `last` 0 too, but what comes into it is new.)
    let first = known != Some(validity);
    // A folder's first sync: its recent mail, to show soon; older mail
    // follows (`older`).
    let window = last == 0;
    let mut new_uids: Vec<u32> = if window {
        let since = (now() - FIRST_DAYS * 86400).max(acc.mail_from().unwrap_or(0));
        s.uid_search(format!("SINCE {}", util::imap_date(since))).await?.into_iter().collect()
    } else if mb.uid_next.is_some_and(|n| n <= last + 1) {
        Vec::new()
    } else {
        s.uid_search(format!("UID {}:*", last + 1)).await?.into_iter().filter(|u| *u > last).collect()
    };
    new_uids.sort_unstable();
    let mut added = 0;
    for chunk in new_uids.chunks(25) {
        let mut arrived = Vec::new();
        let set: Vec<String> = chunk.iter().map(u32::to_string).collect();
        let fetches: Vec<_> = s.uid_fetch(set.join(","), "(UID FLAGS INTERNALDATE BODY.PEEK[])").await?.try_collect().await?;
        for m in &fetches {
            let (Some(uid), Some(body)) = (m.uid, m.body()) else { continue };
            let flags: Vec<String> = m.flags().filter_map(|f| flag_text(&f)).collect();
            let date = m.internal_date().map(|d| d.timestamp()).unwrap_or_else(now);
            let kept = slim(acc, body, date);
            let (id, new) = app.mail.add_fetched(f.mailbox, &kept, &flags, date, uid)?;
            if new {
                added += 1;
                if f.remote == "INBOX" {
                    invites::from_mail(app, acc, body);
                    if !first && !flags.iter().any(|f| f.eq_ignore_ascii_case("\\Seen")) {
                        arrived.push(id);
                    }
                }
            }
            last = last.max(uid);
        }
        app.mail.set_remote_state(f.mailbox, validity, last)?;
        notify::arrived(app, &arrived);
    }
    // Whatever is older is for `older`, as far as the folder goes now.
    if window {
        last = last.max(mb.uid_next.unwrap_or(1).saturating_sub(1));
    }
    if new_uids.is_empty() || window {
        app.mail.set_remote_state(f.mailbox, validity, last)?;
    }

    // Flags and deletions on the provider's side. With CONDSTORE, when no
    // message left (as many now as before, plus the new ones), only the
    // flags changed since are fetched, if any.
    let rows = app.mail.remote_rows(f.mailbox);
    let now = seen_of(&mb);
    let since = before.filter(|b| condstore && b.modseq.is_some() && now.exists as usize == b.exists as usize + new_uids.len()).and_then(|b| b.modseq);
    if since.is_some() && since == now.modseq {
        return Ok((added, Some(now)));
    }
    if let (Some(since), Some(min)) = (since, rows.iter().map(|r| r.1).min()) {
        let fetches: Vec<_> = s.uid_fetch(format!("{min}:*"), format!("(UID FLAGS) (CHANGEDSINCE {since})")).await?.try_collect().await?;
        let ids: HashMap<u32, (i64, &Vec<String>)> = rows.iter().map(|(id, uid, flags)| (*uid, (*id, flags))).collect();
        for m in &fetches {
            let Some((id, flags)) = m.uid.and_then(|u| ids.get(&u)) else { continue };
            let mut a: Vec<String> = m.flags().filter_map(|f| flag_text(&f)).collect();
            let mut b = (*flags).clone();
            a.sort();
            b.sort();
            if a != b {
                app.mail.sync_flags(*id, &a)?;
            }
        }
    } else if let Some(min) = rows.iter().map(|r| r.1).min() {
        let fetches: Vec<_> = s.uid_fetch(format!("{min}:*"), "(UID FLAGS)").await?.try_collect().await?;
        let remote: HashMap<u32, Vec<String>> =
            fetches.iter().filter_map(|m| Some((m.uid?, m.flags().filter_map(|f| flag_text(&f)).collect()))).collect();
        for (id, uid, flags) in rows {
            match remote.get(&uid) {
                None => app.mail.remove(id)?,
                Some(r) => {
                    let mut a = r.clone();
                    let mut b = flags.clone();
                    a.sort();
                    b.sort();
                    if a != b {
                        app.mail.sync_flags(id, r)?;
                    }
                }
            }
        }
    }
    Ok((added, Some(now)))
}

/// How far back a folder's first sync goes; the rest comes after.
const FIRST_DAYS: i64 = 30;
/// Older mail fetched per folder and pass, so new mail isn't kept waiting.
const OLDER_PER_PASS: usize = 500;
/// Older mail fetched per account and day (Gmail allows about 2.5 GB a
/// day through IMAP, and stops the account's IMAP for the day past it).
const OLDER_PER_DAY: u64 = 1_500_000_000;

/// Per account: (day, bytes of older mail fetched that day).
static SPENT: std::sync::LazyLock<std::sync::Mutex<HashMap<String, (i64, u64)>>> = std::sync::LazyLock::new(Default::default);

fn spent(acc: &Account, more: u64) -> u64 {
    let mut s = SPENT.lock().unwrap();
    let today = now() / 86400;
    let e = s.entry(acc.id.clone()).or_insert((today, 0));
    if e.0 != today {
        *e = (today, 0);
    }
    e.1 += more;
    e.1
}

/// The message as kept: without its attachments if it is from before the
/// account keeps them.
fn slim<'a>(acc: &Account, raw: &'a [u8], date: i64) -> std::borrow::Cow<'a, [u8]> {
    match acc.attachments_from().filter(|&t| date < t).and_then(|_| crate::mime::strip(raw)) {
        Some(s) => std::borrow::Cow::Owned(s),
        None => std::borrow::Cow::Borrowed(raw),
    }
}

/// What `older` did in a folder.
#[derive(Default)]
struct Older {
    fetched: usize,
    /// Left for another pass.
    more: bool,
    /// Stopped for the day.
    paused: bool,
}

/// In the folder just synced (still selected): the attachments asked back
/// (kept without them, but now from after the account keeps them), then
/// the older mail since the account keeps it, newest first, a batch per
/// pass. Done once the folder has it all.
async fn older(app: &Arc<App>, acc: &Account, s: &mut Imap, f: &Folder) -> Result<Older> {
    let mut out = Older::default();
    let since = acc.mail_from().unwrap_or(0);
    let budget_left = || spent(acc, 0) < OLDER_PER_DAY;
    let restore = match acc.attachments_from() {
        Some(t) => app.mail.stripped_rows(f.mailbox, t.max(since)),
        None => app.mail.stripped_rows(f.mailbox, since),
    };
    let filled = app.mail.filled_since(f.mailbox).is_some_and(|d| d <= since);
    if restore.is_empty() && filled {
        return Ok(out);
    }
    if !budget_left() {
        out.paused = true;
        return Ok(out);
    }
    let rows: HashMap<u32, i64> = restore.iter().map(|(id, uid)| (*uid, *id)).collect();
    for chunk in restore.chunks(25) {
        if !budget_left() || out.fetched >= OLDER_PER_PASS {
            out.more = budget_left();
            out.paused = !budget_left();
            return Ok(out);
        }
        let set: Vec<String> = chunk.iter().map(|(_, u)| u.to_string()).collect();
        let fetches: Vec<_> = s.uid_fetch(set.join(","), "(UID BODY.PEEK[])").await?.try_collect().await?;
        for m in &fetches {
            let (Some(uid), Some(body)) = (m.uid, m.body()) else { continue };
            let Some(&id) = rows.get(&uid) else { continue };
            spent(acc, body.len() as u64);
            app.mail.set_content(id, body)?;
            out.fetched += 1;
        }
    }
    if !restore.is_empty() {
        app.mail.gc_blobs();
    }
    if filled {
        return Ok(out);
    }
    let (_, last) = app.mail.remote_state(f.mailbox);
    let query = if since > 0 { format!("SINCE {}", util::imap_date(since)) } else { "ALL".into() };
    let known: std::collections::HashSet<u32> = app.mail.remote_rows(f.mailbox).into_iter().map(|r| r.1).collect();
    let mut missing: Vec<u32> = s.uid_search(query).await?.into_iter().filter(|u| *u <= last && !known.contains(u)).collect();
    missing.sort_unstable_by(|a, b| b.cmp(a));
    for chunk in missing.chunks(25) {
        if !budget_left() || out.fetched >= OLDER_PER_PASS {
            out.more = budget_left();
            out.paused = !budget_left();
            return Ok(out);
        }
        let set: Vec<String> = chunk.iter().map(u32::to_string).collect();
        let fetches: Vec<_> = s.uid_fetch(set.join(","), "(UID FLAGS INTERNALDATE BODY.PEEK[])").await?.try_collect().await?;
        for m in &fetches {
            let (Some(uid), Some(body)) = (m.uid, m.body()) else { continue };
            spent(acc, body.len() as u64);
            let flags: Vec<String> = m.flags().filter_map(|f| flag_text(&f)).collect();
            let date = m.internal_date().map(|d| d.timestamp()).unwrap_or_else(now);
            let (_, new) = app.mail.add_fetched(f.mailbox, &slim(acc, body, date), &flags, date, uid)?;
            // Invitations in it too, for events still to come.
            if new && f.remote == "INBOX" {
                invites::from_mail(app, acc, body);
            }
            out.fetched += 1;
        }
    }
    app.mail.set_filled_since(f.mailbox, since)?;
    Ok(out)
}

fn is_refusal(e: &async_imap::error::Error) -> bool {
    matches!(e, async_imap::error::Error::No(_) | async_imap::error::Error::Bad(_))
}

/// Replays ops on the provider, in order. A refused op is retried a few
/// times, then dropped; a broken connection ends the session.
async fn replay(app: &App, acc: &Account, s: &mut Imap, has_move: bool, has_uidplus: bool) -> Result<()> {
    for (id, op) in app.mail.ops(&acc.id) {
        let result = run_op(app, s, &op, has_move, has_uidplus).await;
        match result {
            Ok(()) => app.mail.op_done(id),
            Err(e) if is_refusal(&e) => {
                let dropped = app.mail.op_failed(id, &e.to_string());
                tracing::warn!(account = %acc.id, ?op, dropped, "provider refused: {e}");
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

async fn drain<S: futures::Stream<Item = async_imap::error::Result<T>>, T>(stream: S) -> async_imap::error::Result<()> {
    let _: Vec<T> = std::pin::pin!(stream).try_collect().await?;
    Ok(())
}

async fn run_op(app: &App, s: &mut Imap, op: &Op, has_move: bool, has_uidplus: bool) -> async_imap::error::Result<()> {
    match op {
        Op::Flags { remote, uid, add, remove } => {
            s.select(remote).await?;
            if !add.is_empty() {
                drain(s.uid_store(uid.to_string(), format!("+FLAGS.SILENT {}", flag_list(add))).await?).await?;
            }
            if !remove.is_empty() {
                drain(s.uid_store(uid.to_string(), format!("-FLAGS.SILENT {}", flag_list(remove))).await?).await?;
            }
        }
        Op::Expunge { remote, uid } => {
            s.select(remote).await?;
            drain(s.uid_store(uid.to_string(), "+FLAGS.SILENT (\\Deleted)").await?).await?;
            if has_uidplus {
                drain(s.uid_expunge(uid.to_string()).await?).await?;
            } else {
                drain(s.expunge().await?).await?;
            }
        }
        Op::Move { remote, uid, to } => {
            s.select(remote).await?;
            if has_move {
                s.uid_mv(uid.to_string(), to).await?;
            } else {
                s.uid_copy(uid.to_string(), to).await?;
                drain(s.uid_store(uid.to_string(), "+FLAGS.SILENT (\\Deleted)").await?).await?;
                drain(s.expunge().await?).await?;
            }
        }
        Op::Copy { remote, uid, to } => {
            s.select(remote).await?;
            s.uid_copy(uid.to_string(), to).await?;
        }
        Op::Append { to, blob, flags } => {
            let Some(raw) = app.mail.blob(blob) else { return Ok(()) };
            let flags = flag_list(flags);
            s.append(to, Some(flags.as_str()), None, raw).await?;
        }
    }
    Ok(())
}
