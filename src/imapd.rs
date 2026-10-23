//! The IMAP server mail apps connect to (IMAP4rev1 with IDLE, MOVE, UIDPLUS,
//! SPECIAL-USE, LITERAL+). It speaks plain TCP: façade adds TLS in front.
//!
//! Folders: the user's tree (`INBOX`, `Sent`, `Work/Clients`…, each gathering
//! folders of several accounts; special-use flags come from it), then
//! `Accounts/<Account>/<folder>` for each folder of each account.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use crate::App;
use crate::mailstore::{ACCOUNTS, ALL_INBOXES, Mailbox, Msg, StoreMode};
use crate::mime::{self, istr};
use crate::mutf7;
use crate::util;

const CAPS: &str = "IMAP4rev1 LITERAL+ SASL-IR ID ENABLE IDLE NAMESPACE UIDPLUS MOVE UNSELECT SPECIAL-USE CHILDREN AUTH=PLAIN";
const MAX_LITERAL: usize = 64 * 1024 * 1024;

pub async fn serve(app: Arc<App>, listener: TcpListener) {
    loop {
        let Ok((stream, peer)) = listener.accept().await else { continue };
        let app = app.clone();
        tokio::spawn(async move {
            if let Err(e) = Conn::new(app, stream).run().await {
                tracing::debug!(%peer, "imap connection ended: {e:#}");
            }
        });
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Atom(String),
    Str(Vec<u8>),
    List(Vec<Tok>),
}

impl Tok {
    pub fn text(&self) -> String {
        match self {
            Tok::Atom(a) => a.clone(),
            Tok::Str(s) => String::from_utf8_lossy(s).into_owned(),
            Tok::List(_) => String::new(),
        }
    }

    fn upper(&self) -> String {
        self.text().to_ascii_uppercase()
    }
}

/// Splits a command (literals inline, as `{n}\r\n<n bytes>`) into tokens.
pub fn tokenize(b: &[u8]) -> Result<Vec<Tok>> {
    fn list(b: &[u8], i: &mut usize, close: Option<u8>) -> Result<Vec<Tok>> {
        let mut out = Vec::new();
        while *i < b.len() {
            match b[*i] {
                b' ' | b'\r' | b'\n' => *i += 1,
                b'(' => {
                    *i += 1;
                    out.push(Tok::List(list(b, i, Some(b')'))?));
                }
                c if Some(c) == close => {
                    *i += 1;
                    return Ok(out);
                }
                b')' => bail!("unbalanced )"),
                b'"' => {
                    *i += 1;
                    let mut s = Vec::new();
                    while *i < b.len() && b[*i] != b'"' {
                        if b[*i] == b'\\' && *i + 1 < b.len() {
                            *i += 1;
                        }
                        s.push(b[*i]);
                        *i += 1;
                    }
                    *i += 1;
                    out.push(Tok::Str(s));
                }
                b'{' => {
                    let end = b[*i..].iter().position(|&c| c == b'}').map(|p| *i + p).ok_or_else(|| anyhow::anyhow!("bad literal"))?;
                    let n: usize = std::str::from_utf8(&b[*i + 1..end])?.trim_end_matches('+').parse()?;
                    let mut start = end + 1;
                    if b.get(start) == Some(&b'\r') {
                        start += 1;
                    }
                    if b.get(start) == Some(&b'\n') {
                        start += 1;
                    }
                    let stop = (start + n).min(b.len());
                    out.push(Tok::Str(b[start..stop].to_vec()));
                    *i = stop;
                }
                _ => {
                    let start = *i;
                    let mut depth = 0;
                    while *i < b.len() {
                        let c = b[*i];
                        if c == b'[' {
                            depth += 1;
                        } else if c == b']' {
                            depth -= 1;
                        } else if depth == 0 && (c == b' ' || c == b'(' || c == b')' || c == b'\r' || c == b'\n') {
                            break;
                        }
                        *i += 1;
                    }
                    out.push(Tok::Atom(String::from_utf8_lossy(&b[start..*i]).into_owned()));
                }
            }
        }
        Ok(out)
    }
    let mut i = 0;
    list(b, &mut i, None)
}

struct Selected {
    mailbox: Mailbox,
    read_only: bool,
    /// (uid, row id) in sequence order, as this session last told the client.
    seq: Vec<(u32, i64)>,
    modseq: i64,
}

struct Conn {
    app: Arc<App>,
    reader: BufReader<tokio::net::tcp::OwnedReadHalf>,
    writer: tokio::net::tcp::OwnedWriteHalf,
    authed: bool,
    sel: Option<Selected>,
}

impl Conn {
    fn new(app: Arc<App>, stream: TcpStream) -> Conn {
        let (r, w) = stream.into_split();
        Conn { app, reader: BufReader::new(r), writer: w, authed: false, sel: None }
    }

    async fn send(&mut self, data: impl AsRef<[u8]>) -> Result<()> {
        self.writer.write_all(data.as_ref()).await?;
        Ok(())
    }

    async fn line(&mut self, s: &str) -> Result<()> {
        self.send(format!("{s}\r\n")).await
    }

    /// One command, with its literals, or `None` at end of stream.
    async fn read_command(&mut self) -> Result<Option<Vec<u8>>> {
        let mut out = Vec::new();
        loop {
            let mut line = Vec::new();
            let n = tokio::time::timeout(Duration::from_secs(35 * 60), self.reader.read_until(b'\n', &mut line)).await??;
            if n == 0 {
                return Ok(if out.is_empty() { None } else { Some(out) });
            }
            out.extend_from_slice(&line);
            let trimmed = line.strip_suffix(b"\r\n").or_else(|| line.strip_suffix(b"\n")).unwrap_or(&line);
            let Some(open) = trimmed.ends_with(b"}").then(|| trimmed.iter().rposition(|&c| c == b'{')).flatten() else {
                return Ok(Some(out));
            };
            let spec = std::str::from_utf8(&trimmed[open + 1..trimmed.len() - 1]).unwrap_or("");
            let plus = spec.ends_with('+');
            let Ok(len) = spec.trim_end_matches('+').parse::<usize>() else { return Ok(Some(out)) };
            if len > MAX_LITERAL {
                bail!("literal too large");
            }
            if !plus {
                self.line("+ Ready").await?;
            }
            let mut buf = vec![0; len];
            self.reader.read_exact(&mut buf).await?;
            out.extend_from_slice(&buf);
        }
    }

    async fn run(mut self) -> Result<()> {
        self.line(&format!("* OK [CAPABILITY {CAPS}] valet ready")).await?;
        while let Some(cmd) = self.read_command().await? {
            let toks = match tokenize(&cmd) {
                Ok(t) => t,
                Err(_) => {
                    self.line("* BAD unparsable command").await?;
                    continue;
                }
            };
            let (Some(tag), Some(name)) = (toks.first(), toks.get(1)) else {
                if !toks.is_empty() {
                    self.line("* BAD missing command").await?;
                }
                continue;
            };
            let tag = tag.text();
            let name = name.upper();
            let args = toks[2..].to_vec();
            match self.command(&tag, &name, &args).await {
                Ok(Flow::Continue) => {}
                Ok(Flow::Logout) => return Ok(()),
                Err(e) => self.line(&format!("{tag} NO {}", one_line(&format!("{e:#}")))).await?,
            }
        }
        Ok(())
    }

    async fn command(&mut self, tag: &str, name: &str, args: &[Tok]) -> Result<Flow> {
        match name {
            "CAPABILITY" => {
                self.line(&format!("* CAPABILITY {CAPS}")).await?;
                self.ok(tag, "CAPABILITY").await?;
            }
            "NOOP" | "CHECK" => {
                self.refresh(true).await?;
                self.ok(tag, name).await?;
            }
            "LOGOUT" => {
                self.line("* BYE see you").await?;
                self.ok(tag, "LOGOUT").await?;
                return Ok(Flow::Logout);
            }
            "ID" => {
                self.line("* ID (\"name\" \"valet\")").await?;
                self.ok(tag, "ID").await?;
            }
            "STARTTLS" => self.line(&format!("{tag} BAD TLS is handled in front of valet")).await?,
            "LOGIN" => {
                let (user, pass) = (arg(args, 0)?, arg(args, 1)?);
                self.login(tag, &user, &pass).await?;
            }
            "AUTHENTICATE" => {
                if !arg(args, 0)?.eq_ignore_ascii_case("PLAIN") {
                    self.line(&format!("{tag} NO only PLAIN")).await?;
                    return Ok(Flow::Continue);
                }
                let response = match args.get(1) {
                    Some(t) => t.text(),
                    None => {
                        self.line("+ ").await?;
                        let mut l = String::new();
                        self.reader.read_line(&mut l).await?;
                        l.trim().to_string()
                    }
                };
                let decoded = base64::engine::general_purpose::STANDARD.decode(response.trim()).unwrap_or_default();
                let parts: Vec<String> = decoded.split(|&b| b == 0).map(|p| String::from_utf8_lossy(p).into_owned()).collect();
                let (user, pass) = match parts.as_slice() {
                    [_, u, p] => (u.clone(), p.clone()),
                    _ => (String::new(), String::new()),
                };
                self.login(tag, &user, &pass).await?;
            }
            _ if !self.authed => self.line(&format!("{tag} BAD log in first")).await?,
            "ENABLE" => {
                self.line("* ENABLED").await?;
                self.ok(tag, "ENABLE").await?;
            }
            "NAMESPACE" => {
                self.line("* NAMESPACE ((\"\" \"/\")) NIL NIL").await?;
                self.ok(tag, "NAMESPACE").await?;
            }
            "LIST" | "LSUB" => self.list(tag, name, args).await?,
            "STATUS" => self.status(tag, args).await?,
            "SELECT" | "EXAMINE" => self.select(tag, &mailbox_arg(args, 0)?, name == "EXAMINE").await?,
            "CREATE" | "DELETE" | "RENAME" => {
                let done = match name {
                    "CREATE" => self.create(&mailbox_arg(args, 0)?).await,
                    "DELETE" => match self.app.mail.mailbox_named(&mailbox_arg(args, 0)?) {
                        Some(b) if b.is_node() => self.app.mail.delete_node(b.id),
                        Some(b) => crate::folders::delete(&self.app, b.id).await,
                        None => Err(anyhow::anyhow!("no such folder")),
                    },
                    _ => self.rename(&mailbox_arg(args, 0)?, &mailbox_arg(args, 1)?).await,
                };
                match done {
                    Ok(()) => self.ok(tag, name).await?,
                    Err(e) => self.line(&format!("{tag} NO {}", format!("{e:#}").replace(['\r', '\n'], " "))).await?,
                }
            }
            "SUBSCRIBE" | "UNSUBSCRIBE" => self.ok(tag, name).await?,
            "APPEND" => self.append(tag, args).await?,
            "IDLE" => self.idle(tag).await?,
            _ if self.sel.is_none() => self.line(&format!("{tag} BAD select a mailbox first")).await?,
            "CLOSE" | "UNSELECT" => {
                if name == "CLOSE" {
                    let sel = self.sel.as_ref().unwrap();
                    if !sel.read_only {
                        let ids = self.deleted(None);
                        self.app.mail.expunge(&ids)?;
                    }
                }
                self.sel = None;
                self.ok(tag, name).await?;
            }
            "EXPUNGE" => {
                let ids = self.deleted(None);
                self.app.mail.expunge(&ids)?;
                self.refresh(true).await?;
                self.ok(tag, "EXPUNGE").await?;
            }
            "SEARCH" => self.search(tag, args, false).await?,
            "FETCH" => self.fetch(tag, args, false).await?,
            "STORE" => self.store(tag, args, false).await?,
            "COPY" | "MOVE" => self.copy(tag, args, false, name == "MOVE").await?,
            "UID" => {
                let sub = arg(args, 0)?.to_ascii_uppercase();
                let rest = &args[1..];
                match sub.as_str() {
                    "FETCH" => self.fetch(tag, rest, true).await?,
                    "SEARCH" => self.search(tag, rest, true).await?,
                    "STORE" => self.store(tag, rest, true).await?,
                    "COPY" | "MOVE" => self.copy(tag, rest, true, sub == "MOVE").await?,
                    "EXPUNGE" => {
                        let set = arg(rest, 0)?;
                        let uids: HashSet<u32> = self.resolve(&set, true).into_iter().map(|i| self.sel.as_ref().unwrap().seq[i].0).collect();
                        let ids = self.deleted(Some(&uids));
                        self.app.mail.expunge(&ids)?;
                        self.refresh(true).await?;
                        self.ok(tag, "UID EXPUNGE").await?;
                    }
                    _ => self.line(&format!("{tag} BAD unknown UID command")).await?,
                }
            }
            _ => self.line(&format!("{tag} BAD unknown command {name}")).await?,
        }
        Ok(Flow::Continue)
    }

    /// CREATE `Accounts/<Account>/…/<name>`: inside an account, at the
    /// provider. Anywhere else: a node of the tree (and those above it).
    async fn create(&mut self, path: &str) -> Result<()> {
        let path = path.trim_end_matches('/');
        if let Some(rest) = in_accounts(path) {
            let (parent, leaf) = rest.rsplit_once('/').context("folders go inside an account: Accounts/<Account>/<name>")?;
            if let Some(acc) = self.app.accounts().into_iter().find(|a| a.name == parent) {
                return crate::folders::create(&self.app, &acc.id, None, leaf).await.map(|_| ());
            }
            let p = self.app.mail.mailbox_named(&format!("{ACCOUNTS}/{parent}")).filter(|b| !b.is_node()).context("no such parent folder")?;
            return crate::folders::create(&self.app, p.account.as_deref().unwrap_or(""), Some(p.id), leaf).await.map(|_| ());
        }
        let parts: Vec<&str> = path.split('/').collect();
        let mut parent = None;
        for (i, part) in parts.iter().enumerate() {
            parent = Some(match self.app.mail.mailbox_named(&parts[..=i].join("/")) {
                Some(b) if b.is_node() && i + 1 < parts.len() => b.id,
                Some(_) => bail!("{path} exists already"),
                None => self.app.mail.create_node(parent, part)?,
            });
        }
        Ok(())
    }

    /// RENAME: an account's folder within its parent folder; a node anywhere
    /// in the tree.
    async fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        let b = self.app.mail.mailbox_named(from).context("no such folder")?;
        let to = to.trim_end_matches('/');
        if !b.is_node() {
            let (fp, _) = from.rsplit_once('/').unwrap_or(("", from));
            let (tp, leaf) = to.rsplit_once('/').unwrap_or(("", to));
            anyhow::ensure!(fp == tp, "an account's folders can be renamed, not moved to another parent");
            return crate::folders::rename(&self.app, b.id, leaf).await;
        }
        anyhow::ensure!(in_accounts(to).is_none(), "{ACCOUNTS} holds the accounts' own folders");
        let (parent, leaf) = match to.rsplit_once('/') {
            Some((p, leaf)) => (Some(self.app.mail.mailbox_named(p).filter(|b| b.is_node()).context("no such parent folder")?.id), leaf),
            None => (None, to),
        };
        let mut places = self.app.mail.places();
        let i = places.iter().position(|p| p.id == b.id).context("no such folder")?;
        let mut p = places.remove(i);
        p.label = leaf.to_string();
        if p.parent == parent {
            places.insert(i, p);
        } else {
            p.parent = parent;
            places.push(p);
        }
        self.app.mail.arrange(&places)
    }

    /// Before filing these messages into a node: the node gets a folder in
    /// each of their accounts that it has none in.
    async fn landing(&self, target: &Mailbox, ids: &[i64]) -> Result<()> {
        if !target.is_node() {
            return Ok(());
        }
        let mut accounts: Vec<String> = ids
            .iter()
            .filter_map(|&id| self.app.mail.message(id))
            .filter_map(|m| self.app.mail.mailbox(self.app.mail.source(m).mailbox)?.account)
            .collect();
        accounts.sort();
        accounts.dedup();
        for a in accounts {
            crate::folders::member_in(&self.app, target.id, &a).await?;
        }
        Ok(())
    }

    async fn ok(&mut self, tag: &str, what: &str) -> Result<()> {
        self.line(&format!("{tag} OK {what} completed")).await
    }

    async fn login(&mut self, tag: &str, user: &str, pass: &str) -> Result<()> {
        if self.app.config.check_password(user, pass) {
            self.authed = true;
            self.line(&format!("{tag} OK [CAPABILITY {CAPS}] logged in")).await
        } else {
            tokio::time::sleep(Duration::from_secs(1)).await;
            self.line(&format!("{tag} NO [AUTHENTICATIONFAILED] wrong user name or password")).await
        }
    }

    // Folders

    /// Every name an app can see: the tree's nodes in order, then the
    /// accounts' folders, with `Accounts` and account names as parents.
    fn names(&self) -> Vec<(String, Option<Mailbox>)> {
        let boxes = self.app.mail.mailboxes();
        let by_id: HashMap<i64, &Mailbox> = boxes.iter().map(|b| (b.id, b)).collect();
        let mut out: Vec<(String, Option<Mailbox>)> =
            self.app.mail.tree().iter().filter_map(|n| by_id.get(&n.id)).map(|m| (m.name.clone(), Some((*m).clone()))).collect();
        for m in boxes.iter().filter(|b| !b.is_node()) {
            let mut path = String::new();
            let parts: Vec<&str> = m.name.split('/').collect();
            for p in &parts[..parts.len() - 1] {
                if !path.is_empty() {
                    path.push('/');
                }
                path.push_str(p);
                if !out.iter().any(|(n, _)| *n == path) && !boxes.iter().any(|b| b.name == path) {
                    out.push((path.clone(), None));
                }
            }
            out.push((m.name.clone(), Some(m.clone())));
        }
        out
    }

    async fn list(&mut self, tag: &str, cmd: &str, args: &[Tok]) -> Result<()> {
        // LIST [(selection options)] reference pattern(s) [RETURN (…)]
        let args: Vec<&Tok> = args.iter().skip_while(|t| matches!(t, Tok::List(_))).collect();
        let reference = args.first().map(|t| mutf7::decode(&t.text())).unwrap_or_default();
        let patterns: Vec<String> = match args.get(1) {
            Some(Tok::List(l)) => l.iter().map(|t| mutf7::decode(&t.text())).collect(),
            Some(t) => vec![mutf7::decode(&t.text())],
            None => vec![String::new()],
        };
        if patterns.iter().all(|p| p.is_empty()) {
            self.line(&format!("* {cmd} (\\Noselect) \"/\" \"\"")).await?;
            return self.ok(tag, cmd).await;
        }
        let names = self.names();
        let mut lines = Vec::new();
        for (name, mbox) in &names {
            if !patterns.iter().any(|p| wildcard(&format!("{reference}{p}"), name)) {
                continue;
            }
            let children = names.iter().any(|(n, _)| n.starts_with(&format!("{name}/")));
            let mut flags = vec![if children { "\\HasChildren" } else { "\\HasNoChildren" }.to_string()];
            match mbox {
                None => flags.push("\\Noselect".into()),
                // Only the tree's: an account's own Sent isn't *the* Sent.
                Some(m) if m.is_node() && !m.special.is_empty() => flags.push(m.special.clone()),
                _ => {}
            }
            lines.push(format!("* {cmd} ({}) \"/\" {}", flags.join(" "), istr(&mutf7::encode(name))));
        }
        for l in lines {
            self.line(&l).await?;
        }
        self.ok(tag, cmd).await
    }

    fn find_mailbox(&self, name: &str) -> Result<Mailbox> {
        self.app.mail.mailbox_named(name).ok_or_else(|| anyhow::anyhow!("[NONEXISTENT] no folder {name}"))
    }

    async fn status(&mut self, tag: &str, args: &[Tok]) -> Result<()> {
        let name = mailbox_arg(args, 0)?;
        let m = self.find_mailbox(&name)?;
        let (count, unseen) = self.app.mail.count(m.id);
        let items = match args.get(1) {
            Some(Tok::List(l)) => l.iter().map(Tok::upper).collect(),
            _ => vec![],
        };
        let mut parts = Vec::new();
        for it in items {
            match it.as_str() {
                "MESSAGES" => parts.push(format!("MESSAGES {count}")),
                "RECENT" => parts.push("RECENT 0".into()),
                "UIDNEXT" => parts.push(format!("UIDNEXT {}", m.uidnext)),
                "UIDVALIDITY" => parts.push(format!("UIDVALIDITY {}", m.uidvalidity)),
                "UNSEEN" => parts.push(format!("UNSEEN {unseen}")),
                _ => {}
            }
        }
        self.line(&format!("* STATUS {} ({})", istr(&mutf7::encode(&m.name)), parts.join(" "))).await?;
        self.ok(tag, "STATUS").await
    }

    async fn select(&mut self, tag: &str, name: &str, read_only: bool) -> Result<()> {
        self.sel = None;
        let m = self.find_mailbox(name)?;
        let msgs = self.app.mail.messages(m.id);
        let modseq = msgs.iter().map(|m| m.modseq).max().unwrap_or(0);
        let first_unseen = msgs.iter().position(|x| !has_flag(&x.flags, "\\Seen"));
        self.line("* FLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft $Forwarded $Junk $NotJunk)").await?;
        self.line("* OK [PERMANENTFLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft \\*)] flags").await?;
        self.line(&format!("* {} EXISTS", msgs.len())).await?;
        self.line("* 0 RECENT").await?;
        if let Some(i) = first_unseen {
            self.line(&format!("* OK [UNSEEN {}] first unseen", i + 1)).await?;
        }
        self.line(&format!("* OK [UIDVALIDITY {}] uids valid", m.uidvalidity)).await?;
        self.line(&format!("* OK [UIDNEXT {}] next uid", m.uidnext)).await?;
        let seq = msgs.iter().map(|x| (x.uid, x.id)).collect();
        let mode = if read_only { "READ-ONLY" } else { "READ-WRITE" };
        self.sel = Some(Selected { mailbox: m, read_only, seq, modseq });
        self.line(&format!("{tag} OK [{mode}] selected")).await
    }

    /// Tells the client what changed in the selected mailbox since it last
    /// heard: expunges (when allowed), new messages, flag changes.
    async fn refresh(&mut self, expunges: bool) -> Result<()> {
        let Some(sel) = self.sel.as_mut() else { return Ok(()) };
        let current = self.app.mail.messages(sel.mailbox.id);
        let present: HashSet<i64> = current.iter().map(|m| m.id).collect();
        let mut out = Vec::new();
        if expunges {
            for i in (0..sel.seq.len()).rev() {
                if !present.contains(&sel.seq[i].1) {
                    out.push(format!("* {} EXPUNGE", i + 1));
                    sel.seq.remove(i);
                }
            }
        }
        let max_uid = sel.seq.iter().map(|s| s.0).max().unwrap_or(0);
        let added: Vec<&Msg> = current.iter().filter(|m| m.uid > max_uid).collect();
        if !added.is_empty() {
            sel.seq.extend(added.iter().map(|m| (m.uid, m.id)));
            out.push(format!("* {} EXISTS", sel.seq.len()));
        }
        for m in &current {
            if m.modseq > sel.modseq && m.uid <= max_uid
                && let Some(pos) = sel.seq.iter().position(|s| s.1 == m.id) {
                    out.push(format!("* {} FETCH (UID {} FLAGS ({}))", pos + 1, m.uid, m.flags.join(" ")));
                }
        }
        sel.modseq = current.iter().map(|m| m.modseq).max().unwrap_or(sel.modseq).max(sel.modseq);
        for l in out {
            self.line(&l).await?;
        }
        Ok(())
    }

    async fn idle(&mut self, tag: &str) -> Result<()> {
        let mut changes = self.app.mail.changed.subscribe();
        self.line("+ idling").await?;
        self.refresh(true).await?;
        let deadline = tokio::time::sleep(Duration::from_secs(29 * 60));
        tokio::pin!(deadline);
        loop {
            let mut l = String::new();
            tokio::select! {
                r = self.reader.read_line(&mut l) => {
                    if r? == 0 { bail!("closed during IDLE") }
                    break;
                }
                r = changes.recv() => {
                    if let Err(tokio::sync::broadcast::error::RecvError::Closed) = r { bail!("shutting down") }
                    // Let a burst of changes settle.
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    while changes.try_recv().is_ok() {}
                    self.refresh(true).await?;
                }
                _ = &mut deadline => {
                    self.line("* BYE idle too long").await?;
                    bail!("idle timeout");
                }
            }
        }
        self.ok(tag, "IDLE").await
    }

    async fn append(&mut self, tag: &str, args: &[Tok]) -> Result<()> {
        let name = mailbox_arg(args, 0)?;
        let m = self.find_mailbox(&name)?;
        let mut flags = Vec::new();
        let mut date = crate::mailstore::now();
        let mut raw = None;
        for t in &args[1..] {
            match t {
                Tok::List(l) => flags = l.iter().map(Tok::text).collect(),
                Tok::Str(s) if raw.is_none() && util::parse_imap_datetime(&String::from_utf8_lossy(s)).is_some() && s.len() < 40 => {
                    date = util::parse_imap_datetime(&String::from_utf8_lossy(s)).unwrap();
                }
                Tok::Str(s) => raw = Some(s.clone()),
                Tok::Atom(_) => {}
            }
        }
        let raw = raw.ok_or_else(|| anyhow::anyhow!("no message"))?;
        if m.is_node() {
            let account = self.app.mail.sender_account(&raw).context("no account sends as this message's From address: add it to an account's other addresses on valet's Mail page")?;
            crate::folders::member_in(&self.app, m.id, &account).await?;
        }
        let uid = self.app.mail.append(m.id, &raw, &flags, date)?;
        self.refresh(false).await?;
        self.line(&format!("{tag} OK [APPENDUID {} {uid}] APPEND completed", m.uidvalidity)).await
    }

    // Messages

    /// Indices into the selected mailbox's sequence for a sequence set.
    fn resolve(&self, set: &str, uid: bool) -> Vec<usize> {
        let Some(sel) = &self.sel else { return vec![] };
        let max = if uid { sel.seq.iter().map(|s| s.0).max().unwrap_or(0) } else { sel.seq.len() as u32 };
        let mut wanted: Vec<(u32, u32)> = Vec::new();
        for item in set.split(',') {
            let num = |s: &str| if s == "*" { Some(max) } else { s.parse::<u32>().ok() };
            let (a, b) = match item.split_once(':') {
                Some((a, b)) => (num(a), num(b)),
                None => (num(item), num(item)),
            };
            if let (Some(a), Some(b)) = (a, b) {
                wanted.push((a.min(b), a.max(b)));
            }
        }
        let inside = |n: u32| wanted.iter().any(|&(a, b)| n >= a && n <= b);
        (0..sel.seq.len())
            .filter(|&i| if uid { inside(sel.seq[i].0) } else { inside(i as u32 + 1) })
            .collect()
    }

    /// Rows flagged \Deleted (among `uids` if given).
    fn deleted(&self, uids: Option<&HashSet<u32>>) -> Vec<i64> {
        let Some(sel) = &self.sel else { return vec![] };
        self.app
            .mail
            .messages(sel.mailbox.id)
            .into_iter()
            .filter(|m| has_flag(&m.flags, "\\Deleted") && uids.is_none_or(|u| u.contains(&m.uid)))
            .map(|m| m.id)
            .collect()
    }

    async fn fetch(&mut self, tag: &str, args: &[Tok], uid: bool) -> Result<()> {
        let set = arg(args, 0)?;
        let mut items: Vec<String> = match args.get(1) {
            Some(Tok::List(l)) => l.iter().map(Tok::text).collect(),
            Some(t) => vec![t.text()],
            None => bail!("FETCH what?"),
        };
        let expand = |i: &str| -> Option<Vec<&'static str>> {
            match i.to_ascii_uppercase().as_str() {
                "ALL" => Some(vec!["FLAGS", "INTERNALDATE", "RFC822.SIZE", "ENVELOPE"]),
                "FAST" => Some(vec!["FLAGS", "INTERNALDATE", "RFC822.SIZE"]),
                "FULL" => Some(vec!["FLAGS", "INTERNALDATE", "RFC822.SIZE", "ENVELOPE", "BODY"]),
                _ => None,
            }
        };
        if items.len() == 1 && let Some(e) = expand(&items[0]) {
            items = e.into_iter().map(String::from).collect();
        }
        if uid && !items.iter().any(|i| i.eq_ignore_ascii_case("UID")) {
            items.insert(0, "UID".into());
        }
        let indices = self.resolve(&set, uid);
        let read_only = self.sel.as_ref().unwrap().read_only;
        for i in indices {
            let (_, id) = self.sel.as_ref().unwrap().seq[i];
            let Some(mut m) = self.app.mail.message(id) else { continue };
            let needs_body = items.iter().any(|i| {
                let u = i.to_ascii_uppercase();
                u != "UID" && u != "FLAGS" && u != "INTERNALDATE" && u != "RFC822.SIZE"
            });
            let raw = if needs_body { self.app.mail.blob(&m.blob).unwrap_or_default() } else { Vec::new() };
            let part = needs_body.then(|| mime::parse(&raw));
            // Reading a body (not PEEK) marks the message \Seen.
            let marks_seen = items.iter().any(|i| {
                let u = i.to_ascii_uppercase();
                (u.starts_with("BODY[") || u == "RFC822" || u == "RFC822.TEXT") && !u.starts_with("BODY.PEEK")
            });
            let mut flags_changed = false;
            if marks_seen && !read_only && !has_flag(&m.flags, "\\Seen")
                && let Some((_, f)) = self.app.mail.store(&[id], &StoreMode::Add, &["\\Seen".into()])?.pop() {
                    m.flags = f;
                    flags_changed = true;
                }
            let mut out: Vec<u8> = format!("* {} FETCH (", i + 1).into_bytes();
            let mut first = true;
            let mut sep = |out: &mut Vec<u8>| {
                if !first {
                    out.push(b' ');
                }
                first = false;
            };
            let mut wrote_flags = false;
            for item in &items {
                let u = item.to_ascii_uppercase();
                match u.as_str() {
                    "UID" => {
                        sep(&mut out);
                        out.extend(format!("UID {}", m.uid).bytes());
                    }
                    "FLAGS" => {
                        sep(&mut out);
                        wrote_flags = true;
                        out.extend(format!("FLAGS ({})", m.flags.join(" ")).bytes());
                    }
                    "INTERNALDATE" => {
                        sep(&mut out);
                        out.extend(format!("INTERNALDATE \"{}\"", util::imap_datetime(m.date)).bytes());
                    }
                    "RFC822.SIZE" => {
                        sep(&mut out);
                        out.extend(format!("RFC822.SIZE {}", m.size).bytes());
                    }
                    "ENVELOPE" => {
                        sep(&mut out);
                        out.extend(format!("ENVELOPE {}", mime::envelope(&raw, part.as_ref().unwrap())).bytes());
                    }
                    "BODYSTRUCTURE" | "BODY" => {
                        sep(&mut out);
                        let bs = mime::bodystructure(&raw, part.as_ref().unwrap(), u == "BODYSTRUCTURE");
                        out.extend(format!("{u} {bs}").bytes());
                    }
                    "RFC822" | "RFC822.HEADER" | "RFC822.TEXT" => {
                        sep(&mut out);
                        let spec = match u.as_str() {
                            "RFC822" => "",
                            "RFC822.HEADER" => "HEADER",
                            _ => "TEXT",
                        };
                        let data = mime::section(&raw, part.as_ref().unwrap(), spec).unwrap_or_default();
                        out.extend(format!("{u} {{{}}}\r\n", data.len()).bytes());
                        out.extend(data);
                    }
                    _ if u.starts_with("BODY[") || u.starts_with("BODY.PEEK[") || u.starts_with("BINARY") => {
                        let open = item.find('[').unwrap();
                        let close = item.rfind(']').unwrap_or(item.len());
                        let spec = &item[open + 1..close];
                        let partial = item[close..].strip_prefix(']').and_then(|p| p.strip_prefix('<')).and_then(|p| p.strip_suffix('>'));
                        let mut data = mime::section(&raw, part.as_ref().unwrap(), spec).unwrap_or_default();
                        let mut label = format!("BODY[{spec}]");
                        if let Some(p) = partial {
                            let (start, len) = match p.split_once('.') {
                                Some((a, b)) => (a.parse().unwrap_or(0usize), b.parse().unwrap_or(usize::MAX)),
                                None => (p.parse().unwrap_or(0), usize::MAX),
                            };
                            let s = start.min(data.len());
                            let e = s.saturating_add(len).min(data.len());
                            data = data[s..e].to_vec();
                            label.push_str(&format!("<{start}>"));
                        }
                        sep(&mut out);
                        out.extend(format!("{label} {{{}}}\r\n", data.len()).bytes());
                        out.extend(data);
                    }
                    _ => {}
                }
            }
            if flags_changed && !wrote_flags {
                sep(&mut out);
                out.extend(format!("FLAGS ({})", m.flags.join(" ")).bytes());
            }
            out.extend(b")\r\n");
            self.send(&out).await?;
        }
        if let Some(sel) = self.sel.as_mut() {
            // Our own \Seen changes need no echo later.
            sel.modseq = self.app.mail.messages(sel.mailbox.id).iter().map(|m| m.modseq).max().unwrap_or(sel.modseq).max(sel.modseq);
        }
        self.refresh(uid).await?;
        self.ok(tag, if uid { "UID FETCH" } else { "FETCH" }).await
    }

    async fn store(&mut self, tag: &str, args: &[Tok], uid: bool) -> Result<()> {
        if self.sel.as_ref().unwrap().read_only {
            return self.line(&format!("{tag} NO read-only")).await;
        }
        let set = arg(args, 0)?;
        let what = arg(args, 1)?.to_ascii_uppercase();
        let flags: Vec<String> = match args.get(2) {
            Some(Tok::List(l)) => l.iter().map(Tok::text).collect(),
            _ => args[2..].iter().map(Tok::text).collect(),
        };
        let mode = if what.starts_with('+') {
            StoreMode::Add
        } else if what.starts_with('-') {
            StoreMode::Remove
        } else {
            StoreMode::Set
        };
        let silent = what.ends_with(".SILENT");
        let indices = self.resolve(&set, uid);
        let ids: Vec<i64> = indices.iter().map(|&i| self.sel.as_ref().unwrap().seq[i].1).collect();
        let result = self.app.mail.store(&ids, &mode, &flags)?;
        if !silent {
            for (i, (_, f)) in indices.iter().zip(result) {
                let u = self.sel.as_ref().unwrap().seq[*i].0;
                let uid_part = if uid { format!("UID {u} ") } else { String::new() };
                self.line(&format!("* {} FETCH ({uid_part}FLAGS ({}))", i + 1, f.join(" "))).await?;
            }
        }
        if let Some(sel) = self.sel.as_mut() {
            sel.modseq = self.app.mail.messages(sel.mailbox.id).iter().map(|m| m.modseq).max().unwrap_or(sel.modseq).max(sel.modseq);
        }
        self.refresh(uid).await?;
        self.ok(tag, if uid { "UID STORE" } else { "STORE" }).await
    }

    async fn copy(&mut self, tag: &str, args: &[Tok], uid: bool, moving: bool) -> Result<()> {
        let set = arg(args, 0)?;
        let target = self.find_mailbox(&mailbox_arg(args, 1)?)?;
        let indices = self.resolve(&set, uid);
        let ids: Vec<i64> = indices.iter().map(|&i| self.sel.as_ref().unwrap().seq[i].1).collect();
        self.landing(&target, &ids).await?;
        let pairs = self.app.mail.copy(&ids, target.id, moving)?;
        let code = if pairs.is_empty() {
            String::new()
        } else {
            let src: Vec<String> = pairs.iter().map(|p| p.0.to_string()).collect();
            let dst: Vec<String> = pairs.iter().map(|p| p.1.to_string()).collect();
            format!("[COPYUID {} {} {}] ", target.uidvalidity, src.join(","), dst.join(","))
        };
        let what = if moving { "MOVE" } else { "COPY" };
        if moving {
            self.line(&format!("* OK {code}moved")).await?;
            self.refresh(true).await?;
            self.ok(tag, what).await
        } else {
            self.refresh(uid).await?;
            self.line(&format!("{tag} OK {code}{what} completed")).await
        }
    }

    async fn search(&mut self, tag: &str, args: &[Tok], uid: bool) -> Result<()> {
        let mut args = args.to_vec();
        if args.first().is_some_and(|t| t.upper() == "CHARSET") {
            args.drain(..2.min(args.len()));
        }
        if args.first().is_some_and(|t| t.upper() == "RETURN") {
            args.drain(..2.min(args.len()));
        }
        let mut pos = 0;
        let mut all = Vec::new();
        while pos < args.len() {
            all.push(parse_criterion(&args, &mut pos)?);
        }
        let crit = Criterion::And(all);
        let sel = self.sel.as_ref().unwrap();
        let max_uid = sel.seq.iter().map(|s| s.0).max().unwrap_or(0);
        let mut hits = Vec::new();
        for (i, (u, id)) in sel.seq.iter().enumerate() {
            let Some(m) = self.app.mail.message(*id) else { continue };
            let mut ctx = SearchCtx { app: &self.app, msg: &m, seq: i as u32 + 1, max_seq: sel.seq.len() as u32, max_uid, parsed: None };
            if crit.matches(&mut ctx) {
                hits.push(if uid { *u } else { i as u32 + 1 });
            }
        }
        let list: Vec<String> = hits.iter().map(u32::to_string).collect();
        self.line(&format!("* SEARCH{}{}", if list.is_empty() { "" } else { " " }, list.join(" "))).await?;
        self.refresh(uid).await?;
        self.ok(tag, if uid { "UID SEARCH" } else { "SEARCH" }).await
    }
}

