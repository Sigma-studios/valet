# valet

One server, one password, for all your mailboxes and your calendar, used from
the native apps of every device.

- **Calendar** (CalDAV): Apple Calendar, DAVx⁵ on Android, Thunderbird,
  GNOME/Evolution. Every event gets the alerts you chose, stored in the event
  itself, so they ring natively everywhere, even offline.
- **Mail** (IMAP + SMTP): valet logs in to each of your mailboxes (Gmail,
  IONOS, Microsoft 365…), keeps a copy, and serves them all as one account:
  `INBOX` gathers every inbox, `<Account>/<folder>` holds each account's
  folders. What you do in your mail app (read, flag, move, delete, file)
  reaches the provider; what happens at the provider reaches your apps, at
  once for inboxes (IDLE). Mail you send goes out through the account it is
  from, chosen by its From address.
- **Invitations** arriving in an inbox land in the calendar (an "Invitations"
  calendar until answered), and can be answered from valet's Mail page: the
  organizer gets the reply from the account the invitation came to.

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
| `VALET_ADDRESSES` | more addresses of yours, comma-separated, for invitations (the accounts' own addresses count already) |
| `DATA_DIR` | where `valet.db` (calendar) and `mail.db` live (default `data`) |
| `LISTEN` | web UI and CalDAV, default `0.0.0.0:8080` |
| `IMAP_LISTEN` | default `0.0.0.0:1143` |
| `SMTP_LISTEN` | mail submission, default `0.0.0.0:1587` |

valet speaks plain HTTP, IMAP and SMTP: put it behind something that adds TLS
(façade does, for the web and, with `--tls-port`, for the mail ports).

### Mail accounts

Added, edited and removed on the Mail page, and kept in `mail.db`. Gmail,
IONOS and Microsoft 365 are presets: type the address and the password; any
other provider takes its IMAP and SMTP servers. valet logs in to both servers
before saving, so a wrong password is caught there. Each account has:

| Field | |
|---|---|
| name | the folder mail apps show for the account |
| address, other addresses | what the account may send as |
| IMAP, SMTP servers | `host:port`; IMAP is SSL/TLS; SMTP port 465 is SSL/TLS, others STARTTLS |
| login | if it isn't the address |
| days of mail | how far back the first sync of each folder goes (default 90) |
| folders to leave out | provider folders, by name |

Gmail needs an app password (myaccount.google.com/apppasswords, with 2-step
verification on). Its "All Mail", "Important" and "Starred" views are left out:
they only repeat other folders.

**Microsoft 365** only accepts OAuth. The preset uses Thunderbird's public
client (organisations that allow Thunderbird allow it), whose redirect URI is
`https://localhost`: press *Log in to Microsoft* on the Mail page, and once the
browser ends on a page that doesn't load, paste its address into the Mail page.

Accounts used to come from a `VALET_ACCOUNTS` variable (a JSON list of the same
fields); if it is set when valet has no account yet, they are imported once.

## Running it on façade

```sh
facade app create valet --git https://github.com/Sigma-studios/valet \
  --rust-bin valet --port 8080 --domain mail.example.com --volume data:/data --memory 512 \
  --tls-port 993:1143 --tls-port 465:1587
facade env set valet DATA_DIR=/data VALET_USER=me --public
facade env set valet VALET_PASSWORD   # read from stdin
facade deploy valet
```

Then add the mailboxes on the Mail page.

Mail apps then use `mail.example.com`, IMAP on 993 and SMTP on 465, both
SSL/TLS, with valet's user name and password (the Connect apps page lists it
all).

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

Locally, mail apps connect to `localhost` on 1143 (IMAP) and 1587 (SMTP)
without encryption. For a test provider without encryption (a local
GreenMail, say), import an account with `"tls": false` through `VALET_ACCOUNTS`.
