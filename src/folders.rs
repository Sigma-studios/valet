//! Creating, renaming and deleting an account's folders: done at the
//! provider first (so its refusals reach whoever asked), then mirrored here.
//! Used by the webmail and by mail apps (IMAP CREATE, RENAME, DELETE), and
//! when mail is filed into a tree node with no folder in its account yet.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use futures::TryStreamExt;

use crate::accounts::{self, Account, Imap};
use crate::mailstore::{Mailbox, account_path};
use crate::{App, mutf7};

/// The provider's hierarchy delimiter.
async fn delimiter(s: &mut Imap) -> Result<String> {
    // No pattern is sent as "" (Some("") would go unquoted).
    let names: Vec<_> = s.list(Some(""), None).await?.try_collect().await?;
    Ok(names.first().and_then(|n| n.delimiter()).unwrap_or("/").to_string())
}

fn check_name(name: &str, delim: &str) -> Result<()> {
    let name = name.trim();
    if name.is_empty() {
        bail!("a folder needs a name");
    }
    if name.contains('/') || name.contains(delim) {
        bail!("a folder's name can't contain {}", if delim == "/" { "/".to_string() } else { format!("/ or {delim}") });
    }
    if name.eq_ignore_ascii_case("INBOX") {
        bail!("INBOX is taken");
    }
    Ok(())
}

/// A folder valet manages for the user: not the inbox, not a special one.
fn own(b: &Mailbox) -> Result<&str> {
    let remote = b.remote.as_deref().context("that's not an account's folder")?;
    if remote.eq_ignore_ascii_case("INBOX") || !b.special.is_empty() {
        bail!("{} is one of the provider's own folders", b.path());
    }
    Ok(remote)
}

async fn session(app: &App, account: &str) -> Result<(Account, Imap)> {
    let acc = app.account(account).context("no such account")?;
    if acc.retired.is_some() {
        bail!("{} is retired: its folders no longer change at the provider", acc.name);
    }
    let s = accounts::imap(&acc, &app.mail).await?;
    Ok((acc, s))
}

fn done(app: &App, account: &str) {
    app.mail.waker(account).notify_one();
}

/// A new folder in `account`, at the top or inside `parent` (a mailbox id).
/// Returns its mailbox id.
pub async fn create(app: &Arc<App>, account: &str, parent: Option<i64>, name: &str) -> Result<i64> {
    let parent = match parent {
        Some(id) => Some(app.mail.mailbox(id).filter(|b| b.account.as_deref() == Some(account)).context("no such folder")?),
        None => None,
    };
    let (acc, mut s) = session(app, account).await?;
    let delim = delimiter(&mut s).await?;
    let name = name.trim();
    check_name(name, &delim)?;
    let (remote, local) = match &parent {
        Some(p) => (format!("{}{delim}{}", p.remote.as_deref().unwrap_or(""), mutf7::encode(name)), format!("{}/{name}", p.name)),
        None => (mutf7::encode(name), account_path(&acc.name, name)),
    };
    s.create(&remote).await.with_context(|| format!("the provider refused to create {name}"))?;
    let _ = s.subscribe(&remote).await;
    let _ = s.logout().await;
    let id = app.mail.remote_mailbox(account, &remote, &local, "")?;
    done(app, account);
    Ok(id)
}

/// Renames a folder (its subfolders follow).
pub async fn rename(app: &Arc<App>, id: i64, name: &str) -> Result<()> {
    let b = app.mail.mailbox(id).context("no such folder")?;
    let old_remote = own(&b)?.to_string();
    let account = b.account.clone().context("no account")?;
    let (_, mut s) = session(app, &account).await?;
    let delim = delimiter(&mut s).await?;
    let name = name.trim();
    check_name(name, &delim)?;
    let new_remote = match old_remote.rsplit_once(delim.as_str()) {
        Some((parent, _)) => format!("{parent}{delim}{}", mutf7::encode(name)),
        None => mutf7::encode(name),
    };
    let new_local = match b.name.rsplit_once('/') {
        Some((parent, _)) => format!("{parent}/{name}"),
        None => name.to_string(),
    };
    if new_remote == old_remote {
        return Ok(());
    }
    s.rename(&old_remote, &new_remote).await.with_context(|| format!("the provider refused to rename it to {name}"))?;
    let _ = s.subscribe(&new_remote).await;
    let _ = s.logout().await;
    app.mail.rename_mailboxes(&account, &old_remote, &new_remote, &delim, &b.name, &new_local)?;
    done(app, &account);
    Ok(())
}

/// Deletes a folder and the mail in it, at the provider and here.
pub async fn delete(app: &Arc<App>, id: i64) -> Result<()> {
    let b = app.mail.mailbox(id).context("no such folder")?;
    let remote = own(&b)?.to_string();
    let account = b.account.clone().context("no account")?;
    if app.mail.mailboxes().iter().any(|c| c.account == b.account && c.name.starts_with(&format!("{}/", b.name))) {
        bail!("delete its subfolders first");
    }
    let (_, mut s) = session(app, &account).await?;
    s.delete(&remote).await.context("the provider refused to delete it")?;
    let _ = s.unsubscribe(&remote).await;
    let _ = s.logout().await;
    app.mail.prune_mailboxes(
        &account,
        &app.mail.mailboxes().into_iter().filter(|m| m.account.as_deref() == Some(account.as_str()) && m.id != id).filter_map(|m| m.remote).collect::<Vec<_>>(),
    )?;
    done(app, &account);
    Ok(())
}

/// The folder of `account` that a tree node gathers. If it has none, one is
/// made at the provider and joins the node: inside the folder its parent
/// node gathers in that account, else named after the node's path (parents
/// made first, and joining their nodes if those have none either).
pub async fn member_in(app: &Arc<App>, node: i64, account: &str) -> Result<i64> {
    if let Some(m) = app.mail.member_of(node, account) {
        return Ok(m);
    }
    let acc = app.account(account).context("no such account")?;
    let tree = app.mail.tree();
    let mut chain = Vec::new();
    let mut at = Some(node);
    while let Some(id) = at {
        let n = tree.iter().find(|n| n.id == id).context("that's not a folder of your tree")?;
        chain.push(n.clone());
        at = n.parent;
    }
    chain.reverse();
    let mut parent: Option<Mailbox> = None;
    for n in &chain {
        let folder = match app.mail.member_of(n.id, account) {
            Some(m) => m,
            None => {
                let local = match &parent {
                    Some(p) => format!("{}/{}", p.name, n.label),
                    None => account_path(&acc.name, &n.label),
                };
                let existing = app.mail.mailboxes().into_iter().find(|b| b.account.as_deref() == Some(account) && b.name == local);
                let id = match existing {
                    Some(b) => b.id,
                    None => create(app, account, parent.as_ref().map(|p| p.id), &n.label).await?,
                };
                app.mail.add_member(n.id, id)?;
                id
            }
        };
        parent = Some(app.mail.mailbox(folder).context("no such folder")?);
    }
    parent.map(|p| p.id).context("no such folder")
}
