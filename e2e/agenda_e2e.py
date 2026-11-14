"""The agenda for a desktop bar: a device token made and revoked on the
settings page, GET /api/agenda with it, and CalDAV's time-range and expand
finding the same occurrences (after calendar_e2e.py, valet running)."""

import base64
import html
import os
import re
from datetime import date, datetime, timedelta
from zoneinfo import ZoneInfo

import requests

from common import WEB, check, flash, web

PARIS = ZoneInfo("Europe/Paris")


def dav(method, path, body="", **headers):
    auth = base64.b64encode(f"{os.environ['VALET_USER']}:{os.environ['VALET_PASSWORD']}".encode()).decode()
    return requests.request(method, WEB + path, data=body.encode(), headers={"Authorization": f"Basic {auth}", **headers})


def main():
    s = web()
    # A weekly series, Mondays 9:00 in Paris, one moved and one cancelled.
    monday = date.today() - timedelta(days=date.today().weekday()) + timedelta(days=7)
    ymd = lambda d: d.strftime("%Y%m%d")
    second, third = monday + timedelta(days=7), monday + timedelta(days=14)
    ics = (
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:agenda-e2e\r\nSUMMARY:Sync\r\nLOCATION:Room 4\r\n"
        f"DTSTART;TZID=Europe/Paris:{ymd(monday)}T090000\r\nDTEND;TZID=Europe/Paris:{ymd(monday)}T093000\r\nRRULE:FREQ=WEEKLY;COUNT=4\r\nEND:VEVENT\r\n"
        f"BEGIN:VEVENT\r\nUID:agenda-e2e\r\nRECURRENCE-ID;TZID=Europe/Paris:{ymd(second)}T090000\r\nSUMMARY:Sync (moved)\r\n"
        f"DTSTART;TZID=Europe/Paris:{ymd(second)}T160000\r\nDTEND;TZID=Europe/Paris:{ymd(second)}T163000\r\nEND:VEVENT\r\n"
        f"BEGIN:VEVENT\r\nUID:agenda-e2e\r\nRECURRENCE-ID;TZID=Europe/Paris:{ymd(third)}T090000\r\nSUMMARY:Sync\r\nSTATUS:CANCELLED\r\n"
        f"DTSTART;TZID=Europe/Paris:{ymd(third)}T090000\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
    )
    check(dav("PUT", "/dav/cal/work/agenda-e2e.ics", ics, **{"Content-Type": "text/calendar"}).status_code == 201, "a series put over CalDAV")

    # The token: made on the settings page, shown once.
    check(requests.get(f"{WEB}/api/agenda").status_code == 401, "the agenda needs a token")
    page = s.get(f"{WEB}/tokens").text
    check("Agenda tokens" in page and "No token yet." in page, "the settings page for tokens")
    r = s.post(f"{WEB}/tokens", data={"name": "Bar <test>"})
    token = re.search(r'<code id="token">(valet_[0-9a-f]{64})</code>', r.text)
    check(token and "Bar &lt;test&gt;" in r.text, "a token made, shown once")
    token = token.group(1)
    check(token not in s.get(f"{WEB}/tokens").text and "never" in s.get(f"{WEB}/tokens").text, "and never again")
    bearer = {"Authorization": f"Bearer {token}"}
    check(requests.get(f"{WEB}/calendar/month/{monday:%Y-%m}", headers=bearer).status_code == 401, "it opens nothing but the agenda")
    check(requests.request("PROPFIND", f"{WEB}/dav/cal/", headers=bearer).status_code == 401, "not CalDAV either")

    r = requests.get(f"{WEB}/api/agenda", params={"from": monday.isoformat(), "days": 28}, headers=bearer)
    check(r.ok and r.headers["cache-control"] == "no-store", f"the agenda read with the token: {r.status_code}")
    a = r.json()
    sync = [e for e in a["events"] if e["id"] == "agenda-e2e"]
    paris = lambda d, h, m=0: datetime(d.year, d.month, d.day, h, m, tzinfo=PARIS).isoformat()
    check([(e["title"], e["start"]) for e in sync] == [("Sync", paris(monday, 9)), ("Sync (moved)", paris(second, 16)), ("Sync", paris(monday + timedelta(days=21), 9))],
          f"the series: moved, cancelled left out, local times with their offset: {[(e['title'], e['start']) for e in sync]}")
    e = sync[1]
    check(e["calendar"] == "Work" and re.fullmatch(r"#[0-9a-f]{6}", e["color"]) and e["location"] == "Room 4" and e["allDay"] is False and e["pending"] is False, f"its fields: {e}")
    check(e["url"] == f"{WEB}/calendar#day/{second.isoformat()}/work/agenda-e2e.ics", f"its link: {e['url']}")
    starts = [datetime.fromisoformat(x["start"]) if not x["allDay"] else datetime.fromisoformat(x["start"]).replace(tzinfo=PARIS) for x in a["events"]]
    check(starts == sorted(starts), "by start")
    check(requests.get(f"{WEB}/api/agenda", params={"days": 400}, headers=bearer).json()["days"] == 62, "at most 62 days")
    check(requests.get(f"{WEB}/api/agenda", params={"from": "tomorrow"}, headers=bearer).status_code == 400, "a bad date is refused")
    check(s.get(f"{WEB}/api/agenda").ok, "logged in, the agenda reads too")

    # CalDAV, asked for the same days, finds the same occurrences.
    start = datetime(monday.year, monday.month, monday.day, tzinfo=PARIS).astimezone(ZoneInfo("UTC"))
    end = (datetime(monday.year, monday.month, monday.day, tzinfo=PARIS) + timedelta(days=28)).astimezone(ZoneInfo("UTC"))
    z = lambda t: t.strftime("%Y%m%dT%H%M%SZ")
    query = f"""<?xml version="1.0"?><C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
      <D:prop><D:getetag/><C:calendar-data><C:expand start="{z(start)}" end="{z(end)}"/></C:calendar-data></D:prop>
      <C:filter><C:comp-filter name="VCALENDAR"><C:comp-filter name="VEVENT"><C:time-range start="{z(start)}" end="{z(end)}"/></C:comp-filter></C:comp-filter></C:filter>
    </C:calendar-query>"""
    found = []
    for cal in [c["id"] for c in s.get(f"{WEB}/calendar/month/{monday:%Y-%m}").json()["calendars"]]:
        r = dav("REPORT", f"/dav/cal/{cal}/", query, Depth="1")
        check(r.status_code == 207, f"calendar-query on {cal}")
        for data in re.findall(r"<C:calendar-data>(.*?)</C:calendar-data>", r.text, re.S):
            text = html.unescape(data).replace("\r\n ", "")
            check("RRULE" not in text and "VTIMEZONE" not in text, "expanded: no rules, no zones")
            for ev in re.findall(r"BEGIN:VEVENT\r\n(.*?)END:VEVENT", text, re.S):
                v = dict(re.findall(r"^([A-Z-]+)(?:;[^:]*)?:(.*?)\r$", ev, re.M))
                found.append((v["UID"], v["DTSTART"]))
    def caldav_start(e):
        if e["allDay"]:
            return e["start"].replace("-", "")
        return z(datetime.fromisoformat(e["start"]).astimezone(ZoneInfo("UTC")))
    agenda = sorted((e["id"], caldav_start(e)) for e in a["events"])
    check(sorted(found) == agenda, f"CalDAV's time-range and expand give the agenda's occurrences ({len(agenda)})")

    # Revoked, it opens nothing.
    tid = re.search(r'action="/tokens/(\d+)/revoke"', s.get(f"{WEB}/tokens").text).group(1)
    check(flash(s.post(f"{WEB}/tokens/{tid}/revoke"))[0].startswith("Revoked"), "revoked on the page")
    check(requests.get(f"{WEB}/api/agenda", headers=bearer).status_code == 401, "and refused after")


main()
