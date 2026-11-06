"""Spelling and grammar checking in the composer: /compose/check (logged
out, loading, English and French paragraphs, correct text), then in a real
(headless) Chrome: what is typed gets underlined, a click on a mistake
offers its corrections, picking one replaces the word, quoted mail is not
checked.

  spell_e2e.py <dir>    (valet running)
A screenshot lands in <dir>/shots."""

import os
import sys
import time

import requests

from cdp import Browser
from common import WEB, check, web

out = sys.argv[1]
shots = os.path.join(out, "shots")
os.makedirs(shots, exist_ok=True)

r = requests.post(f"{WEB}/compose/check", json={"text": "Hello"})
check(r.status_code == 401, f"logged out, a check is refused ({r.status_code})")

s = web()


def ask(text):
    end = time.time() + 120
    while time.time() < end:
        j = s.post(f"{WEB}/compose/check", json={"text": text}).json()
        check(j.get("enabled") is True, f"checking is on ({j})")
        if j.get("ready"):
            return j["mistakes"]
        time.sleep(1)
    check(False, "the languages load within two minutes")


def words(text, mistakes):
    units = text.encode("utf-16-le")
    return [units[m["start"] * 2:m["end"] * 2].decode("utf-16-le") for m in mistakes]


text = "Bonjour Marie,\nLes fleurs que j’ai acheté sont belles, il y a du soleil.\nThanks for you're help. I recieve it tomorow 😀.\n— Antonin"
found = ask(text)
got = words(text, found)
print("found:", list(zip(got, [m["rule"] for m in found])))
check("acheté" in got, "a French agreement mistake is found")
check(next(m for m in found if words(text, [m])[0] == "acheté")["replacements"] == ["achetées"], "with its correction")
check("recieve" in got and "tomorow" in got, "English misspellings in the next paragraph (after an emoji, offsets in UTF-16)")
check(next(m for m in found if words(text, [m])[0] == "recieve")["replacements"][0] == "receive", "receive is the first correction")
check("you're" in got, "you're for your")
check(not any("y a" in w for w in got), "il y a is fine")
check({m["lang"] for m in found} == {"fr", "en"}, "each paragraph in its language")
clean = "Bonjour Kasia,\nPourrions-nous nous voir jeudi à 15 h ? Dis-moi quand tu es disponible.\nI don't think that that is a problem, it's fine."
check(ask(clean) == [], "correct French and English: nothing flagged")
too_long = s.post(f"{WEB}/compose/check", json={"text": "a" * 100_001})
check(too_long.status_code == 413, "too long a text is refused")

b = Browser(os.path.join(out, "chrome-spell"))
b.go(f"{WEB}/login")
b.js(f"document.querySelector('input[name=password]').value = {os.environ['VALET_PASSWORD']!r}; document.querySelector('form').submit()")
b.settle(1.5)
b.go(f"{WEB}/compose", 2)
check(b.js("!!(CSS.highlights && document.querySelector('form.write .editor'))"), "the composer, with highlights")
# a quoted mail below, as a reply has: its mistakes are someone else's
b.js("""const ed = document.querySelector('form.write .editor');
ed.innerHTML = '<div><br></div><blockquote><div>Teh qoted mesage.</div></blockquote>';
ed.focus(); const r = document.createRange(); r.setStart(ed.firstChild, 0); r.collapse(true);
getSelection().removeAllRanges(); getSelection().addRange(r);""")
b.call("Input.insertText", text="I will recieve the report tomorrow.")


def wait(expr, what, timeout=30):
    end = time.time() + timeout
    while time.time() < end:
        if b.js(expr):
            print(f"ok: {what}")
            return
        b.settle(0.3)
    check(False, what)


wait("CSS.highlights.get('valet-spelling').size > 0", "the misspelling is underlined after typing stops")
marked = b.js("[...CSS.highlights.get('valet-spelling')].map(r => r.toString())")
check(marked == ["recieve"], f"only the typed mistake, not the quoted mail ({marked})")
rect = b.js("(() => { const r = [...CSS.highlights.get('valet-spelling')][0].getBoundingClientRect(); return [r.left + r.width / 2, r.top + r.height / 2]; })()")
for kind in ("mousePressed", "mouseReleased"):
    b.call("Input.dispatchMouseEvent", type=kind, x=rect[0], y=rect[1], button="left", clickCount=1)
wait("!!document.querySelector('.spell-pop')", "a click on it opens its corrections")
options = b.js("[...document.querySelectorAll('.spell-pop button')].map(x => x.textContent)")
check(options[0] == "receive" and options[-1] == "Ignore this word", f"receive first, then ignore ({options})")
import base64  # noqa: E402
open(os.path.join(shots, "spell.png"), "wb").write(base64.b64decode(b.call("Page.captureScreenshot")["data"]))
b.js("document.querySelector('.spell-pop button').click()")
wait("document.querySelector('form.write .editor').innerText.includes('I will receive the report')", "picking it replaces the word")
check(not b.js("!!document.querySelector('.spell-pop')"), "and closes the menu")
wait("CSS.highlights.get('valet-spelling').size === 0", "nothing is underlined any more")
b.js("document.execCommand('undo')")
wait("document.querySelector('form.write .editor').innerText.includes('recieve')", "undo brings the word back")
check(b.errors == [], f"no script errors ({b.errors})")
print("spell e2e: all good")
