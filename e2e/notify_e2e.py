"""Notifications of new mail, against GreenMail: the arrivals the Android
app asks for (/mail/new.json), then, with CHROME_PUSH=1, real Web Push in a
headless Chrome (subscribed through the Settings page; valet pushes through
Google's push service, so this needs the internet).

  notify_e2e.py <dir>    (after mail_e2e.py seed, valet running)"""

import os
import re
import sys
import time

from common import WEB, check, gm_imap, message, node_id, valet_imap, wait


def status(m, name):
    typ, data = m.status(f'"{name}"', "(MESSAGES)")
    return int(data[0].split(b"MESSAGES ")[1].rstrip(b")")) if typ == "OK" else -1


def deliver(subject, user="test", seen=False, frm="Ann Lee <ann@else.org>"):
    g = gm_imap(user)
    g.append("INBOX", "(\\Seen)" if seen else None, None, message(subject, frm=frm, to=f"{user}@local.test"))
    g.logout()


def new(s, after=None):
    r = s.get(f"{WEB}/mail/new.json", params={} if after is None else {"after": after})
    assert r.ok, r.status_code
    return r.json()


def badge(s):
    r = s.get(f"{WEB}/mail/badge.json")
    assert r.ok, r.status_code
    return r.json()["badge"]


def api(s):
    import requests
    check(requests.get(f"{WEB}/mail/new.json").status_code == 401, "the list of arrivals needs a login")
    first = new(s)
    check(first == {"last": 0, "notes": []}, f"the first fetch of each folder tells of nothing: {first}")
    check(requests.get(f"{WEB}/mail/badge.json").status_code == 401, "the badge needs a login")
    check(badge(s) == 0, "the mail there before, unread or not, isn't in the badge")

    deliver("lunch tomorrow?")
    s.post(f"{WEB}/mail/refresh")
    got = wait(lambda: new(s, 0)["notes"], "a message come into an inbox is an arrival")
    n = got[0]
    check(len(got) == 1 and n["title"] == "Ann Lee" and n["body"].startswith("lunch tomorrow?\n"), f"said as who it's from, the subject, then the first words: {n}")
    check(n["account"] == "Test", "the account it came to, as there are two")
    check(re.fullmatch(r"/inbox\?open=\d+", n["url"]) and n["tag"] == f"mail-{n['message']}", f"it opens the message beside the inbox: {n['url']}")
    page = s.get(WEB + n["url"]).text
    check("lunch tomorrow?" in page and 'class="row open"' in page, "that address shows it, open in the list")
    last = new(s)["last"]
    check(last == 1 and new(s, last)["notes"] == [], "nothing after the last arrival")

    deliver("already read", seen=True)
    deliver("for org", user="org", frm="bob@else.org")
    s.post(f"{WEB}/mail/refresh")
    got = wait(lambda: new(s, last)["notes"], "the other account's new mail too")
    check([x["title"] for x in got] == ["bob@else.org"], f"mail read elsewhere already isn't told of; a sender without a name is their address: {got}")
    last = new(s)["last"]

    for i in range(4):
        deliver(f"batch {i}")
    s.post(f"{WEB}/mail/refresh")
    got = wait(lambda: new(s, last)["notes"], "four at once")
    check(len(got) == 1 and got[0]["title"] == "4 new messages" and got[0]["tag"] == "mail" and got[0]["message"] is None, f"several at once: one note for them all: {got}")
    check(got[0]["body"].count("Ann Lee: batch") == 4 and got[0]["url"] == "/inbox", "listing each")
    last = new(s)["last"]

    deliver("to be read")
    s.post(f"{WEB}/mail/refresh")
    n = wait(lambda: new(s, last)["notes"], "one more")[0]
    before = badge(s)
    check(before >= 5, f"the badge counts the new mail still unread: {before}")
    listed = re.findall(r'name="id" value="(\d+)"', s.get(f"{WEB}/box/{node_id(s, 'INBOX')}", params={"read": "new"}).text)
    check(len(listed) == before, f"the Filter's New lists what it counts ({len(listed)})")
    check(s.post(f"{WEB}/msg/{n['message']}/seen").status_code == 204, "Mark as read (the notification's action)")
    check(badge(s) == before - 1, "read: out of the badge")
    check(new(s, last)["notes"] == [], "read since: no longer told of")
    return new(s)["last"]


