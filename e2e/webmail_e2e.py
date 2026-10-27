"""The webmail with the tree, against GreenMail; with a scratch directory
given and Chrome there, the list's Filter menu in a headless Chrome.

  webmail_e2e.py [<dir>]"""

import email.utils
import html
import imaplib
import os
import re
import sys
import time
from datetime import datetime, timezone

import requests

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
    # From the folder shown: it is synced right after the inboxes, and told.
    g = gm_imap("org")
    g.append("Projects", None, None, message("fresh-two", to="org@local.test"))
    g.logout()
    r = s.post(f"{WEB}/mail/refresh", params={"folder": node_id(s, "Projects")})
    check(r.ok and r.json() == {"done": True, "failed": [], "waiting": []}, f"Refresh of a folder answers: {r.text}")
    check("fresh-two" in rows(s, node_id(s, "Projects")), "the folder shown is in sync when it answers")
    check('data-folder="' in s.get(f"{WEB}/box/{inbox}").text, "the button names the folder shown")
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

    # Reading beside the list.
    g = gm_imap("test")
    for i in range(3):
        g.append("INBOX", None, None, message(f"bulk-{i}"))
    g.logout()
    s.post(f"{WEB}/mail/refresh")
    in_box = rows(s, inbox)
    b0, b1, b2 = in_box["bulk-0"], in_box["bulk-1"], in_box["bulk-2"]
    listed = f"/box/{inbox}?page=0"
    page = s.get(f"{WEB}{listed}&open={b0}").text
    check('class="readcol"' in page and "<h1>bulk-0</h1>" in page and 'value="archive"' in page, "a message opens beside the list, with Archive among its actions")
    check(re.search(rf'<div class="row open">\s*<label class="pick"><input type="checkbox" name="id" value="{b0}"', page), "its row is marked open, and read")
    r = s.post(f"{WEB}/msg/{b0}", data={"action": "flag", "back": str(inbox), "list": listed}, allow_redirects=False)
    check(r.headers["location"] == f"{listed}&open={b0}", "an action there comes back to the list, the message still open")
    r = s.post(f"{WEB}/msg/{b0}", data={"action": "flag", "list": "https://evil.example/box/1"}, allow_redirects=False)
    check(r.headers["location"].startswith("/msg/"), "a list address that isn't valet's is ignored")

    # Several at once.
    bulk = lambda action, *ids: s.post(f"{WEB}/box/{inbox}/bulk", data=[("back", listed), ("action", action)] + [("id", str(i)) for i in ids])
    check(flash(bulk("unseen", b0, b1))[0] == "Marked 2 messages unread.", "mark two unread")
    check(flash(bulk("seen", b0, b1))[0] == "Marked 2 messages read.", "and read again")
    check(flash(bulk("archive", b0, b1))[0] == "Archived 2 messages.", "archive two")
    check(node_id(s, "Archive") and "Archive" in gm_folders("test"), "Test had no Archive: one is made, in the tree and at the provider")
    wait(lambda: {"bulk-0", "bulk-1"} <= set(gm_subjects("test", "Archive")), "they are in Test's Archive at GreenMail")
    check("bulk-0" not in rows(s, inbox) and "bulk-0" in rows(s, node_id(s, "Archive")), "out of the inbox, in the Archive folder")
    check(flash(bulk("delete", b2))[0] == "Moved 1 message to the Trash.", "delete one")
    wait(lambda: "bulk-2" in gm_subjects("test", "Trash"), "it is in Test's Trash at GreenMail")
    check(flash(bulk("archive"))[1] == "No message selected.", "nothing ticked: said so")
    # Fetched ahead by the list's script: the message's part only, not marked read; then marked read.
    g = gm_imap("test")
    g.append("INBOX", None, None, message("ahead"))
    g.logout()
    s.post(f"{WEB}/mail/refresh")
    ahead = rows(s, inbox)["ahead"]
    unread = lambda: bool(re.search(rf'<div class="row unread">\s*<label class="pick"><input type="checkbox" name="id" value="{ahead}"', s.get(f"{WEB}{listed}").text))
    part = s.get(f"{WEB}{listed}&open={ahead}&fragment=1").text
    check(part.startswith('<div class="readcol" data-subject="ahead">') and "<h1>ahead</h1>" in part and 'class="folders"' not in part, "a message's part alone, for the list's script")
    check(unread(), "fetching it ahead doesn't mark it read")
    check(s.post(f"{WEB}/msg/{ahead}/seen").status_code == 204 and not unread(), "opening it does")
    page = s.get(f"{WEB}{listed}&open={in_box['wake-up']}").text
    check(re.search(rf'<div class="row open">.*?<a class="link" href="{re.escape(listed)}"', page.replace("&amp;", "&"), re.S), "the open message's row closes it")

    # An invitation ignored: no answer sent, off the calendar, not waiting.
    # Meanwhile, an event at the same time is named on it.
    r = s.post(f"{WEB}/calendar/event", data={"calendar": "", "title": "Dentist", "start_date": "2099-11-05", "start_time": "11:30",
                                              "end_date": "2099-11-05", "end_time": "12:30", "tz": "Europe/Paris", "repeat": "none"})
    check(r.ok, f"an event at the invitation's time: {r.text}")
    invite = (
        "From: Org <org@local.test>\r\nTo: test@local.test\r\nSubject: Invitation: Planning\r\nMessage-ID: <ignore-me@e2e>\r\nMIME-Version: 1.0\r\n"
        "Content-Type: text/calendar; method=REQUEST; charset=utf-8\r\n\r\n"
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:ignore-me@e2e\r\nDTSTART:20991105T100000Z\r\n"
        "DTEND:20991105T110000Z\r\nSUMMARY:Planning\r\nORGANIZER:mailto:org@local.test\r\n"
        "ATTENDEE;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:test@local.test\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
    ).encode()
    g = gm_imap("test")
    g.append("INBOX", None, None, invite)
    g.logout()
    wait(lambda: "Planning" in s.get(f"{WEB}/invitations").text, "the invitation is waiting")
    planning = lambda: [e for e in s.get(f"{WEB}/calendar/month/2099-11").json()["events"] if e["title"] == "Planning"]
    check(planning() and planning()[0]["calendar"] == "invitations", "in the Invitations calendar meanwhile")
    dentist = [e for e in s.get(f"{WEB}/calendar/month/2099-11").json()["events"] if e["title"] == "Dentist"]
    check(planning()[0]["unanswered"] and not dentist[0]["unanswered"], "told apart as not answered yet; my own event isn't")
    card = s.get(f"{WEB}/invitations").text
    check(re.search(r"Overlaps <b>Dentist</b> <time[^>]*>05/11/2099 11:30</time>–12:30 \(Calendar\)", card), "it says what it overlaps, not itself")
    inv = re.search(r'action="/invites/(\d+)"', s.get(f"{WEB}/invitations").text).group(1)
    sent_before = len(gm_subjects("org", "INBOX"))
    r = s.post(f"{WEB}/invites/{inv}", data={"answer": "IGNORED", "calendar": ""})
    check(flash(r)[0] == "Ignored: the organizer isn't told.", "Ignore")
    page = s.get(f"{WEB}/invitations").text
    check("Nothing waiting" in page and ">ignored<" not in page, "no longer waiting, so not listed at first")
    page = s.get(f"{WEB}/invitations?status=answered").text
    check(">ignored<" in page and "Planning" in page and ">Answered<" in page, "listed as ignored among the answered, a chip saying so")
    check(not planning(), "off the Invitations calendar")
    time.sleep(2)
    check(len(gm_subjects("org", "INBOX")) == sent_before, "no answer reached the organizer")

    # Another, accepted from its event's card in the calendar.
    g = gm_imap("test")
    g.append("INBOX", None, None, invite.replace(b"ignore-me@e2e", b"accept-me@e2e").replace(b"Planning", b"Retro"))
    g.logout()
    retro = lambda: [e for e in s.get(f"{WEB}/calendar/month/2099-11").json()["events"] if e["title"] == "Retro"]
    wait(lambda: retro(), "the second invitation is on the calendar")
    check(retro()[0]["unanswered"] and retro()[0].get("invite"), "with the invitation to answer it")
    mail = retro()[0].get("mail")
    check(mail and "Invitation: Retro" in s.get(f"{WEB}/msg/{mail}").text, "and the mail it came in, to open")
    check(f'href="/msg/{mail}"' in s.get(f"{WEB}/invitations").text, "the invitations page links that mail too")
    check("invite" not in dentist[0], "an event of my own has none")
    r = s.post(f"{WEB}/calendar/invite", data={"invite": retro()[0]["invite"], "answer": "MAYBE?", "calendar": "calendar"})
    check(r.status_code == 422 and r.json()["error"] == "not answered: unknown answer", "an unknown answer is refused, as JSON")
    r = s.post(f"{WEB}/calendar/invite", data={"invite": retro()[0]["invite"], "answer": "ACCEPTED", "calendar": "calendar"})
    check(r.ok and r.json() == {"ok": True, "waiting": 0}, f"accepted from the calendar, nothing left waiting: {r.text}")
    check(retro() and retro()[0]["calendar"] == "calendar" and not retro()[0]["unanswered"] and "invite" not in retro()[0], "moved to the calendar picked, answered")
    wait(lambda: "Accepted: Retro" in gm_subjects("org", "INBOX"), "the organizer gets the answer")
    check(re.search(r"Retro.*?answered: accepted", s.get(f"{WEB}/invitations?status=all").text, re.S), "the Invitations page knows")
    # Its search and filters.
    titles = lambda **q: set(re.findall(r'<div class="card"><div[^>]*><div><b>([^<]*)</b>', s.get(f"{WEB}/invitations", params=q).text))
    check(titles(status="all") == {"Planning", "Retro"}, "all of them")
    check(titles(status="all", q="retro") == {"Retro"}, "searched by title")
    check(titles(status="all", q="org@local") == {"Planning", "Retro"} and not titles(status="all", q="nobody-at-all"), "searched by organizer")
    check(titles(status="all", **{"from": "org@"}) == {"Planning", "Retro"} and not titles(status="all", **{"from": "else@"}), "filtered by organizer")
    check(not titles(status="all", when="past"), "none past")
    check(not titles(status="weird") and "No invitation matches" not in s.get(f"{WEB}/invitations", params={"status": "weird"}).text, "an unknown status: the default")
    page = s.get(f"{WEB}/invitations", params={"status": "all", "q": "retro"}).text
    check('name="back" value="/invitations?q=retro&amp;status=all"' in page, "an answer comes back to the list as filtered")
    r = s.post(f"{WEB}/invites/{inv}", data={"answer": "IGNORED", "back": "/invitations?q=planning&status=all"}, allow_redirects=False)
    check(r.headers["location"].startswith("/invitations?q=planning&status=all&ok="), f"and does: {r.headers['location']}")
    r = s.post(f"{WEB}/invites/{inv}", data={"answer": "IGNORED", "back": "https://elsewhere.example/"}, allow_redirects=False)
    check(r.headers["location"].startswith("/invitations?ok="), "not elsewhere")
    m.logout()

    # Open pages are told of changes, to fetch their list again.
    check(requests.get(f"{WEB}/mail/events", timeout=10).status_code == 401, "changes are told only when logged in")
    ev = s.get(f"{WEB}/mail/events", stream=True, timeout=10)
    check(ev.headers["content-type"].startswith("text/event-stream"), "as server-sent events")
    g = gm_imap("test")
    g.append("INBOX", None, None, message("told-live"))
    g.logout()
    told = next((l for l in ev.iter_lines(chunk_size=1, decode_unicode=True) if l == "event: changed"), None)
    ev.close()
    check(told, "mail come in is told")

    filters(s, inbox)
    if len(sys.argv) > 1:
        from cdp import CHROME
        if os.access(CHROME, os.X_OK):
            filter_menu(s, inbox, sys.argv[1])
            invitation_filters(sys.argv[1])
        else:
            print("(no Chrome: the Filter menu's browser checks skipped)")
    all_mail(s, inbox)
    retiring(s, inbox)


