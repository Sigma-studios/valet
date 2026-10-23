"""The tree editor's routes, on the data run_mail_e2e.sh left (no GreenMail:
nothing here reaches a provider)."""

import re

from common import WEB, check, flash, listing, members_on_page, node_id, q, tree_rows, valet_imap, web


def status(m, name):
    typ, data = m.status(q(name), "(MESSAGES)")
    return int(data[0].split(b"MESSAGES ")[1].rstrip(b")")) if typ == "OK" else -1


def main():
    s = web()
    m = valet_imap()
    labels = [l for _, p, l in tree_rows(s) if p is None]
    check(labels == ["INBOX", "Drafts", "Sent", "Spam", "Trash", "Projects"], f"the tree run_mail_e2e left: {labels}")
    page = s.get(f"{WEB}/tree").text
    check("Folders not in your tree" in page and "Org › Real" in page, "folders made later wait to be added")
    inbox = node_id(s, "INBOX")
    side = s.get(f"{WEB}/box/{inbox}").text
    check('id="accounts"' in side and "Edit folders" in side and "Accounts › Test" in side, "the sidebar has the tree, Edit folders, and Accounts")
    spam = s.get(f"{WEB}/box/{node_id(s, 'Spam')}").text
    check('<span class="tag">Org</span>' in spam, "a node's list tells each message's account")

    r = s.post(f"{WEB}/tree", data={"name": "Work", "parent": ""})
    work = node_id(s, "Work")
    check(work and flash(r)[0] == "Created Work.", "create Work")
    s.post(f"{WEB}/tree", data={"name": "Clients", "parent": str(work)})
    clients = node_id(s, "Clients", work)
    check(clients is not None, "create Clients inside Work")
    for name, why in [("Work", "same name, same place"), ("Accounts", "reserved"), ("a/b", "no /")]:
        r = s.post(f"{WEB}/tree", data={"name": name, "parent": ""})
        check(flash(r)[1], f"refused: {name} ({why}): {flash(r)[1]}")

    r = s.post(f"{WEB}/tree/{clients}", data={"action": "rename", "label": "Customers"})
    check(node_id(s, "Customers", work) == clients and "Work/Customers" in listing(m), "rename, and mail apps see it")

    have = members_on_page(s, clients)
    pick = [have[("Test", "Clients/Acme")], have[("Org", "Projects")]]
    r = s.post(f"{WEB}/tree/{clients}", data=[("action", "members")] + [("member", str(i)) for i in pick])
    check(flash(r)[0] == "Saved." and status(m, "Work/Customers") == 2, "members: it gathers Test's Clients/Acme and Org's Projects")
    r = s.post(f"{WEB}/tree/{clients}", data=[("action", "members"), ("member", str(pick[1]))])
    check(status(m, "Work/Customers") == 1, "one taken out")

    s.post(f"{WEB}/tree/{work}", data={"action": "special", "special": "\\Archive"})
    check("\\Archive" in listing(m)["Work"], "Work is the Archive folder for mail apps")
    s.post(f"{WEB}/tree/{clients}", data={"action": "special", "special": "\\Archive"})
    names = listing(m)
    check("\\Archive" in names["Work/Customers"] and "\\Archive" not in names["Work"], "each special use goes to one folder")

    def shape():
        return [(l, p) for _, p, l in tree_rows(s)]

    s.post(f"{WEB}/tree/{clients}", data={"action": "outdent"})
    check(("Customers", None) in shape() and shape().index(("Customers", None)) == shape().index(("Work", None)) + 1, "outdent: right after its parent")
    s.post(f"{WEB}/tree/{clients}", data={"action": "up"})
    check(shape().index(("Customers", None)) + 1 == shape().index(("Work", None)), "up")
    s.post(f"{WEB}/tree/{clients}", data={"action": "down"})
    s.post(f"{WEB}/tree/{clients}", data={"action": "indent"})
    check(("Customers", work) in shape(), "down, then indent: back inside Work")
    r = s.post(f"{WEB}/tree/{inbox}", data={"action": "up"})
    check(flash(r)[1], "INBOX can't go up")

    # Drag and drop sends the whole tree.
    rows = tree_rows(s)
    projects = node_id(s, "Projects")
    bad = [{"id": i, "parent": (clients if i == work else p), "label": l} for i, p, l in rows]
    r = s.post(f"{WEB}/tree/layout", json=bad)
    check(r.status_code == 422 and "inside itself" in r.json()["error"], "a cycle is refused")
    good = [{"id": i, "parent": (projects if i == work else p), "label": l} for i, p, l in rows]
    r = s.post(f"{WEB}/tree/layout", json=good)
    check(r.ok and "Projects/Work/Customers" in listing(m), "Work dropped into Projects")
    r = s.post(f"{WEB}/tree/layout", json=good[:-1])
    check(r.status_code == 422, "a partial tree is refused")

    r = s.post(f"{WEB}/tree/{work}", data={"action": "delete"})
    check(flash(r)[1], "a folder with folders inside isn't deleted")
    r = s.post(f"{WEB}/tree/{clients}", data={"action": "delete"})
    check(node_id(s, "Customers", work) is None and status(m, "Accounts/Org/Projects") == 1, "delete: gone, its mail stays in the account")

    page = s.get(f"{WEB}/tree").text
    real = re.findall(r'<span style="flex:1">([^<]+)</span><button class="step" type="submit">Add to the tree</button>', page)
    adds = re.findall(r'action="/tree/add/(\d+)"', page)
    real_id = adds[real.index("Org › Real")]
    r = s.post(f"{WEB}/tree/add/{real_id}")
    check(node_id(s, "Real") and f'action="/tree/add/{real_id}"' not in s.get(f"{WEB}/tree").text, "Add to the tree")

    r = s.post(f"{WEB}/tree/reset")
    labels = [(l, p is None) for _, p, l in tree_rows(s)]
    check(flash(r)[0] and ("Work", True) not in labels and ("Acme", False) in labels and ("Real", True) in labels, f"reset to the default tree: {labels}")
    m.logout()


main()
