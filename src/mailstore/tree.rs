//! The user's tree of folders. A node is a mailbox row without an account
//! (like `INBOX`) plus a `tree` row (its parent, place among its siblings and
//! label). It gathers real folders of any accounts (`tree_members`) and shows
//! their messages as link rows. Its IMAP name is its path of labels
//! (`Work/Clients/Acme`); `INBOX` and `Accounts` are taken at the top.
//!
//! The default tree: `INBOX` with every inbox, a node per special use
//! (Drafts, Sent, Archive, Spam, Trash) with the folders of that use, then
//! the other folders, those with the same path in several accounts together.
//! An account's folders are placed so when it is first synced; folders that
//! appear later wait under `Accounts/` until added from the tree editor.
//! Among the special folders, *All mail* (`\All`) gathers no folder in
//! particular: every account folder's mail, Spam and Trash too.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;

use super::{ACCOUNTS, ALL_INBOXES, MailStore, Mailbox, boxes};

/// The special uses a node can have, with the default tree's label for each.
pub const SPECIALS: &[(&str, &str)] = &[("\\Drafts", "Drafts"), ("\\Sent", "Sent"), ("\\Archive", "Archive"), ("\\Junk", "Spam"), ("\\Trash", "Trash")];

/// The special use of the node with every account folder's mail.
pub const ALL_MAIL: &str = "\\All";
const ALL_MAIL_LABEL: &str = "All mail";

/// The default tree's top, after INBOX: the special folders in this order.
const TOP: &[&str] = &["\\Drafts", "\\Sent", "\\Archive", ALL_MAIL, "\\Junk", "\\Trash"];

/// Folder names that mean a special use when the provider doesn't flag any.
const SPECIAL_NAMES: &[(&str, &[&str])] = &[
    ("\\Drafts", &["drafts", "brouillons", "entwürfe"]),
    ("\\Sent", &["sent", "sent mail", "sent items", "sent messages", "envoyés", "gesendet"]),
    ("\\Archive", &["archive", "archives"]),
    ("\\Junk", &["spam", "junk", "junk email", "junk e-mail", "courrier indésirable"]),
    ("\\Trash", &["trash", "deleted items", "deleted messages", "bin", "corbeille", "papierkorb"]),
];

#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    /// Its mailbox row.
    pub id: i64,
    pub parent: Option<i64>,
    pub label: String,
    pub special: String,
    /// The real folders it gathers.
    pub members: Vec<i64>,
    /// 0 at the top.
    pub depth: usize,
}

/// A node's place, as the editor sends the whole tree: siblings in order.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Place {
    pub id: i64,
    pub parent: Option<i64>,
    pub label: String,
}

#[derive(Clone, Copy, Debug)]
pub enum Shift {
    Up,
    Down,
    /// Into the previous sibling, last.
    Indent,
    /// Out of its parent, right after it.
    Outdent,
}

fn inbox_id(c: &Connection) -> rusqlite::Result<i64> {
    c.query_row("SELECT id FROM mailboxes WHERE name = ?1", [ALL_INBOXES], |r| r.get(0))
}

/// The All mail node, if the tree has one.
pub(super) fn all_mail(c: &Connection) -> rusqlite::Result<Option<i64>> {
    c.prepare_cached("SELECT mailbox FROM tree WHERE special = ?1")?.query_row([ALL_MAIL], |r| r.get(0)).optional()
}

/// The nodes gathering a real folder.
pub(super) fn nodes_of(c: &Connection, mailbox: i64) -> rusqlite::Result<Vec<i64>> {
    let mut st = c.prepare_cached("SELECT node FROM tree_members WHERE member = ?1")?;
    st.query_map([mailbox], |r| r.get(0))?.collect()
}