def all_mail(s, inbox):
    """All mail: every folder's mail, once; Spam and Trash told, and hidden on
    demand; Not spam; no unread count; nothing moved into it."""
    every = node_id(s, "All mail")
    check(every, "the tree has All mail")
    side = s.get(f"{WEB}/box/{inbox}").text
    link = re.search(rf'<a class="folder[^"]*" href="/box/{every}"[^>]*>(.*?)</a>', side, re.S).group(1)
    check('class="badge"' in side and 'class="badge"' not in link, "no unread count on All mail (the inbox has one)")

    def listed(**p):
        """{subject: its row's HTML} of All mail."""
        page = s.get(f"{WEB}/box/{every}", params=p).text
        found = (re.search(r'<span class="subject">([^<]*)</span>', c) for c in page.split('<div class="row')[1:])
        return {html.unescape(f.group(1)): f.string for f in found if f}

    rows_ = listed()
    check({"hello-1", "spam-test", "bulk-0", "bulk-2", "org-hello", "ahead"} <= set(rows_), f"every folder's mail, both accounts': {sorted(rows_)}")
    check('<span class="tag bin">Spam</span>' in rows_["hello-1"] and '<span class="tag bin">Trash</span>' in rows_["spam-test"], "Spam and Trash told on their rows")
    check('class="tag bin"' not in rows_["bulk-0"] and '<span class="tag">Test</span>' in rows_["bulk-0"], "the others have their account only")
    hidden = listed(hide="1")
    check("hello-1" not in hidden and "spam-test" not in hidden and "bulk-0" in hidden, "Hide Spam and Trash")
    page = s.get(f"{WEB}/box/{every}", params={"hide": "1"}).text
    check("Without Spam and Trash" in page and 'name="hide" value="1" checked' in page, "with its chip, ticked in the menu")
    check('name="hide"' not in s.get(f"{WEB}/box/{inbox}").text, "offered in All mail only")
    check(set(listed(q="spam-test")) == {"spam-test"}, "the search goes through everything, the Trash too")

    # A message in two folders of an account shows once.
    twice = message("twice")
    g = gm_imap("test")
    g.append("INBOX", None, None, twice)
    g.append("Projects", None, None, twice)
    g.logout()
    r = s.post(f"{WEB}/mail/refresh", params={"folder": every})
    check(r.ok and r.json()["done"], f"Refresh from All mail goes through every folder: {r.text}")
    check("twice" in rows(s, inbox) and "twice" in rows(s, node_id(s, "Projects")), "in Test's inbox and Projects")
    check(s.get(f"{WEB}/box/{every}").text.count('<span class="subject">twice</span>') == 1, "once in All mail")
    check('class="tag bin"' not in listed()["twice"], "its inbox copy")

    ids = rows(s, every)
    view = s.get(f"{WEB}/msg/{ids['hello-1']}").text
    check('value="notspam"' in view and 'value="notspam"' not in s.get(f"{WEB}/msg/{ids['bulk-0']}").text, "Not spam, for a message in Spam only")
    menu = view.split('name="to"')[1].split("</select>")[0]
    check(f'value="{every}"' not in menu, "Move to… doesn't offer All mail")
    check("Delete this message for good?" in s.get(f"{WEB}/msg/{ids['spam-test']}").text and "for good?" not in view, "deleting from the Trash asks first")
    r = s.post(f"{WEB}/msg/{ids['bulk-0']}", data={"action": "move", "to": str(every), "back": str(every)})
    check(flash(r)[1] == "All mail shows the mail of every folder: put it in one of them", f"nothing moves into it: {flash(r)}")

    listed_at = f"/box/{every}?page=0"
    bulk = lambda action, *subjects_: s.post(f"{WEB}/box/{every}/bulk", data=[("back", listed_at), ("action", action)] + [("id", str(ids[x])) for x in subjects_])
    check(flash(bulk("notspam", "bulk-0"))[1] == "None of them is in Spam.", "Not spam on mail out of Spam: said so")
    check(flash(bulk("notspam", "hello-1", "bulk-0"))[0] == "Moved 1 message to the inbox.", "Not spam from All mail")
    wait(lambda: "hello-1" in gm_subjects("test", "INBOX") and "hello-1" not in gm_subjects("test", "Spam"), "back in Test's inbox at GreenMail")
    check("hello-1" in rows(s, inbox) and 'class="tag bin"' not in listed()["hello-1"], "in the inbox; no longer told Spam")
    check(flash(bulk("delete", "spam-test"))[0] == "Deleted 1 message.", "deleted from All mail, out of the Trash: for good")
    wait(lambda: "spam-test" not in gm_subjects("test", "Trash"), "gone from Test's Trash at GreenMail")
    check("spam-test" not in listed(), "and from All mail")


