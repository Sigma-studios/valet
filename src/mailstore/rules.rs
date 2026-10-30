//! Mail rules: new inbox mail from a sender goes to a folder of the tree as it
//! comes in (see `crate::sync`). A rule whose folder was deleted waits for
//! another, and files nothing meanwhile.

use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, params};

use super::MailStore;

#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    pub id: i64,
    /// `someone@acme.com`, or `@acme.com` for a domain and its subdomains.
    pub sender: String,
    /// The tree node it files into.
    pub node: Option<i64>,
}

/// What was typed as a rule's sender, as kept: an address (`Name <a@b.c>` will
/// do) or a domain (`@b.c`, `b.c`), in lower case.
fn sender_key(typed: &str) -> Result<String> {
    let list = crate::mime::addresses(typed);
    let [(_, mailbox, host)] = list.as_slice() else {
        anyhow::bail!("a rule takes one address or domain");
    };
    let key = if host.is_empty() {
        ensure!(mailbox.contains('.'), "{mailbox} is neither an address nor a domain");
        format!("@{mailbox}")
    } else {
        format!("{mailbox}@{host}")
    };
    ensure!(!key.contains(char::is_whitespace), "an address or domain has no spaces");
    Ok(key.to_lowercase())
}

/// The rule for this sender: its address's first, else that of the closest domain.
fn closest<'a>(rules: &'a [Rule], mailbox: &str, host: &str) -> Option<&'a Rule> {
    let host = host.to_lowercase();
    let address = format!("{}@{host}", mailbox.to_lowercase());
    let applies = |r: &&Rule| match r.sender.strip_prefix('@') {
        Some(domain) => host == domain || host.ends_with(&format!(".{domain}")),
        None => r.sender == address,
    };
    rules.iter().filter(applies).max_by_key(|r| (!r.sender.starts_with('@'), r.sender.len()))
}

impl MailStore {
    pub fn rules(&self) -> Vec<Rule> {
        let c = self.db();
        let mut st = c.prepare("SELECT id, sender, node FROM rules ORDER BY sender").unwrap();
        st.query_map([], |r| Ok(Rule { id: r.get(0)?, sender: r.get(1)?, node: r.get(2)? })).unwrap().filter_map(|r| r.ok()).collect()
    }

    fn check_rule(&self, id: Option<i64>, sender: &str, node: i64) -> Result<()> {
        ensure!(self.tree().iter().any(|n| n.id == node), "no such folder");
        ensure!(node != self.all_inboxes(), "new mail is in the inbox already");
        ensure!(!self.mailbox(node).context("no such folder")?.is_all_mail(), "{}", super::ALL_MAIL_ISNT_A_PLACE);
        let taken: Option<i64> = self.db().query_row("SELECT id FROM rules WHERE sender = ?1", [sender], |r| r.get(0)).optional()?;
        ensure!(taken.is_none() || taken == id, "{sender} has a rule already");
        Ok(())
    }

    pub fn add_rule(&self, sender: &str, node: i64) -> Result<i64> {
        let sender = sender_key(sender)?;
        self.check_rule(None, &sender, node)?;
        let c = self.db();
        c.execute("INSERT INTO rules (sender, node) VALUES (?1, ?2)", params![sender, node])?;
        Ok(c.last_insert_rowid())
    }

    pub fn set_rule(&self, id: i64, sender: &str, node: i64) -> Result<()> {
        let sender = sender_key(sender)?;
        self.check_rule(Some(id), &sender, node)?;
        ensure!(self.db().execute("UPDATE rules SET sender = ?2, node = ?3 WHERE id = ?1", params![id, sender, node])? == 1, "no such rule");
        Ok(())
    }

    pub fn delete_rule(&self, id: i64) -> Result<()> {
        self.db().execute("DELETE FROM rules WHERE id = ?1", [id])?;
        Ok(())
    }