/// Every node, depth first, siblings in order.
fn load(c: &Connection) -> rusqlite::Result<Vec<Node>> {
    let mut members: HashMap<i64, Vec<i64>> = HashMap::new();
    {
        let mut st = c.prepare("SELECT node, member FROM tree_members ORDER BY member")?;
        for r in st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))? {
            let (n, m) = r?;
            members.entry(n).or_default().push(m);
        }
    }
    let mut st = c.prepare("SELECT mailbox, parent, label, special FROM tree ORDER BY position, mailbox")?;
    let rows: Vec<Node> = st
        .query_map([], |r| {
            let id = r.get(0)?;
            Ok(Node { id, parent: r.get(1)?, label: r.get(2)?, special: r.get(3)?, members: members.remove(&id).unwrap_or_default(), depth: 0 })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let ids: HashSet<i64> = rows.iter().map(|n| n.id).collect();
    let mut out = Vec::with_capacity(rows.len());
    fn walk(rows: &[Node], parent: Option<i64>, depth: usize, ids: &HashSet<i64>, out: &mut Vec<Node>) {
        for n in rows.iter().filter(|n| n.parent.filter(|p| ids.contains(p)) == parent) {
            if out.iter().any(|o: &Node| o.id == n.id) {
                continue;
            }
            out.push(Node { depth, ..n.clone() });
            walk(rows, Some(n.id), depth + 1, ids, out);
        }
    }
    walk(&rows, None, 0, &ids, &mut out);
    Ok(out)
}

fn places_of(nodes: &[Node]) -> Vec<Place> {
    nodes.iter().map(|n| Place { id: n.id, parent: n.parent, label: n.label.clone() }).collect()
}

fn check_label(label: &str) -> Result<()> {
    ensure!(!label.is_empty(), "a folder needs a name");
    ensure!(!label.contains('/'), "a folder's name can't contain /");
    ensure!(!label.chars().any(char::is_control) && label.chars().count() <= 200, "that name won't do");
    Ok(())
}

fn reserved(label: &str) -> bool {
    label.eq_ignore_ascii_case(ALL_INBOXES) || label.eq_ignore_ascii_case(ACCOUNTS)
}

/// Writes the whole tree: places (every node once, siblings in order),
/// labels and the mailbox names that follow from them.
fn arrange(c: &Connection, places: &[Place]) -> Result<()> {
    let existing: HashSet<i64> = {
        let mut st = c.prepare("SELECT mailbox FROM tree")?;
        st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
    };
    let given: HashSet<i64> = places.iter().map(|p| p.id).collect();
    ensure!(given.len() == places.len() && given == existing, "the tree changed meanwhile: reload the page");
    let inbox = inbox_id(c)?;
    let by_id: HashMap<i64, &Place> = places.iter().map(|p| (p.id, p)).collect();
    let mut siblings: HashSet<(Option<i64>, String)> = HashSet::new();
    for p in places {
        let label = p.label.trim();
        check_label(label)?;
        if let Some(parent) = p.parent {
            ensure!(by_id.contains_key(&parent), "no such parent folder");
        }
        if p.id == inbox {
            ensure!(p.parent.is_none() && label == ALL_INBOXES, "INBOX stays at the top, named INBOX");
        } else if p.parent.is_none() && reserved(label) {
            bail!("{label} is taken at the top");
        }
        ensure!(siblings.insert((p.parent, label.to_lowercase())), "two folders in the same place can't both be called {label}");
        let mut up = p.parent;
        let mut steps = 0;
        while let Some(u) = up {
            steps += 1;
            ensure!(u != p.id && steps <= places.len(), "a folder can't go inside itself");
            up = by_id[&u].parent;
        }
    }
    let path = |id: i64| {
        let mut parts = Vec::new();
        let mut at = Some(id);
        while let Some(i) = at {
            parts.push(by_id[&i].label.trim());
            at = by_id[&i].parent;
        }
        parts.reverse();
        parts.join("/")
    };
    let mut next: HashMap<Option<i64>, i64> = HashMap::new();
    for p in places {
        let pos = next.entry(p.parent).or_insert(0);
        c.execute("UPDATE tree SET parent = ?2, position = ?3, label = ?4 WHERE mailbox = ?1", params![p.id, p.parent, *pos, p.label.trim()])?;
        *pos += 1;
    }
    // Names in two steps, so that swapping two names doesn't collide.
    for p in places {
        c.execute("UPDATE mailboxes SET name = char(1) || id WHERE id = ?1", [p.id])?;
    }
    for p in places {
        c.execute("UPDATE mailboxes SET name = ?2 WHERE id = ?1", params![p.id, path(p.id)])?;
    }
    Ok(())
}

/// A new node's rows; [`arrange`] gives it its place and name.
fn insert_node(c: &Connection, parent: Option<i64>, label: &str, special: &str) -> Result<i64> {
    c.execute(
        "INSERT INTO mailboxes (name, uidvalidity) VALUES (char(1) || 'new' || (SELECT COALESCE(MAX(id), 0) + 1 FROM mailboxes), ?1)",
        [super::now() as u32],
    )?;
    let id = c.last_insert_rowid();
    c.execute("INSERT INTO tree (mailbox, parent, position, label, special) VALUES (?1, ?2, 1000000, ?3, ?4)", params![id, parent, label, special])?;
    Ok(id)
}

/// Makes `members` (real folders) what the node gathers. Link rows follow:
/// gone for folders taken out, added (as new mail) for folders put in.
fn set_members_in(c: &Connection, node: i64, members: &[i64]) -> Result<()> {
    let old: HashSet<i64> = {
        let mut st = c.prepare("SELECT member FROM tree_members WHERE node = ?1")?;
        st.query_map([node], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
    };
    let new: HashSet<i64> = members.iter().copied().collect();
    for &m in &new {
        let real: Option<bool> = c.query_row("SELECT account IS NOT NULL FROM mailboxes WHERE id = ?1", [m], |r| r.get(0)).optional()?;
        ensure!(real == Some(true), "only accounts' folders can be gathered");
    }
    for &m in old.difference(&new) {
        c.execute("DELETE FROM messages WHERE mailbox = ?1 AND link IN (SELECT id FROM messages WHERE mailbox = ?2)", params![node, m])?;
        c.execute("DELETE FROM tree_members WHERE node = ?1 AND member = ?2", params![node, m])?;
    }
    let mut added: Vec<i64> = new.difference(&old).copied().collect();
    added.sort_unstable();
    for m in added {
        c.execute("INSERT INTO tree_members (node, member) VALUES (?1, ?2)", params![node, m])?;
        let rows: Vec<(i64, String, String, i64, i64, String)> = {
            let mut st = c.prepare("SELECT id, blob, flags, date, size, message_id FROM messages WHERE mailbox = ?1 AND link IS NULL ORDER BY date, id")?;
            st.query_map([m], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?.collect::<rusqlite::Result<_>>()?
        };
        for (id, blob, flags, date, size, mid) in rows {
            MailStore::insert_row(c, node, &blob, &flags, date, size as usize, &mid, None, Some(id))?;
        }
    }
    Ok(())
}

/// The special use the default tree gives a folder: the provider's flag, or
/// a common name when the account flags no folder of that use.
fn default_special(f: &Mailbox, flagged: &HashSet<(String, String)>) -> Option<&'static str> {
    if let Some((s, _)) = SPECIALS.iter().find(|(s, _)| f.special.eq_ignore_ascii_case(s)) {
        return Some(s);
    }
    let path = f.path().to_lowercase();
    if path.contains('/') {
        return None;
    }
    let account = f.account.clone().unwrap_or_default();
    SPECIAL_NAMES.iter().find(|(s, names)| names.contains(&path.as_str()) && !flagged.contains(&(account.clone(), s.to_string()))).map(|(s, _)| *s)
}

/// A label free among a node's children (only reserved names can clash).
fn free_label(places: &[Place], parent: Option<i64>, wanted: &str) -> String {
    let taken = |l: &str| (parent.is_none() && reserved(l)) || places.iter().any(|p| p.parent == parent && p.label.eq_ignore_ascii_case(l));
    let mut label = wanted.to_string();
    let mut n = 2;
    while taken(&label) {
        label = format!("{wanted} {n}");
        n += 1;
    }
    label
}

/// Where a new top folder of special use `special` goes: after INBOX and the
/// special folders before it in [`TOP`]'s order. `specials`: the nodes' (use → node).
fn top_place(places: &[Place], specials: &HashMap<String, i64>, inbox: i64, special: &str) -> usize {
    let order = |s: &str| TOP.iter().position(|x| *x == s);
    let mine = order(special);
    let node_special = |id: i64| specials.iter().find(|(_, v)| **v == id).map(|(k, _)| k.as_str());
    places
        .iter()
        .position(|p| p.parent.is_none() && p.id != inbox && node_special(p.id).and_then(order).is_none_or(|o| Some(o) > mine))
        .unwrap_or(places.len())
}

/// Makes the All mail node, among the special folders, with a link row for
/// every account folder's message.
fn make_all_mail(c: &Connection) -> Result<i64> {
    let nodes = load(c)?;
    let mut places = places_of(&nodes);
    let specials: HashMap<String, i64> = nodes.iter().filter(|n| !n.special.is_empty()).map(|n| (n.special.clone(), n.id)).collect();
    let label = free_label(&places, None, ALL_MAIL_LABEL);
    let id = insert_node(c, None, &label, ALL_MAIL)?;
    places.insert(top_place(&places, &specials, inbox_id(c)?, ALL_MAIL), Place { id, parent: None, label });
    arrange(c, &places)?;
    let rows: Vec<(i64, String, String, i64, i64, String)> = {
        let mut st = c.prepare(
            "SELECT m.id, m.blob, m.flags, m.date, m.size, m.message_id FROM messages m JOIN mailboxes b ON b.id = m.mailbox
             WHERE m.link IS NULL AND b.account IS NOT NULL ORDER BY m.date, m.id",
        )?;
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?.collect::<rusqlite::Result<_>>()?
    };
    for (row, blob, flags, date, size, mid) in rows {
        MailStore::insert_row(c, id, &blob, &flags, date, size as usize, &mid, None, Some(row))?;
    }
    Ok(id)
}

/// Puts real folders in the tree by the default tree's rules. Returns the
/// node each joined.
fn place(c: &Connection, folders: &[Mailbox]) -> Result<Vec<i64>> {
    let all = boxes(c);
    let flagged: HashSet<(String, String)> =
        all.iter().filter(|b| !b.is_node() && !b.special.is_empty()).map(|b| (b.account.clone().unwrap_or_default(), b.special.clone())).collect();
    let accounts: Vec<String> = {
        let mut st = c.prepare("SELECT id FROM accounts ORDER BY position, id")?;
        st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
    };
    let rank = |a: &Option<String>| accounts.iter().position(|x| Some(x) == a.as_ref()).unwrap_or(usize::MAX);
    let mut folders: Vec<&Mailbox> = folders.iter().filter(|f| !f.is_node()).collect();
    folders.sort_by_key(|f| (!f.is_inbox(), f.path().to_lowercase(), rank(&f.account)));

    let nodes = load(c)?;
    let mut places = places_of(&nodes);
    let mut specials: HashMap<String, i64> = nodes.iter().filter(|n| !n.special.is_empty()).map(|n| (n.special.clone(), n.id)).collect();
    let inbox = inbox_id(c)?;
    let mut joins = Vec::new();
    for f in folders {
        let node = if f.is_inbox() {
            inbox
        } else if let Some(special) = default_special(f, &flagged) {
            match specials.get(special) {
                Some(&n) => n,
                None => {
                    let label = SPECIALS.iter().find(|(s, _)| *s == special).map(|(_, l)| *l).unwrap_or("Folder");
                    let free = nodes.iter().find(|n| n.parent.is_none() && n.special.is_empty() && n.label.eq_ignore_ascii_case(label) && n.id != inbox);
                    let n = match free {
                        Some(n) => {
                            c.execute("UPDATE tree SET special = ?2 WHERE mailbox = ?1", params![n.id, special])?;
                            n.id
                        }
                        None => {
                            let label = free_label(&places, None, label);
                            let n = insert_node(c, None, &label, special)?;
                            places.insert(top_place(&places, &specials, inbox, special), Place { id: n, parent: None, label });
                            n
                        }
                    };
                    specials.insert(special.to_string(), n);
                    n
                }
            }
        } else {
            let mut parent = None;
            for part in f.path().split('/').map(str::trim).filter(|p| !p.is_empty()) {
                let found = places.iter().find(|p| p.parent == parent && p.label.eq_ignore_ascii_case(part) && !(parent.is_none() && part.eq_ignore_ascii_case(ACCOUNTS)));
                parent = Some(match found {
                    Some(p) => p.id,
                    None => {
                        let label = free_label(&places, parent, part);
                        let n = insert_node(c, parent, &label, "")?;
                        places.push(Place { id: n, parent, label });
                        n
                    }
                });
            }
            match parent {
                Some(p) => p,
                None => continue,
            }
        };
        joins.push((node, f.id));
    }
    arrange(c, &places)?;
    let mut out = Vec::new();
    for (node, folder) in joins {
        let mut members: Vec<i64> = {
            let mut st = c.prepare("SELECT member FROM tree_members WHERE node = ?1")?;
            st.query_map([node], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        members.push(folder);
        set_members_in(c, node, &members)?;
        out.push(node);
    }
    Ok(out)
}

/// Real folders in no node.
fn unplaced_in(c: &Connection) -> Vec<Mailbox> {
    boxes(c).into_iter().filter(|b| !b.is_node() && nodes_of(c, b.id).is_ok_and(|n| n.is_empty())).collect()
}

/// Moves real folders under `Accounts/` and builds the tree from them, once;
/// gives it All mail, once (it stays deleted if the user deletes it).
pub(super) fn migrate(c: &Connection) -> Result<()> {
    let done = |key: &str| c.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get::<_, i64>(0)).optional();
    if done("tree")?.is_none() {
        migrate_tree(c)?;
    }
    if done("all_mail")?.is_none() {
        let tx = c.unchecked_transaction()?;
        if all_mail(&tx)?.is_none() {
            make_all_mail(&tx)?;
        }
        tx.execute("INSERT INTO meta (key, value) VALUES ('all_mail', 1)", [])?;
        tx.commit()?;
    }
    Ok(())
}

fn migrate_tree(c: &Connection) -> Result<()> {
    let tx = c.unchecked_transaction()?;
    tx.execute("UPDATE mailboxes SET name = ?1 || '/' || name WHERE account IS NOT NULL", [ACCOUNTS])?;
    let inbox = inbox_id(&tx)?;
    tx.execute("INSERT OR IGNORE INTO tree (mailbox, parent, position, label) VALUES (?1, NULL, 0, ?2)", params![inbox, ALL_INBOXES])?;
    // INBOX's rows link to the account inboxes' already.
    tx.execute("INSERT OR IGNORE INTO tree_members (node, member) SELECT ?1, id FROM mailboxes WHERE account IS NOT NULL AND remote = 'INBOX'", [inbox])?;
    let only_inbox: bool = tx.query_row("SELECT COUNT(*) = 1 FROM tree", [], |r| r.get(0))?;
    if only_inbox {
        place(&tx, &unplaced_in(&tx))?;
    }
    tx.execute("INSERT INTO meta (key, value) VALUES ('tree', 1)", [])?;
    tx.commit()?;
    Ok(())
}

/// One node's new place in the tree, by a button of the editor.
pub fn shifted(mut places: Vec<Place>, id: i64, how: Shift) -> Result<Vec<Place>> {
    let i = places.iter().position(|p| p.id == id).context("no such folder")?;
    let parent = places[i].parent;
    let prev = places[..i].iter().rposition(|p| p.parent == parent);
    let next = places[i + 1..].iter().position(|p| p.parent == parent).map(|j| j + i + 1);
    match how {
        Shift::Up => places.swap(i, prev.context("it's first already")?),
        Shift::Down => places.swap(i, next.context("it's last already")?),
        Shift::Indent => {
            let into = places[prev.context("there's no folder above it to go into")?].id;
            let mut p = places.remove(i);
            p.parent = Some(into);
            places.push(p);
        }
        Shift::Outdent => {
            let out = parent.context("it's at the top already")?;
            let mut p = places.remove(i);
            p.parent = places.iter().find(|x| x.id == out).and_then(|x| x.parent);
            let at = places.iter().position(|x| x.id == out).map_or(places.len(), |a| a + 1);
            places.insert(at, p);
        }
    }
    Ok(places)
}

impl MailStore {
    /// Runs `f` in a transaction, then tells IDLE sessions.
    fn in_tx<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let out = {
            let c = self.db();
            let tx = c.unchecked_transaction()?;
            let out = f(&tx)?;
            tx.commit()?;
            out
        };
        self.notify();
        Ok(out)
    }

    /// The All mail node, if the tree has one.
    pub fn all_mail(&self) -> Option<i64> {
        all_mail(&self.db()).ok().flatten()
    }

    /// Every node, depth first.
    pub fn tree(&self) -> Vec<Node> {
        load(&self.db()).unwrap_or_default()
    }

    pub fn node(&self, id: i64) -> Option<Node> {
        self.tree().into_iter().find(|n| n.id == id)
    }

    /// The whole tree's places, for [`shifted`] and [`arrange`](Self::arrange).
    pub fn places(&self) -> Vec<Place> {
        places_of(&self.tree())
    }

    /// Rewrites the tree's shape (every node once, siblings in order).
    pub fn arrange(&self, places: &[Place]) -> Result<()> {
        self.in_tx(|c| arrange(c, places))
    }

    /// A new, empty node, last inside `parent` (or at the top).
    pub fn create_node(&self, parent: Option<i64>, label: &str) -> Result<i64> {
        self.in_tx(|c| {
            let mut places = places_of(&load(c)?);
            ensure!(parent.is_none_or(|p| places.iter().any(|x| x.id == p)), "no such parent folder");
            let label = label.trim();
            check_label(label)?;
            let id = insert_node(c, parent, label, "")?;
            places.push(Place { id, parent, label: label.to_string() });
            arrange(c, &places)?;
            Ok(id)
        })
    }

    pub fn rename_node(&self, id: i64, label: &str) -> Result<()> {
        self.in_tx(|c| {
            let mut places = places_of(&load(c)?);
            places.iter_mut().find(|p| p.id == id).context("no such folder")?.label = label.trim().to_string();
            arrange(c, &places)
        })
    }

    pub fn shift_node(&self, id: i64, how: Shift) -> Result<()> {
        self.in_tx(|c| arrange(c, &shifted(places_of(&load(c)?), id, how)?))
    }

    /// Removes a node without children; the mail stays in its folders.
    pub fn delete_node(&self, id: i64) -> Result<()> {
        self.in_tx(|c| {
            ensure!(id != inbox_id(c)?, "INBOX stays");
            let nodes = load(c)?;
            ensure!(nodes.iter().any(|n| n.id == id), "no such folder");
            ensure!(!nodes.iter().any(|n| n.parent == Some(id)), "move or delete the folders inside it first");
            c.execute("DELETE FROM messages WHERE mailbox = ?1", [id])?;
            c.execute("DELETE FROM tree_members WHERE node = ?1", [id])?;
            c.execute("DELETE FROM tree WHERE mailbox = ?1", [id])?;
            c.execute("DELETE FROM mailboxes WHERE id = ?1", [id])?;
            c.execute("UPDATE rules SET node = NULL WHERE node = ?1", [id])?;
            Ok(())
        })
    }

    /// What the node gathers (real folders).
    pub fn set_members(&self, node: i64, members: &[i64]) -> Result<()> {
        self.in_tx(|c| {
            ensure!(load(c)?.iter().any(|n| n.id == node), "no such folder");
            ensure!(all_mail(c)? != Some(node), "All mail gathers every folder by itself");
            set_members_in(c, node, members)
        })
    }

    pub fn add_member(&self, node: i64, member: i64) -> Result<()> {
        let mut members = self.node(node).context("no such folder")?.members;
        members.push(member);
        self.set_members(node, &members)
    }

    /// The real folder of `account` that `node` gathers (the one of the
    /// node's own special use first, if several).
    pub fn member_of(&self, node: i64, account: &str) -> Option<i64> {
        let c = self.db();
        c.query_row(
            "SELECT m.id FROM tree_members tm JOIN mailboxes m ON m.id = tm.member LEFT JOIN tree t ON t.mailbox = tm.node
             WHERE tm.node = ?1 AND m.account = ?2
             ORDER BY m.remote = 'INBOX' DESC, (m.special != '' AND m.special = t.special) DESC, m.id LIMIT 1",
            params![node, account],
            |r| r.get(0),
        )
        .optional()
        .unwrap()
    }

    /// Gives the node a special use (`\Sent`…, or none), taking it from the
    /// node that had it: each is used once.
    pub fn set_special(&self, node: i64, special: &str) -> Result<()> {
        ensure!(special.is_empty() || SPECIALS.iter().any(|(s, _)| *s == special), "unknown special use");
        self.in_tx(|c| {
            ensure!(node != inbox_id(c)?, "INBOX is the inbox");
            ensure!(all_mail(c)? != Some(node), "All mail is all the mail");
            ensure!(c.execute("UPDATE tree SET special = special WHERE mailbox = ?1", [node])? == 1, "no such folder");
            if !special.is_empty() {
                c.execute("UPDATE tree SET special = '' WHERE special = ?1", [special])?;
            }
            c.execute("UPDATE tree SET special = ?2 WHERE mailbox = ?1", params![node, special])?;
            Ok(())
        })
    }

    /// Places the account's folders that are in no node yet, as in the default tree.
    pub fn place_account(&self, account: &str) -> Result<()> {
        self.in_tx(|c| {
            let folders: Vec<Mailbox> = unplaced_in(c).into_iter().filter(|b| b.account.as_deref() == Some(account)).collect();
            place(c, &folders).map(|_| ())
        })
    }

    /// "Add to the tree": puts a real folder where the default tree would.
    /// Returns the node it joined.
    pub fn place_folder(&self, folder: i64) -> Result<i64> {
        self.in_tx(|c| {
            let f = super::mailbox(c, folder).filter(|b| !b.is_node()).context("no such folder")?;
            place(c, &[f])?.pop().context("that folder can't go in the tree")
        })
    }

    /// Real folders in no node.
    pub fn unplaced(&self) -> Vec<Mailbox> {
        unplaced_in(&self.db())
    }

    /// Back to the default tree, from every account's folders.
    pub fn reset_tree(&self) -> Result<()> {
        self.in_tx(|c| {
            let inbox = inbox_id(c)?;
            let rules: Vec<(i64, String)> = {
                let mut st = c.prepare("SELECT r.id, m.name FROM rules r JOIN mailboxes m ON m.id = r.node")?;
                st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
            };
            for n in load(c)?.into_iter().filter(|n| n.id != inbox) {
                c.execute("DELETE FROM messages WHERE mailbox = ?1", [n.id])?;
                c.execute("DELETE FROM tree_members WHERE node = ?1", [n.id])?;
                c.execute("DELETE FROM tree WHERE mailbox = ?1", [n.id])?;
                c.execute("DELETE FROM mailboxes WHERE id = ?1", [n.id])?;
            }
            c.execute("UPDATE tree SET special = '' WHERE mailbox = ?1", [inbox])?;
            let inboxes: Vec<i64> = boxes(c).iter().filter(|b| !b.is_node() && b.is_inbox()).map(|b| b.id).collect();
            set_members_in(c, inbox, &inboxes)?;
            place(c, &unplaced_in(c))?;
            make_all_mail(c)?;
            // A rule follows its folder, if the new tree has one of that name.
            for (id, name) in rules {
                c.execute("UPDATE rules SET node = (SELECT id FROM mailboxes WHERE name = ?2 AND account IS NULL) WHERE id = ?1", params![id, name])?;
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Op, StoreMode, account_path};
    use super::*;

    fn raw(id: &str, from: &str) -> Vec<u8> {
        format!("From: {from}\r\nMessage-ID: <{id}>\r\nSubject: s\r\n\r\nbody\r\n").into_bytes()
    }

    fn store() -> MailStore {
        let s = MailStore::memory();
        let json = r#"[{"id":"g","name":"Gmail","address":"me@gmail.com","imap":"i:993","smtp":"s:465","password":"p"},
            {"id":"i","name":"IONOS","address":"me@ionos.fr","aliases":["hi@ionos.fr"],"imap":"i:993","smtp":"s:465","password":"p"}]"#;
        let accs: Vec<crate::accounts::Account> = serde_json::from_str(json).unwrap();
        for a in &accs {
            s.put_account(a).unwrap();
        }
        s
    }

    fn folder(s: &MailStore, acc: &str, name: &str, remote: &str, special: &str) -> i64 {
        s.remote_mailbox(acc, remote, &account_path(if acc == "g" { "Gmail" } else { "IONOS" }, name), special).unwrap()
    }

    fn names(s: &MailStore) -> Vec<String> {
        s.tree().iter().map(|n| s.mailbox(n.id).unwrap().name).collect()
    }

    fn by_name(s: &MailStore, name: &str) -> i64 {
        s.mailbox_named(name).unwrap().id
    }

    #[test]
    fn default_tree_from_accounts() {
        let s = store();
        let g_in = folder(&s, "g", "Inbox", "INBOX", "");
        let g_spam = folder(&s, "g", "Spam", "[Gmail]/Spam", "\\Junk");
        let g_sent = folder(&s, "g", "Sent Mail", "[Gmail]/Sent Mail", "\\Sent");
        let g_proj = folder(&s, "g", "Projects", "Projects", "");
        folder(&s, "g", "Projects/2026", "Projects/2026", "");
        // IONOS flags nothing: names tell.
        let i_in = folder(&s, "i", "Inbox", "INBOX", "");
        let i_junk = folder(&s, "i", "Junk", "Junk", "");
        let i_sent = folder(&s, "i", "Gesendet", "Gesendet", "");
        let i_trash = folder(&s, "i", "Papierkorb", "Papierkorb", "");
        let i_proj = folder(&s, "i", "projects", "projects", "");
        folder(&s, "i", "Clients", "Clients", "");
        s.place_account("g").unwrap();
        s.place_account("i").unwrap();
        assert_eq!(names(&s), ["INBOX", "Sent", "All mail", "Spam", "Trash", "Projects", "Projects/2026", "Clients"]);
        let node = |name: &str| s.node(by_name(&s, name)).unwrap();
        assert_eq!(node("INBOX").members, [g_in, i_in]);
        assert_eq!((node("Spam").members, node("Spam").special), (vec![g_spam, i_junk], "\\Junk".to_string()));
        assert_eq!(node("Sent").members, [g_sent, i_sent]);
        assert_eq!(node("Trash").members, [i_trash]);
        assert_eq!(node("Projects").members, [g_proj, i_proj]);
        assert_eq!(node("Projects/2026").depth, 1);
        assert!(s.unplaced().is_empty());

        // A folder made later waits under Accounts until added; a special one joins its node.
        let g_drafts = folder(&s, "g", "Drafts", "[Gmail]/Drafts", "\\Drafts");
        let g_misc = folder(&s, "g", "Misc", "Misc", "");
        assert_eq!(s.unplaced().len(), 2);
        let drafts = s.place_folder(g_drafts).unwrap();
        assert_eq!(names(&s)[..3], ["INBOX", "Drafts", "Sent"], "Drafts goes before Sent");
        assert_eq!(s.node(drafts).unwrap().special, "\\Drafts");
        s.place_folder(g_misc).unwrap();
        assert_eq!(names(&s).last().unwrap(), "Misc");

        // LIST-wise: nothing named like a real folder.
        assert!(s.mailboxes().iter().filter(|b| !b.is_node()).all(|b| b.name.starts_with("Accounts/")));

        // Reset: the same tree, made again (new ids, same shape).
        s.rename_node(by_name(&s, "Projects"), "Work").unwrap();
        s.reset_tree().unwrap();
        assert_eq!(names(&s), ["INBOX", "Drafts", "Sent", "All mail", "Spam", "Trash", "Clients", "Misc", "Projects", "Projects/2026"]);
    }

    #[test]
    fn nodes_link_flags_and_expunge_everywhere() {
        let s = store();
        let spam = folder(&s, "g", "Spam", "Spam", "\\Junk");
        let other = s.create_node(None, "Suspicious").unwrap();
        s.place_account("g").unwrap();
        let spam_node = by_name(&s, "Spam");
        s.add_member(other, spam).unwrap();
        let (id, _) = s.add_fetched(spam, &raw("a", "x@y.z"), &[], 0, 5).unwrap();
        let in_spam = &s.messages(spam_node)[0];
        let in_other = &s.messages(other)[0];
        assert_eq!((in_spam.link, in_other.link), (Some(id), Some(id)));

        s.store(&[in_other.id], &StoreMode::Add, &["\\Seen".into()]).unwrap();
        assert_eq!(s.message(in_spam.id).unwrap().flags, ["\\Seen"]);
        assert_eq!(s.message(id).unwrap().flags, ["\\Seen"]);
        assert!(matches!(&s.ops("g")[0].1, Op::Flags { uid: 5, .. }));
        s.expunge(&[in_spam.id]).unwrap();
        assert!(s.messages(spam).is_empty() && s.messages(spam_node).is_empty() && s.messages(other).is_empty());
        assert!(matches!(&s.ops("g")[1].1, Op::Expunge { uid: 5, .. }));
    }

    #[test]
    fn members_change_link_rows() {
        let s = store();
        let a = folder(&s, "g", "A", "A", "");
        let b = folder(&s, "i", "B", "B", "");
        s.add_fetched(a, &raw("1", "x@y.z"), &[], 10, 1).unwrap();
        s.add_fetched(b, &raw("2", "x@y.z"), &["\\Seen".into()], 20, 1).unwrap();
        s.add_fetched(b, &raw("3", "x@y.z"), &[], 30, 2).unwrap();
        let n = s.create_node(None, "Both").unwrap();
        s.set_members(n, &[a, b]).unwrap();
        assert_eq!(s.count(n), (3, 2));
        s.set_members(n, &[b]).unwrap();
        assert_eq!(s.count(n), (2, 1));
        assert!(s.messages(n).iter().all(|m| s.message(m.link.unwrap()).unwrap().mailbox == b));
        assert!(s.set_members(n, &[n]).is_err(), "a node gathers real folders only");
        // A folder gone at the provider leaves the tree.
        s.prune_mailboxes("i", &[]).unwrap();
        assert!(s.node(n).unwrap().members.is_empty() && s.messages(n).is_empty());
        s.delete_node(n).unwrap();
        assert!(s.mailbox(n).is_none());
        assert_eq!(s.count(a).0, 1, "its mail stays");
    }

    #[test]
    fn filing_into_a_node_picks_the_account_folder() {
        let s = store();
        let g_in = folder(&s, "g", "Inbox", "INBOX", "");
        let g_spam = folder(&s, "g", "Spam", "[Gmail]/Spam", "\\Junk");
        let i_sent = folder(&s, "i", "Sent", "Sent", "\\Sent");
        let g_sent = folder(&s, "g", "Sent", "[Gmail]/Sent", "\\Sent");
        s.place_account("g").unwrap();
        s.place_account("i").unwrap();
        let (spam, sent) = (by_name(&s, "Spam"), by_name(&s, "Sent"));
        assert_eq!((s.member_of(spam, "g"), s.member_of(spam, "i")), (Some(g_spam), None));
        assert_eq!(s.member_of(s.all_inboxes(), "g"), Some(g_in));

        let (id, _) = s.add_fetched(g_in, &raw("a", "x@y.z"), &[], 0, 9).unwrap();
        let pairs = s.copy(&[id], spam, true).unwrap();
        assert_eq!(s.messages(g_spam).len(), 1);
        assert_eq!(pairs[0].1, s.messages(spam)[0].uid, "COPYUID speaks of the node's UID");
        assert!(matches!(&s.ops("g")[0].1, Op::Move { uid: 9, to, .. } if to == "[Gmail]/Spam"));
        let moved = s.messages(g_spam)[0].id;
        assert!(s.copy(&[moved], by_name(&s, "INBOX"), false).is_ok());
        let empty = s.create_node(None, "Empty").unwrap();
        assert!(s.copy(&[moved], empty, false).is_err(), "no folder of that account in it yet");

        // Appending: the From address picks the account.
        let uid = s.append(sent, &raw("b", "Me <HI@ionos.fr>"), &[], 0).unwrap();
        assert_eq!(s.messages(i_sent).len(), 1);
        assert!(s.messages(g_sent).is_empty());
        assert_eq!(s.messages(sent)[0].uid, uid);
        assert!(matches!(&s.ops("i")[0].1, Op::Append { to, .. } if to == "Sent"));
        assert!(s.append(sent, &raw("c", "nobody@else.org"), &[], 0).is_err());
        assert_eq!(s.special_folder("i", "\\Sent"), Some(i_sent));
    }

    #[test]
    fn arranging_checks_the_tree() {
        let s = store();
        s.delete_node(s.all_mail().unwrap()).unwrap();
        let work = s.create_node(None, "Work").unwrap();
        let clients = s.create_node(Some(work), "Clients").unwrap();
        let acme = s.create_node(Some(clients), "Acme").unwrap();
        let home = s.create_node(None, "Home").unwrap();
        assert_eq!(names(&s), ["INBOX", "Work", "Work/Clients", "Work/Clients/Acme", "Home"]);
        assert!(s.create_node(None, "work").is_err(), "same name, same place");
        assert!(s.create_node(None, "accounts").is_err() && s.create_node(None, "Inbox").is_err());
        assert!(s.create_node(Some(work), "a/b").is_err() && s.create_node(None, " ").is_err());
        assert!(s.create_node(Some(work), "Inbox").is_ok(), "only taken at the top");
        let mut places = s.places();
        places.iter_mut().find(|p| p.id == work).unwrap().parent = Some(acme);
        assert!(s.arrange(&places).is_err(), "no cycles");
        assert!(s.rename_node(s.all_inboxes(), "Mail").is_err() && s.delete_node(s.all_inboxes()).is_err());
        assert!(s.delete_node(work).is_err(), "it has folders inside");

        s.shift_node(home, Shift::Up).unwrap();
        assert_eq!(names(&s)[1], "Home");
        s.shift_node(work, Shift::Indent).unwrap();
        assert_eq!(s.mailbox(acme).unwrap().name, "Home/Work/Clients/Acme");
        s.shift_node(clients, Shift::Outdent).unwrap();
        assert_eq!(names(&s), ["INBOX", "Home", "Home/Work", "Home/Work/Inbox", "Home/Clients", "Home/Clients/Acme"]);
        assert!(s.shift_node(home, Shift::Outdent).is_err() && s.shift_node(s.all_inboxes(), Shift::Up).is_err());
        // Swapping two names in one go.
        let mut places = s.places();
        for p in places.iter_mut() {
            p.label = match p.label.as_str() {
                "Work" => "Clients".into(),
                "Clients" => "Work".into(),
                l => l.into(),
            };
        }
        s.arrange(&places).unwrap();
        assert_eq!(s.mailbox(acme).unwrap().name, "Home/Work/Acme");

        s.set_special(home, "\\Archive").unwrap();
        s.set_special(acme, "\\Archive").unwrap();
        assert_eq!((s.node(home).unwrap().special.as_str(), s.mailbox(acme).unwrap().special.as_str()), ("", "\\Archive"));
        assert!(s.set_special(home, "\\Inbox").is_err());
    }

    #[test]
    fn migrates_a_database_of_before() {
        // Today's shape: real folders named <Account>/<folder>, INBOX linking to the inboxes.
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(super::super::SCHEMA).unwrap();
        c.execute_batch(
            r#"INSERT INTO accounts VALUES ('g', 1, '{"id":"g","name":"Gmail","address":"me@gmail.com","imap":"i","smtp":"s","password":"p"}');
            INSERT INTO accounts VALUES ('i', 2, '{"id":"i","name":"IONOS","address":"me@ionos.fr","imap":"i","smtp":"s","password":"p"}');
            INSERT INTO mailboxes (id, name, uidvalidity, uidnext) VALUES (1, 'INBOX', 1, 2);
            INSERT INTO mailboxes (id, name, account, remote, special, uidvalidity, uidnext) VALUES
                (2, 'Gmail/Inbox', 'g', 'INBOX', '', 1, 2), (3, 'Gmail/Spam', 'g', '[Gmail]/Spam', '\Junk', 1, 2),
                (4, 'IONOS/Inbox', 'i', 'INBOX', '', 1, 1), (5, 'IONOS/Spam', 'i', 'Spam', '', 1, 2), (6, 'IONOS/Work', 'i', 'Work', '', 1, 1);
            INSERT INTO messages (id, mailbox, uid, blob, flags, date, size, message_id, remote_uid, link, modseq) VALUES
                (1, 2, 1, 'h1', '', 0, 1, '', 1, NULL, 1), (2, 1, 1, 'h1', '', 0, 1, '', NULL, 1, 2),
                (3, 3, 1, 'h2', '', 0, 1, '', 1, NULL, 3), (4, 5, 1, 'h3', '\Seen', 0, 1, '', 1, NULL, 4);"#,
        )
        .unwrap();
        let s = MailStore::with(c).unwrap();
        assert_eq!(s.mailbox(2).unwrap().name, "Accounts/Gmail/Inbox");
        assert_eq!(names(&s), ["INBOX", "All mail", "Spam", "Work"]);
        assert_eq!(s.messages(1).len(), 1, "INBOX's rows stay as they were");
        assert_eq!(s.count(by_name(&s, "All mail")), (3, 2), "every account folder's mail");
        assert_eq!(s.node(1).unwrap().members, [2, 4]);
        let spam = by_name(&s, "Spam");
        assert_eq!(s.count(spam), (2, 1));
        assert_eq!(s.mailbox(spam).unwrap().special, "\\Junk");
        // Twice is once.
        migrate(&s.db()).unwrap();
        assert_eq!(s.mailbox(2).unwrap().name, "Accounts/Gmail/Inbox");
        assert_eq!(names(&s), ["INBOX", "All mail", "Spam", "Work"]);
        // New mail in a gathered folder shows in its node.
        s.add_fetched(5, &raw("n", "x@y.z"), &[], 0, 2).unwrap();
        assert_eq!(s.count(spam).0, 3);
        assert_eq!(s.count(by_name(&s, "All mail")).0, 4);
    }

    #[test]
    fn all_mail_has_every_folders_mail() {
        let s = store();
        let g_in = folder(&s, "g", "Inbox", "INBOX", "");
        let g_spam = folder(&s, "g", "Spam", "[Gmail]/Spam", "\\Junk");
        let i_trash = folder(&s, "i", "Trash", "Trash", "\\Trash");
        let i_work = folder(&s, "i", "Work", "Work", "");
        s.add_fetched(g_in, &raw("a", "x@y.z"), &[], 10, 1).unwrap();
        s.place_account("g").unwrap();
        s.place_account("i").unwrap();
        let all = s.all_mail().unwrap();
        assert_eq!(names(&s), ["INBOX", "All mail", "Spam", "Trash", "Work"]);
        assert_eq!(s.mailbox(all).unwrap().special, ALL_MAIL);
        assert!(s.mailbox(all).unwrap().is_all_mail());
        let (spam, _) = s.add_fetched(g_spam, &raw("b", "x@y.z"), &[], 20, 1).unwrap();
        s.add_fetched(i_trash, &raw("c", "x@y.z"), &["\\Seen".into()], 30, 1).unwrap();
        let (work, _) = s.add_fetched(i_work, &raw("d", "x@y.z"), &[], 40, 1).unwrap();
        assert_eq!(s.count(all), (4, 3), "Spam and Trash too");
        assert!(s.node(all).unwrap().members.is_empty(), "no folder chosen: they all are");

        // Out of Spam: still there, once.
        s.copy(&[spam], g_in, true).unwrap();
        assert_eq!(s.count(all).0, 4);
        s.expunge(&[s.row_in(all, work).unwrap()]).unwrap();
        assert!(s.messages(i_work).is_empty() && s.count(all).0 == 3, "expunged from All mail: from its folder");

        // Not a place of its own.
        let one = s.messages(g_in)[0].id;
        assert!(s.copy(&[one], all, false).is_err() && s.append(all, &raw("e", "me@gmail.com"), &[], 0).is_err());
        assert!(s.set_members(all, &[g_in]).is_err() && s.set_special(all, "").is_err());
        assert_eq!(s.count(all).0, 3);

        // Specials made later go around it; a folder made later is still waiting to be added.
        let g_archive = folder(&s, "g", "Archive", "Archive", "\\Archive");
        folder(&s, "g", "Later", "Later", "");
        assert_eq!(s.unplaced().len(), 2);
        s.place_folder(g_archive).unwrap();
        assert_eq!(names(&s)[..3], ["INBOX", "Archive", "All mail"]);

        // Deleted, it stays so; the default tree has it again, with all the mail.
        s.delete_node(all).unwrap();
        migrate(&s.db()).unwrap();
        assert!(s.all_mail().is_none());
        s.add_fetched(g_in, &raw("f", "x@y.z"), &[], 50, 2).unwrap();
        s.reset_tree().unwrap();
        assert_eq!(s.count(s.all_mail().unwrap()).0, 4);
        assert_eq!(names(&s)[..4], ["INBOX", "Archive", "All mail", "Spam"]);
    }
}