def retiring(s, inbox):
    """An account out of service: its sync stops, its mail stays, changes stay in valet; then syncing again."""
    r = s.post(f"{WEB}/mail/accounts/org/retire", data={"action": "retire"})
    check(flash(r)[0] == "Org no longer syncs: its mail stays here.", "stop syncing Org, keep the mail")
    card = re.search(r'<div class="card retired">.*?</div>\s*</div>', r.text, re.S)
    check(card and "Org" in card.group(0) and "Retired" in card.group(0) and "Sync again" in card.group(0), "its card says so, with Sync again")
    status = s.get(f"{WEB}/mail/status").json()["org"]
    check("Not synced since" in status and "●" not in status, f"no error shown for it: {status}")
    check("org-hello" in rows(s, inbox), "its mail is still in the combined inbox")

    g = gm_imap("org")
    g.append("INBOX", None, None, message("org-after", to="org@local.test"))
    g.logout()
    r = s.post(f"{WEB}/mail/refresh")
    check(r.ok and r.json() == {"done": True, "failed": [], "waiting": []}, f"Refresh leaves it out, no failure: {r.text}")
    time.sleep(2)
    check("org-after" not in rows(s, inbox), "nothing new is fetched")

    hello = rows(s, inbox)["org-hello"]
    r = s.post(f"{WEB}/msg/{hello}", data={"action": "flag", "back": str(inbox)})
    check("★ Unflag" in r.text, "flagging its mail works here")
    time.sleep(2)
    g = gm_imap("org")
    g.select("INBOX")
    flagged = g.search(None, '(FLAGGED SUBJECT "org-hello")')[1][0]
    g.logout()
    check(flagged == b"", "and stays in valet: not flagged at the provider")
    s.post(f"{WEB}/tree", data={"name": "Only-new", "parent": ""})
    r = s.post(f"{WEB}/msg/{hello}", data={"action": "move", "to": str(node_id(s, "Only-new")), "back": str(inbox)})
    check("Org is retired: its folders no longer change at the provider" in (flash(r)[1] or ""), f"no folder is made for it: {flash(r)}")

    page = s.get(f"{WEB}/compose").text
    check('<option value="org@local.test"' not in page and '<option value="test@local.test"' in page, "it isn't offered to send from")
    r = s.post(f"{WEB}/compose", files={"from": (None, "org@local.test"), "to": (None, "x@y.z"), "subject": (None, "nope"), "html": (None, "<div>hi</div>")})
    check("Not sent: pick an address to send from" in r.text, "nor does it send")
    reply = s.get(f"{WEB}/compose", params={"reply": hello}).text
    check('<option value="test@local.test" selected' in reply or 'selected' not in reply.split('name="from"')[1].split("</select>")[0], "a reply to its mail goes from a live account")

    r = s.post(f"{WEB}/mail/accounts/org/retire", data={"action": "resume"})
    check(flash(r)[0] == "Syncing Org again." and '<div class="card retired">' not in r.text, "Sync again")
    wait(lambda: "org-after" in rows(s, inbox), "what came meanwhile is fetched")


