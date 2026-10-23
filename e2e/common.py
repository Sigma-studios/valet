"""Helpers for the e2e scripts: IMAP to valet and to GreenMail, the web UI
with a session, waiting for the sync."""

import email.utils
import html
import imaplib
import os
import re
import sys
import time
import uuid

import requests

imaplib.Commands.setdefault("MOVE", ("SELECTED",))

WEB = os.environ["E2E_WEB"]


def check(cond, what):
    if not cond:
        print(f"FAIL: {what}")
        sys.exit(1)
    print(f"ok: {what}")


def wait(fn, what, timeout=60):
    """Waits until fn() is truthy (exceptions count as no), returns it."""
    end = time.time() + timeout
    last = None
    while time.time() < end:
        try:
            v = fn()
            if v:
                print(f"ok: {what}")
                return v
        except Exception as e:  # noqa: BLE001
            last = e
        time.sleep(0.5)
    print(f"FAIL: {what} (waited {timeout}s; last error: {last})")
    sys.exit(1)


def message(subject, frm="someone@else.org", to="test@local.test"):
    return (
        f"From: {frm}\r\nTo: {to}\r\nSubject: {subject}\r\nDate: {email.utils.formatdate()}\r\n"
        f"Message-ID: <{uuid.uuid4().hex}@e2e>\r\n\r\n{subject} body\r\n"
    ).encode()


def valet_imap():
    m = imaplib.IMAP4("127.0.0.1", int(os.environ["E2E_IMAP_PORT"]))
    m.login(os.environ["VALET_USER"], os.environ["VALET_PASSWORD"])
    return m


def gm_imap(user):
    m = imaplib.IMAP4("127.0.0.1", int(os.environ["E2E_GM_IMAP_PORT"]))
    m.login(f"{user}@local.test", "pw")
    return m


def q(name):
    return '"' + name.replace("\\", "\\\\").replace('"', '\\"') + '"'


def listing(m, pattern="*"):
    """{name: [flags]} from LIST."""
    typ, data = m.list('""', pattern)
    assert typ == "OK", data
    out = {}
    for line in data:
        if not line:
            continue
        mt = re.match(rb'\((.*?)\) (?:"."|NIL) (.*)', line)
        name = mt.group(2).decode()
        if name.startswith('"'):
            name = name[1:-1].replace('\\"', '"').replace("\\\\", "\\")
        out[name] = mt.group(1).decode().split()
    return out


def subjects(m, mailbox):
    """{subject: uid} in a mailbox (selected afterwards)."""
    typ, data = m.select(q(mailbox))
    assert typ == "OK", (mailbox, data)
    typ, data = m.uid("SEARCH", None, "ALL")
    uids = data[0].split()
    if not uids:
        return {}
    typ, data = m.uid("FETCH", b",".join(uids).decode(), "(UID BODY.PEEK[HEADER.FIELDS (SUBJECT)])")
    out = {}
    for item in data:
        if isinstance(item, tuple):
            uid = int(re.search(rb"UID (\d+)", item[0]).group(1))
            head = item[1].decode().strip()
            out[head.split(":", 1)[1].strip() if ":" in head else ""] = uid
    return out


def gm_subjects(user, mailbox):
    m = gm_imap(user)
    try:
        return subjects(m, mailbox)
    finally:
        m.logout()


def gm_folders(user):
    m = gm_imap(user)
    try:
        return listing(m)
    finally:
        m.logout()


def web():
    s = requests.Session()
    r = s.post(WEB + "/login", data={"password": os.environ["VALET_PASSWORD"]}, allow_redirects=False)
    assert r.status_code in (302, 303), r.status_code
    # The cookie is Secure: requests won't send it over plain HTTP as it came.
    name, value = r.headers["set-cookie"].split(";")[0].split("=", 1)
    s.cookies.clear()
    s.cookies.set(name, value)
    return s


def flash(r):
    """(ok, err) messages of the page a POST redirected to."""
    ok = re.search(r'<p class="ok">(.*?)</p>', r.text)
    err = re.search(r'<p class="err">(.*?)</p>', r.text)
    return (html.unescape(ok.group(1)) if ok else None, html.unescape(err.group(1)) if err else None)


def tree_rows(s):
    """[(id, parent, label)] of the tree editor, in order."""
    page = s.get(WEB + "/tree").text
    rows = re.findall(r'class="trow[^"]*" data-id="(\d+)" data-parent="(\d*)" data-label="([^"]*)"', page)
    return [(int(i), int(p) if p else None, html.unescape(l)) for i, p, l in rows]


def node_id(s, label, parent=None):
    for i, p, l in tree_rows(s):
        if l == label and p == parent:
            return i
    return None


def members_on_page(s, node):
    """{(account, folder): id} of a node's members checklist (every real folder)."""
    page = s.get(f"{WEB}/tree?node={node}").text
    out = {}
    for acc, body in re.findall(r"<legend>(.*?)</legend>(.*?)</fieldset>", page, re.S):
        for fid, name in re.findall(r'value="(\d+)"(?: checked)?>([^<]+)', body):
            out[(html.unescape(acc), html.unescape(name.strip()))] = int(fid)
    return out


def rows(s, box):
    """{subject: message id} of a webmail list."""
    page = s.get(f"{WEB}/box/{box}").text
    found = re.findall(r'<a class="row[^"]*" href="/(?:msg/|compose\?draft=)(\d+)">.*?<span class="subject">([^<]*)</span>', page, re.S)
    return {html.unescape(subj): int(i) for i, subj in found}
