//! Mail storage: a local copy of each account's folders (named
//! `Accounts/<Account>/<folder>`), plus the user's own tree of folders, whose
//! nodes gather real folders of several accounts (their rows link to the real
//! rows). `INBOX` is the node gathering every account's inbox, *All mail*
//! the one with every account folder's mail. See [`tree`].
//!
//! Mail apps see local UIDs; each row remembers its UID on the provider. What
//! apps change (flags, deletions, moves, appends) is applied here at once and
//! queued as an [`Op`] for the account's sync task to replay on the provider.
//! A message moved or appended here exists locally before the provider has
//! it ("placeholder", no remote UID); when the sync later fetches it, it is
//! matched to the placeholder instead of being added twice.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{Notify, broadcast, watch};

mod tree;

pub use tree::{ALL_MAIL, Node, Place, SPECIALS, Shift};

pub const ALL_INBOXES: &str = "INBOX";
/// Why mail can't be moved, copied or saved into All mail.
pub const ALL_MAIL_ISNT_A_PLACE: &str = "All mail shows the mail of every folder: put it in one of them";
/// Where the accounts' real folders live: `Accounts/<Account>/<folder>`.
pub const ACCOUNTS: &str = "Accounts";

/// The local name of an account's folder.
pub fn account_path(account_name: &str, folder: &str) -> String {
    format!("{ACCOUNTS}/{account_name}/{folder}")
}

#[derive(Clone, Debug)]
pub struct Mailbox {
    pub id: i64,
    /// What apps see, `/`-separated, Unicode.
    pub name: String,
    pub account: Option<String>,
    /// The provider's name for it (modified UTF-7, as the provider spells it).
    pub remote: Option<String>,
    /// `\Sent`, `\Trash`… or empty: the provider's flag for a real folder,
    /// the tree's for a node.
    pub special: String,
    pub uidvalidity: u32,
    pub uidnext: u32,
}

impl Mailbox {
    /// A node of the tree (gathers real folders), not an account's folder.
    pub fn is_node(&self) -> bool {
        self.account.is_none()
    }

    /// The tree's All mail: every account folder's mail, filed nowhere itself.
    pub fn is_all_mail(&self) -> bool {
        self.is_node() && self.special == ALL_MAIL
    }

    pub fn is_inbox(&self) -> bool {
        self.remote.as_deref().is_some_and(|r| r.eq_ignore_ascii_case("INBOX"))
    }

    /// An account folder's path within its account (`Accounts/Work/Projects/2026` → `Projects/2026`).
    pub fn path(&self) -> &str {
        if self.is_node() {
            return &self.name;
        }
        self.name.splitn(3, '/').nth(2).unwrap_or(&self.name)
    }
}

#[derive(Clone, Debug)]
pub struct Msg {
    pub id: i64,
    pub mailbox: i64,
    pub uid: u32,
    pub blob: String,
    pub flags: Vec<String>,
    /// Unix seconds.
    pub date: i64,
    pub size: u32,
    pub modseq: i64,
    pub remote_uid: Option<u32>,
    pub link: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    Flags { remote: String, uid: u32, add: Vec<String>, remove: Vec<String> },
    Expunge { remote: String, uid: u32 },
    Move { remote: String, uid: u32, to: String },
    Copy { remote: String, uid: u32, to: String },
    Append { to: String, blob: String, flags: Vec<String> },
}

pub struct MailStore {
    conn: Mutex<Connection>,
    /// Something changed in some mailbox (sessions in IDLE re-check theirs).
    pub changed: broadcast::Sender<()>,
    wake: Mutex<HashMap<String, Arc<Notify>>>,
    /// Per account: how far its sync has got.
    passes: Mutex<HashMap<String, Passes>>,
    /// Bumped whenever `passes` changes.
    passed: watch::Sender<()>,
    /// Per account: provider folders its next pass syncs right after the inbox.
    hurry: Mutex<HashMap<String, HashSet<String>>>,
}

/// What an account's mail takes here, compressed, by kind.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Usage {
    pub messages: u64,
    /// Out of Spam and Trash: the messages without their attachments (text,
    /// formatting, headers); their attachments (about, from their size).
    pub text: u64,
    pub attachments: u64,
    /// In Spam or Trash, everything.
    pub bins: u64,
    pub attachment_count: u64,
    /// Attachments not kept (from before the account keeps them).
    pub removed: u64,
}

impl Usage {
    pub fn total(&self) -> u64 {
        self.text + self.attachments + self.bins
    }
}

/// What keeping less of an account's mail would remove.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Trim {
    pub messages: u64,
    /// Freed: what only those messages use.
    pub message_bytes: u64,
    /// Messages that would lose attachments, how many, and about what they weigh.
    pub with_attachments: u64,
    pub attachments: u64,
    pub attachment_bytes: u64,
}

impl Trim {
    pub fn is_empty(&self) -> bool {
        self.messages == 0 && self.with_attachments == 0
    }
}

/// An account's sync passes, numbered as they start, for Refresh to know
/// when one that started after it is done. (A pass cut short, the sync
/// restarted with new settings, never finishes: counting finished passes
/// would then fall behind for good.)
#[derive(Clone, Copy, Debug, Default)]
pub struct Passes {
    /// The number of the latest pass started.
    pub started: u64,
    /// The latest through the inbox and the folders asked for first.
    pub ahead: u64,
    /// The latest finished.
    pub finished: u64,
    /// Sessions ended by an error.
    pub failed: u64,
}

