# valet

One server, one password, for all your mailboxes and your calendar, used from
the native apps of every device.

- **Calendar** (CalDAV): Apple Calendar, DAVx⁵ on Android, Thunderbird,
  GNOME/Evolution, and valet's own calendar page. Every event gets the alerts
  you chose, stored in the event itself, so they ring natively everywhere,
  even offline; on valet's Android app, they can ring like an alarm.
- **Mail** (IMAP + SMTP): valet logs in to each of your mailboxes (Gmail,
  IONOS, Microsoft 365…), keeps a copy, and serves them all as one account,
  with one tree of folders you shape yourself (see [Folders](#folders)).
  What you do in your mail app (read, flag, move, delete, file)
  reaches the provider; what happens at the provider reaches your apps, at
  once for inboxes (IDLE). Mail you send goes out through the account it is
  from, chosen by its From address.
- **Invitations** arriving in an inbox land in the calendar (an "Invitations"
  calendar until answered), and can be answered from valet's Invitations page,
  which names the events each one would overlap: the organizer gets the reply
  from the account the invitation came to.

- **Webmail** on valet's own site, from any browser: your tree of folders
  (and every account's own folders), search, reading, and writing with
  formatting (reply, reply all, forward, attachments). What you write is saved
  as a draft in the account's Drafts folder every few seconds, so it carries on
  from another device or app. Folders can be created, renamed and deleted, here
  or from a mail app, and the provider follows. It works on the same copy as the mail
  apps, so read, flagged, moved and deleted messages match everywhere. HTML
  mail is shown in a frame where nothing runs, and remote images (which tell
  senders you opened a message) stay blocked until asked for.

## Folders

Mail apps and the webmail show one tree of folders, the same everywhere. Each
folder of it gathers real folders of any of the accounts: one *Spam* shows the
spam of all of them, one *Sent* everything sent, and *Work → Clients → Acme*
can gather a folder of two accounts. The first time an account syncs, its
folders go in the tree:

1. `INBOX`: every account's inbox;
2. *Drafts*, *Sent*, *Archive*, *Spam*, *Trash*: the folders the provider
   flags for that use (or, if it flags none, named like it: `Junk`, `Sent
   Items`, `Corbeille`…). Mail apps see these as their special folders;
3. the other folders, nested as they are; folders with the same path in
   several accounts together.

The webmail's *Edit folders* page changes it: create, rename, nest (drag and
drop, or arrows), delete, and choose what each folder gathers and its special
use; *Reset to the default tree* builds it again. Folders made later at the
provider wait there, under *Folders not in your tree*. Mail apps can create,
rename, move and delete tree folders too; that's valet's own, nothing changes
at the providers.

Mail filed into a tree folder (moved, or saved by an app, like sent mail
into *Sent*) goes to the real folder it gathers **in the message's own account**
(for saved mail, the account its From address is); a message never changes
account. If the folder has none in that account yet, valet makes one at the
provider, named after it. Deleting moves to the Trash of the message's account.

Each account's real folders stay reachable under `Accounts/<Account>/…`
(collapsed under *Accounts* in the webmail); there, folders are made, renamed
and deleted at the provider.

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

At the server's root, with the same password. The bar holds the everyday
things: **Mail** (the webmail), **Calendar** (month, week and day views of
every calendar, repeating events included; events are added, edited and
deleted there, and reach every app at its next sync), and **Invitations**
(events waiting for an answer). **Settings** holds the rest: the time zone
every page shows times in (Paris unless changed; hovering a time names its
zone, and the device's own time when it is set to another), mail accounts,
calendars and their default patterns, alert patterns, the alerts of each
event, `.ics` import (Google, Outlook, IONOS exports; importing again updates
instead of duplicating), and how to connect each app.

## The apps

The site installs as an app (a PWA): Chrome, Edge or Brave offer to install
it, Safari on a Mac has File → Add to Dock, and phones Add to Home Screen. Its
service worker keeps the pages you open and the calendar from a month back to
six months ahead, so they stay readable offline (marked so; changes wait for a
connection). Logging out forgets them.

**Android**: `android/` is valet's own app, the same site in a window plus
what a site can't do: **alarms that ring** (full screen over the lock screen,
until stopped or snoozed) for the alerts of patterns marked *Ring like an
alarm*. The phone sets them a week ahead from valet's list, so they ring
offline too, and looks again every few hours, after a restart, and when
events or patterns change. It needs no Play Store and no developer account:

```sh
cd android
./gradlew assembleDebug            # Android Studio's JDK: JAVA_HOME=…/Android Studio.app/Contents/jbr/Contents/Home
adb install -r app/build/outputs/apk/debug/app-debug.apk   # or copy the .apk to the phone and open it
```

At first launch it asks for valet's address. Updates must be signed with the
same key (the debug key in `~/.android/`), or the app has to be removed first.

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

End-to-end tests run valet against GreenMail, in a scratch directory holding
`gm/greenmail-standalone-2.1.14.jar` (from Maven Central), with Java and
Python with `requests` (and `websocket-client` for the browser checks; `PYTHON=…` picks the interpreter):

```sh
cargo build
e2e/run_mail_e2e.sh <dir>   # IMAP; leaves <dir>/mdata for run_ui.sh
e2e/run_ui.sh <dir>         # the tree editor's routes
e2e/run_webmail.sh <dir>
e2e/run_calendar.sh <dir>   # the calendar page and the app; with Chrome, also in a headless Chrome, offline included
```