def filters(s, inbox):
    """The list's filters, alone and together, and the people suggested."""
    att = (
        "From: =?utf-8?q?Zo=C3=A9_Martin?= <zoe@acme.test>\r\nTo: test@local.test\r\nCc: Bob <bob@acme.test>\r\n"
        f"Subject: filt-att\r\nDate: {email.utils.formatdate()}\r\nMIME-Version: 1.0\r\n"
        "Content-Type: multipart/mixed; boundary=b\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nthe report\r\n"
        "--b\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename=r.pdf\r\n\r\n%PDF-1\r\n--b--\r\n"
    ).encode()
    old = re.sub(rb"Date: [^\r]*", b"Date: Wed, 15 Jan 2020 10:00:00 +0000", message("filt-old"))
    g = gm_imap("test")
    g.append("INBOX", r"(\Flagged)", None, att)
    g.append("INBOX", None, imaplib.Time2Internaldate(datetime(2020, 1, 15, 10, tzinfo=timezone.utc)), old)
    g.logout()
    wait(lambda: {"filt-att", "filt-old"} <= set(rows(s, inbox)), "two messages to filter come in")
    page = lambda **p: s.get(f"{WEB}/box/{inbox}", params=p).text
    listed = lambda **p: {html.unescape(t) for t in re.findall(r'<span class="subject">([^<]*)</span>', page(**p))}
    everything = listed()

    check(listed(**{"from": "ZOÉ"}) == {"filt-att"} and listed(**{"from": "acme.test"}) == {"filt-att"}, "From: part of the sender's name or address")
    check(listed(to="bob") == {"filt-att"}, "To: Cc counts")
    with_files = listed(attachment="1")
    check("filt-att" in with_files and "filt-old" not in with_files and "Invitation: Planning" not in with_files, f"with attachments (an invitation's .ics isn't one): {with_files}")
    check(listed(invite="1") == {"Invitation: Planning", "Invitation: Retro"}, "invitations, not the answer to one")
    starred = listed(starred="1")
    check("filt-att" in starred and "filt-old" not in starred, "starred only")
    check("ahead" in listed(read="read") and "ahead" not in listed(read="unread") and "filt-att" in listed(read="unread"), "read, unread")
    check(listed(when="custom", since="2020-01-31", until="2020-01-01") == {"filt-old"}, "between two dates (either order)")
    check("filt-old" not in listed(when="week") and "filt-att" in listed(when="today"), "the last days")
    check(listed(when="", since="2020-01-01", until="2020-01-31") == everything, "dates count only for Between dates")
    check(listed(read="unread", attachment="1", **{"from": "zoe"}) == {"filt-att"}, "filters together")
    check(listed(q="report", starred="1") == {"filt-att"} and "No message matches." in page(q="report", read="read"), "and with the search")

    on = page(read="unread", **{"from": "zoe"})
    check('<span class="count" aria-label="2 on">2</span>' in on, "the Filter button counts the filters on")
    chips = dict((html.unescape(t), html.unescape(h)) for h, t in re.findall(r'<a class="chip" href="([^"]*)"[^>]*>([^<]*)<', on))
    check(chips == {"Unread": f"/box/{inbox}?page=0&from=zoe", "From: zoe": f"/box/{inbox}?page=0&read=unread"}, f"a chip each, which removes it: {chips}")
    check(f'href="/box/{inbox}?page=0"' in on and "Clear filters" in on, "and Clear filters")
    check('<input type="radio" name="read" value="unread" checked>' in on and 'name="from" value="zoe"' in on, "the menu shows what is on")

    sug = lambda field, q: s.get(f"{WEB}/box/{inbox}/addresses", params={"field": field, "q": q}).json()
    check(sug("from", "zo")[:1] == [{"name": "Zoé Martin", "address": "zoe@acme.test"}], f"senders suggested as typed: {sug('from', 'zo')}")
    check(sug("to", "bo")[:1] == [{"name": "Bob", "address": "bob@acme.test"}] and not sug("from", "bob@"), "recipients for To")
    check(sug("from", "")[0]["address"] == "someone@else.org", "nothing typed: the most frequent first")
    check(requests.get(f"{WEB}/box/{inbox}/addresses?q=zo", timeout=10).status_code == 401, "suggestions only when logged in")


