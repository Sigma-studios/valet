"""The calendar page's routes, the alarms feed for the Android app, and what
makes valet an installable app (manifest, service worker, icons)."""

import json
import re
from datetime import date, datetime, timedelta
from zoneinfo import ZoneInfo

import requests

from common import WEB, check, flash, web


def main():
    s = web()
    # No calendar yet: the first event makes one, rather than failing.
    first = {"title": "First", "start_date": "2026-11-02", "start_time": "10:00", "end_date": "2026-11-02", "end_time": "11:00", "tz": "Europe/Paris", "repeat": "none"}
    r = s.post(f"{WEB}/calendar/event", data={**first, "calendar": ""})
    check(r.ok and r.json()["calendar"] == "calendar", f"with no calendar, the first event makes one: {r.text}")
    check(s.post(f"{WEB}/calendar/event/delete", data={"calendar": "calendar", "name": r.json()["name"]}).ok, "and it can go")
    r = s.post(f"{WEB}/calendar/event", data={**first, "calendar": ""})
    check(r.status_code == 422 and r.json()["error"] == "pick a calendar", "with one already, no calendar picked is refused")
    r = s.post(f"{WEB}/calendar/event", data={"title": "Broken"})
    check(r.status_code == 422 and "start_date" in r.json()["error"], f"a broken form says why, as JSON: {r.text}")
    for name in ["Personal", "Work"]:
        s.post(f"{WEB}/calendars", data={"name": name})
    page = s.get(f"{WEB}/calendars").text
    check("Personal" in page and "Work" in page, "two calendars")

    # A pattern that rings, made with the new form's fields.
    r = s.post(f"{WEB}/patterns", data={"name": "Wake", "tag": "wake", "alerts": "2h, 1d 18:00", "ring": "1"})
    check(flash(r)[0] == "Created Wake.", "pattern Wake, ringing")
    page = s.get(f"{WEB}/patterns").text
    check("rings on the Android app" in page and "patterns.js" in page and "data-alerts" in page, "the pattern list says it rings; the form has the row editor")
    wake = int(re.search(r'href="/patterns/(\d+)"', page).group(1))

    soon = date.today() + timedelta(days=3)
    month = soon.strftime("%Y-%m")
    after = (soon.replace(day=1) + timedelta(days=32)).strftime("%Y-%m")
    form = {
        "calendar": "personal", "title": "Dentist, then lunch", "start_date": soon.isoformat(), "start_time": "10:00",
        "end_date": soon.isoformat(), "end_time": "11:30", "tz": "Europe/Paris", "location": "Rue X", "notes": "bring\ncard",
        "repeat": "none", "pattern": str(wake),
    }
    r = s.post(f"{WEB}/calendar/event", data=form)
    check(r.ok and r.json()["ok"], "create a one-off event")
    dentist = r.json()["name"]
    r = s.post(f"{WEB}/calendar/event", data={**form, "title": "Standup", "start_time": "09:30", "end_time": "09:45", "repeat": "weekly", "pattern": "", "location": "", "notes": ""})
    standup = r.json()["name"]
    r = s.post(f"{WEB}/calendar/event", data={**form, "title": "Holidays", "all_day": "1", "end_date": (soon + timedelta(days=2)).isoformat(), "repeat": "none", "pattern": "", "calendar": "work"})
    check(r.ok, "create an all-day event over three days")
    bad = s.post(f"{WEB}/calendar/event", data={**form, "end_date": (soon - timedelta(days=1)).isoformat()})
    check(bad.status_code == 422 and "ends before" in bad.json()["error"], "an end before the start is refused")
    bad = s.post(f"{WEB}/calendar/event", data={**form, "title": " "})
    check(bad.status_code == 422, "a title is needed")

    feed = s.get(f"{WEB}/calendar/month/{month}").json()
    titles = [e["title"] for e in feed["events"]]
    check({"Dentist, then lunch", "Standup", "Holidays"} <= set(titles), "the month feed has the three events")
    later = [e["title"] for e in s.get(f"{WEB}/calendar/month/{after}").json()["events"]]
    check(later.count("Standup") >= 4, f"the weekly one repeats ({later.count('Standup')} times the month after)")
    d = next(e for e in feed["events"] if e["title"] == "Dentist, then lunch")
    check(d["start"] == int(datetime(soon.year, soon.month, soon.day, 10, 0, tzinfo=ZoneInfo("Europe/Paris")).timestamp()), "10:00 in Paris, as a moment")
    check(d["notes"] == "bring\ncard" and d["location"] == "Rue X" and not d["allDay"], "text kept as typed")
    h = next(e for e in feed["events"] if e["title"] == "Holidays")
    check(h["allDay"] and h["start"] == soon.isoformat() and h["end"] == (soon + timedelta(days=3)).isoformat(), "all-day: days, end excluded")
    check({c["id"] for c in feed["calendars"]} >= {"personal", "work"} and any(p["ring"] for p in feed["patterns"]), "calendars and patterns come along")
    check(d.get("alert") == wake and d.get("alertBy") == "chosen", f"an event says the pattern it follows, and why: {d}")
    check("alert" not in h, "one without a pattern says none")
    p = next(p for p in feed["patterns"] if p["id"] == wake)
    check(re.fullmatch(r"#[0-9a-f]{6}", p["color"]) and p["timed"] == ["6 h before", "2 h before"] and p["allDay"][0] == "18:00 the day before", f"patterns come with their color and alerts said: {p}")

    # A pattern's color, picked; an event following it by its tag (it stays, for the browser's badge).
    r = s.post(f"{WEB}/patterns", data={"name": "Focus", "tag": "focus", "alerts": "5m", "color": "#0d9488"})
    check(flash(r)[0] == "Created Focus.", "pattern Focus, teal")
    check(s.post(f"{WEB}/patterns", data={"name": "Odd", "tag": "odd", "alerts": "5m", "color": "red"}).ok, "a color that isn't one is ignored")
    s.post(f"{WEB}/calendar/event", data={**form, "title": "Deep work #focus", "start_time": "14:00", "end_time": "16:00", "pattern": "", "location": "", "notes": ""})
    feed = s.get(f"{WEB}/calendar/month/{month}").json()
    focus = next(p for p in feed["patterns"] if p["tag"] == "focus")
    odd = next(p for p in feed["patterns"] if p["tag"] == "odd")
    deep = next(e for e in feed["events"] if e["title"] == "Deep work #focus")
    check(focus["color"] == "#0d9488" and odd["color"].startswith("#") and deep["alert"] == focus["id"] and deep["alertBy"] == "tag", f"the tag's pattern, in its color: {deep}")
    page = s.get(f"{WEB}/patterns/{focus['id']}").text
    check('type="color" name="color" value="#0d9488"' in page, "its form keeps the color")
    s.post(f"{WEB}/patterns/{focus['id']}", data={"name": "Focus", "tag": "focus", "alerts": "5m", "color": "#7c3aed"})
    check(next(p for p in s.get(f"{WEB}/calendar/month/{month}").json()["patterns"] if p["tag"] == "focus")["color"] == "#7c3aed", "and saves a new one")

    one = s.get(f"{WEB}/calendar/event", params={"calendar": "personal", "name": standup}).json()
    check(one["repeat"] == "weekly" and one["pattern"] is None, "the series opens for editing")
    r = s.post(f"{WEB}/calendar/event", data={**form, "from_calendar": "personal", "name": standup, "calendar": "work", "title": "Standup (team)",
                                                "start_time": "09:30", "end_time": "09:45", "repeat": "daily", "pattern": "", "location": "", "notes": ""})
    check(r.ok and r.json()["calendar"] == "work", "edit: renamed, every day, moved to Work")
    feed = s.get(f"{WEB}/calendar/month/{after}").json()
    team = [e for e in feed["events"] if e["title"] == "Standup (team)"]
    check(len(team) >= 28 and all(e["calendar"] == "work" for e in team) and not any(e["title"] == "Standup" for e in feed["events"]), "the series follows")

    # The Android app's alarms: Wake's alerts only (on a timed event, "1d 18:00" is 6 h before).
    alarms = s.get(f"{WEB}/calendar/alarms.json", params={"tz": "Europe/Paris"}).json()["alarms"]
    check([(a["title"], d["start"] - a["at"]) for a in alarms] == [("Dentist, then lunch", 21600), ("Dentist, then lunch", 7200)], f"the dentist's two alarms ring, in order: {alarms}")

    r = s.post(f"{WEB}/calendar/event/delete", data={"calendar": "personal", "name": dentist})
    check(r.ok and not any(e["title"].startswith("Dentist") for e in s.get(f"{WEB}/calendar/month/{month}").json()["events"]), "delete")
    check(s.get(f"{WEB}/calendar/alarms.json").json()["alarms"] == [], "no alarm left")
    check(s.post(f"{WEB}/calendar/event/delete", data={"calendar": "personal", "name": dentist}).status_code == 422, "deleting twice says it's gone")

    # The Events tab: the repeat rule first, then the first time.
    events = s.get(f"{WEB}/events").text
    check("Every day at 09:30" in events and "first: " in events, "Events shows how it repeats, then the first time")

    # Pages and the app around them.
    cal = s.get(f"{WEB}/calendar").text
    check('id="cal"' in cal and "/assets/calendar.js?v=" in cal and 'rel="manifest"' in cal, "the calendar page and the app's head")
    check('href="/invitations"' in cal and 'href="/settings"' in cal and "Mail accounts" not in cal, "the bar: everyday things, settings apart")
    sett = s.get(f"{WEB}/patterns").text
    check('class="subnav"' in sett and "Mail accounts" in sett, "settings pages have their own menu")
    check(s.get(f"{WEB}/", allow_redirects=False).headers.get("location") == "/inbox", "/ opens the mail")
    check("Nothing waiting" in s.get(f"{WEB}/invitations").text, "invitations page")
    m = s.get(f"{WEB}/manifest.webmanifest")
    man = json.loads(m.text)
    check(m.headers["content-type"].startswith("application/manifest+json") and man["start_url"] == "/inbox" and len(man["icons"]) == 3, "manifest")
    for icon in man["icons"]:
        r = s.get(WEB + icon["src"])
        check(r.ok and r.content[:4] == b"\x89PNG" and "immutable" in r.headers["cache-control"], f"icon {icon['sizes']} {icon['purpose']}")
    sw = s.get(f"{WEB}/sw.js")
    check(sw.ok and "__VERSION__" not in sw.text and "/offline" in sw.text and sw.headers["cache-control"] == "no-cache", "service worker, versioned")
    off = s.get(f"{WEB}/offline")
    check("You're offline" in off.text and '<link rel="manifest"' in off.text and off.headers["content-type"].startswith("text/html"), "offline page, with the manifest")
    check(requests.get(f"{WEB}/calendar/month/{month}").status_code == 401, "the feed needs logging in")


main()