    /// The node this message's rule files it into, if any: by its From address.
    pub fn rule_for(&self, raw: &[u8]) -> Option<i64> {
        let p = crate::mime::parse(raw);
        let (_, mailbox, host) = crate::mime::addresses(&p.header(raw, "From")?).into_iter().next()?;
        closest(&self.rules(), &mailbox, &host)?.node
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn from(sender: &str) -> Vec<u8> {
        format!("From: {sender}\r\nMessage-ID: <1@x>\r\nSubject: s\r\n\r\nbody\r\n").into_bytes()
    }

    #[test]
    fn senders_are_kept_alike() {
        assert_eq!(sender_key(" Zoé <Zoe@Acme.COM> ").unwrap(), "zoe@acme.com");
        assert_eq!(sender_key("@Acme.com").unwrap(), "@acme.com");
        assert_eq!(sender_key("acme.com").unwrap(), "@acme.com");
        for wrong in ["", "acme", "a@b.c, d@e.f", "a b@c.d"] {
            assert!(sender_key(wrong).is_err(), "{wrong}");
        }
    }

    #[test]
    fn the_closest_rule_files() {
        let s = MailStore::memory();
        let (news, zoe, sub) = (s.create_node(None, "News").unwrap(), s.create_node(None, "Zoe").unwrap(), s.create_node(None, "Sub").unwrap());
        s.add_rule("@acme.com", news).unwrap();
        s.add_rule("zoe@acme.com", zoe).unwrap();
        s.add_rule("@mail.acme.com", sub).unwrap();
        assert_eq!(s.rule_for(&from("Zoe <ZOE@acme.com>")), Some(zoe), "an address before its domain");
        assert_eq!(s.rule_for(&from("info@acme.com")), Some(news));
        assert_eq!(s.rule_for(&from("info@eu.acme.com")), Some(news), "subdomains too");
        assert_eq!(s.rule_for(&from("info@eu.mail.acme.com")), Some(sub), "the closest domain");
        assert_eq!(s.rule_for(&from("info@notacme.com")), None);
        assert_eq!(s.rule_for(b"Subject: nobody\r\n\r\nbody\r\n"), None);
    }

    #[test]
    fn a_rule_needs_a_folder_mail_can_go_to() {
        let s = MailStore::memory();
        let node = s.create_node(None, "News").unwrap();
        assert!(s.add_rule("a@b.c", s.all_inboxes()).is_err());
        assert!(s.add_rule("a@b.c", 99_999).is_err());
        let id = s.add_rule("a@b.c", node).unwrap();
        assert!(s.add_rule("A@B.C", node).unwrap_err().to_string().contains("already"));
        let other = s.add_rule("d@e.f", node).unwrap();
        assert!(s.set_rule(other, "a@b.c", node).is_err());
        s.set_rule(id, "a@b.c", node).unwrap();
        s.delete_rule(other).unwrap();
        assert_eq!(s.rules().len(), 1);
    }

    #[test]
    fn a_deleted_folder_leaves_its_rules_waiting() {
        let s = MailStore::memory();
        let other = s.create_node(None, "Other").unwrap();
        let news = s.create_node(None, "News").unwrap();
        s.add_rule("a@b.c", news).unwrap();
        s.add_rule("d@e.f", other).unwrap();
        s.delete_node(news).unwrap();
        // Not whichever folder comes next, even with the same id.
        assert_eq!(s.create_node(None, "New").unwrap(), news);
        assert_eq!(s.rule_for(&from("a@b.c")), None);
        assert_eq!(s.rule_for(&from("d@e.f")), Some(other));
        assert_eq!(s.rules().iter().filter(|r| r.node.is_none()).map(|r| r.sender.as_str()).collect::<Vec<_>>(), ["a@b.c"]);
    }

    #[test]
    fn resetting_the_tree_keeps_the_rules_of_folders_it_has_again() {
        let s = MailStore::memory();
        s.put_account(&serde_json::from_str(r#"{"id":"g","name":"Gmail","address":"me@gmail.com","imap":"i:993","smtp":"s:465","password":"p"}"#).unwrap()).unwrap();
        s.remote_mailbox("g", "Receipts", &crate::mailstore::account_path("Gmail", "Receipts"), "").unwrap();
        s.place_account("g").unwrap();
        let receipts = s.tree().into_iter().find(|n| n.label == "Receipts").unwrap().id;
        let mine = s.create_node(None, "Mine").unwrap();
        s.add_rule("a@b.c", receipts).unwrap();
        s.add_rule("d@e.f", mine).unwrap();
        s.reset_tree().unwrap();
        let again = s.tree().into_iter().find(|n| n.label == "Receipts").unwrap().id;
        assert_eq!(s.rule_for(&from("a@b.c")), Some(again));
        assert_eq!(s.rule_for(&from("d@e.f")), None, "the default tree has no Mine");
    }
}
