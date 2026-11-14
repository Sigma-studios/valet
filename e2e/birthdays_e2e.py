"""Birthdays: saved from the calendar page's routes as yearly events in a
Birthdays calendar, their Birthday pattern notifying on the Android app
(alarms.json, for builds that ask), their age on the calendar and in the
agenda (after calendar_e2e.py, valet running)."""

import re
from datetime import date, timedelta

import requests

from common import WEB, check, web


def main():
    s = web()
    today = date.today()
    tomorrow = today + timedelta(days=1)
    r = s.post(f"{WEB}/calendar/birthdays", data={"person": "Ann", "month": tomorrow.month, "day": tomorrow.day, "year": tomorrow.year - 40})
    check(r.ok and r.json()["calendar"] == "birthdays", f"a birthday saved: {r.text}")
    ann = r.json()["name"]
    check(s.post(f"{WEB}/calendar/birthdays", data={"person": "Bob", "month": 2, "day": 29, "year": ""}).ok, "one without a year, on 29 February")
    bad = s.post(f"{WEB}/calendar/birthdays", data={"person": "Cy", "month": 2, "day": 29, "year": 1991})
    check(bad.status_code == 422 and "29 February" in bad.json()["error"], "1991 had no 29 February")
    check(s.post(f"{WEB}/calendar/birthdays", data={"person": " ", "month": 1, "day": 1}).status_code == 422, "a name is needed")

    j = s.get(f"{WEB}/calendar/birthdays").json()
    first = j["birthdays"][0]
    check(first["person"] == "Ann" and first["next"] == tomorrow.isoformat() and first["turns"] == 40, f"the next first, with the age it turns: {first}")
    bob = next(b for b in j["birthdays"] if b["person"] == "Bob")
    check("year" not in bob and "turns" not in bob and bob["next"][5:] in ("02-28", "02-29"), f"no year, no age: {bob}")

    # The calendar and its pattern, made with the first one.
    patterns = s.get(f"{WEB}/patterns").text
    check("Birthday" in patterns and "notifies on the Android app" in patterns, "the Birthday pattern, notifying")
    pid = re.search(r'href="/patterns/(\d+)">Birthday<', patterns).group(1)
    edit = s.get(f"{WEB}/patterns/{pid}").text
    check('<option value="notify" selected' in edit and "6h, 08:00" in edit, "its form: a notification, the day before at 18:00 and at 8:00")

    # The Android app: builds asking for notifications get them, said so; older ones don't.
    alarms = s.get(f"{WEB}/calendar/alarms.json", params={"tz": "Europe/Paris", "notify": "1"}).json()["alarms"]
    mine = [a for a in alarms if a["title"] == "🎂 Ann turns 40"]
    check(len(mine) >= 1 and all(a.get("notify") for a in mine), f"Ann's reminders, as notifications: {mine}")
    check(not any("Ann" in a["title"] for a in s.get(f"{WEB}/calendar/alarms.json", params={"tz": "Europe/Paris"}).json()["alarms"]), "not to builds that only ring")

    # The age on the calendar and in the agenda.
    feed = s.get(f"{WEB}/calendar/month/{tomorrow:%Y-%m}").json()
    e = next(e for e in feed["events"] if e["title"] == "🎂 Ann")
    check(e["allDay"] and e["age"] == 40 and e["calendar"] == "birthdays", f"the month feed: {e}")
    a = s.get(f"{WEB}/api/agenda", params={"from": tomorrow.isoformat()}).json()["events"]
    check(any(x["title"] == "🎂 Ann" and x["age"] == 40 and x["start"] == tomorrow.isoformat() for x in a), "the agenda")

    # Edited, then deleted like any event.
    check(s.post(f"{WEB}/calendar/birthdays", data={"calendar": "birthdays", "name": ann, "person": "Ann B.", "month": tomorrow.month, "day": tomorrow.day, "year": ""}).ok, "edited: the year forgotten")
    first = s.get(f"{WEB}/calendar/birthdays").json()["birthdays"][0]
    check(first["person"] == "Ann B." and "turns" not in first and first["name"] == ann, "the same event, no age")
    check(s.post(f"{WEB}/calendar/event/delete", data={"calendar": "birthdays", "name": ann}).ok, "deleted")
    check(requests.get(f"{WEB}/calendar/birthdays").status_code == 401, "the list needs logging in")


main()
