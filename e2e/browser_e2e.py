"""The calendar page and the installable app in a real (headless) Chrome:
views drawn without script errors, the event form, the pattern editor,
times in the zone of the settings (the browser is in New York, valet in
Paris), then, with valet stopped, reading offline.

  browser_e2e.py online <dir>    (after calendar_e2e.py, valet running)
  browser_e2e.py offline <dir>   (valet stopped)
Screenshots land in <dir>/shots."""

import os
import sys
import time
from datetime import date, datetime, timedelta
from zoneinfo import ZoneInfo

from cdp import Browser
from common import WEB, check

phase, out = sys.argv[1], sys.argv[2]
shots = os.path.join(out, "shots")
os.makedirs(shots, exist_ok=True)
profile = os.path.join(out, "chrome")
# calendar_e2e.py's events start in three days.
day = (date.today() + timedelta(days=3)).isoformat()


def wait(b, expr, what, timeout=10):
    end = time.time() + timeout
    while time.time() < end:
        if b.js(expr):
            print(f"ok: {what}")
            return
        b.settle(0.3)
    check(False, what)


def ev_text(b, title):
    return b.js(f"[...document.querySelectorAll('.cal-week .ev')].find(e => e.textContent.includes({title!r}))?.querySelector('.t')?.textContent || ''")


def ev_title(b, title):
    return b.js(f"[...document.querySelectorAll('.cal-week .ev')].find(e => e.textContent.includes({title!r}))?.title || ''")