def invitation_filters(out):
    """The Invitations page's Filter menu in Chrome: not answered and upcoming
    at first, each choice applied in place."""
    from cdp import Browser
    shots = os.path.join(out, "shots")
    os.makedirs(shots, exist_ok=True)
    b = Browser(os.path.join(out, "chrome-invites"), port=9336)
    b.go(f"{WEB}/login")
    b.js(f"document.querySelector('input[name=password]').value = {os.environ['VALET_PASSWORD']!r}; document.querySelector('form').submit()")
    b.settle(1.5)
    b.go(f"{WEB}/invitations", 1.5)

    def until(expr, what):
        end = time.time() + 10
        while time.time() < end and not b.js(expr):
            b.settle(0.2)
        check(b.js(expr), what)

    cards = "JSON.stringify([...document.querySelectorAll('.card b:first-child')].map(e => e.textContent).sort())"
    check(b.js(cards) == "[]" and b.js("document.querySelector('details.filter > summary .count')?.textContent") == "2", "both answered: none shown, two filters on")
    check(b.js("[...document.querySelectorAll('.filters-on a.chip')].map(a => a.firstChild.textContent).join()") == "Not answered,Upcoming", "said on chips")
    b.js("document.querySelector('details.filter > summary').click()")
    check(b.js("document.querySelector('.filter .panel').checkVisibility() && getComputedStyle(document.querySelector('.filter .panel')).position === 'absolute'"), "Filter opens its menu, styled as the mail's")
    b.js("window.samePage = true")
    b.js("document.querySelector('input[name=status][value=all]').click()")
    until(f"location.search === '?status=all' && {cards} === '[\"Planning\",\"Retro\"]'", "All applies at once")
    check(b.js("window.samePage && document.querySelector('details.filter').open"), "in place, the menu still open")
    check(b.js("document.querySelector('form[action^=\"/invites/\"] input[name=back]').value") == "/invitations?status=all", "an answer comes back here")
    b.js("{ const i = document.getElementById('filter-from'); i.focus(); i.value = 'nobody'; i.dispatchEvent(new Event('input', { bubbles: true })) }")
    until(f"{cards} === '[]' && document.body.textContent.includes('No invitation matches')", "an organizer typed applies after a pause")
    b.shot(f"{shots}/invitations-filter.png")
    b.js("document.querySelector('[data-swap=filter-reset]').click()")
    b.settle(1.5)
    check(b.js("location.search") == "" and b.js(cards) == "[]", "Reset: back to those waiting")
    check(not b.errors, f"no script errors: {b.errors}")
    b.close()


