"""Mail apps' view of the tree, over IMAP.

  mail_e2e.py seed    folders and mail at GreenMail, before valet starts
  mail_e2e.py check   valet's IMAP against what GreenMail ends up with
"""

import sys

from common import check, gm_folders, gm_imap, gm_subjects, listing, message, q, subjects, valet_imap, wait

SEED = {
    "test": {"INBOX": ["hello-1", "hello-2", "hello-3"], "Spam": ["spam-test"], "Sent": [], "Trash": [], "Drafts": [], "Projects": ["proj-test"]},
    "org": {"INBOX": ["org-hello"], "Spam": ["spam-org"], "Sent": [], "Drafts": [], "Projects": ["proj-org"]},
}


def seed():
    for user, folders in SEED.items():
        m = gm_imap(user)
        for folder, subjects_ in folders.items():
            if folder != "INBOX":
                m.create(folder)
            for s in subjects_:
                typ, _ = m.append(folder, None, None, message(s, to=f"{user}@local.test"))
                assert typ == "OK"
        m.logout()
    print("ok: seeded GreenMail")


def status(m, name):
    typ, data = m.status(q(name), "(MESSAGES)")
    return int(data[0].split(b"MESSAGES ")[1].rstrip(b")")) if typ == "OK" else -1


def run():
    m = valet_imap()
    wait(lambda: status(m, "Spam") == 2 and status(m, "Projects") == 2 and status(m, "INBOX") == 4, "both accounts synced into the tree")

    names = listing(m)
    order = list(names)
    check(order[0] == "INBOX", "INBOX comes first")
    for name, flag in [("Drafts", "\\Drafts"), ("Sent", "\\Sent"), ("Spam", "\\Junk"), ("Trash", "\\Trash")]:
        check(flag in names.get(name, []), f"{name} is listed with {flag}")
    check("Projects" in names and not any(f.startswith("\\") and f not in ("\\HasChildren", "\\HasNoChildren") for f in names["Projects"]), "Projects is a plain folder")
    check("\\Noselect" in names["Accounts"] and "\\Noselect" in names["Accounts/Test"], "Accounts and Accounts/Test are only parents")
    real = {n: f for n, f in names.items() if n.startswith("Accounts/")}
    check({"Accounts/Test/Spam", "Accounts/Test/Sent", "Accounts/Org/Spam", "Accounts/Test/Inbox"} <= set(real), "real folders are under Accounts")
    check(all(not ({"\\Sent", "\\Junk", "\\Trash", "\\Drafts", "\\Archive"} & set(f)) for f in real.values()), "real folders carry no special-use flag")
    check(order.index("Projects") < order.index("Accounts"), "the tree comes before Accounts")

    check(set(subjects(m, "Spam")) == {"spam-test", "spam-org"}, "Spam shows both accounts' spam")

    # All mail: every account folder's mail, Spam and Trash too; nothing filed into it.
    check("\\All" in names.get("All mail", []) and order.index("Sent") < order.index("All mail") < order.index("Spam"), "All mail is listed with \\All, after Sent, before Spam")
    everything = {"hello-1", "hello-2", "hello-3", "spam-test", "proj-test", "org-hello", "spam-org", "proj-org"}
    check(set(subjects(m, "All mail")) == everything, "All mail has every folder's mail")
    check(m.uid("MOVE", str(subjects(m, "INBOX")["hello-3"]), q("All mail"))[0] == "NO", "MOVE into All mail is refused")
    check(m.append(q("All mail"), None, None, message("into-all", frm="Org <org@local.test>"))[0] == "NO", "and APPEND")
    check("hello-3" in subjects(m, "INBOX") and len(subjects(m, "All mail")) == 8, "nothing moved")
    check(set(subjects(m, "Projects")) == {"proj-test", "proj-org"}, "Projects gathers the same path of both accounts")

    # MOVE into a node: the message's own account's folder.
    inbox = subjects(m, "INBOX")
    typ, _ = m.uid("MOVE", str(inbox["hello-1"]), "Spam")
    check(typ == "OK", "MOVE into Spam")
    wait(lambda: "hello-1" in gm_subjects("test", "Spam") and "hello-1" not in gm_subjects("test", "INBOX"), "it is in Test's Spam at GreenMail")
    check("hello-1" not in gm_subjects("org", "Spam"), "and not in Org's")
    check("hello-1" in subjects(m, "Spam"), "and in the Spam node")
    check(len(subjects(m, "All mail")) == 8 and "hello-1" in subjects(m, "All mail"), "still once in All mail")

    # APPEND into a node: the account its From is.
    typ, data = m.append("Sent", "(\\Seen)", None, message("sent-by-org", frm="Org <org@local.test>", to="x@y.z"))
    check(typ == "OK" and b"APPENDUID" in data[0], "APPEND into Sent")
    wait(lambda: "sent-by-org" in gm_subjects("org", "Sent"), "it is in Org's Sent at GreenMail")
    check("sent-by-org" not in gm_subjects("test", "Sent"), "and not in Test's")
    typ, data = m.append("Sent", None, None, message("from-nobody", frm="nobody@else.org"))
    check(typ == "NO", "APPEND from an address no account sends as is refused")

    # A new node: mail moved into it makes the folder at the provider.
    check(m.create("Clients/Acme")[0] == "OK", "CREATE Clients/Acme")
    names = listing(m)
    check("Clients" in names and "Clients/Acme" in names and "\\HasChildren" in names["Clients"], "both levels are listed")
    check(status(m, "Clients/Acme") == 0, "it starts empty")
    inbox = subjects(m, "INBOX")
    check(m.uid("MOVE", str(inbox["hello-2"]), q("Clients/Acme"))[0] == "OK", "MOVE into Clients/Acme")
    wait(lambda: "hello-2" in gm_subjects("test", "Clients.Acme"), "GreenMail's Test got Clients.Acme, with the message")
    check("Clients" in gm_folders("test") and "Clients.Acme" not in gm_folders("org"), "parent made too, nothing in Org")
    check("hello-2" in subjects(m, "Clients/Acme"), "the node shows it")

    # Nodes: renamed, moved, deleted in the tree only.
    check(m.rename("Clients", "Customers")[0] == "OK", "RENAME Clients Customers")
    names = listing(m)
    check("Customers/Acme" in names and "Clients/Acme" not in names, "its child follows")
    check("hello-2" in subjects(m, "Customers/Acme"), "with its mail")
    check("Clients.Acme" in gm_folders("test"), "the provider's folders stay as they are")
    check(m.rename("Customers/Acme", "Acme")[0] == "OK", "RENAME moves a node to the top")
    check(m.delete("Customers")[0] == "OK", "DELETE of an empty node")
    m.create("A/B")
    check(m.delete("A")[0] == "NO", "DELETE of a node with children is refused")
    check(m.delete("A/B")[0] == "OK" and m.delete("A")[0] == "OK", "children first, then it")
    check(m.delete("Acme")[0] == "OK", "DELETE Acme")
    check("hello-2" in gm_subjects("test", "Clients.Acme"), "the mail stays in the account's folder")
    names = listing(m)
    check(not {"A", "Acme", "Customers"} & set(names) and "Accounts/Test/Clients/Acme" in names, "gone from the tree, still under Accounts")

    check(m.create("Accounts")[0] == "NO" and m.create("INBOX")[0] == "NO", "Accounts and INBOX are taken")
    check(m.rename("Spam", "Accounts/Spam")[0] == "NO", "nodes don't go under Accounts")

    # Real folders as before, under Accounts.
    check(m.create("Accounts/Org/Real")[0] == "OK", "CREATE Accounts/Org/Real")
    wait(lambda: "Real" in gm_folders("org"), "made at GreenMail")
    check("Real" not in listing(m), "not put in the tree by itself")

    # Flags and deletions through a node reach the provider.
    spam = subjects(m, "Spam")
    m.uid("STORE", str(spam["spam-org"]), "+FLAGS", "(\\Flagged)")

    def org_flagged():
        g = gm_imap("org")
        g.select("Spam")
        typ, data = g.uid("SEARCH", None, "FLAGGED")
        g.logout()
        return data[0].split()

    wait(org_flagged, "a flag set in the Spam node reaches Org's Spam")
    m.uid("STORE", str(spam["spam-test"]), "+FLAGS.SILENT", "(\\Deleted)")
    m.expunge()
    wait(lambda: "spam-test" not in gm_subjects("test", "Spam"), "an expunge in the node reaches Test's Spam")
    check("spam-test" not in subjects(m, "Spam") and "spam-org" in subjects(m, "Spam"), "the node follows")
    m.logout()


if __name__ == "__main__":
    {"seed": seed, "check": run}[sys.argv[1]]()
