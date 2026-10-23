"""The webmail with the tree, against GreenMail."""

from common import (WEB, check, flash, gm_folders, gm_imap, gm_subjects, members_on_page, message, node_id, q, rows,
                    subjects, valet_imap, wait)


def status(m, name):
    typ, data = m.status(q(name), "(MESSAGES)")
    return int(data[0].split(b"MESSAGES ")[1].rstrip(b")")) if typ == "OK" else -1


def main():
    from common import web
    m = valet_imap()
    wait(lambda: status(m, "Spam") == 2 and status(m, "INBOX") == 4, "both accounts synced into the tree")
    s = web()
    inbox, spam = node_id(s, "INBOX"), node_id(s, "Spam")

    side = s.get(f"{WEB}/box/{inbox}").text
    check('<details class="accounts" id="accounts">' in side, "Accounts is closed by default")
    check('<optgroup label="Accounts › Org">' in side and "Edit folders" in side, "the phone menu lists the tree, then each account")
    # Refresh: a message in a folder IDLE doesn't watch shows at once.
    check('class="refresh"' in side, "the list has a Refresh button")
    g = gm_imap("org")
    g.append("Projects", None, None, message("fresh-one", to="org@local.test"))
    g.logout()
    r = s.post(f"{WEB}/mail/refresh")
    check(r.ok and r.json()["done"], f"Refresh waits for every account's pass: {r.text}")
    check("fresh-one" in rows(s, node_id(s, "Projects")), "the new message is listed when it answers")
    in_box = rows(s, inbox)
    view = s.get(f"{WEB}/msg/{in_box['hello-1']}").text
    check('<optgroup label="Your folders">' in view and '<optgroup label="Accounts › Test">' in view, "Move to… offers the tree, then the account's folders")

    # Moving into a node: the message's own account's folder there.
    r = s.post(f"{WEB}/msg/{in_box['hello-1']}", data={"action": "move", "to": str(spam), "back": str(inbox)})
    check(flash(r)[0] == "Moved.", "move hello-1 into Spam")
    wait(lambda: "hello-1" in gm_subjects("test", "Spam"), "it is in Test's Spam at GreenMail")

    # Into a new, empty node: the folder is made in the message's account.
    s.post(f"{WEB}/tree", data={"name": "Receipts", "parent": ""})
    receipts = node_id(s, "Receipts")
    r = s.post(f"{WEB}/msg/{in_box['hello-2']}", data={"action": "move", "to": str(receipts), "back": str(inbox)})
    check(flash(r)[0] == "Moved.", "move hello-2 into the new Receipts")
    check("Receipts" in gm_folders("test") and "Receipts" not in gm_folders("org"), "Receipts made in Test only")
    wait(lambda: "hello-2" in gm_subjects("test", "Receipts"), "with the message, at GreenMail")
    check("hello-2" in rows(s, receipts), "the node lists it")

    real = members_on_page(s, receipts)
    r = s.post(f"{WEB}/msg/{in_box['hello-3']}", data={"action": "move", "to": str(real[("Org", "Projects")]), "back": str(inbox)})
    check(flash(r)[1], "no moving to another account's folder")

    # "Make it in every account".
    r = s.post(f"{WEB}/tree/{receipts}", data={"action": "create_all"})
    check(flash(r)[0] == "Made in Org." and "Receipts" in gm_folders("org"), "Make it in every account: Org's Receipts")
    check(members_on_page(s, receipts) and "Make it in every account" not in s.get(f"{WEB}/tree?node={receipts}").text, "nothing left to make")

    # Deleting from a node: to the message's account's Trash (Test has one, Org none).
    in_spam = rows(s, spam)
    r = s.post(f"{WEB}/msg/{in_spam['spam-test']}", data={"action": "delete", "back": str(spam)})
    check(flash(r)[0] == "Moved to the Trash.", "delete spam-test from the Spam node")
    wait(lambda: "spam-test" in gm_subjects("test", "Trash"), "it is in Test's Trash at GreenMail")
    r = s.post(f"{WEB}/msg/{in_spam['spam-org']}", data={"action": "delete", "back": str(spam)})
    check(flash(r)[0] == "Deleted.", "Org has no Trash: deleted for good")
    wait(lambda: "spam-org" not in gm_subjects("org", "Spam"), "gone from Org's Spam")

    # Drafts go to the From account's Drafts, and show in the Drafts node.
    form = {"from": (None, "org@local.test"), "to": (None, "x@y.z"), "subject": (None, "draft-1"), "html": (None, "<p>hi</p>")}
    r = s.post(f"{WEB}/compose/draft", files=form)
    check(r.ok and r.json()["draft"], "save a draft from org@local.test")
    wait(lambda: "draft-1" in gm_subjects("org", "Drafts"), "it is in Org's Drafts at GreenMail")
    check("draft-1" in subjects(m, "Drafts") and "draft-1" in rows(s, node_id(s, "Drafts")), "and in the Drafts node")

    # A folder made at the provider waits to be added.
    g = gm_imap("test")
    g.create("Later")
    g.append("INBOX", None, None, message("wake-up"))  # IDLE on the inbox: valet looks again
    g.logout()
    wait(lambda: "Test › Later" in s.get(f"{WEB}/tree").text, "Later shows among the folders not in the tree")
    r = s.post(f"{WEB}/tree/add/{members_on_page(s, receipts)[('Test', 'Later')]}")
    check(flash(r)[0] == "Added." and node_id(s, "Later"), "Add to the tree")
    m.logout()


main()
