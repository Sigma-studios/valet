"""Completing addresses in the composer: /compose/addresses (logged out,
people written to before people heard from, accents aside, our own address
left out), then in a real (headless) Chrome: typing in To shows them, Enter
takes one with its name and keeps what was already there.

  address_e2e.py seed   mail at GreenMail, before valet starts
  address_e2e.py <dir>  (valet running)"""

import sys
import time

import requests

from common import WEB, check, gm_imap, message, web


def seed():
    m = gm_imap("test")
    m.create("Sent")
    m.append("Sent", None, None, message("lunch", frm="Test <test@local.test>", to="\"Zoé Durand\" <zoe@far.test>, Bob <bob@far.test>"))
    m.append("Sent", None, None, message("again", frm="test@local.test", to="Zoé Durand <zoe@far.test>"))
    m.append("INBOX", None, None, message("hi", frm="Zorro <zorro@else.org>"))
    m.append("INBOX", None, None, message("news", frm="Shop <noreply@shop.test>"))
    m.logout()
    print("ok: seeded GreenMail")


if sys.argv[1] == "seed":
    seed()
    sys.exit()

out = sys.argv[1]
check(requests.get(f"{WEB}/compose/addresses?q=zo").status_code == 401, "logged out, nothing is suggested")
s = web()


def ask(q):
    return [(p["name"], p["address"]) for p in s.get(f"{WEB}/compose/addresses", params={"q": q}).json()]


end = time.time() + 60
while time.time() < end and len(ask("zo")) < 2:
    time.sleep(1)
check(ask("zo") == [("Zoé Durand", "zoe@far.test"), ("Zorro", "zorro@else.org")], f"written to before heard from ({ask('zo')})")
check(ask("ZOE")[0][1] == "zoe@far.test" and ask("durand")[0][1] == "zoe@far.test", "any case, by a word of the name")
check(ask("zoé") == ask("zoe")[:len(ask("zoé"))], "accents aside")
check(ask("shop") == [], "an address that only sends is left out")
check(all(a != "test@local.test" for _, a in ask("test")), "our own address is left out")

from cdp import Browser  # noqa: E402
import os  # noqa: E402

b = Browser(os.path.join(out, "chrome-addresses"))
b.go(f"{WEB}/login")
b.js(f"document.querySelector('input[name=password]').value = {os.environ['VALET_PASSWORD']!r}; document.querySelector('form').submit()")
b.settle(1.5)
b.go(f"{WEB}/compose", 2)


def wait(expr, what, timeout=10):
    end = time.time() + timeout
    while time.time() < end:
        if b.js(expr):
            print(f"ok: {what}")
            return
        b.settle(0.2)
    check(False, what)


b.js("const t = document.querySelector('input[name=to]'); t.focus(); t.value = 'bob@far.test, '; t.setSelectionRange(t.value.length, t.value.length);")
b.call("Input.insertText", text="zo")
wait("document.querySelectorAll('.addr-pop li').length === 2", "typing shows the two")
check(b.js("document.querySelector('.addr-pop li.on').textContent") == "Zoé Durandzoe@far.test", "the first chosen")
b.call("Input.dispatchKeyEvent", type="keyDown", key="ArrowDown", code="ArrowDown", windowsVirtualKeyCode=40)
check(b.js("document.querySelectorAll('.addr-pop li')[1].classList.contains('on')"), "the arrow moves to the next")
b.call("Input.dispatchKeyEvent", type="keyDown", key="ArrowUp", code="ArrowUp", windowsVirtualKeyCode=38)
b.call("Input.dispatchKeyEvent", type="keyDown", key="Enter", code="Enter", windowsVirtualKeyCode=13)
wait("!document.querySelector('.addr-pop')", "Enter takes it")
value = b.js("document.querySelector('input[name=to]').value")
check(value == "bob@far.test, Zoé Durand <zoe@far.test>, ", f"with its name, after what was there ({value!r})")
check(b.js("location.pathname") == "/compose", "and does not send the form")
b.call("Input.insertText", text="b")
b.settle(0.6)
check(not b.js("[...document.querySelectorAll('.addr-pop li')].some(li => li.textContent.includes('bob@far.test'))"), "an address already in the field is not offered again")
b.call("Input.dispatchKeyEvent", type="keyDown", key="Escape", code="Escape", windowsVirtualKeyCode=27)
check(not b.js("!!document.querySelector('.addr-pop')"), "Escape closes it")
check(b.errors == [], f"no script errors ({b.errors})")
print("address e2e: all good")
