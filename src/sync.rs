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

/// (Re)starts the account's sync task, with its current settings.
pub fn restart(app: &Arc<App>, id: &str) {
    let mut syncs = app.syncs.lock().unwrap();
    if let Some(old) = syncs.remove(id) {
        old.abort();
    }
    let Some(acc) = app.account(id) else { return };
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
    loop {
        app.mail.pass(&acc.id, Pass::Start);
        let hurry = app.mail.hurried(&acc.id);
        replay(app, acc, &mut s, has_move, has_uidplus).await?;
        let mut folders = folders(app, acc, &mut s).await?;
        // The inbox, then the folders asked for (Refresh, on the folder shown).
        folders.sort_by_key(|f| (f.remote != "INBOX", !hurry.contains(&f.remote)));
        let ahead = folders.iter().take_while(|f| f.remote == "INBOX" || hurry.contains(&f.remote)).count();
        let mut fetched = 0;
        let first = app.mail.status(&acc.id).is_none_or(|(ok, _, _)| !ok);
        for (i, f) in folders.iter().enumerate() {
            if i == ahead {
                app.mail.pass(&acc.id, Pass::Ahead);
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
            }
        }
        if ahead == folders.len() {
            app.mail.pass(&acc.id, Pass::Ahead);
        }
        app.mail.set_status(&acc.id, true, &format!("{} folders in sync{}", folders.len(), if fetched > 0 { format!(", {fetched} new") } else { String::new() }));
        app.mail.pass(&acc.id, Pass::Finish);
        // Ops queued while syncing go out before waiting.
        if !app.mail.ops(&acc.id).is_empty() {
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
    let mut new_uids: Vec<u32> = if last == 0 {
        let since = util::imap_date(now() - acc.history_days as i64 * 86400);
        s.uid_search(format!("SINCE {since}")).await?.into_iter().collect()
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
            let (id, new) = app.mail.add_fetched(f.mailbox, body, &flags, date, uid)?;
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
    if new_uids.is_empty() {
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