def badges(b, s):
    """The app's badge, as pages set it: what they set is recorded (headless
    Chrome has no Dock)."""
    b.call("Page.addScriptToEvaluateOnNewDocument", source="""
        window.badges = [];
        navigator.setAppBadge = (n) => { window.badges.push(n); return Promise.resolve(); };
        navigator.clearAppBadge = () => { window.badges.push(0); return Promise.resolve(); };""")
    last = lambda: b.js("window.badges.at(-1) ?? null")

    def unread():
        # From the last arrivals back (four or more at once make one note without its message).
        top = new(s)["last"]
        return next(n["message"] for k in range(top - 1, -1, -1) for n in new(s, k)["notes"] if n["message"])

    def follows(what):
        end = time.time() + 10
        while time.time() < end and last() != badge(s):
            b.settle(0.5)
        check(last() == badge(s), f"{what}: the badge follows ({last()}, valet counts {badge(s)})")

    b.go(WEB + "/calendar", 2)
    follows("a page opened")
    check(badge(s) > 0, "with new mail unread")
    s.post(f"{WEB}/msg/{unread()}/seen")
    follows("on the calendar, mail read from another app")
    b.go(WEB + "/inbox", 2)
    b.js("Object.defineProperty(document, 'hidden', { get: () => true })")
    s.post(f"{WEB}/msg/{unread()}/seen")
    follows("the inbox in the background, mail read from another app")
    before = badge(s)
    deliver("while hidden")
    s.post(f"{WEB}/mail/refresh")
    wait(lambda: badge(s) > before, "new mail")
    follows("the inbox in the background, new mail")
    b.go(WEB + "/settings", 3)


def chrome(s, out):
    from cdp import Browser
    profile = os.path.join(out, "chrome-push")
    b = Browser(profile, port=9334)
    try:
        b.call("Browser.grantPermissions", permissions=["notifications"], origin=WEB)
        b.go(WEB + "/login")
        b.js(f"document.querySelector('input[name=password]').value = {os.environ['VALET_PASSWORD']!r}; document.querySelector('form').submit()")
        b.settle(2)
        b.go(WEB + "/settings", 3)
        check(b.js("document.getElementById('push-state').textContent") == "Off for this device.", "Settings: off for this device at first")
        b.js("document.getElementById('push-on').click()")
        end = time.time() + 30
        while time.time() < end and "On for this device." != b.js("document.getElementById('push-state')?.textContent"):
            b.settle(1)
        check(b.js("document.getElementById('push-state').textContent") == "On for this device.", "Turn on: subscribed (through Google's push service)")
        check(b.js("!document.querySelector('tr[data-endpoint] .this').hidden"), "listed as this device")
        b.js("document.getElementById('push-test').click()")
        shown = lambda: b.js("navigator.serviceWorker.ready.then((r) => r.getNotifications()).then((ns) => ns.map((n) => [n.title, n.body, n.tag]))")
        got = None
        end = time.time() + 60
        while time.time() < end and not got:
            b.settle(1)
            got = [n for n in shown() if n[2] == "test"]
        check(got and got[0][0] == "valet", f"Send a test: pushed, decrypted and shown: {got} ({b.js('document.getElementById(\"push-state\").textContent')})")

        deliver("pushed one", frm="Carl Mo <carl@else.org>")
        s.post(f"{WEB}/mail/refresh")
        got = None
        end = time.time() + 60
        while time.time() < end and not got:
            b.settle(1)
            got = [n for n in shown() if n[1].startswith("pushed one")]
        check(got and got[0][0] == "Carl Mo · Test", f"new mail: pushed and shown as a notification: {got}")
        badges(b, s)
        check(not b.errors, f"no script errors: {b.errors}")

        b.js("document.getElementById('push-off').click()")
        b.settle(3)
        check(b.js("document.getElementById('push-state').textContent") == "Off for this device.", "Turn off")
        check("<tr data-endpoint" not in s.get(f"{WEB}/settings").text, "valet forgets the device")

        b.js("document.getElementById('push-on').click()")
        wait(lambda: "<tr data-endpoint" in s.get(f"{WEB}/settings").text, "on again")
        b.go(WEB + "/settings", 2)
        b.shot(os.path.join(out, "notifications.png"))
        b.js("document.querySelector('form[action=\"/logout\"] button').click()")
        b.settle(3)
        check("<tr data-endpoint" not in s.get(f"{WEB}/settings").text, "logging out stops this device's notifications")
    finally:
        b.close()


def main():
    from common import web
    m = valet_imap()
    wait(lambda: status(m, "INBOX") == 4, "both accounts synced")
    s = web()
    api(s)
    if os.environ.get("CHROME_PUSH"):
        chrome(s, sys.argv[1])


if __name__ == "__main__":
    main()