enum Flow {
    Continue,
    Logout,
}

fn one_line(s: &str) -> String {
    s.replace(['\r', '\n'], " ")
}

fn arg(args: &[Tok], i: usize) -> Result<String> {
    args.get(i).map(Tok::text).ok_or_else(|| anyhow::anyhow!("missing argument"))
}

fn mailbox_arg(args: &[Tok], i: usize) -> Result<String> {
    let name = mutf7::decode(&arg(args, i)?);
    Ok(if name.eq_ignore_ascii_case("INBOX") { ALL_INBOXES.to_string() } else { name })
}

/// The rest of a name under `Accounts/`, if it is there.
fn in_accounts(name: &str) -> Option<&str> {
    name.strip_prefix(ACCOUNTS).and_then(|r| r.strip_prefix('/'))
}

fn has_flag(flags: &[String], f: &str) -> bool {
    flags.iter().any(|x| x.eq_ignore_ascii_case(f))
}

/// IMAP LIST wildcards: `*` matches anything, `%` anything but `/`.
fn wildcard(pattern: &str, name: &str) -> bool {
    fn go(p: &[char], n: &[char]) -> bool {
        match p.first() {
            None => n.is_empty(),
            Some('*') => (0..=n.len()).any(|i| go(&p[1..], &n[i..])),
            Some('%') => (0..=n.len()).take_while(|&i| i == 0 || n[i - 1] != '/').any(|i| go(&p[1..], &n[i..])),
            Some(c) => n.first() == Some(c) && go(&p[1..], &n[1..]),
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    go(&p, &n) || (pattern.eq_ignore_ascii_case("INBOX") && name == ALL_INBOXES)
}

// SEARCH

#[derive(Debug)]
enum Criterion {
    And(Vec<Criterion>),
    Or(Box<Criterion>, Box<Criterion>),
    Not(Box<Criterion>),
    All,
    Flag(String, bool),
    Header(String, String),
    Body(String),
    Text(String),
    Before(i64),
    On(i64),
    Since(i64),
    Larger(u32),
    Smaller(u32),
    Seq(String),
    Uid(String),
}

fn parse_criterion(args: &[Tok], pos: &mut usize) -> Result<Criterion> {
    let tok = args.get(*pos).ok_or_else(|| anyhow::anyhow!("incomplete SEARCH"))?.clone();
    *pos += 1;
    let next = |pos: &mut usize| -> Result<String> {
        let t = args.get(*pos).ok_or_else(|| anyhow::anyhow!("incomplete SEARCH"))?;
        *pos += 1;
        Ok(t.text())
    };
    let date = |s: String| util::parse_imap_date(&s).ok_or_else(|| anyhow::anyhow!("bad date {s}"));
    if let Tok::List(l) = &tok {
        let mut p = 0;
        let mut all = Vec::new();
        while p < l.len() {
            all.push(parse_criterion(l, &mut p)?);
        }
        return Ok(Criterion::And(all));
    }
    let word = tok.upper();
    let flag = |f: &str, on: bool| Criterion::Flag(f.into(), on);
    Ok(match word.as_str() {
        "ALL" => Criterion::All,
        "ANSWERED" => flag("\\Answered", true),
        "UNANSWERED" => flag("\\Answered", false),
        "DELETED" => flag("\\Deleted", true),
        "UNDELETED" => flag("\\Deleted", false),
        "DRAFT" => flag("\\Draft", true),
        "UNDRAFT" => flag("\\Draft", false),
        "FLAGGED" => flag("\\Flagged", true),
        "UNFLAGGED" => flag("\\Flagged", false),
        "SEEN" | "OLD" => flag("\\Seen", true),
        "UNSEEN" | "NEW" => flag("\\Seen", false),
        "RECENT" => Criterion::Not(Box::new(Criterion::All)),
        "KEYWORD" => Criterion::Flag(next(pos)?, true),
        "UNKEYWORD" => Criterion::Flag(next(pos)?, false),
        "FROM" | "TO" | "CC" | "BCC" | "SUBJECT" => Criterion::Header(word.clone(), next(pos)?),
        "HEADER" => {
            let h = next(pos)?;
            Criterion::Header(h.to_ascii_uppercase(), next(pos)?)
        }
        "BODY" => Criterion::Body(next(pos)?),
        "TEXT" => Criterion::Text(next(pos)?),
        "BEFORE" | "SENTBEFORE" => Criterion::Before(date(next(pos)?)?),
        "ON" | "SENTON" => Criterion::On(date(next(pos)?)?),
        "SINCE" | "SENTSINCE" => Criterion::Since(date(next(pos)?)?),
        "LARGER" => Criterion::Larger(next(pos)?.parse()?),
        "SMALLER" => Criterion::Smaller(next(pos)?.parse()?),
        "UID" => Criterion::Uid(next(pos)?),
        "NOT" => Criterion::Not(Box::new(parse_criterion(args, pos)?)),
        "OR" => {
            let a = parse_criterion(args, pos)?;
            let b = parse_criterion(args, pos)?;
            Criterion::Or(Box::new(a), Box::new(b))
        }
        _ if word.chars().all(|c| c.is_ascii_digit() || c == ':' || c == ',' || c == '*') => Criterion::Seq(word),
        _ => bail!("unsupported SEARCH key {word}"),
    })
}

struct SearchCtx<'a> {
    app: &'a App,
    msg: &'a Msg,
    seq: u32,
    max_seq: u32,
    max_uid: u32,
    parsed: Option<(Vec<u8>, String, String)>,
}

impl SearchCtx<'_> {
    /// (raw, lower-cased decoded headers, lower-cased decoded body text).
    fn content(&mut self) -> &(Vec<u8>, String, String) {
        if self.parsed.is_none() {
            let raw = self.app.mail.blob(&self.msg.blob).unwrap_or_default();
            let (headers, body) = match mail_parser::MessageParser::default().parse(&raw) {
                Some(m) => {
                    let mut h = String::new();
                    for (name, value) in m.headers_raw() {
                        h.push_str(&format!("{}: {}\n", name.to_lowercase(), value.trim()));
                    }
                    if let Some(s) = m.subject() {
                        h.push_str(&format!("subject: {s}\n"));
                    }
                    for (key, addrs) in [("from", m.from()), ("to", m.to()), ("cc", m.cc()), ("bcc", m.bcc())] {
                        for a in addrs.into_iter().flat_map(|a| a.iter()) {
                            h.push_str(&format!("{key}: {} {}\n", a.name().unwrap_or(""), a.address().unwrap_or("")));
                        }
                    }
                    let mut b = String::new();
                    for i in 0..m.text_body.len() {
                        if let Some(t) = m.body_text(i) {
                            b.push_str(&t);
                        }
                    }
                    if b.is_empty() {
                        for i in 0..m.html_body.len() {
                            if let Some(t) = m.body_html(i) {
                                b.push_str(&t);
                            }
                        }
                    }
                    (h.to_lowercase(), b.to_lowercase())
                }
                None => (String::new(), String::from_utf8_lossy(&raw).to_lowercase()),
            };
            self.parsed = Some((raw, headers, body));
        }
        self.parsed.as_ref().unwrap()
    }
}