def filter_menu(s, inbox, out):
    """The Filter menu in Chrome: opens, suggests, applies each choice in place, shows its chips."""
    from cdp import Browser
    shots = os.path.join(out, "shots")
    os.makedirs(shots, exist_ok=True)
    b = Browser(os.path.join(out, "chrome-filters"), port=9335)
    b.go(f"{WEB}/login")
    b.js(f"document.querySelector('input[name=password]').value = {os.environ['VALET_PASSWORD']!r}; document.querySelector('form').submit()")
    b.settle(1.5)
    b.go(f"{WEB}/box/{inbox}", 1.5)

    def until(expr, what):
        end = time.time() + 10
        while time.time() < end and not b.js(expr):
            b.settle(0.2)
        check(b.js(expr), what)

    check(b.js("!document.querySelector('.filter .panel').checkVisibility()"), "the menu starts closed")
    b.js("document.querySelector('details.filter > summary').click()")
    check(b.js("document.querySelector('.filter .panel').checkVisibility() && getComputedStyle(document.querySelector('.filter .panel')).position === 'absolute'"), "Filter opens its menu")
    b.js("window.samePage = true")
    b.js("document.querySelector('input[name=read][value=unread]').click()")
    until("new URLSearchParams(location.search).get('read') === 'unread' && ![...document.querySelectorAll('.list .subject')].some(e => e.textContent === 'ahead')",
          "Unread applies at once, without Apply")
    check(b.js("window.samePage && document.querySelector('details.filter').open"), "the list swapped in place, the menu still open")
    check(b.js("document.querySelector('details.filter > summary .count')?.textContent") == "1", "the button counts it")
    b.js("{ const i = document.getElementById('filter-from'); i.focus(); i.value = 'zo'; i.dispatchEvent(new Event('input')) }")
    until("!document.getElementById('suggest-from').hidden && (document.querySelector('#suggest-from li')?.textContent || '').includes('zoe@acme.test')", "From suggests as it is typed")
    b.shot(f"{shots}/filter-menu.png")
    key = lambda k: b.js(f"document.getElementById('filter-from').dispatchEvent(new KeyboardEvent('keydown', {{ key: {k!r}, bubbles: true, cancelable: true }}))")
    key("ArrowDown")
    check(b.js("document.querySelector('#suggest-from li').getAttribute('aria-selected')") == "true", "arrows go through the suggestions")
    key("Enter")
    check(b.js("document.getElementById('filter-from').value") == "zoe@acme.test" and b.js("document.getElementById('suggest-from').hidden"), "Enter picks one")
    until("location.search === '?page=0&read=unread&from=zoe%40acme.test'", "picking applies it; only what is on goes in the address")
    until("JSON.stringify([...document.querySelectorAll('.list .subject')].map(e => e.textContent)) === '[\"filt-att\"]'", "the list is filtered")
    b.js("{ const w = document.querySelector('select[name=when]'); w.value = 'custom'; w.dispatchEvent(new Event('change', { bubbles: true })) }")
    check(b.js("getComputedStyle(document.querySelector('.filter .range')).display") == "flex", "Between dates shows the two dates")
    b.js("{ const w = document.querySelector('select[name=when]'); w.value = ''; w.dispatchEvent(new Event('change', { bubbles: true })) }")
    until("location.search === '?page=0&read=unread&from=zoe%40acme.test'", "back to any time")
    check(b.js("window.samePage && document.querySelector('details.filter').open && !document.querySelector('.filter .foot button')"), "all in place, the menu open, no Apply button")
    check(b.js("[...document.querySelectorAll('.filters-on a.chip')].map(a => a.firstChild.textContent)") == ["Unread", "From: zoe@acme.test"], "with its chips")
    b.shot(f"{shots}/filtered.png")
    b.js("document.querySelector('details.filter > summary').click()")
    b.js("document.querySelector('.list').click()")
    check(not b.js("document.querySelector('details.filter').open"), "a click elsewhere closes the menu")
    b.js("document.querySelector('.filters-on a.chip').click()")
    b.settle(1.5)
    check(b.js("new URLSearchParams(location.search).toString()") == "page=0&from=zoe%40acme.test", "a chip's ✕ removes that filter only")
    check(not b.errors, f"no script errors: {b.errors}")
    b.close()


main()
