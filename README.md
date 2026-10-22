# valet

A personal calendar server that gives every event the alerts you chose, on
every device. Calendar apps (Apple Calendar, DAVx⁵ on Android, Thunderbird,
GNOME/Evolution) connect to it over CalDAV, and the alerts are stored in the
events themselves: they ring natively everywhere, even offline.

Later, valet will also gather several mailboxes (IMAP/SMTP) behind one account.

## Alert patterns

A pattern is a named, reusable set of alerts with a tag: *Meeting* (`#meeting`)
12 min and 2 min before, *Flight* (`#flight`) 1 day and 3 h before, *Silent*
(`#silent`) none. For each event, the first of these that applies wins:

1. a `#tag` in the event's title, typed in any app (`Lyon #flight`);
2. the pattern chosen for that event on valet's Events page;
3. alerts set by hand in a calendar app, kept exactly as they are (Apple
   Calendar's automatic default alerts don't count);
4. the calendar's default pattern, one for timed events and one for all-day
   events.

Editing a pattern or a default updates every event concerned; apps pick the
change up at their next sync. Alerts valet adds carry `X-VALET:<tag>`.

Alerts are typed as `12m, 2m` (before the start), `1d 3h`, `+30m` (after it),
or, for all-day events (which start at midnight), a clock time: `9:00` is 9 am
that day, `1d 18:00` is 6 pm the day before.

## Web UI

At the server's root, with the same password: patterns, calendars and their
defaults, the list of events with the pattern applying to each (and a choice
per event), `.ics` import (Google, Outlook, IONOS exports; importing again
updates instead of duplicating), and per-app setup instructions.

## Configuration

All through the environment:

| Variable | |
|---|---|
| `VALET_PASSWORD` | the one password (at least 12 characters), for the web UI and the apps |
| `VALET_USER` | the user name apps log in with (default `valet`) |
| `VALET_ADDRESSES` | your email addresses, comma-separated, so apps recognise you in invitations |
| `DATA_DIR` | where `valet.db` (SQLite) lives (default `data`) |
| `LISTEN` | default `0.0.0.0:8080` |

valet speaks plain HTTP: put it behind something that does HTTPS (façade does).

## Running it on façade

```sh
facade app create valet --git https://github.com/Sigma-studios/valet \
  --rust-bin valet --port 8080 --domain mail.sigma-dev.eu --volume data:/data --memory 256
facade env set valet DATA_DIR=/data VALET_USER=antonin --public
facade env set valet VALET_PASSWORD VALET_ADDRESSES   # each read from stdin
facade deploy valet
```

## CalDAV layout

| Path | |
|---|---|
| `/.well-known/caldav` | redirects to `/dav/` |
| `/dav/principal/` | the user |
| `/dav/cal/` | calendar home |
| `/dav/cal/<id>/` | a calendar (`GET` exports it as one `.ics`) |
| `/dav/cal/<id>/<name>.ics` | one event, with its recurrence exceptions |

Supported: PROPFIND, PROPPATCH, MKCALENDAR (and extended MKCOL), GET, PUT,
DELETE with ETags, and the `calendar-query`, `calendar-multiget` and
`sync-collection` reports. Calendars hold events only (no tasks). There is no
server-side scheduling: apps send invitation replies by email, as they do
without it.

## Developing

```sh
cargo test
VALET_PASSWORD='a long test password' cargo run   # http://localhost:8080
```