fn in_set(set: &str, n: u32, max: u32) -> bool {
    set.split(',').any(|item| {
        let num = |s: &str| if s == "*" { Some(max) } else { s.parse::<u32>().ok() };
        match item.split_once(':') {
            Some((a, b)) => match (num(a), num(b)) {
                (Some(a), Some(b)) => n >= a.min(b) && n <= a.max(b),
                _ => false,
            },
            None => num(item) == Some(n),
        }
    })
}

impl Criterion {
    fn matches(&self, c: &mut SearchCtx) -> bool {
        let day = c.msg.date.div_euclid(86400);
        match self {
            Criterion::And(list) => list.iter().all(|x| x.matches(c)),
            Criterion::Or(a, b) => a.matches(c) || b.matches(c),
            Criterion::Not(a) => !a.matches(c),
            Criterion::All => true,
            Criterion::Flag(f, on) => has_flag(&c.msg.flags, f) == *on,
            Criterion::Before(d) => day < *d,
            Criterion::On(d) => day == *d,
            Criterion::Since(d) => day >= *d,
            Criterion::Larger(n) => c.msg.size > *n,
            Criterion::Smaller(n) => c.msg.size < *n,
            Criterion::Seq(s) => in_set(s, c.seq, c.max_seq),
            Criterion::Uid(s) => in_set(s, c.msg.uid, c.max_uid),
            Criterion::Header(name, value) => {
                let (name, value) = (name.to_lowercase(), value.to_lowercase());
                let headers = &c.content().1;
                headers.lines().any(|l| l.starts_with(&format!("{name}:")) && l[name.len() + 1..].contains(&value))
            }
            Criterion::Body(v) => c.content().2.contains(&v.to_lowercase()),
            Criterion::Text(v) => {
                let v = v.to_lowercase();
                let (_, h, b) = c.content();
                h.contains(&v) || b.contains(&v)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens() {
        let t = tokenize(b"a1 UID FETCH 1:* (FLAGS BODY.PEEK[HEADER.FIELDS (FROM TO)]<0.100>)\r\n").unwrap();
        assert_eq!(t[3], Tok::Atom("1:*".into()));
        assert_eq!(t[4], Tok::List(vec![Tok::Atom("FLAGS".into()), Tok::Atom("BODY.PEEK[HEADER.FIELDS (FROM TO)]<0.100>".into())]));
        let t = tokenize(b"a2 LOGIN \"me\" {3}\r\nx\"y\r\n").unwrap();
        assert_eq!(t[2], Tok::Str(b"me".to_vec()));
        assert_eq!(t[3], Tok::Str(b"x\"y".to_vec()));
        let t = tokenize(b"a3 SELECT \"a \\\"b\\\"\"\r\n").unwrap();
        assert_eq!(t[2], Tok::Str(b"a \"b\"".to_vec()));
    }

    #[test]
    fn wildcards() {
        assert!(wildcard("*", "Work/Inbox"));
        assert!(wildcard("%", "Work"));
        assert!(!wildcard("%", "Work/Inbox"));
        assert!(wildcard("Work/%", "Work/Inbox"));
        assert!(wildcard("inbox", "INBOX"));
        assert!(!wildcard("Work/%", "Gmail/Inbox"));
    }

    #[test]
    fn sets() {
        assert!(in_set("1:3,7", 2, 10));
        assert!(in_set("5:*", 10, 10));
        assert!(!in_set("5:*", 4, 10));
        assert!(in_set("*", 10, 10));
    }
}