def online():
    b = Browser(profile, zone="America/New_York")
    b.go(f"{WEB}/login")
    b.js(f"document.querySelector('input[name=password]').value = {os.environ['VALET_PASSWORD']!r}; document.querySelector('form').submit()")
    b.settle(1.5)
    b.go(f"{WEB}/calendar#week/{day}", 2)
    wait(b, "!!document.querySelector('.cal-week .grid')", "the week view is drawn")
    wait(b, "document.querySelectorAll('.cal-week .ev').length > 0", "with its events")
    check(b.js("Intl.DateTimeFormat().resolvedOptions().timeZone") == "America/New_York", "the browser is in New York")
    check(ev_text(b, "Standup (team)").startswith("09:30"), f"09:30 in Paris shows as 09:30, not New York's time ({ev_text(b, 'Standup (team)')!r})")
    check(b.js("document.querySelector('.cal-zone')?.textContent") == "Paris time", "the bar says the times are Paris time")
    tops = b.js("[...new Set([...document.querySelectorAll('.cal-week .ev')].filter(e => e.textContent.includes('Standup (team)')).map(e => e.style.top))]")
    check(len(tops) == 1, f"every standup at the same height, days of daylight saving too ({tops})")
    hover = ev_title(b, "Standup (team)")
    check("Paris time (Europe/Paris, " in hover and "On this device (New York time)" in hover, f"an event's hover names the zone, and the device's time: {hover!r}")
    b.shot(f"{shots}/week.png")
    b.js(f"location.hash = 'month/{day}'")
    wait(b, "document.querySelectorAll('.cal-month .cell').length >= 28", "the month view is drawn")
    b.shot(f"{shots}/month.png")
    b.js(f"location.hash = 'day/{day}'")
    wait(b, "document.querySelectorAll('.cal-week .col').length === 1", "the day view is drawn")

    # A new event through the form.
    b.js("[...document.querySelectorAll('.cal-bar button')].find(x => x.textContent === 'New event').click()")
    wait(b, "document.querySelector('dialog.cal-dlg').open && !!document.querySelector('dialog form [name=title]')", "the form opens")
    b.js("const f = document.querySelector('dialog form'); f.title.value = 'Made in the browser'; f.start_time.value = '14:00'; f.end_time.value = '15:15'")
    b.shot(f"{shots}/form.png")
    b.js("document.querySelector('dialog form').requestSubmit()")
    wait(b, "!document.querySelector('dialog.cal-dlg').open", "saved and closed")
    wait(b, "[...document.querySelectorAll('.cal-week .ev')].some(e => e.textContent.includes('Made in the browser'))", "it shows at once")
    b.js("[...document.querySelectorAll('.cal-week .ev')].find(e => e.textContent.includes('Made in the browser')).click()")
    wait(b, "document.querySelector('dialog.cal-dlg h2')?.textContent === 'Made in the browser'", "its card opens")
    check("14:00" in b.js("document.querySelector('dialog .meta').textContent"), "with its time")
    check("Paris time" in b.js("document.querySelector('dialog .meta').textContent"), "and its zone")
    check("null" not in b.js("document.querySelector('dialog.cal-dlg').textContent"), "nothing missing shows as null")
    made = b.js(f"fetch('/calendar/month/{day[:7]}').then(r => r.json()).then(j => j.events.find(e => e.title === 'Made in the browser').start)")
    paris = int(datetime.fromisoformat(f"{day}T14:00").replace(tzinfo=ZoneInfo("Europe/Paris")).timestamp())
    check(made == paris, "typed at 14:00 in a New York browser, saved as 14:00 in Paris")
    b.shot(f"{shots}/card.png")
    b.js("document.querySelector('dialog.cal-dlg').close()")

    # Times the server writes: the zone on hover, and the device's when first hovered.
    b.go(f"{WEB}/events")
    check("Paris time (Europe/Paris, " in b.js("document.querySelector('time[datetime]')?.title || ''"), "the Events page's times name their zone")
    b.js("document.querySelector('time[datetime]').dispatchEvent(new MouseEvent('mouseover', { bubbles: true }))")
    check("On this device (New York time)" in b.js("document.querySelector('time[datetime]').title"), "and give the device's time on hover")
    check("Paris time" in b.js("[...document.querySelectorAll('td span[title]')].find(s => s.textContent.startsWith('Every'))?.title || ''"), "a series' rule names its zone on hover")

    # Another zone, from the settings.
    b.go(f"{WEB}/settings")
    check(b.js("document.getElementById('zone').value") == "Europe/Paris", "Paris until changed")
    wait(b, "!document.getElementById('zone-here').hidden", "the settings offer the device's zone")
    b.js("document.getElementById('zone-here').click()")
    b.settle(1.5)
    check("Times are shown in New York time now." in b.js("document.body.textContent"), "the device's zone is saved")
    b.go(f"{WEB}/calendar#week/{day}", 2)
    wait(b, "document.querySelector('.cal-zone')?.textContent === 'New York time'", "the calendar follows it")
    wait(b, "!!document.querySelector('.cal-week .ev')", "the week is drawn again")
    # The week's first standup (its Monday), in New York: 5 or 6 hours earlier, as daylight saving goes.
    monday = date.fromisoformat(day) - timedelta(days=date.fromisoformat(day).weekday())
    ny = datetime.combine(monday, datetime.min.time().replace(hour=9, minute=30), ZoneInfo("Europe/Paris")).astimezone(ZoneInfo("America/New_York")).strftime("%H:%M")
    check(ev_text(b, "Standup (team)").startswith(ny), f"09:30 in Paris is {ny} in New York ({ev_text(b, 'Standup (team)')!r})")
    check("On this device" not in ev_title(b, "Standup (team)"), "no second time when the device is in the zone shown")
    b.go(f"{WEB}/settings")
    b.js("const z = document.getElementById('zone'); z.value = 'Mars/Olympus'; z.form.submit()")
    b.settle(1.5)
    check("is not a time zone" in b.js("document.body.textContent"), "a zone that isn't one is refused")
    b.js("const z = document.getElementById('zone'); z.value = 'Europe/Paris'; z.form.submit()")
    b.settle(1.5)
    check("Times are shown in Paris time now." in b.js("document.body.textContent"), "back to Paris")

    # The service worker, and months kept ahead.
    b.go(f"{WEB}/calendar", 3)
    wait(b, "!!navigator.serviceWorker.controller", "the service worker runs the page")
    wait(b, "caches.open('valet-data').then(c => c.keys()).then(k => k.length >= 8)", "eight months kept for offline", 20)
    for page in ["/inbox", "/settings", "/invitations"]:
        b.go(WEB + page)
    wait(b, "caches.open('valet-pages').then(c => c.keys()).then(k => k.length >= 3)", "visited pages kept")

    # The mail sidebar: folders start closed, but for the way to the one shown.
    b.go(f"{WEB}/inbox", 1)
    for name, parent in [("Work", None), ("Clients", "Work"), ("Acme", "Clients")]:
        pid = b.js(f"[...document.querySelectorAll('.folders .node')].find(n => n.querySelector('.name').textContent === {parent!r})?.dataset.id || ''") if parent else ""
        b.js(f"fetch('/tree', {{ method: 'POST', body: new URLSearchParams({{ name: {name!r}, parent: {pid!r} }}) }}).then(() => location.reload())")
        b.settle(1)
    node = lambda name: f"[...document.querySelectorAll('.folders .node')].find(n => n.querySelector('.name').textContent === {name!r})"
    check(b.js(f"!{node('Work')}.hidden && {node('Clients')}.hidden && {node('Acme')}.hidden"), "folders inside others start hidden")
    check(b.js("getComputedStyle(document.body).overflow === 'hidden' && getComputedStyle(document.querySelector('.pane')).overflowY === 'auto' && getComputedStyle(document.querySelector('.pane > .toolbar')).position === 'sticky'"),
          "the mail column scrolls, under its pinned toolbar, not the page")
    check(b.js("getComputedStyle(document.querySelector('.folders a.folder')).textAlign") == "left", "folder names are aligned left")
    b.js(f"{node('Work')}.querySelector('button.twisty').click()")
    check(b.js(f"!{node('Clients')}.hidden && {node('Acme')}.hidden"), "opening Work shows Clients only")
    acme = b.js(f"{node('Acme')}.querySelector('a').getAttribute('href')")
    b.js("sessionStorage.clear()")
    b.go(WEB + acme, 1)
    check(b.js(f"!{node('Acme')}.hidden && {node('Acme')}.querySelector('a.folder.on') !== null"), "in Acme, the way to it is open")
    b.shot(f"{shots}/sidebar.png")

    # The bar's links sit at the same place on every page.
    spots = {}
    for page in ["/inbox", "/calendar", "/invitations", "/settings", "/patterns"]:
        b.go(WEB + page, 1.5)
        spots[page] = b.js("[...document.querySelectorAll('header nav > a, header nav > form')].map(e => Math.round(e.getBoundingClientRect().left) + ':' + Math.round(e.getBoundingClientRect().top)).join(' ')")
        check(b.js("document.querySelector('header').getBoundingClientRect().right === innerWidth && document.scrollingElement.scrollHeight <= innerHeight"),
              f"{page}: the bar spans the window, which doesn't scroll itself (no strip on the side)")
    check(len(set(spots.values())) == 1, f"the bar doesn't move from page to page: {spots}")
    b.js("document.querySelector('main').append(Object.assign(document.createElement('div'), { style: 'height:3000px' }))")
    check(b.js("(m => m.scrollHeight > m.clientHeight && getComputedStyle(m).overflowY === 'auto')(document.querySelector('main')) && document.scrollingElement.scrollHeight <= innerHeight"),
          "a long page scrolls under the bar, not the window")
    # Large screens: the mail uses the room, nothing squeezed or scrolling sideways.
    for w, h in [(2560, 1440), (3840, 2160)]:
        b.call("Emulation.setDeviceMetricsOverride", width=w, height=h, deviceScaleFactor=1, mobile=False)
        b.go(f"{WEB}/inbox", 1.5)
        size = b.js("[document.querySelector('.mail').getBoundingClientRect().width, document.querySelector('.pane').getBoundingClientRect().width]")
        check(size[0] > 1900 and size[1] > 1600 and not b.js("(p => p.scrollWidth > p.clientWidth)(document.querySelector('.pane'))"),
              f"on a {w}px-wide screen the mail spreads out (mail, list widths: {size})")
    b.call("Emulation.clearDeviceMetricsOverride")

    # The pattern editor: rows, not a typed list.
    b.go(f"{WEB}/patterns")
    href = b.js("document.querySelector('td a[href^=\"/patterns/\"]').getAttribute('href')")
    b.go(WEB + href)
    wait(b, "document.querySelectorAll('.alert-row').length === 2", "the Wake pattern shows two alert rows")
    b.js("[...document.querySelectorAll('.alert-add button')][0].click()")
    b.js("const r = [...document.querySelectorAll('.alert-row')].pop(); r.querySelector('input').value = 30; r.querySelectorAll('select')[0].value = 'm'")
    b.shot(f"{shots}/pattern.png")
    b.js("document.querySelector('[data-alerts]').closest('form').requestSubmit()")
    b.settle(1.5)
    check("30 min before" in b.js("document.body.textContent"), "a row added in the editor is saved")
    check(not b.errors, f"no script errors: {b.errors}")
    b.close()

    # A phone.
    p = Browser(profile + "-phone", width=390, height=844, port=9334, mobile=True)
    p.go(f"{WEB}/login")
    p.js(f"document.querySelector('input[name=password]').value = {os.environ['VALET_PASSWORD']!r}; document.querySelector('form').submit()")
    p.settle(1.5)
    p.go(f"{WEB}/calendar#month/{day}", 2)
    wait(p, "document.querySelectorAll('.cal-month .cell').length >= 28", "the month view on a phone")
    p.shot(f"{shots}/phone-month.png")
    p.js(f"location.hash = 'day/{day}'")
    wait(p, "!!document.querySelector('.cal-week .col')", "the day view on a phone")
    p.shot(f"{shots}/phone-day.png")
    check(not p.errors, f"no script errors on the phone: {p.errors}")
    p.close()


def offline():
    b = Browser(profile)
    b.go(f"{WEB}/calendar#month/{day}", 4)
    wait(b, "document.querySelectorAll('.cal-month .chip').length > 0", "offline, the month still shows its events", 15)
    check("Offline" in b.js("(document.getElementById('offline-banner')?.textContent || '') + (document.querySelector('.cal-note')?.textContent || '')"), "it says it's offline, and as of when")
    b.shot(f"{shots}/offline-month.png")
    for _ in range(3):
        b.js("document.querySelectorAll('.cal-bar button')[2].click()")
        b.settle(1)
    check(b.js("document.querySelectorAll('.cal-month .cell').length") >= 28 and "weren't kept" not in b.js("document.body.textContent"), "three months ahead are there too")
    b.go(f"{WEB}/settings", 3)
    wait(b, "!!document.querySelector('meta[name=valet-offline]') && !!document.getElementById('offline-banner')", "a page seen before shows, marked offline", 15)
    b.shot(f"{shots}/offline-page.png")
    b.go(f"{WEB}/box/987654", 4)
    check("You're offline" in b.js("document.body.textContent"), "a page never seen says so")
    b.close()


{"online": online, "offline": offline}[phase]()