pub enum Pass {
    Start,
    /// Of the pass with that number.
    Ahead(u64),
    Finish(u64),
    Fail,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS blobs (hash TEXT PRIMARY KEY, data BLOB NOT NULL, atts INTEGER, att_bytes INTEGER, removed INTEGER, text_bytes INTEGER, len INTEGER);
-- What each kept message weighs, apart from it (reading a column after a
-- message's data means reading all of it).
CREATE TABLE IF NOT EXISTS facts (
    hash TEXT PRIMARY KEY, len INTEGER NOT NULL, atts INTEGER NOT NULL, att_bytes INTEGER NOT NULL,
    removed INTEGER NOT NULL, text_bytes INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS mailboxes (
    id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, account TEXT, remote TEXT,
    special TEXT NOT NULL DEFAULT '', uidvalidity INTEGER NOT NULL, uidnext INTEGER NOT NULL DEFAULT 1,
    remote_validity INTEGER, remote_last INTEGER NOT NULL DEFAULT 0, filled_since INTEGER);
CREATE UNIQUE INDEX IF NOT EXISTS mailboxes_remote ON mailboxes (account, remote);
CREATE TABLE IF NOT EXISTS messages (
    id INTEGER PRIMARY KEY, mailbox INTEGER NOT NULL, uid INTEGER NOT NULL, blob TEXT NOT NULL,
    flags TEXT NOT NULL, date INTEGER NOT NULL, size INTEGER NOT NULL, message_id TEXT NOT NULL,
    remote_uid INTEGER, link INTEGER, modseq INTEGER NOT NULL, UNIQUE (mailbox, uid));
CREATE INDEX IF NOT EXISTS messages_remote ON messages (mailbox, remote_uid);
CREATE INDEX IF NOT EXISTS messages_link ON messages (link);
CREATE INDEX IF NOT EXISTS messages_message_id ON messages (message_id);
CREATE TABLE IF NOT EXISTS ops (
    id INTEGER PRIMARY KEY, account TEXT NOT NULL, op TEXT NOT NULL, tries INTEGER NOT NULL DEFAULT 0, error TEXT);
CREATE TABLE IF NOT EXISTS tokens (account TEXT PRIMARY KEY, refresh TEXT NOT NULL, access TEXT, expires INTEGER);
CREATE TABLE IF NOT EXISTS status (account TEXT PRIMARY KEY, ok INTEGER NOT NULL, message TEXT NOT NULL, at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS doomed (mailbox INTEGER NOT NULL, blob TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS accounts (id TEXT PRIMARY KEY, position INTEGER NOT NULL, data TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS tree (
    mailbox INTEGER PRIMARY KEY, parent INTEGER, position INTEGER NOT NULL, label TEXT NOT NULL, special TEXT NOT NULL DEFAULT '');
CREATE TABLE IF NOT EXISTS tree_members (node INTEGER NOT NULL, member INTEGER NOT NULL, PRIMARY KEY (node, member));
CREATE INDEX IF NOT EXISTS tree_members_member ON tree_members (member);
CREATE TABLE IF NOT EXISTS invites (
    id INTEGER PRIMARY KEY, account TEXT NOT NULL, uid TEXT NOT NULL, method TEXT NOT NULL, sequence INTEGER NOT NULL,
    summary TEXT NOT NULL, organizer TEXT NOT NULL, dtstart TEXT NOT NULL, ics TEXT NOT NULL,
    response TEXT, received INTEGER NOT NULL, message TEXT NOT NULL DEFAULT '', UNIQUE (account, uid, method, sequence));
CREATE TABLE IF NOT EXISTS arrivals (seq INTEGER PRIMARY KEY AUTOINCREMENT, message INTEGER NOT NULL, at INTEGER NOT NULL);
";

/// Arrivals (`a`) whose message is still unread in its account's inbox.
const STILL_NEW: &str = "arrivals a JOIN messages m ON m.id = a.message JOIN mailboxes b ON b.id = m.mailbox
    WHERE b.account IS NOT NULL AND b.remote = 'INBOX' COLLATE NOCASE AND ' ' || m.flags || ' ' NOT LIKE '% \\Seen %'";

/// Mailboxes, with a node's special use taken from the tree.
const MBOX_SELECT: &str = "SELECT m.id, m.name, m.account, m.remote, CASE WHEN m.account IS NULL THEN COALESCE(t.special, '') ELSE m.special END,
    m.uidvalidity, m.uidnext FROM mailboxes m LEFT JOIN tree t ON t.mailbox = m.id";
const MSG_COLS: &str = "id, mailbox, uid, blob, flags, date, size, modseq, remote_uid, link";

fn mbox_row(r: &rusqlite::Row) -> rusqlite::Result<Mailbox> {
    Ok(Mailbox {
        id: r.get(0)?,
        name: r.get(1)?,
        account: r.get(2)?,
        remote: r.get(3)?,
        special: r.get(4)?,
        uidvalidity: r.get(5)?,
        uidnext: r.get(6)?,
    })
}

fn msg_row(r: &rusqlite::Row) -> rusqlite::Result<Msg> {
    let flags: String = r.get(4)?;
    Ok(Msg {
        id: r.get(0)?,
        mailbox: r.get(1)?,
        uid: r.get(2)?,
        blob: r.get(3)?,
        flags: split_flags(&flags),
        date: r.get(5)?,
        size: r.get(6)?,
        modseq: r.get(7)?,
        remote_uid: r.get(8)?,
        link: r.get(9)?,
    })
}

fn boxes(c: &Connection) -> Vec<Mailbox> {
    let mut st = c.prepare(&format!("{MBOX_SELECT} ORDER BY m.name = 'INBOX' DESC, m.name")).unwrap();
    st.query_map([], mbox_row).unwrap().filter_map(Result::ok).collect()
}

fn mailbox(c: &Connection, id: i64) -> Option<Mailbox> {
    c.query_row(&format!("{MBOX_SELECT} WHERE m.id = ?1"), [id], mbox_row).optional().unwrap()
}

pub fn split_flags(s: &str) -> Vec<String> {
    s.split_whitespace().map(str::to_string).collect()
}

fn join_flags(f: &[String]) -> String {
    let mut f: Vec<&str> = f.iter().map(String::as_str).filter(|f| !f.eq_ignore_ascii_case("\\Recent")).collect();
    f.sort_unstable();
    f.dedup();
    f.join(" ")
}

/// How every compressed message starts; a message as received never does.
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];

/// A message ready to keep: its hash, compressed, what its attachments
/// weigh. Worked out before taking the store.
struct Packed {
    hash: String,
    data: Vec<u8>,
    atts: crate::mime::Attachments,
    text_bytes: u64,
}

impl Packed {
    fn of(raw: &[u8]) -> Packed {
        let data = pack(raw);
        Packed { hash: hex::encode(Sha256::digest(raw)), atts: crate::mime::attachments(raw), text_bytes: text_bytes(raw, data.len()), data }
    }
}

/// Notes what a kept message weighs (`len`: kept, compressed).
fn put_facts(c: &Connection, hash: &str, len: usize, a: crate::mime::Attachments, text_bytes: u64) -> rusqlite::Result<usize> {
    c.execute(
        "INSERT INTO facts (hash, len, atts, att_bytes, removed, text_bytes) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT (hash) DO UPDATE SET len = ?2, atts = ?3, att_bytes = ?4, removed = ?5, text_bytes = ?6",
        params![hash, len as i64, a.count, a.bytes as i64, a.removed, text_bytes as i64],
    )
}

/// What the message takes kept without its attachments (`stored`: kept with them).
fn text_bytes(raw: &[u8], stored: usize) -> u64 {
    match crate::mime::strip(raw) {
        Some(slim) => pack(&slim).len().min(stored) as u64,
        None => stored as u64,
    }
}

fn pack(raw: &[u8]) -> Vec<u8> {
    zstd::bulk::compress(raw, 6).unwrap_or_else(|_| raw.to_vec())
}

fn unpack(data: Vec<u8>) -> Vec<u8> {
    if data.starts_with(&ZSTD_MAGIC) {
        zstd::decode_all(data.as_slice()).unwrap_or(data)
    } else {
        data
    }
}

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

pub fn message_id(raw: &[u8]) -> String {
    let p = crate::mime::parse(raw);
    p.header(raw, "Message-ID").unwrap_or_default()
}

pub enum StoreMode {
    Set,
    Add,
    Remove,
}

impl MailStore {
    pub fn open(path: &Path) -> Result<MailStore> {
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.execute_batch("PRAGMA journal_mode = WAL;")?;
        Self::with(conn)
    }

    pub fn memory() -> MailStore {
        Self::with(Connection::open_in_memory().unwrap()).unwrap()
    }

    fn with(conn: Connection) -> Result<MailStore> {
        conn.execute_batch(SCHEMA)?;
        let store = MailStore { conn: Mutex::new(conn), changed: broadcast::channel(64).0, wake: Mutex::new(HashMap::new()), passes: Mutex::new(HashMap::new()), passed: watch::channel(()).0, hurry: Mutex::new(HashMap::new()) };
        {
            let c = store.db();
            c.execute(
                "INSERT OR IGNORE INTO mailboxes (name, uidvalidity) VALUES (?1, ?2)",
                params![ALL_INBOXES, now() as u32],
            )?;
            tree::migrate(&c)?;
            // Added later: the Message-ID of the mail an invitation came in;
            // what a message's attachments weigh; how far back a folder's
            // older mail has been fetched.
            for (table, column, decl) in [
                ("invites", "message", "TEXT NOT NULL DEFAULT ''"),
                ("blobs", "atts", "INTEGER"),
                ("blobs", "att_bytes", "INTEGER"),
                ("blobs", "removed", "INTEGER"),
                ("blobs", "text_bytes", "INTEGER"),
                ("blobs", "len", "INTEGER"),
                ("mailboxes", "filled_since", "INTEGER"),
            ] {
                let has: bool = c.query_row(&format!("SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = '{column}'"), [], |r| r.get(0))?;
                if !has {
                    c.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl};"))?;
                }
            }
        }
        Ok(store)
    }

    fn db(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn next_modseq(c: &Connection) -> rusqlite::Result<i64> {
        c.query_row(
            "INSERT INTO meta (key, value) VALUES ('modseq', 1)
             ON CONFLICT (key) DO UPDATE SET value = value + 1 RETURNING value",
            [],
            |r| r.get(0),
        )
    }

    fn notify(&self) {
        let _ = self.changed.send(());
    }

    /// Wakes the account's sync task (it has ops to replay).
    pub fn waker(&self, account: &str) -> Arc<Notify> {
        self.wake.lock().unwrap().entry(account.to_string()).or_default().clone()
    }

    /// Notes a step of the account's sync; a pass started gets its number.
    pub fn pass(&self, account: &str, step: Pass) -> u64 {
        let n = {
            let mut p = self.passes.lock().unwrap();
            let e = p.entry(account.to_string()).or_default();
            match step {
                Pass::Start => {
                    e.started += 1;
                    e.started
                }
                Pass::Ahead(n) => {
                    e.ahead = e.ahead.max(n);
                    n
                }
                Pass::Finish(n) => {
                    e.finished = e.finished.max(n);
                    n
                }
                Pass::Fail => {
                    e.failed += 1;
                    0
                }
            }
        };
        self.passed.send_replace(());
        n
    }

    pub fn passes(&self, account: &str) -> Passes {
        self.passes.lock().unwrap().get(account).copied().unwrap_or_default()
    }

    /// Changes whenever a pass counter does.
    pub fn passed(&self) -> watch::Receiver<()> {
        self.passed.subscribe()
    }

    /// Asks the account's next pass to sync these provider folders first.
    pub fn hurry(&self, account: &str, remotes: impl IntoIterator<Item = String>) {
        self.hurry.lock().unwrap().entry(account.to_string()).or_default().extend(remotes);
    }

    /// The folders asked for first, forgotten here.
    pub fn hurried(&self, account: &str) -> HashSet<String> {
        self.hurry.lock().unwrap().remove(account).unwrap_or_default()
    }

    // Accounts

    pub fn accounts(&self) -> Vec<crate::accounts::Account> {
        let c = self.db();
        let mut st = c.prepare("SELECT data FROM accounts ORDER BY position, id").unwrap();
        st.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .filter_map(Result::ok)
            .filter_map(|d| serde_json::from_str(&d).ok())
            .collect()
    }

    /// Adds the account, or replaces the one with its id (keeping its place).
    pub fn put_account(&self, acc: &crate::accounts::Account) -> Result<()> {
        self.db().execute(
            "INSERT INTO accounts (id, position, data) VALUES (?1, (SELECT COALESCE(MAX(position), 0) + 1 FROM accounts), ?2)
             ON CONFLICT (id) DO UPDATE SET data = excluded.data",
            params![acc.id, serde_json::to_string(acc)?],
        )?;
        Ok(())
    }

    /// Stops replaying the account's changes, and forgets how its sync went:
    /// for a retired account (its waiting changes would never reach the
    /// provider), or one syncing again.
    pub fn forget_sync(&self, id: &str) -> Result<()> {
        let c = self.db();
        c.execute("DELETE FROM ops WHERE account = ?1", [id])?;
        c.execute("DELETE FROM status WHERE account = ?1", [id])?;
        Ok(())
    }

    /// Forgets the account and everything kept for it (its mail stays at the provider).
    pub fn delete_account(&self, id: &str) -> Result<()> {
        self.prune_mailboxes(id, &[])?;
        let c = self.db();
        for table in ["accounts WHERE id", "ops WHERE account", "tokens WHERE account", "status WHERE account", "invites WHERE account"] {
            c.execute(&format!("DELETE FROM {table} = ?1"), [id])?;
        }
        c.execute(
            "DELETE FROM blobs WHERE hash NOT IN (SELECT blob FROM messages)
             AND NOT EXISTS (SELECT 1 FROM ops WHERE instr(ops.op, blobs.hash) > 0)",
            [],
        )?;
        c.execute("DELETE FROM facts WHERE hash NOT IN (SELECT hash FROM blobs)", [])?;
        Ok(())
    }

    // Mailboxes

    pub fn mailboxes(&self) -> Vec<Mailbox> {
        boxes(&self.db())
    }

    pub fn mailbox(&self, id: i64) -> Option<Mailbox> {
        mailbox(&self.db(), id)
    }

    pub fn mailbox_named(&self, name: &str) -> Option<Mailbox> {
        let c = self.db();
        let name = if name.eq_ignore_ascii_case("INBOX") { ALL_INBOXES } else { name };
        c.query_row(&format!("{MBOX_SELECT} WHERE m.name = ?1"), [name], mbox_row).optional().unwrap()
    }

    pub fn all_inboxes(&self) -> i64 {
        self.mailbox_named(ALL_INBOXES).unwrap().id
    }

    /// The local mailbox mirroring `remote` of `account`, created if needed.
    pub fn remote_mailbox(&self, account: &str, remote: &str, name: &str, special: &str) -> Result<i64> {
        let c = self.db();
        let existing: Option<i64> = c
            .query_row("SELECT id FROM mailboxes WHERE account = ?1 AND remote = ?2", [account, remote], |r| r.get(0))
            .optional()?;
        if let Some(id) = existing {
            c.execute("UPDATE mailboxes SET special = ?2 WHERE id = ?1", params![id, special])?;
            return Ok(id);
        }
        let mut name = name.to_string();
        while c.query_row("SELECT COUNT(*) FROM mailboxes WHERE name = ?1", [&name], |r| r.get::<_, i64>(0))? > 0 {
            name.push('_');
        }
        c.execute(
            "INSERT INTO mailboxes (name, account, remote, special, uidvalidity) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![name, account, remote, special, now() as u32],
        )?;
        let id = c.last_insert_rowid();
        // Every account's inbox is in INBOX.
        if remote == "INBOX" {
            c.execute("INSERT OR IGNORE INTO tree_members (node, member) SELECT id, ?1 FROM mailboxes WHERE name = ?2", params![id, ALL_INBOXES])?;
        }
        Ok(id)
    }

    /// Mailboxes of `account` whose remote name is not in `keep` disappear.
    pub fn prune_mailboxes(&self, account: &str, keep: &[String]) -> Result<()> {
        let gone: Vec<i64> = self
            .mailboxes()
            .into_iter()
            .filter(|m| m.account.as_deref() == Some(account) && !keep.iter().any(|k| Some(k) == m.remote.as_ref()))
            .map(|m| m.id)
            .collect();
        for id in gone {
            self.reset_mailbox(id)?;
            let c = self.db();
            c.execute("DELETE FROM tree_members WHERE member = ?1", [id])?;
            c.execute("DELETE FROM mailboxes WHERE id = ?1", [id])?;
        }
        Ok(())
    }

    /// After a rename at the provider: the folder and its subfolders take
    /// their new remote and local names, keeping their messages.
    pub fn rename_mailboxes(&self, account: &str, old_remote: &str, new_remote: &str, delim: &str, old_name: &str, new_name: &str) -> Result<()> {
        {
            let c = self.db();
            let tx = c.unchecked_transaction()?;
            let rows: Vec<(i64, String, String)> = {
                let mut st = tx.prepare("SELECT id, remote, name FROM mailboxes WHERE account = ?1")?;
                st.query_map([account], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.filter_map(Result::ok).collect()
            };
            let child_remote = format!("{old_remote}{delim}");
            let child_name = format!("{old_name}/");
            for (id, remote, name) in rows {
                if remote != old_remote && !remote.starts_with(&child_remote) {
                    continue;
                }
                let remote = format!("{new_remote}{}", &remote[old_remote.len()..]);
                let name = if name == old_name || name.starts_with(&child_name) { format!("{new_name}{}", &name[old_name.len()..]) } else { name };
                tx.execute("UPDATE mailboxes SET remote = ?2, name = ?3 WHERE id = ?1", params![id, remote, name])?;
            }
            tx.commit()?;
        }
        self.notify();
        Ok(())
    }

    /// (remote UIDVALIDITY, highest remote UID fetched).
    pub fn remote_state(&self, mailbox: i64) -> (Option<u32>, u32) {
        let c = self.db();
        c.query_row("SELECT remote_validity, remote_last FROM mailboxes WHERE id = ?1", [mailbox], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap_or((None, 0))
    }

    pub fn set_remote_state(&self, mailbox: i64, validity: u32, last: u32) -> Result<()> {
        self.db().execute(
            "UPDATE mailboxes SET remote_validity = ?2, remote_last = MAX(remote_last, ?3) WHERE id = ?1",
            params![mailbox, validity, last],
        )?;
        Ok(())
    }

    /// Forgets a mailbox's messages (the provider's UIDVALIDITY changed); apps
    /// see a new UIDVALIDITY and resync.
    pub fn reset_mailbox(&self, mailbox: i64) -> Result<()> {
        {
            let c = self.db();
            c.execute("DELETE FROM messages WHERE link IN (SELECT id FROM messages WHERE mailbox = ?1)", [mailbox])?;
            c.execute("DELETE FROM messages WHERE mailbox = ?1", [mailbox])?;
            c.execute(
                "UPDATE mailboxes SET uidvalidity = uidvalidity + 1, remote_validity = NULL, remote_last = 0, filled_since = NULL WHERE id = ?1",
                [mailbox],
            )?;
        }
        self.notify();
        Ok(())
    }

    // Messages

    pub fn blob(&self, hash: &str) -> Option<Vec<u8>> {
        let data: Vec<u8> = {
            let c = self.db();
            c.query_row("SELECT data FROM blobs WHERE hash = ?1", [hash], |r| r.get(0)).optional().unwrap()?
        };
        Some(unpack(data))
    }

    /// Keeps a message packed beforehand (see `Packed::of`), if not kept already.
    fn put_blob(c: &Connection, p: &Packed) -> rusqlite::Result<String> {
        c.execute(
            "INSERT OR IGNORE INTO blobs (hash, data, atts, att_bytes, removed, text_bytes, len) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![p.hash, p.data, p.atts.count, p.atts.bytes as i64, p.atts.removed, p.text_bytes as i64, p.data.len() as i64],
        )?;
        put_facts(c, &p.hash, p.data.len(), p.atts, p.text_bytes)?;
        Ok(p.hash.clone())
    }

    /// Once: compresses the messages kept before valet compressed them, and
    /// notes what their attachments weigh, a few at a time (the store stays
    /// usable meanwhile); then drops what no message uses any more, and
    /// gives the room back to the disk.
    pub fn compact(&self) {
        if self.db().query_row("SELECT 1 FROM meta WHERE key = 'compacted'", [], |_| Ok(())).optional().ok().flatten().is_some() {
            return;
        }
        let started = std::time::Instant::now();
        let (mut after, mut packed, mut before, mut now_bytes) = (0i64, 0u64, 0u64, 0u64);
        loop {
            let rows: Vec<(i64, Vec<u8>)> = {
                let c = self.db();
                let mut st = c
                    .prepare("SELECT rowid, data FROM blobs WHERE rowid > ?1 AND (atts IS NULL OR substr(data, 1, 4) != ?2) ORDER BY rowid LIMIT 100")
                    .unwrap();
                st.query_map(params![after, ZSTD_MAGIC.as_slice()], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().filter_map(Result::ok).collect()
            };
            let Some(last) = rows.last().map(|r| r.0) else { break };
            after = last;
            let done: Vec<(i64, Option<Vec<u8>>, crate::mime::Attachments)> = rows
                .into_iter()
                .map(|(id, data)| {
                    let was = data.len() as u64;
                    let compressed = data.starts_with(&ZSTD_MAGIC);
                    let raw = unpack(data);
                    let a = crate::mime::attachments(&raw);
                    let new = (!compressed).then(|| pack(&raw));
                    before += was;
                    now_bytes += new.as_ref().map_or(was, |n| n.len() as u64);
                    (id, new, a)
                })
                .collect();
            let c = self.db();
            for (id, new, a) in done {
                packed += new.is_some() as u64;
                let _ = c.execute(
                    "UPDATE blobs SET data = COALESCE(?2, data), atts = ?3, att_bytes = ?4, removed = ?5, len = length(COALESCE(?2, data)) WHERE rowid = ?1",
                    params![id, new, a.count, a.bytes as i64, a.removed],
                );
            }
        }
        let gone = self.gc_blobs();
        tracing::info!(
            "compressed {packed} messages, {} MB to {} MB; {gone} no longer used dropped; {:.0?}",
            before / 1_000_000,
            now_bytes / 1_000_000,
            started.elapsed()
        );
        let c = self.db();
        // The file shrinks only by rewriting it, once; afterwards, as space is freed.
        let mode: i64 = c.query_row("PRAGMA auto_vacuum", [], |r| r.get(0)).unwrap_or(0);
        if mode != 2 {
            let t = std::time::Instant::now();
            match c.execute_batch("PRAGMA auto_vacuum = INCREMENTAL; VACUUM;") {
                Ok(()) => tracing::info!("mail.db rewritten in {:.0?}", t.elapsed()),
                Err(e) => tracing::warn!("mail.db not rewritten: {e}"),
            }
        }
        let _ = c.execute("INSERT OR REPLACE INTO meta (key, value) VALUES ('compacted', 1)", []);
    }

    /// Once for the messages kept before they were: what each weighs (see
    /// `facts`), a few at a time, worked out without holding the store.
    pub fn measure(&self) {
        let (mut after, mut n) = (0i64, 0);
        loop {
            let rows: Vec<(i64, String, Vec<u8>)> = {
                let c = self.db();
                let mut st = c
                    .prepare(
                        "SELECT b.rowid, b.hash, b.data FROM blobs b WHERE b.rowid > ?1
                         AND NOT EXISTS (SELECT 1 FROM facts f WHERE f.hash = b.hash) ORDER BY b.rowid LIMIT 50",
                    )
                    .unwrap();
                st.query_map([after], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().filter_map(Result::ok).collect()
            };
            let Some(last) = rows.last().map(|r| r.0) else { break };
            after = last;
            let measured: Vec<(String, usize, crate::mime::Attachments, u64)> = rows
                .into_iter()
                .map(|(_, hash, data)| {
                    let len = data.len();
                    let raw = unpack(data);
                    (hash, len, crate::mime::attachments(&raw), text_bytes(&raw, len))
                })
                .collect();
            {
                let c = self.db();
                for (hash, len, a, t) in &measured {
                    let _ = put_facts(&c, hash, *len, *a, *t);
                    n += 1;
                }
            }
            // Pages waiting for the store get it in between.
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        if n > 0 {
            tracing::info!("measured {n} messages");
        }
    }

    /// Drops what no message, waiting change or doomed copy uses; frees the
    /// room on disk. Returns how many went.
    pub fn gc_blobs(&self) -> usize {
        let c = self.db();
        let n = c
            .execute(
                "DELETE FROM blobs WHERE hash NOT IN (SELECT blob FROM messages) AND hash NOT IN (SELECT blob FROM doomed)
                 AND NOT EXISTS (SELECT 1 FROM ops WHERE instr(ops.op, blobs.hash) > 0)",
                [],
            )
            .unwrap_or(0);
        let _ = c.execute("DELETE FROM facts WHERE hash NOT IN (SELECT hash FROM blobs)", []);
        let _ = c.execute_batch("PRAGMA incremental_vacuum;");
        n
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_row(
        c: &Connection,
        mailbox: i64,
        blob: &str,
        flags: &str,
        date: i64,
        size: usize,
        mid: &str,
        remote_uid: Option<u32>,
        link: Option<i64>,
    ) -> rusqlite::Result<i64> {
        let uid: u32 = c.query_row(
            "UPDATE mailboxes SET uidnext = uidnext + 1 WHERE id = ?1 RETURNING uidnext - 1",
            [mailbox],
            |r| r.get(0),
        )?;
        let modseq = Self::next_modseq(c)?;
        c.execute(
            "INSERT INTO messages (mailbox, uid, blob, flags, date, size, message_id, remote_uid, link, modseq)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![mailbox, uid, blob, flags, date, size as i64, mid, remote_uid, link, modseq],
        )?;
        let id = c.last_insert_rowid();
        // A real folder's messages also appear in each node gathering it, and in All mail.
        if link.is_none() {
            for node in tree::nodes_of(c, mailbox)?.into_iter().chain(tree::all_mail(c)?.filter(|&n| n != mailbox)) {
                Self::insert_row(c, node, blob, flags, date, size, mid, None, Some(id))?;
            }
        }
        Ok(id)
    }

    /// Stores a message fetched from the provider; matches it to a
    /// placeholder of the same mailbox if there is one. Returns its id and
    /// whether it is new to valet.
    pub fn add_fetched(&self, mailbox: i64, raw: &[u8], flags: &[String], date: i64, remote_uid: u32) -> Result<(i64, bool)> {
        let mid = message_id(raw);
        let known = |c: &Connection| -> rusqlite::Result<Option<i64>> {
            c.query_row("SELECT id FROM messages WHERE mailbox = ?1 AND remote_uid = ?2", params![mailbox, remote_uid], |r| r.get(0)).optional()
        };
        if let Some(id) = known(&self.db())? {
            return Ok((id, false));
        }
        // Compressed before taking the store, which pages wait for meanwhile.
        let packed = Packed::of(raw);
        let id = {
            let c = self.db();
            if let Some(id) = known(&c)? {
                return Ok((id, false));
            }
            let blob = Self::put_blob(&c, &packed)?;
            let doomed = c.execute("DELETE FROM doomed WHERE rowid = (SELECT rowid FROM doomed WHERE mailbox = ?1 AND blob = ?2 LIMIT 1)", params![mailbox, blob])?;
            if doomed > 0 {
                drop(c);
                if let Some((account, remote)) = self.remote_of(mailbox) {
                    self.enqueue(&account, &Op::Expunge { remote, uid: remote_uid })?;
                }
                return Ok((0, false));
            }
            let placeholder: Option<i64> = c
                .query_row(
                    "SELECT id FROM messages WHERE mailbox = ?1 AND remote_uid IS NULL AND link IS NULL
                     AND (blob = ?2 OR (message_id != '' AND message_id = ?3)) ORDER BY id LIMIT 1",
                    params![mailbox, blob, mid],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(id) = placeholder {
                c.execute("UPDATE messages SET remote_uid = ?2 WHERE id = ?1", params![id, remote_uid])?;
                return Ok((id, false));
            }
            Self::insert_row(&c, mailbox, &blob, &join_flags(flags), date, raw.len(), &mid, Some(remote_uid), None)?
        };
        self.notify();
        Ok((id, true))
    }

    pub fn messages(&self, mailbox: i64) -> Vec<Msg> {
        let c = self.db();
        let mut st = c.prepare(&format!("SELECT {MSG_COLS} FROM messages WHERE mailbox = ?1 ORDER BY uid")).unwrap();
        st.query_map([mailbox], msg_row).unwrap().filter_map(Result::ok).collect()
    }

    pub fn message(&self, id: i64) -> Option<Msg> {
        let c = self.db();
        c.query_row(&format!("SELECT {MSG_COLS} FROM messages WHERE id = ?1"), [id], msg_row).optional().unwrap()
    }

    pub fn count(&self, mailbox: i64) -> (u32, u32) {
        let c = self.db();
        c.query_row(
            "SELECT COUNT(*), COALESCE(SUM(' ' || flags || ' ' NOT LIKE '% \\Seen %'), 0) FROM messages WHERE mailbox = ?1",
            [mailbox],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap_or((0, 0))
    }

    /// For a node's rows: (row → the account folder its message is in, its Message-ID).
    pub fn origins(&self, node: i64) -> HashMap<i64, (i64, String)> {
        let c = self.db();
        let mut st = c.prepare("SELECT n.id, s.mailbox, s.message_id FROM messages n JOIN messages s ON s.id = n.link WHERE n.mailbox = ?1").unwrap();
        st.query_map([node], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?)))).unwrap().filter_map(Result::ok).collect()
    }

    /// The account folder's row behind a node's row (itself otherwise).
    pub fn source(&self, m: Msg) -> Msg {
        match m.link.and_then(|l| self.message(l)) {
            Some(s) => s,
            None => m,
        }
    }

    /// Sets flags as the provider reports them (no op queued).
    pub fn sync_flags(&self, id: i64, flags: &[String]) -> Result<()> {
        let f = join_flags(flags);
        {
            let c = self.db();
            let modseq = Self::next_modseq(&c)?;
            c.execute("UPDATE messages SET flags = ?2, modseq = ?3 WHERE (id = ?1 OR link = ?1) AND flags != ?2", params![id, f, modseq])?;
        }
        self.notify();
        Ok(())
    }

    /// Removes a message (and its INBOX row) without queuing an op.
    pub fn remove(&self, id: i64) -> Result<()> {
        {
            let c = self.db();
            c.execute("DELETE FROM messages WHERE id = ?1 OR link = ?1", [id])?;
        }
        self.notify();
        Ok(())
    }

    // What is kept of an account's mail: since when, with attachments since when.

    /// Since when (Unix seconds, 0 for all of it) the folder's older mail has
    /// all been fetched; None until it has.
    pub fn filled_since(&self, mailbox: i64) -> Option<i64> {
        let c = self.db();
        c.query_row("SELECT filled_since FROM mailboxes WHERE id = ?1", [mailbox], |r| r.get(0)).optional().ok().flatten().flatten()
    }

    pub fn set_filled_since(&self, mailbox: i64, since: i64) -> Result<()> {
        self.db().execute("UPDATE mailboxes SET filled_since = ?2 WHERE id = ?1", params![mailbox, since])?;
        Ok(())
    }

    /// What the account's mail before `mail` (Unix seconds), and its
    /// attachments before `attachments`, weigh here: what keeping less would
    /// remove. A message in several of its folders counts once.
    pub fn trim_plan(&self, account: &str, mail: Option<i64>, attachments: Option<i64>) -> Trim {
        let c = self.db();
        let mail = mail.unwrap_or(i64::MIN);
        let rows = "SELECT m.blob FROM messages m JOIN mailboxes b ON b.id = m.mailbox WHERE b.account = ?1";
        let (messages, message_bytes): (i64, i64) = c
            .query_row(
                &format!(
                    "SELECT (SELECT COUNT(DISTINCT x.blob) FROM ({rows} AND m.date < ?2) x),
                     (SELECT COALESCE(SUM(len), 0) FROM facts WHERE hash IN ({rows} AND m.date < ?2)
                      AND hash NOT IN (SELECT m.blob FROM messages m JOIN mailboxes b ON b.id = m.mailbox
                                       WHERE m.link IS NULL AND NOT (b.account IS NOT NULL AND b.account = ?1 AND m.date < ?2)))"
                ),
                params![account, mail],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap_or((0, 0));
        let (with_attachments, count, bytes): (i64, i64, i64) = match attachments {
            Some(before) if before > mail => c
                .query_row(
                    &format!(
                        "SELECT COUNT(*), COALESCE(SUM(atts), 0), COALESCE(SUM(att_bytes), 0) FROM facts
                         WHERE atts > 0 AND hash IN ({rows} AND m.date >= ?2 AND m.date < ?3)"
                    ),
                    params![account, mail, before],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .unwrap_or((0, 0, 0)),
            _ => (0, 0, 0),
        };
        Trim {
            messages: messages as u64,
            message_bytes: message_bytes as u64,
            with_attachments: with_attachments as u64,
            attachments: count as u64,
            attachment_bytes: bytes as u64,
        }
    }

    /// Forgets the account's mail from before `before` (it stays at the
    /// provider); returns how many rows went.
    pub fn drop_mail_before(&self, account: &str, before: i64) -> Result<usize> {
        let n = {
            let c = self.db();
            let rows = "SELECT m.id FROM messages m JOIN mailboxes b ON b.id = m.mailbox WHERE b.account = ?1 AND m.date < ?2";
            c.execute(&format!("DELETE FROM messages WHERE link IN ({rows})"), params![account, before])?;
            let n = c.execute(&format!("DELETE FROM messages WHERE id IN ({rows})"), params![account, before])?;
            // The folders have their mail since then only: an earlier date fetches the rest again.
            c.execute("UPDATE mailboxes SET filled_since = MAX(filled_since, ?2) WHERE account = ?1 AND filled_since IS NOT NULL", params![account, before])?;
            n
        };
        if n > 0 {
            self.notify();
            self.gc_blobs();
        }
        Ok(n)
    }

    /// Keeps the account's mail from before `before` without its
    /// attachments; returns how many messages lost theirs.
    pub fn strip_before(&self, account: &str, before: i64) -> Result<usize> {
        let mut n = 0;
        loop {
            let rows: Vec<(i64, String)> = {
                let c = self.db();
                let mut st = c.prepare(
                    "SELECT m.id, m.blob FROM messages m JOIN mailboxes b ON b.id = m.mailbox LEFT JOIN facts x ON x.hash = m.blob
                     WHERE b.account = ?1 AND m.link IS NULL AND m.date < ?2 AND (x.atts > 0 OR x.hash IS NULL) LIMIT 50",
                )?;
                st.query_map(params![account, before], |r| Ok((r.get(0)?, r.get(1)?)))?.filter_map(Result::ok).collect()
            };
            if rows.is_empty() {
                break;
            }
            for (id, blob) in rows {
                let raw = self.blob(&blob).unwrap_or_default();
                match crate::mime::strip(&raw) {
                    Some(slim) => {
                        self.set_content(id, &slim)?;
                        n += 1;
                    }
                    // Nothing to remove (or not measured yet, and none): noted, not asked again.
                    None => {
                        let len = pack(&raw).len();
                        put_facts(&self.db(), &blob, len, crate::mime::attachments(&raw), len as u64)?;
                    }
                }
            }
        }
        if n > 0 {
            self.notify();
            self.gc_blobs();
        }
        Ok(n)
    }

    /// The folder's messages kept without their attachments, since `since`:
    /// (row, remote UID), to fetch whole again.
    pub fn stripped_rows(&self, mailbox: i64, since: i64) -> Vec<(i64, u32)> {
        let c = self.db();
        let mut st = c
            .prepare(
                "SELECT m.id, m.remote_uid FROM messages m JOIN facts x ON x.hash = m.blob
                 WHERE m.mailbox = ?1 AND m.remote_uid IS NOT NULL AND m.date >= ?2 AND x.removed > 0",
            )
            .unwrap();
        st.query_map(params![mailbox, since], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().filter_map(Result::ok).collect()
    }

    /// The message (and its rows in the tree) is now `raw`: the same mail,
    /// with or without its attachments.
    pub fn set_content(&self, id: i64, raw: &[u8]) -> Result<()> {
        let packed = Packed::of(raw);
        let c = self.db();
        let blob = Self::put_blob(&c, &packed)?;
        c.execute("UPDATE messages SET blob = ?2, size = ?3 WHERE id = ?1 OR link = ?1", params![id, blob, raw.len() as i64])?;
        Ok(())
    }

    /// What each account's mail takes here, compressed, by kind (a message
    /// in several of its folders counts once; in Spam or Trash only if all
    /// its copies are). One pass over the messages, for every account.
    pub fn usage(&self) -> std::collections::HashMap<String, Usage> {
        let c = self.db();
        let mut st = c
            .prepare(
                r"WITH r AS (SELECT b.account, m.blob, MIN(CASE WHEN lower(b.special) IN ('\junk', '\trash') THEN 1 ELSE 0 END) AS bin
                             FROM messages m JOIN mailboxes b ON b.id = m.mailbox WHERE b.account IS NOT NULL GROUP BY b.account, m.blob),
                  x AS (SELECT r.account, r.bin, f.len, f.atts, f.removed, min(f.text_bytes, f.len) AS text
                        FROM r JOIN facts f ON f.hash = r.blob)
                  SELECT account, COUNT(*),
                         COALESCE(SUM(CASE WHEN bin = 0 THEN text END), 0),
                         COALESCE(SUM(CASE WHEN bin = 0 THEN len - text END), 0),
                         COALESCE(SUM(CASE WHEN bin = 1 THEN len END), 0),
                         COALESCE(SUM(CASE WHEN bin = 0 THEN atts END), 0),
                         COALESCE(SUM(removed), 0)
                  FROM x GROUP BY account",
            )
            .unwrap();
        st.query_map([], |r| {
            let n = |i: usize| r.get::<_, i64>(i).map(|v| v.max(0) as u64);
            Ok((r.get::<_, String>(0)?, Usage { messages: n(1)?, text: n(2)?, attachments: n(3)?, bins: n(4)?, attachment_count: n(5)?, removed: n(6)? }))
        })
        .unwrap()
        .filter_map(Result::ok)
        .collect()
    }

    /// The size of mail.db.
    pub fn db_bytes(&self) -> u64 {
        let c = self.db();
        c.query_row("SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()", [], |r| r.get::<_, i64>(0)).unwrap_or(0) as u64
    }

    /// Rows mirroring the provider, by remote UID.
    pub fn remote_rows(&self, mailbox: i64) -> Vec<(i64, u32, Vec<String>)> {
        let c = self.db();
        let mut st = c.prepare("SELECT id, remote_uid, flags FROM messages WHERE mailbox = ?1 AND remote_uid IS NOT NULL").unwrap();
        st.query_map([mailbox], |r| Ok((r.get(0)?, r.get(1)?, split_flags(&r.get::<_, String>(2)?))))
            .unwrap()
            .filter_map(Result::ok)
            .collect()
    }

    pub fn changed_since(&self, mailbox: i64, modseq: i64) -> Vec<Msg> {
        let c = self.db();
        let mut st = c.prepare(&format!("SELECT {MSG_COLS} FROM messages WHERE mailbox = ?1 AND modseq > ?2")).unwrap();
        st.query_map(params![mailbox, modseq], msg_row).unwrap().filter_map(Result::ok).collect()
    }

    // Changes from mail apps

    /// For the provider; a retired account's changes stay here.
    fn enqueue(&self, account: &str, op: &Op) -> Result<()> {
        let c = self.db();
        let retired = c
            .query_row("SELECT data FROM accounts WHERE id = ?1", [account], |r| r.get::<_, String>(0))
            .optional()?
            .and_then(|d| serde_json::from_str::<crate::accounts::Account>(&d).ok())
            .is_some_and(|a| a.retired.is_some());
        if retired {
            return Ok(());
        }
        c.execute("INSERT INTO ops (account, op) VALUES (?1, ?2)", params![account, serde_json::to_string(op)?])?;
        drop(c);
        self.waker(account).notify_one();
        Ok(())
    }

    /// The account and provider mailbox name a local mailbox mirrors.
    fn remote_of(&self, mailbox: i64) -> Option<(String, String)> {
        let m = self.mailbox(mailbox)?;
        Some((m.account?, m.remote?))
    }

    /// Where a message of `account` filed into `target` goes: `target` if it
    /// is a real folder, else the node's folder in that account. Returns
    /// (real folder, account, provider name).
    fn landing(&self, target: i64, account: Option<&str>) -> Result<(i64, String, String)> {
        let b = self.mailbox(target).context("no such folder")?;
        ensure!(!b.is_all_mail(), "{ALL_MAIL_ISNT_A_PLACE}");
        let folder = if b.is_node() {
            let account = account.context("that message isn't in an account")?;
            self.member_of(target, account).with_context(|| format!("{} has no folder in that account yet", b.name))?
        } else {
            target
        };
        let (account, remote) = self.remote_of(folder).context("no such folder")?;
        Ok((folder, account, remote))
    }

    /// `row`'s row in `mailbox`, a node gathering its folder (itself if it is in `mailbox`).
    pub fn row_in(&self, mailbox: i64, row: i64) -> Option<i64> {
        let c = self.db();
        c.query_row("SELECT id FROM messages WHERE mailbox = ?1 AND (id = ?2 OR link = ?2)", params![mailbox, row], |r| r.get(0)).optional().unwrap()
    }

    // Arrivals: messages just come in, to tell of (see `crate::notify`).

    /// Notes messages (real rows) as just come in; forgets those of more
    /// than three days ago, but for those still new (see `badge`).
    pub fn add_arrivals(&self, ids: &[i64]) -> Result<()> {
        let c = self.db();
        let t = now();
        c.execute(&format!("DELETE FROM arrivals WHERE at < ?1 AND message NOT IN (SELECT a.message FROM {STILL_NEW})"), [t - 3 * 86400])?;
        for id in ids {
            c.execute("INSERT INTO arrivals (message, at) VALUES (?1, ?2)", params![id, t])?;
        }
        Ok(())
    }

    /// The last arrival's number (0 before the first); numbers only grow.
    pub fn last_arrival(&self) -> i64 {
        let c = self.db();
        c.query_row("SELECT seq FROM sqlite_sequence WHERE name = 'arrivals'", [], |r| r.get(0)).optional().ok().flatten().unwrap_or(0)
    }

    /// The app's badge: messages come in as new mail and still new (unread,
    /// in their inbox), so that old unread mail doesn't count.
    pub fn badge(&self) -> u32 {
        let c = self.db();
        c.query_row(&format!("SELECT COUNT(DISTINCT a.message) FROM {STILL_NEW}"), [], |r| r.get(0)).unwrap_or(0)
    }

    /// The messages come in after arrival `seq`, in order.
    pub fn arrivals_after(&self, seq: i64) -> Vec<i64> {
        let c = self.db();
        let mut st = c.prepare("SELECT message FROM arrivals WHERE seq > ?1 ORDER BY seq").unwrap();
        st.query_map([seq], |r| r.get(0)).unwrap().filter_map(Result::ok).collect()
    }

    /// The UID `row` (a real row, or one linking to it) has in `mailbox`.
    fn uid_in(&self, mailbox: i64, row: i64) -> u32 {
        let c = self.db();
        c.query_row("SELECT uid FROM messages WHERE mailbox = ?1 AND (id = ?2 OR link = ?2)", params![mailbox, row], |r| r.get(0)).unwrap_or(0)
    }

    /// The account a message is from (its From address), to file it into a node.
    pub fn sender_account(&self, raw: &[u8]) -> Option<String> {
        let p = crate::mime::parse(raw);
        let from = p.header(raw, "From").and_then(|f| crate::mime::addresses(&f).into_iter().next()).map(|(_, m, h)| format!("{m}@{h}"))?;
        let accounts = self.accounts();
        // A live account first, if a retired one had the same address.
        accounts.iter().find(|a| a.sends_now(&from)).or_else(|| accounts.iter().find(|a| a.sends_as(&from))).map(|a| a.id.clone())
    }

    /// The account folder for a special use (`\Trash`…): the one the provider
    /// flags, else the one the tree's node of that use gathers.
    pub fn special_folder(&self, account: &str, special: &str) -> Option<i64> {
        let boxes = self.mailboxes();
        let flagged = boxes.iter().find(|b| b.account.as_deref() == Some(account) && b.special.eq_ignore_ascii_case(special));
        if let Some(b) = flagged {
            return Some(b.id);
        }
        let node = boxes.iter().find(|b| b.is_node() && b.special.eq_ignore_ascii_case(special))?;
        self.member_of(node.id, account)
    }

    /// STORE: returns each message's new flags.
    pub fn store(&self, ids: &[i64], mode: &StoreMode, flags: &[String]) -> Result<Vec<(i64, Vec<String>)>> {
        let mut out = Vec::new();
        for &id in ids {
            let Some(m) = self.message(id) else { continue };
            let src = self.source(m.clone());
            let old = src.flags.clone();
            let has = |list: &[String], f: &String| list.iter().any(|x| x.eq_ignore_ascii_case(f));
            let new: Vec<String> = match mode {
                StoreMode::Set => flags.to_vec(),
                StoreMode::Add => {
                    let mut n = old.clone();
                    n.extend(flags.iter().filter(|f| !has(&old, f)).cloned());
                    n
                }
                StoreMode::Remove => old.iter().filter(|f| !has(flags, f)).cloned().collect(),
            };
            let add: Vec<String> = new.iter().filter(|f| !has(&old, f)).cloned().collect();
            let remove: Vec<String> = old.iter().filter(|f| !has(&new, f)).cloned().collect();
            if add.is_empty() && remove.is_empty() {
                out.push((id, old));
                continue;
            }
            {
                let c = self.db();
                let modseq = Self::next_modseq(&c)?;
                c.execute("UPDATE messages SET flags = ?2, modseq = ?3 WHERE id = ?1 OR link = ?1", params![src.id, join_flags(&new), modseq])?;
            }
            if let (Some(uid), Some((account, remote))) = (src.remote_uid, self.remote_of(src.mailbox)) {
                self.enqueue(&account, &Op::Flags { remote, uid, add, remove })?;
            }
            out.push((id, new));
        }
        self.notify();
        Ok(out)
    }

    /// EXPUNGE of these rows: removed here and on the provider.
    pub fn expunge(&self, ids: &[i64]) -> Result<()> {
        for &id in ids {
            let Some(m) = self.message(id) else { continue };
            let src = self.source(m);
            match (src.remote_uid, self.remote_of(src.mailbox)) {
                (Some(uid), Some((account, remote))) => self.enqueue(&account, &Op::Expunge { remote, uid })?,
                // Not at the provider yet: it won't be (a draft saved again before it got there).
                // Not at the provider yet: it won't be, if the append is still
                // waiting; if it is on its way, it goes as soon as it comes back.
                (None, Some((account, _))) => {
                    let c = self.db();
                    let cancelled = c.execute(
                        "DELETE FROM ops WHERE account = ?1 AND instr(op, '\"op\":\"append\"') > 0 AND instr(op, ?2) > 0",
                        params![account, src.blob],
                    )?;
                    if cancelled == 0 {
                        c.execute("INSERT INTO doomed (mailbox, blob) VALUES (?1, ?2)", params![src.mailbox, src.blob])?;
                    }
                }
                _ => {}
            }
            let c = self.db();
            c.execute("DELETE FROM messages WHERE id = ?1 OR link = ?1 OR id = ?2", params![src.id, id])?;
        }
        self.notify();
        Ok(())
    }

    /// COPY or MOVE to `target`; the new rows are placeholders until the sync
    /// fetches them. Returns (source uid, new uid) pairs for COPYUID.
    /// Into a node, each message goes to the node's folder in its own account.
    pub fn copy(&self, ids: &[i64], target: i64, moving: bool) -> Result<Vec<(u32, u32)>> {
        let mut pairs = Vec::new();
        for &id in ids {
            let Some(m) = self.message(id) else { continue };
            let src = self.source(m.clone());
            let remote = self.remote_of(src.mailbox);
            let (folder, t_account, t_remote) = self.landing(target, remote.as_ref().map(|r| r.0.as_str()))?;
            if moving && folder == src.mailbox {
                pairs.push((m.uid, self.uid_in(target, src.id)));
                continue;
            }
            let new_id = {
                let c = self.db();
                let mid: String = c.query_row("SELECT message_id FROM messages WHERE id = ?1", [src.id], |r| r.get(0))?;
                Self::insert_row(&c, folder, &src.blob, &join_flags(&src.flags), src.date, src.size as usize, &mid, None, None)?
            };
            pairs.push((m.uid, self.uid_in(target, new_id)));
            match (src.remote_uid, remote) {
                (Some(uid), Some((account, remote))) if account == t_account => {
                    let op = if moving {
                        Op::Move { remote, uid, to: t_remote.clone() }
                    } else {
                        Op::Copy { remote, uid, to: t_remote.clone() }
                    };
                    self.enqueue(&account, &op)?;
                }
                (uid, remote) => {
                    self.enqueue(&t_account, &Op::Append { to: t_remote.clone(), blob: src.blob.clone(), flags: src.flags.clone() })?;
                    if moving && let (Some(uid), Some((account, remote))) = (uid, remote) {
                        self.enqueue(&account, &Op::Expunge { remote, uid })?;
                    }
                }
            }
            if moving {
                self.db().execute("DELETE FROM messages WHERE id = ?1 OR link = ?1 OR id = ?2", params![src.id, id])?;
            }
        }
        self.notify();
        Ok(pairs)
    }

    /// APPEND: returns the new UID (in `target`).
    pub fn append(&self, target: i64, raw: &[u8], flags: &[String], date: i64) -> Result<u32> {
        let id = self.append_row(target, raw, flags, date)?;
        Ok(self.uid_in(target, id))
    }

    /// Like [`append`](Self::append), returning the new (real) row's id. Into
    /// a node, the message goes to the node's folder in the account it is from.
    pub fn append_row(&self, target: i64, raw: &[u8], flags: &[String], date: i64) -> Result<i64> {
        let from = match self.mailbox(target) {
            Some(b) if b.is_node() => Some(self.sender_account(raw).context("no account sends as this message's From address")?),
            _ => None,
        };
        let (folder, account, remote) = self.landing(target, from.as_deref())?;
        let packed = Packed::of(raw);
        let id = {
            let c = self.db();
            let blob = Self::put_blob(&c, &packed)?;
            let mid = message_id(raw);
            let id = Self::insert_row(&c, folder, &blob, &join_flags(flags), date, raw.len(), &mid, None, None)?;
            drop(c);
            self.enqueue(&account, &Op::Append { to: remote, blob, flags: flags.to_vec() })?;
            id
        };
        self.notify();
        Ok(id)
    }

    // Ops

    pub fn ops(&self, account: &str) -> Vec<(i64, Op)> {
        let c = self.db();
        let mut st = c.prepare("SELECT id, op FROM ops WHERE account = ?1 ORDER BY id").unwrap();
        st.query_map([account], |r| Ok((r.get(0)?, r.get::<_, String>(1)?)))
            .unwrap()
            .filter_map(Result::ok)
            .filter_map(|(id, s)| serde_json::from_str(&s).ok().map(|op| (id, op)))
            .collect()
    }

    pub fn op_done(&self, id: i64) {
        let _ = self.db().execute("DELETE FROM ops WHERE id = ?1", [id]);
    }

    /// Records a failure; after 5 the op is dropped. True if dropped.
    pub fn op_failed(&self, id: i64, error: &str) -> bool {
        let c = self.db();
        let _ = c.execute("UPDATE ops SET tries = tries + 1, error = ?2 WHERE id = ?1", params![id, error]);
        let tries: i64 = c.query_row("SELECT tries FROM ops WHERE id = ?1", [id], |r| r.get(0)).unwrap_or(99);
        if tries >= 5 {
            let _ = c.execute("DELETE FROM ops WHERE id = ?1", [id]);
            return true;
        }
        false
    }

    pub fn pending_ops(&self, account: &str) -> (usize, Option<String>) {
        let c = self.db();
        c.query_row(
            "SELECT COUNT(*), (SELECT error FROM ops WHERE account = ?1 AND error IS NOT NULL ORDER BY id DESC LIMIT 1) FROM ops WHERE account = ?1",
            [account],
            |r| Ok((r.get::<_, i64>(0)? as usize, r.get(1)?)),
        )
        .unwrap_or((0, None))
    }

    // Account status and OAuth tokens

    pub fn set_status(&self, account: &str, ok: bool, message: &str) {
        let _ = self.db().execute(
            "INSERT INTO status (account, ok, message, at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (account) DO UPDATE SET ok = ?2, message = ?3, at = ?4",
            params![account, ok, message, now()],
        );
    }

    pub fn status(&self, account: &str) -> Option<(bool, String, i64)> {
        let c = self.db();
        c.query_row("SELECT ok, message, at FROM status WHERE account = ?1", [account], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()
            .unwrap()
    }

    pub fn token(&self, account: &str) -> Option<(String, Option<String>, Option<i64>)> {
        let c = self.db();
        c.query_row("SELECT refresh, access, expires FROM tokens WHERE account = ?1", [account], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()
            .unwrap()
    }

    pub fn set_token(&self, account: &str, refresh: &str, access: &str, expires: i64) -> Result<()> {
        self.db().execute(
            "INSERT INTO tokens (account, refresh, access, expires) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (account) DO UPDATE SET refresh = ?2, access = ?3, expires = ?4",
            params![account, refresh, access, expires],
        )?;
        Ok(())
    }

    // Invitations

    /// Records an invitation that came in mail `message` (its Message-ID).
    #[allow(clippy::too_many_arguments)]
    pub fn add_invite(
        &self,
        account: &str,
        uid: &str,
        method: &str,
        sequence: i64,
        summary: &str,
        organizer: &str,
        dtstart: &str,
        ics: &str,
        message: &str,
    ) -> Result<bool> {
        let n = self.db().execute(
            "INSERT OR IGNORE INTO invites (account, uid, method, sequence, summary, organizer, dtstart, ics, received, message)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![account, uid, method, sequence, summary, organizer, dtstart, ics, now(), message],
        )?;
        Ok(n == 1)
    }

    /// Whether a later version of the event (or, for an invitation, its
    /// cancellation) is already recorded.
    pub fn invite_superseded(&self, account: &str, uid: &str, method: &str, sequence: i64) -> bool {
        let c = self.db();
        c.query_row(
            "SELECT 1 FROM invites WHERE account = ?1 AND uid = ?2 AND (sequence > ?3 OR (?4 = 'REQUEST' AND method = 'CANCEL' AND sequence = ?3))",
            params![account, uid, sequence, method],
            |_| Ok(()),
        )
        .optional()
        .ok()
        .flatten()
        .is_some()
    }

    pub fn invites(&self) -> Vec<Invite> {
        let c = self.db();
        let mut st = c
            .prepare("SELECT id, account, uid, method, summary, organizer, dtstart, ics, response, message FROM invites WHERE method = 'REQUEST' ORDER BY dtstart DESC")
            .unwrap();
        st.query_map([], |r| {
            Ok(Invite {
                id: r.get(0)?,
                account: r.get(1)?,
                uid: r.get(2)?,
                method: r.get(3)?,
                summary: r.get(4)?,
                organizer: r.get(5)?,
                dtstart: r.get(6)?,
                ics: r.get(7)?,
                response: r.get(8)?,
                message: r.get(9)?,
            })
        })
        .unwrap()
        .filter_map(Result::ok)
        .collect()
    }

    pub fn invite(&self, id: i64) -> Option<Invite> {
        self.invites().into_iter().find(|i| i.id == id)
    }

    pub fn set_invite_response(&self, id: i64, response: &str) -> Result<()> {
        self.db().execute("UPDATE invites SET response = ?2 WHERE id = ?1", params![id, response])?;
        Ok(())
    }

    /// Whether some invitation doesn't know the mail it came in.
    pub fn invites_without_mail(&self) -> bool {
        self.db().query_row("SELECT EXISTS (SELECT 1 FROM invites WHERE message = '')", [], |r| r.get(0)).unwrap_or(false)
    }

    /// Gives the invitations of `account` with this key, not knowing their mail yet, `message`.
    pub fn set_invite_mail(&self, account: &str, uid: &str, method: &str, sequence: i64, message: &str) -> Result<usize> {
        Ok(self.db().execute(
            "UPDATE invites SET message = ?5 WHERE account = ?1 AND uid = ?2 AND method = ?3 AND sequence = ?4 AND message = ''",
            params![account, uid, method, sequence, message],
        )?)
    }

    /// True the first time it is asked for `key`, never again.
    pub fn first_time(&self, key: &str) -> bool {
        self.db().execute("INSERT OR IGNORE INTO meta (key, value) VALUES (?1, 1)", [key]).is_ok_and(|n| n == 1)
    }

    /// The real rows of the accounts' folders: (account, Message-ID, blob), newest first.
    pub fn account_rows(&self) -> Vec<(String, String, String)> {
        let c = self.db();
        let mut st = c
            .prepare(
                "SELECT b.account, m.message_id, m.blob FROM messages m JOIN mailboxes b ON b.id = m.mailbox
                 WHERE m.link IS NULL AND b.account IS NOT NULL AND m.message_id != '' ORDER BY m.date DESC",
            )
            .unwrap();
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().filter_map(Result::ok).collect()
    }

    /// The message of `account` with this Message-ID, out of the trash and
    /// spam if it is elsewhere too.
    pub fn message_with_id(&self, account: &str, message_id: &str) -> Option<i64> {
        if message_id.is_empty() {
            return None;
        }
        let c = self.db();
        c.query_row(
            "SELECT m.id FROM messages m JOIN mailboxes b ON b.id = m.mailbox
             WHERE b.account = ?1 AND m.message_id = ?2 AND m.link IS NULL
             ORDER BY b.special IN ('\\Trash', '\\Junk'), m.id DESC LIMIT 1",
            params![account, message_id],
            |r| r.get(0),
        )
        .optional()
        .unwrap()
    }
}

#[derive(Clone, Debug)]
pub struct Invite {
    pub id: i64,
    pub account: String,
    pub uid: String,
    pub method: String,
    pub summary: String,
    pub organizer: String,
    pub dtstart: String,
    pub ics: String,
    pub response: Option<String>,
    /// The Message-ID of the mail it came in ('' when not known).
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accounts_are_kept_in_order_and_forgotten_whole() {
        let m = MailStore::memory();
        let json = r#"[{"id":"a","name":"A","address":"a@x.fr","imap":"i:993","smtp":"s:465","password":"p"},
            {"id":"b","name":"B","address":"b@x.fr","imap":"i:993","smtp":"s:465","password":"p"}]"#;
        let accs: Vec<crate::accounts::Account> = serde_json::from_str(json).unwrap();
        m.put_account(&accs[1]).unwrap();
        m.put_account(&accs[0]).unwrap();
        let mut renamed = accs[1].clone();
        renamed.name = "B2".into();
        m.put_account(&renamed).unwrap();
        let names: Vec<String> = m.accounts().into_iter().map(|a| a.name).collect();
        assert_eq!(names, ["B2", "A"]);
        let inbox = m.remote_mailbox("b", "INBOX", "B2/INBOX", "").unwrap();
        m.add_fetched(inbox, &raw("1@x"), &[], 0, 1).unwrap();
        m.set_status("b", true, "ok");
        m.delete_account("b").unwrap();
        assert_eq!(m.accounts().len(), 1);
        assert!(m.mailbox(inbox).is_none() && m.status("b").is_none());
        assert_eq!(m.count(m.all_inboxes()).0, 0);
        assert_eq!(m.db().query_row("SELECT COUNT(*) FROM blobs", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
    }

    #[test]
    fn a_retired_accounts_changes_stay_here() {
        let s = MailStore::memory();
        let json = r#"{"id":"old","name":"Old","address":"me@old.fr","imap":"i:993","smtp":"s:465","password":"p","retired":1}"#;
        s.put_account(&serde_json::from_str(json).unwrap()).unwrap();
        let inbox = s.remote_mailbox("old", "INBOX", "Old/Inbox", "").unwrap();
        let keep = s.remote_mailbox("old", "Keep", "Old/Keep", "").unwrap();
        let (id, _) = s.add_fetched(inbox, &raw("a"), &[], 0, 7).unwrap();
        s.store(&[id], &StoreMode::Add, &["\\Flagged".into()]).unwrap();
        assert_eq!(s.message(id).unwrap().flags, ["\\Flagged"]);
        s.copy(&[id], keep, true).unwrap();
        assert!(s.messages(inbox).is_empty() && s.messages(keep).len() == 1, "moved here");
        s.expunge(&[s.messages(keep)[0].id]).unwrap();
        assert!(s.messages(keep).is_empty() && s.ops("old").is_empty(), "and nothing waits for the provider");
        assert_eq!(s.sender_account(b"From: me@old.fr\r\n\r\n").as_deref(), Some("old"), "its sent mail is still filed in it");
    }

    fn raw(id: &str) -> Vec<u8> {
        format!("Message-ID: <{id}>\r\nSubject: s\r\n\r\nbody\r\n").into_bytes()
    }

    #[test]
    fn an_invitation_finds_the_mail_it_came_in() {
        let s = MailStore::memory();
        let inbox = s.remote_mailbox("work", "INBOX", "Work/Inbox", "").unwrap();
        let trash = s.remote_mailbox("work", "Trash", "Work/Trash", "\\Trash").unwrap();
        let (kept, _) = s.add_fetched(inbox, &raw("inv"), &[], 0, 1).unwrap();
        s.add_fetched(trash, &raw("inv"), &[], 0, 1).unwrap();
        let mid = message_id(&raw("inv"));
        assert_eq!(s.message_with_id("work", &mid), Some(kept), "out of the trash");
        assert_eq!(s.message_with_id("home", &mid), None, "another account's");
        assert_eq!(s.message_with_id("work", ""), None);

        s.add_invite("work", "u", "REQUEST", 0, "Sync", "o@x.fr", "20991023T100000Z", "", "").unwrap();
        assert!(s.invites_without_mail());
        assert_eq!(s.set_invite_mail("work", "u", "REQUEST", 1, &mid).unwrap(), 0, "another version");
        assert_eq!(s.set_invite_mail("work", "u", "REQUEST", 0, &mid).unwrap(), 1);
        assert_eq!(s.set_invite_mail("work", "u", "REQUEST", 0, "<other>").unwrap(), 0, "kept once known");
        assert_eq!(s.invites()[0].message, mid);
        assert!(!s.invites_without_mail());
        assert!(s.first_time("k") && !s.first_time("k"));
    }

    #[test]
    fn the_badge_counts_new_mail_still_unread_in_its_inbox() {
        let s = MailStore::memory();
        let inbox = s.remote_mailbox("work", "INBOX", "Work/Inbox", "").unwrap();
        let keep = s.remote_mailbox("work", "Keep", "Work/Keep", "").unwrap();
        s.add_fetched(inbox, &raw("old"), &[], 0, 1).unwrap();
        let ids: Vec<i64> = (2..6).map(|uid| s.add_fetched(inbox, &raw(&uid.to_string()), &[], 0, uid).unwrap().0).collect();
        assert_eq!(s.badge(), 0, "old unread mail doesn't count");
        s.add_arrivals(&ids).unwrap();
        s.add_arrivals(&ids[..1]).unwrap();
        assert_eq!(s.badge(), 4, "each message once");
        s.store(&[ids[0]], &StoreMode::Add, &["\\Seen".into()]).unwrap();
        s.copy(&[ids[1]], keep, true).unwrap();
        s.expunge(&[ids[2]]).unwrap();
        assert_eq!(s.badge(), 1, "read, filed or deleted: no longer new");
        // Days later, those still new are kept for the badge.
        s.db().execute("UPDATE arrivals SET at = 0", []).unwrap();
        s.add_arrivals(&[]).unwrap();
        assert_eq!(s.badge(), 1);
        assert_eq!(s.db().query_row("SELECT COUNT(*) FROM arrivals", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
    }

    #[test]
    fn inbox_rows_link_and_follow() {
        let s = MailStore::memory();
        let inbox = s.remote_mailbox("work", "INBOX", "Work/Inbox", "").unwrap();
        let (id, new) = s.add_fetched(inbox, &raw("a"), &[], 0, 7).unwrap();
        assert!(new);
        assert!(!s.add_fetched(inbox, &raw("a"), &[], 0, 7).unwrap().1, "same remote uid twice");
        let all = s.all_inboxes();
        let linked = &s.messages(all)[0];
        assert_eq!(linked.link, Some(id));

        // Flagging from INBOX changes the account row and queues one op.
        s.store(&[linked.id], &StoreMode::Add, &["\\Seen".into()]).unwrap();
        assert_eq!(s.message(id).unwrap().flags, ["\\Seen"]);
        assert_eq!(s.ops("work"), [(1, Op::Flags { remote: "INBOX".into(), uid: 7, add: vec!["\\Seen".into()], remove: vec![] })]);
        assert_eq!(s.count(all), (1, 0));

        s.expunge(&[linked.id]).unwrap();
        assert!(s.messages(inbox).is_empty() && s.messages(all).is_empty());
        assert!(matches!(s.ops("work")[1].1, Op::Expunge { uid: 7, .. }));
    }

    #[test]
    fn moves_leave_placeholders_that_the_sync_adopts() {
        let s = MailStore::memory();
        let inbox = s.remote_mailbox("work", "INBOX", "Work/Inbox", "").unwrap();
        let archive = s.remote_mailbox("work", "Archive", "Work/Archive", "\\Archive").unwrap();
        let gmail = s.remote_mailbox("g", "Keep", "Gmail/Keep", "").unwrap();
        let (id, _) = s.add_fetched(inbox, &raw("a"), &[], 0, 7).unwrap();
        s.copy(&[id], archive, true).unwrap();
        assert!(s.messages(inbox).is_empty());
        assert_eq!(s.messages(archive).len(), 1);
        assert!(matches!(&s.ops("work")[0].1, Op::Move { uid: 7, to, .. } if to == "Archive"));
        assert!(!s.add_fetched(archive, &raw("a"), &[], 0, 30).unwrap().1, "adopted");
        assert_eq!(s.messages(archive)[0].remote_uid, Some(30));

        // Across accounts: appended there, expunged here.
        let aid = s.messages(archive)[0].id;
        s.copy(&[aid], gmail, true).unwrap();
        assert!(matches!(&s.ops("g")[0].1, Op::Append { to, .. } if to == "Keep"));
        assert!(matches!(s.ops("work").last().unwrap().1, Op::Expunge { uid: 30, .. }));
        let kept = s.messages(gmail)[0].id;
        let empty = s.create_node(None, "Empty").unwrap();
        assert!(s.copy(&[kept], empty, false).is_err(), "no folder of that account in it");
    }

    #[test]
    fn mail_kept_compressed_and_trimmed() {
        let s = MailStore::memory();
        let inbox = s.remote_mailbox("work", "INBOX", "Work/Inbox", "").unwrap();
        let label = s.remote_mailbox("work", "Label", "Work/Label", "").unwrap();
        let home = s.remote_mailbox("home", "INBOX", "Home/Inbox", "").unwrap();
        // Like a real file: hardly compressible.
        let mut seed = 7u64;
        let pdf: String = (0..40_000)
            .map(|_| {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"[(seed >> 58) as usize] as char
            })
            .collect();
        let with_file = |id: &str| {
            format!(
                "From: a@x.fr\r\nSubject: {id}\r\nMessage-ID: <{id}@x>\r\nContent-Type: multipart/mixed; boundary=b\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\n{}\r\n--b\r\nContent-Type: application/pdf; name=f.pdf\r\nContent-Disposition: attachment; filename=f.pdf\r\nContent-Transfer-Encoding: base64\r\n\r\n{pdf}\r\n--b--\r\n",
                "words ".repeat(500)
            )
            .into_bytes()
        };
        // Old (2001), in two folders; middle (2011); new (2021); another account's old one.
        let (old, _) = s.add_fetched(inbox, &with_file("old"), &[], 1_000_000_000, 1).unwrap();
        s.add_fetched(label, &with_file("old"), &[], 1_000_000_000, 1).unwrap();
        let (mid, _) = s.add_fetched(inbox, &with_file("mid"), &[], 1_300_000_000, 2).unwrap();
        let (new, _) = s.add_fetched(inbox, &with_file("new"), &[], 1_600_000_000, 3).unwrap();
        s.add_fetched(home, &with_file("old"), &[], 1_000_000_000, 1).unwrap();
        let stored = |h: &str| s.db().query_row("SELECT length(data) FROM blobs WHERE hash = ?1", [h], |r| r.get::<_, i64>(0)).unwrap();
        let m = s.message(mid).unwrap();
        assert_eq!(s.blob(&m.blob).unwrap(), with_file("mid"), "read back as it came");
        // Its file, hardly compressible, loses what base64 added.
        assert!((stored(&m.blob) as u64) < m.size as u64 * 3 / 4, "kept compressed: {} of {}", stored(&m.blob), m.size);

        let plan = s.trim_plan("work", Some(1_100_000_000), Some(1_400_000_000));
        assert_eq!((plan.messages, plan.with_attachments, plan.attachments), (1, 1, 1), "{plan:?}: the old one once, the middle one's file");
        assert_eq!(plan.attachment_bytes, 30_000);
        assert_eq!(plan.message_bytes, 0, "the old one is the home account's too: nothing freed");
        assert!(s.trim_plan("work", None, None).is_empty());
        let trash = s.remote_mailbox("work", "Trash", "Work/Trash", "\\Trash").unwrap();
        s.add_fetched(trash, &with_file("binned"), &[], 1_600_000_000, 1).unwrap();
        s.add_fetched(trash, &with_file("new"), &[], 1_600_000_000, 2).unwrap();
        let u = s.usage()["work"];
        assert_eq!((u.messages, u.attachment_count, u.removed), (4, 3, 0), "{u:?}: four messages, once each; the one only in the trash apart");
        assert!(u.bins > 30_000 && u.attachments > 3 * 25_000 && u.text > 0 && u.text < 5_000, "{u:?}");
        let whole: i64 = s
            .db()
            .query_row("SELECT SUM(length(data)) FROM blobs WHERE hash IN (SELECT m.blob FROM messages m JOIN mailboxes b ON b.id = m.mailbox WHERE b.account = 'work')", [], |r| r.get(0))
            .unwrap();
        assert_eq!(u.total(), whole as u64, "all of it, by kind");
        assert_eq!(s.usage()["home"].messages, 1, "each account its own");
        assert!(s.db_bytes() > 0);

        s.set_filled_since(inbox, 0).unwrap();
        assert_eq!(s.drop_mail_before("work", 1_100_000_000).unwrap(), 2);
        assert_eq!((s.filled_since(inbox), s.filled_since(label)), (Some(1_100_000_000), None), "the inbox has its mail since then only");
        assert!(s.message(old).is_none() && s.messages(label).is_empty() && s.messages(home).len() == 1, "only work's old mail went");
        assert_eq!(s.strip_before("work", 1_400_000_000).unwrap(), 1);
        let slim = s.message(mid).unwrap();
        let text = String::from_utf8(s.blob(&slim.blob).unwrap()).unwrap();
        assert!(text.contains("X-Valet-Removed: 30000") && !text.contains(&pdf[..100]) && text.contains("words"), "the file went, the text stayed");
        assert_eq!(slim.size as usize, text.len());
        assert_eq!(s.blob(&s.message(new).unwrap().blob).unwrap(), with_file("new"), "the new one kept whole");
        assert!(s.db().query_row("SELECT 1 FROM blobs WHERE hash = ?1", [&m.blob], |_| Ok(())).optional().unwrap().is_none(), "the whole copy is gone");
        assert!(s.trim_plan("work", Some(1_100_000_000), Some(1_400_000_000)).is_empty(), "nothing left to remove");

        // Asked back: fetched whole again.
        assert_eq!(s.stripped_rows(inbox, 1_200_000_000), [(mid, 2)]);
        assert!(s.stripped_rows(inbox, 1_350_000_000).is_empty());
        s.set_content(mid, &with_file("mid")).unwrap();
        assert!(s.stripped_rows(inbox, 0).is_empty());
        assert_eq!(s.blob(&s.message(mid).unwrap().blob).unwrap(), with_file("mid"));
        assert_eq!(s.gc_blobs(), 1, "the slim copy, no longer used");
    }

    #[test]
    fn mail_kept_before_compression_is_compressed_once() {
        let s = MailStore::memory();
        let inbox = s.remote_mailbox("work", "INBOX", "Work/Inbox", "").unwrap();
        let body = format!("Subject: old\r\nMessage-ID: <o@x>\r\n\r\n{}", "hello ".repeat(5000)).into_bytes();
        let (id, _) = s.add_fetched(inbox, &body, &[], 0, 1).unwrap();
        let hash = s.message(id).unwrap().blob;
        // As an older valet left it: as received, without its facts; and one no message uses.
        s.db().execute("UPDATE blobs SET data = ?2, atts = NULL WHERE hash = ?1", params![hash, body]).unwrap();
        s.db().execute("INSERT INTO blobs (hash, data) VALUES ('orphan', x'00')", []).unwrap();
        assert_eq!(s.blob(&hash).unwrap(), body, "read as it is");
        s.compact();
        let (len, atts): (i64, Option<i64>) = s.db().query_row("SELECT length(data), atts FROM blobs WHERE hash = ?1", [&hash], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert!((len as usize) < body.len() / 10 && atts == Some(0), "compressed, facts noted");
        assert_eq!(s.blob(&hash).unwrap(), body);
        assert_eq!(s.db().query_row("SELECT COUNT(*) FROM blobs", [], |r| r.get::<_, i64>(0)).unwrap(), 1, "the unused one went");
        s.db().execute("UPDATE blobs SET data = ?2 WHERE hash = ?1", params![hash, body]).unwrap();
        s.compact();
        assert_eq!(s.db().query_row("SELECT length(data) FROM blobs", [], |r| r.get::<_, i64>(0)).unwrap() as usize, body.len(), "only once");
    }

    #[test]
    fn invitations_out_of_order() {
        let s = MailStore::memory();
        s.add_invite("w", "u", "REQUEST", 2, "", "", "", "", "").unwrap();
        assert!(s.invite_superseded("w", "u", "REQUEST", 1), "an older version");
        assert!(!s.invite_superseded("w", "u", "REQUEST", 2) && !s.invite_superseded("w", "u", "REQUEST", 3));
        assert!(!s.invite_superseded("x", "u", "REQUEST", 1), "another account's");
        s.add_invite("w", "u", "CANCEL", 3, "", "", "", "", "").unwrap();
        assert!(s.invite_superseded("w", "u", "REQUEST", 3), "cancelled");
        assert!(!s.invite_superseded("w", "u", "REQUEST", 4), "invited again");
        assert!(s.invite_superseded("w", "u", "CANCEL", 1));
    }
}
