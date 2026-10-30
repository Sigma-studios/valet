//! Webmail: folders (the user's tree, then each account's own), message
//! lists, reading (HTML mail in a locked-down frame), actions, writing, and
//! the tree's editor. It works on valet's copy, like the mail apps do, so
//! what happens here reaches the providers and the apps.

use std::borrow::Cow;

use axum::body::Body;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use maud::{Markup, PreEscaped, html};
use serde::Deserialize;

use super::*;
use crate::accounts::Account;
use crate::mailstore::{ALL_INBOXES, ALL_MAIL, Mailbox, Msg, Node, Place, SPECIALS, Shift, StoreMode};

/// Rows a list shows at first, and fetches at a time as it is scrolled.
const PAGE: usize = 50;
/// Rows a list grows to as it is scrolled; past them, its Older link.
const MAX_ROWS: usize = 1000;

fn parse(raw: &[u8]) -> Option<mail_parser::Message<'_>> {
    mail_parser::MessageParser::default().parse(raw)
}

fn has_flag(m: &Msg, flag: &str) -> bool {
    m.flags.iter().any(|f| f.eq_ignore_ascii_case(flag))
}

/// "Name" or the address, for a list.
fn who(a: Option<&mail_parser::Address>) -> String {
    let Some(first) = a.and_then(|a| a.first()) else { return String::new() };
    first.name().filter(|n| !n.trim().is_empty()).or(first.address()).unwrap_or("").to_string()
}

/// "Name <address>, …", in full.
fn everyone(a: Option<&mail_parser::Address>) -> String {
    a.map(|a| {
        a.iter()
            .filter_map(|x| {
                let addr = x.address()?;
                Some(match x.name().filter(|n| !n.trim().is_empty()) {
                    Some(n) => format!("{n} <{addr}>"),
                    None => addr.to_string(),
                })
            })
            .collect::<Vec<_>>()
            .join(", ")
    })
    .unwrap_or_default()
}

fn addresses_of(a: Option<&mail_parser::Address>) -> Vec<String> {
    a.map(|a| a.iter().filter_map(|x| x.address().map(str::to_string)).collect()).unwrap_or_default()
}

/// Seconds since 1970, in the settings' zone; the page's script shortens it
/// (today's mail: just the hour).
fn time(t: i64, long: bool) -> Markup {
    use chrono::TimeZone;
    let Some(d) = crate::calendar::default_zone().timestamp_opt(t, 0).single() else { return html! {} };
    html! {
        time datetime=(d.to_rfc3339()) title=(crate::calendar::zone_note(d)) data-t=(t) data-long=[long.then_some("1")] { (d.format("%d/%m/%Y %H:%M")) }
    }
}

const TIME_SCRIPT: &str = r#"
// Also for parts of the page put in later (a message opened from the list).
window.valetTimes = (root) => {
  // Each format made once (a list grown long has a thousand times).
  const { opts } = window.valetTime, f = (o, locale = []) => new Intl.DateTimeFormat(locale, opts(o));
  const day = f({}, 'en-CA'), full = f({ dateStyle: 'full', timeStyle: 'short' }), hour = f({ hour: '2-digit', minute: '2-digit' });
  const month = f({ day: 'numeric', month: 'short' }), date = f({}), today = day.format(new Date());
  for (const el of root.querySelectorAll('time[data-t]')) {
    const d = new Date(el.dataset.t * 1000), that = day.format(d);
    el.textContent = el.dataset.long ? full.format(d)
      : that === today ? hour.format(d)
      : that.slice(0, 4) === today.slice(0, 4) ? month.format(d)
      : date.format(d);
  }
};
valetTimes(document);
"#;

type Folders = (Vec<(Node, Mailbox)>, Vec<(Account, Vec<Mailbox>)>);

/// Folders: the tree's nodes (depth first), then each account's, in the
/// accounts' order.
fn folders(app: &App) -> Folders {
    let boxes = app.mail.mailboxes();
    let top: Vec<(Node, Mailbox)> =
        app.mail.tree().into_iter().filter_map(|n| boxes.iter().find(|b| b.id == n.id).map(|b| (n, b.clone()))).collect();
    let order = |b: &Mailbox| {
        let rank = ["\\Inbox", "\\Drafts", "\\Sent", "\\Archive", "\\Junk", "\\Trash"].iter().position(|s| b.special.eq_ignore_ascii_case(s));
        let inbox = b.remote.as_deref().is_some_and(|r| r.eq_ignore_ascii_case("INBOX"));
        (if inbox { 0 } else { rank.map_or(99, |r| r + 1) }, b.name.to_lowercase())
    };
    let per = app
        .accounts()
        .into_iter()
        .map(|a| {
            let mut mine: Vec<Mailbox> = boxes.iter().filter(|b| b.account.as_deref() == Some(a.id.as_str())).cloned().collect();
            mine.sort_by_key(order);
            (a, mine)
        })
        .collect();
    (top, per)
}

/// A node's label, or an account folder's path within its account
/// (`Accounts/Work/Projects/2026` → `Projects/2026`).
fn short_name(b: &Mailbox) -> String {
    if b.is_node() {
        return if b.name == ALL_INBOXES { "Inbox".into() } else { b.name.rsplit('/').next().unwrap_or(&b.name).to_string() };
    }
    if b.is_inbox() { "Inbox".into() } else { b.path().to_string() }
}

/// Where a folder is, in full: `Work › Clients`, `Gmail › Spam`.
fn place_name(b: &Mailbox) -> String {
    if b.is_node() {
        return if b.name == ALL_INBOXES { "Inbox".into() } else { b.name.replace('/', " › ") };
    }
    let account = b.name.split('/').nth(1).unwrap_or("");
    format!("{account} › {}", short_name(b))
}

/// Spaces before a node's label in a menu, by its depth.
fn indent(depth: usize) -> String {
    "\u{a0}\u{a0}\u{a0}".repeat(depth)
}

/// The list follows the mail: told by `/mail/events` that something
/// changed, it fetches itself again and swaps in place (with the unread
/// counts), so what is ticked, scrolled to and open stays. (app.js counts the
/// badge on the same connection.) Refresh checks the
/// accounts first (the folder shown right after the inboxes) and says what
/// came of it beside the button.
const REFRESH_SCRIPT: &str = r#"
{
  const b = document.querySelector('button.refresh'), note = document.querySelector('.refresh-note');
  const say = (text, bad) => { note.textContent = text; note.classList.toggle('err', !!bad); };
  // Swaps the list in; answers how many rows are new, or where valet sent the page instead.
  const swap = async () => {
    const split = document.querySelector('.split');
    const page = await fetch(split.dataset.list, { cache: 'no-store' });
    if (page.redirected) return { gone: page.url };
    if (!page.ok) throw new Error(page.status);
    const doc = new DOMParser().parseFromString(await page.text(), 'text/html');
    const col = split.querySelector('.listcol'), fresh = doc.querySelector('.listcol');
    let added = 0;
    if (col && fresh) {
      const before = new Set([...col.querySelectorAll('a.link')].map((a) => a.dataset.id));
      const ticked = new Set([...col.querySelectorAll('input[name=id]:checked')].map((i) => i.value));
      const open = col.querySelector('.row.open a.link')?.dataset.id;
      col.replaceChildren(...fresh.childNodes);
      for (const i of col.querySelectorAll('input[name=id]')) i.checked = ticked.has(i.value);
      const a = open && col.querySelector(`a.link[data-id="${open}"]`);
      if (a) { a.closest('.row').classList.add('open'); a.href = split.dataset.list; }
      valetTimes(col);
      document.dispatchEvent(new Event('valet:list'));
      added = [...col.querySelectorAll('a.link')].filter((a) => !before.has(a.dataset.id)).length;
    }
    // The toolbar's parts saying what the list shows: the Filter button, chips, links.
    for (const el of document.querySelectorAll('[data-swap]')) {
      const f = doc.querySelector(`[data-swap="${el.dataset.swap}"]`);
      if (f) el.replaceWith(f);
    }
    for (const a of document.querySelectorAll('nav.folders a.folder')) {
      const badge = doc.querySelector(`nav.folders a.folder[href="${a.getAttribute('href')}"] .badge`);
      a.querySelector('.badge')?.remove();
      if (badge) a.append(badge);
    }
    return { added };
  };
  // One swap at a time; changes told during one make one more after it.
  let chain = Promise.resolve(), next = null;
  const update = () => {
    if (!next) next = chain = chain.catch(() => {}).then(() => { next = null; return swap(); });
    return next;
  };
  window.valetList = { update };
  // Scrolled near its end (or too short to fill the window), the list adds
  // its next rows, and its address says so: reloading, going back to it,
  // closing a message or acting on some shows as many. Changes swap them
  // all in, as above.
  // (The list comes after this script in the page.)
  const relink = (split, old, grown) => {
    for (const a of split.querySelectorAll('.list a.link[data-open]')) {
      a.dataset.open = `${grown}&open=${a.dataset.id}`;
      a.href = a.closest('.row.open') ? grown : a.dataset.open;
    }
    for (const el of split.querySelectorAll('[href], input[name=back]')) {
      if (el.matches('.list a.link')) continue;
      if (el.getAttribute('href') === old) el.setAttribute('href', grown);
      if (el.value === old) el.value = grown;
    }
    const open = new URLSearchParams(location.search).get('open');
    history.replaceState(history.state, '', open ? `${grown}&open=${open}` : grown);
    split.dataset.list = grown;
  };
  const more = async () => {
    const split = document.querySelector('.split');
    const pager = split.querySelector('.listcol .pager[data-more]'), list = split.querySelector('#bulk .list');
    if (!pager || !list) return;
    const r = await fetch(pager.dataset.more, { cache: 'no-store' });
    if (r.redirected || !r.ok) throw new Error(r.status);
    const t = document.createElement('template');
    t.innerHTML = await r.text();
    // Rows moved down meanwhile (new mail above) are there already.
    const have = new Set([...list.querySelectorAll('a.link')].map((a) => a.dataset.id));
    const rows = [...t.content.querySelectorAll('.more > .row')].filter((row) => !have.has(row.querySelector('a.link').dataset.id));
    valetTimes(t.content);
    relink(split, split.dataset.list, pager.dataset.grown);
    list.append(...rows);
    pager.replaceWith(t.content.querySelector('.pager'));
    document.dispatchEvent(new Event('valet:list'));
  };
  let growing = false, looking = false;
  const grow = () => {
    const pager = document.querySelector('.split .listcol .pager[data-more]');
    // Not while the list is hidden (a message read instead, on a narrow screen).
    if (growing || !pager || !pager.offsetParent || !navigator.onLine) return;
    if (pager.getBoundingClientRect().top > innerHeight + 1200) return;
    growing = true;
    // Failing (valet away), it tries again a little later.
    chain = chain.catch(() => {}).then(more).then(() => true, () => false).then((ok) => { growing = false; ok ? look() : setTimeout(look, 5000); });
  };
  const look = () => { if (!looking) { looking = true; requestAnimationFrame(() => { looking = false; grow(); }); } };
  // Whichever scrolls: the window, or the list's column.
  document.addEventListener('scroll', look, { capture: true, passive: true });
  addEventListener('resize', look);
  document.addEventListener('valet:list', look);
  document.addEventListener('DOMContentLoaded', look);
  b.addEventListener('click', async () => {
    if (!navigator.onLine) { say("You're offline: new mail can be checked once you're back online.", true); return; }
    b.disabled = true;
    b.textContent = '↻ Checking…';
    say('');
    try {
      const r = await fetch(`/mail/refresh?folder=${b.dataset.folder}`, { method: 'POST' });
      if (r.status === 401) { location = '/login'; return; }
      const res = await r.json();
      const { added, gone } = await update();
      if (gone) { location = gone; return; }
      const told = res.failed.map((f) => `${f.account}: ${f.error}`);
      if (res.waiting.length) told.push(`still checking ${res.waiting.join(', ')}`);
      if (!told.length) told.push(added ? `${added} new` : 'Up to date');
      say(told.join(' · '), res.failed.length > 0);
    } catch (_) {
      say("Couldn't check: valet didn't answer.", true);
    } finally {
      b.disabled = false;
      b.textContent = '↻ Refresh';
    }
  });
  // Live: a hidden page catches up when shown again.
  let behind = false;
  const changed = () => {
    if (document.hidden) { behind = true; return; }
    update().then((r) => { if (r.gone) events.close(); }, () => {});
  };
  const events = window.valetEvents = new EventSource('/mail/events');
  events.addEventListener('changed', changed);
  // Back after a break (valet restarted, the network dropped): what changed meanwhile.
  let connected = false;
  events.addEventListener('open', () => { if (connected) changed(); connected = true; });
  document.addEventListener('visibilitychange', () => { if (!document.hidden && behind) { behind = false; changed(); } });
}
"#;

/// Folders start closed, but for the way to the one shown; what is opened
/// stays open from page to page until the browser is closed.
const SIDEBAR_SCRIPT: &str = r#"
(() => {
  const nav = document.querySelector('nav.folders');
  if (!nav) return;
  const opened = new Set(JSON.parse(sessionStorage.getItem('valet.open') || '[]'));
  const rows = [...nav.querySelectorAll('.node')];
  // The folders around the one shown.
  const here = rows.findIndex((r) => r.querySelector('a.folder.on'));
  const around = new Set();
  for (let i = here - 1, d = here >= 0 ? +rows[here].dataset.depth : 0; i >= 0 && d > 0; i--) {
    if (+rows[i].dataset.depth < d) { d = +rows[i].dataset.depth; around.add(rows[i].dataset.id); }
  }
  const apply = () => {
    let limit = Infinity;
    for (const r of rows) {
      const d = +r.dataset.depth, open = opened.has(r.dataset.id) || around.has(r.dataset.id);
      if (d <= limit) limit = Infinity;
      r.hidden = d > limit;
      r.querySelector('button.twisty')?.setAttribute('aria-expanded', String(open));
      if (!r.hidden && !open) limit = d;
    }
  };
  for (const t of nav.querySelectorAll('button.twisty')) {
    t.addEventListener('click', () => {
      const id = t.closest('.node').dataset.id;
      if (t.getAttribute('aria-expanded') === 'true') { opened.delete(id); around.delete(id); } else opened.add(id);
      sessionStorage.setItem('valet.open', JSON.stringify([...opened]));
      apply();
    });
  }
  apply();
  const acc = document.getElementById('accounts');
  if (!acc.open) acc.open = sessionStorage.getItem('valet.accounts') === 'open';
  acc.addEventListener('toggle', () => sessionStorage.setItem('valet.accounts', acc.open ? 'open' : 'closed'));
})();
"#;

/// The arrow of a folder that has folders inside, turned by the stylesheet.
const CHEVRON: &str = r#"<svg viewBox="0 0 16 16" width="12" height="12" aria-hidden="true"><path d="M6 3.5 10.5 8 6 12.5" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/></svg>"#;

fn sidebar(app: &App, current: i64) -> Markup {
    let (tree, per) = folders(app);
    let item = |b: &Mailbox| {
        // All mail's unread are the inbox's, and Spam's: no count.
        let unseen = if b.is_all_mail() { 0 } else { app.mail.count(b.id).1 };
        html! {
            a.folder.on[b.id == current] href={"/box/" (b.id)} title=(place_name(b)) {
                span.name { (short_name(b)) }
                @if unseen > 0 { span.badge title={(unseen) " unread"} { (unseen) } }
            }
        }
    };
    let has_kids = |i: usize| tree.get(i + 1).is_some_and(|(n, _)| n.depth > tree[i].0.depth);
    let in_accounts = per.iter().any(|(_, boxes)| boxes.iter().any(|b| b.id == current));
    html! {
        nav.folders {
            a.compose href="/compose" { "Write" }
            div.section { "Folders" }
            @for (i, (n, b)) in tree.iter().enumerate() {
                div.node data-id=(n.id) data-depth=(n.depth) style={"--depth:" (n.depth)} {
                    @if has_kids(i) { button.twisty type="button" aria-label="Show or hide the folders inside" aria-expanded="false" { (PreEscaped(CHEVRON)) } }
                    @else { span.twisty {} }
                    (item(b))
                }
            }
            a.edit-tree href="/tree" { "Edit folders" }
            details.accounts #accounts open[in_accounts] {
                summary { span.twisty { (PreEscaped(CHEVRON)) } "Accounts" }
                @for (a, boxes) in &per {
                    div.account { span { (a.name) } a href={"/folders/" (a.id)} title="Manage this account's folders" { "Edit" } }
                    @for b in boxes { (item(b)) }
                }
            }
        }
        // Phones: the folders as one menu.
        select.folders-menu onchange="location = this.value" {
            @for (n, b) in &tree { option value={"/box/" (b.id)} selected[b.id == current] { (indent(n.depth)) (short_name(b)) } }
            option value="/tree" { "Edit folders…" }
            @for (a, boxes) in &per {
                optgroup label={"Accounts › " (a.name)} {
                    @for b in boxes { option value={"/box/" (b.id)} selected[b.id == current] { (short_name(b)) } }
                }
            }
        }
        script { (PreEscaped(SIDEBAR_SCRIPT)) }
    }
}

const MAIL_CSS: &str = r#"
.mail { display:grid; grid-template-columns: 230px 1fr; gap:20px; align-items:start; }
.folders { display:flex; flex-direction:column; gap:1px; position:sticky; top:12px; max-height:calc(100vh - 24px); overflow-y:auto; scrollbar-width:thin; scrollbar-color:var(--line) transparent; font-size:14px; }
.folders .section, .folders details.accounts summary { padding:4px 8px; font-size:11px; font-weight:600; text-transform:uppercase; letter-spacing:.06em; color:var(--muted); }
.folders .section { margin-top:6px; }
.folders .node { display:flex; align-items:center; padding-left:calc(var(--depth, 0) * 16px); } .folders .node[hidden] { display:none; }
.folders .twisty { flex:none; width:22px; height:28px; display:flex; align-items:center; justify-content:center; padding:0; border:none; border-radius:4px; background:none; color:var(--muted); cursor:pointer; }
.folders button.twisty:hover { background:var(--line); color:var(--fg); }
.folders .twisty svg { transition:transform .12s; }
.folders button.twisty[aria-expanded=true] svg, .folders details[open] > summary .twisty svg { transform:rotate(90deg); }
.folders a.folder { flex:1; min-width:0; display:flex; align-items:center; gap:8px; padding:4px 8px; border-radius:6px; color:var(--fg); text-align:left; }
.folders a.folder .name { flex:1; min-width:0; overflow:hidden; text-overflow:ellipsis; white-space:nowrap; }
.folders a.folder:hover { background:var(--line); text-decoration:none; }
.folders a.folder.on { background:color-mix(in srgb, var(--accent) 20%, transparent); color:var(--fg); font-weight:600; box-shadow:inset 3px 0 0 var(--accent); }
.folders .badge { flex:none; min-width:20px; padding:0 6px; border-radius:10px; background:var(--line); color:var(--fg); font-size:11px; font-weight:700; line-height:18px; text-align:center; }
.folders a.folder.on .badge { background:var(--accent); color:#fff; }
.folders .account { display:flex; justify-content:space-between; align-items:baseline; margin:10px 8px 2px 30px; font-size:12px; font-weight:600; color:var(--muted); }
.folders .account a { font-weight:400; color:var(--muted); }
.folders details.accounts > a.folder { margin-left:22px; }
.folders a.edit-tree { font-size:13px; color:var(--muted); padding:6px 8px 0 30px; }
.folders details.accounts { margin-top:12px; border-top:1px solid var(--line); padding-top:8px; }
.folders details.accounts summary { cursor:pointer; list-style:none; display:flex; align-items:center; padding-left:0; }
.folders details.accounts summary::-webkit-details-marker { display:none; }
.folders details.accounts summary .twisty { height:20px; }
a.compose { display:block; text-align:center; background:var(--accent); color:#fff; border-radius:6px; padding:7px 10px; margin-bottom:10px; font-weight:600; }
a.compose:hover { text-decoration:none; filter:brightness(1.1); }
.folders-menu { display:none; width:100%; margin-bottom:10px; }
.list { background:var(--card); border:1px solid var(--line); border-radius:8px; overflow:hidden; }
/* Two greys tell them apart, with the bold: unread rows lighter, read darker. */
:root { --row-unread:#ffffff; --row-read:#ecebe9; --row-hover:#e2e0dd; }
@media (prefers-color-scheme: dark) { :root { --row-unread:#34302d; --row-read:#1f1c1a; --row-hover:#3d3936; } }
.list .row { display:flex; align-items:stretch; border-bottom:1px solid var(--line); background:var(--row-read); }
.list .row:last-child { border-bottom:none; }
.list .row.unread { background:var(--row-unread); }
.list .row:hover { background:var(--row-hover); }
.list .row.picked { background:color-mix(in srgb, var(--accent) 18%, var(--row-unread)); }
.list .row.open { background:color-mix(in srgb, var(--accent) 12%, var(--row-unread)); box-shadow:inset 3px 0 0 var(--accent); }
.list label.pick { flex:none; display:flex; flex-direction:row; align-items:center; margin:0; padding:0 2px 0 12px; cursor:pointer; }
.list label.pick input { margin:0; cursor:pointer; }
.list a.link { flex:1; min-width:0; display:grid; grid-template-columns: 14px minmax(110px, 200px) 1fr auto; gap:10px; align-items:baseline; padding:8px 12px 8px 8px; color:var(--fg); }
.list a.link:hover { text-decoration:none; }
.list .row.unread .from, .list .row.unread .subject { font-weight:700; }
.list .from, .list .line { overflow:hidden; text-overflow:ellipsis; white-space:nowrap; }
.list .preview { color:var(--muted); }
.list .when { color:var(--muted); font-size:13px; white-space:nowrap; }
.list .star { color:#eab308; font-size:12px; }
.tag { font-size:11px; border:1px solid var(--line); border-radius:4px; padding:0 4px; color:var(--muted); margin-right:4px; }
/* In All mail: a message in Spam or Trash. */
.tag.bin { color:var(--bad); border-color:currentColor; }
.toolbar button.danger { color:var(--bad); }
/* "Move to…" as wide as what it shows, not as its longest folder name (which
   the open menu still shows in full), so the actions after it don't wrap. */
.toolbar select[name=to] { field-sizing:content; max-width:9.5em; }
/* A message's many actions, compact so they hold on one line where they can. */
.toolbar.msg-actions { gap:5px; }
.toolbar.msg-actions :is(a.button, button, select) { padding:4px 7px; font-size:14px; }
.toolbar button.refresh { white-space:nowrap; }
.toolbar .refresh-note:empty { display:none; }
/* The toolbar: refresh and search, or, while messages are ticked, what to do with them. */
.toolbar .normal, .toolbar .bulk-actions { flex:1; display:flex; gap:8px; flex-wrap:wrap; align-items:center; }
.toolbar .bulk-actions { display:none; }
.pane:has(#bulk input[name=id]:checked) .toolbar .normal { display:none; }
.pane:has(#bulk input[name=id]:checked) .toolbar .bulk-actions { display:flex; }
.toolbar .bulk-actions .count { font-size:14px; font-weight:600; margin-right:4px; }
.toolbar label.pickall { display:flex; flex-direction:row; align-items:center; margin:0 4px 0 12px; }
/* A message opened from the list: beside it when there's room (the list
   then shows subjects only), instead of it otherwise. */
@media (min-width: 1260px) {
  .pane:has(> .split.reading) { overflow:hidden; display:flex; flex-direction:column; padding-bottom:0; }
  .split.reading { flex:1; min-height:0; display:grid; grid-template-columns:minmax(220px, 26%) 1fr; gap:14px; }
  .split.reading > .listcol, .split.reading > .readcol { min-height:0; overflow-y:auto; padding-bottom:16px; scrollbar-width:thin; }
  .split.reading .list a.link { display:block; padding:9px 10px 9px 8px; }
  .split.reading .list .star, .split.reading .list .from, .split.reading .list .tag, .split.reading .list .preview, .split.reading .list .when { display:none; }
  .split.reading .list .line { display:block; }
  .split.reading .readcol > .toolbar { position:sticky; top:0; z-index:2; background:var(--bg); padding-bottom:10px; margin-bottom:2px; }
}
@media (max-width: 1259px) { .split.reading > .listcol { display:none; } }
.toolbar button.refresh:disabled { opacity:.7; cursor:progress; }
.headers { margin-bottom:12px; } .headers h1 { margin:0 0 8px; }
.headers .meta { color:var(--muted); font-size:13px; }
.make-rule { font-size:13px; } .make-rule summary { cursor:pointer; color:var(--accent); width:fit-content; }
.make-rule form { margin:6px 0 2px; } .make-rule label.also { flex-direction:row; align-items:center; gap:4px; }
.reader iframe { width:100%; border:0; background:#fff; border-radius:6px; min-height:200px; display:block; }
.reader pre { white-space:pre-wrap; word-wrap:break-word; font:inherit; margin:0; }
.atts { display:flex; flex-wrap:wrap; gap:8px; margin-top:12px; }
.atts a, .atts .removed { border:1px solid var(--line); border-radius:6px; padding:4px 10px; background:var(--card); }
.atts .removed { border-style:dashed; color:var(--muted); cursor:help; }
.notice { background:var(--card); border:1px solid var(--line); border-radius:6px; padding:6px 10px; font-size:13px; margin-bottom:8px; }
.pager { display:flex; justify-content:space-between; margin-top:10px; }
form.write { display:flex; flex-direction:column; gap:10px; }
form.write textarea { min-height:320px; font:inherit; padding:8px; border:1px solid var(--line); border-radius:6px; background:var(--bg); color:var(--fg); resize:vertical; }
/* Wider than a phone: the window is the app; the bar and the folders stay,
   the list (or message) scrolls under its pinned toolbar. */
@media (min-width: 721px) {
  body:has(> header) > main { display:flex; flex-direction:column; overflow:hidden; padding-bottom:0; }
  .mail { flex:1; min-height:0; align-items:stretch; }
  .mail > nav.folders { position:static; max-height:none; min-height:0; padding-bottom:16px; }
  .mail > .pane { min-width:0; min-height:0; overflow-y:auto; padding-bottom:16px; scrollbar-width:thin; }
  .pane > .toolbar { position:sticky; top:0; z-index:2; background:var(--bg); padding-bottom:10px; margin-bottom:2px; }
}
@media (max-width: 720px) {
  .mail { display:block; } nav.folders { display:none; } .folders-menu { display:block; }
  .list a.link { grid-template-columns: 1fr auto; } .list .star { display:none; }
  .list .line { grid-column: 1 / -1; }
}
"#;

/// A list's toolbar: search, the Filter button and its menu, the filters on
/// as chips (also the Invitations page's).
pub(super) const FILTER_CSS: &str = r#"
.toolbar { display:flex; gap:8px; flex-wrap:wrap; align-items:center; margin-bottom:12px; }
.toolbar form { display:inline; margin:0; }
.toolbar .search { flex:1; min-width:160px; display:flex; position:relative; }
.toolbar .search input { flex:1; min-width:0; }
/* Searching (or filtering): a spinner in the field, the list dimmed; only
   past a moment, so quick answers don't flicker. */
.toolbar .search::after { content:""; position:absolute; right:30px; top:50%; width:12px; height:12px; margin-top:-8px;
  border:2px solid var(--muted); border-right-color:transparent; border-radius:50%; animation:spin .7s linear infinite;
  opacity:0; pointer-events:none; transition:opacity .1s; }
.toolbar.searching .search::after { opacity:1; transition-delay:.15s; }
.split .listcol { transition:opacity .1s; }
.split.searching .listcol { opacity:.5; transition-delay:.15s; }
.toolbar a.button, .toolbar button { background:var(--card); color:var(--fg); border:1px solid var(--line); border-radius:6px; padding:5px 10px; }
.toolbar a.button:hover, .toolbar button:hover { background:var(--line); text-decoration:none; }
/* The Filter button, its menu, and the filters on as chips under the toolbar. */
.toolbar details.filter { position:relative; }
.toolbar details.filter > summary { list-style:none; cursor:pointer; user-select:none; display:flex; align-items:center; gap:6px; background:var(--card); color:var(--fg); border:1px solid var(--line); border-radius:6px; padding:5px 10px; white-space:nowrap; }
.toolbar details.filter > summary::-webkit-details-marker { display:none; }
.toolbar details.filter > summary:hover, .toolbar details.filter[open] > summary { background:var(--line); }
.toolbar details.filter > summary.on { border-color:var(--accent); color:var(--accent); }
.toolbar details.filter > summary .count { background:var(--accent); color:#fff; border-radius:9px; font-size:11px; font-weight:700; line-height:17px; padding:0 6px; }
.filter .panel { position:absolute; right:0; top:calc(100% + 6px); z-index:10; width:320px; max-width:calc(100vw - 24px); max-height:calc(100vh - 130px); overflow-y:auto; display:flex; flex-direction:column; gap:16px; background:var(--card); border:1px solid var(--line); border-radius:10px; padding:14px; box-shadow:0 8px 28px rgb(0 0 0 / .2); }
.filter fieldset { border:none; margin:0; padding:0; display:flex; flex-direction:column; gap:8px; min-width:0; }
.filter legend { padding:0; margin-bottom:8px; font-size:11px; font-weight:600; text-transform:uppercase; letter-spacing:.06em; color:var(--muted); }
.filter .segmented { display:flex; border:1px solid var(--line); border-radius:7px; overflow:hidden; }
.filter .segmented label { flex:1; position:relative; margin:0; }
.filter .segmented label + label { border-left:1px solid var(--line); }
.filter .segmented input { position:absolute; opacity:0; pointer-events:none; }
.filter .segmented span { display:block; text-align:center; padding:5px 0; font-size:14px; color:var(--fg); cursor:pointer; }
.filter .segmented span:hover { background:var(--line); }
.filter .segmented input:checked + span { background:var(--accent); color:#fff; }
.filter .segmented input:focus-visible + span { outline:2px solid var(--accent); outline-offset:-2px; }
.filter label.check { flex-direction:row; align-items:center; gap:8px; font-size:14px; color:var(--fg); cursor:pointer; }
.filter .person { position:relative; display:flex; flex-direction:column; gap:2px; font-size:13px; color:var(--muted); }
.filter .person input, .filter select, .filter .range input { width:100%; }
.filter .range { display:none; gap:8px; } .filter .range label { flex:1; min-width:0; }
.filter:has(select[name=when] option[value=custom]:checked) .range { display:flex; }
.filter .suggest { position:absolute; left:0; right:0; top:100%; z-index:11; margin:3px 0 0; padding:4px; list-style:none; max-height:260px; overflow-y:auto; background:var(--card); border:1px solid var(--line); border-radius:8px; box-shadow:0 6px 20px rgb(0 0 0 / .18); }
.filter .suggest li { display:flex; flex-direction:column; padding:5px 8px; border-radius:5px; cursor:pointer; font-size:14px; color:var(--fg); overflow-wrap:anywhere; }
.filter .suggest li .addr { font-size:12px; color:var(--muted); }
.filter .suggest li:hover, .filter .suggest li[aria-selected=true] { background:var(--line); }
.filter .foot { display:flex; justify-content:space-between; align-items:center; border-top:1px solid var(--line); padding-top:12px; }
.toolbar .filters-on:empty { display:none; }
.toolbar .filters-on { flex-basis:100%; display:flex; flex-wrap:wrap; gap:6px; align-items:center; }
.filters-on a.chip { display:inline-flex; align-items:center; gap:6px; padding:2px 8px 2px 10px; border-radius:14px; font-size:13px; color:var(--fg); background:color-mix(in srgb, var(--accent) 14%, var(--card)); border:1px solid color-mix(in srgb, var(--accent) 35%, transparent); }
.filters-on a.chip span { font-size:11px; color:var(--muted); }
.filters-on a.chip:hover { text-decoration:none; border-color:var(--accent); } .filters-on a.chip:hover span { color:var(--fg); }
"#;

fn mail_page(title: &str, current: i64, app: &App, body: Markup) -> Response {
    page_wide(
        title,
        "/inbox",
        html! {
            style { (PreEscaped(FILTER_CSS)) (PreEscaped(MAIL_CSS)) }
            div.mail {
                (sidebar(app, current))
                div.pane { (body) }
            }
            script { (PreEscaped(TIME_SCRIPT)) }
        },
    )
}

/// INBOX, shown at its own address (where the installed app starts, and
/// what the service worker keeps for offline reading).
pub async fn inbox(State(app): St, headers: HeaderMap, q: Query<ListQuery>) -> Response {
    let id = app.mail.all_inboxes();
    list(State(app), headers, Path(id), q).await
}

#[derive(Deserialize, Default)]
pub struct ListQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    page: usize,
    ok: Option<String>,
    err: Option<String>,
    images: Option<String>,
    /// The message read beside the list.
    open: Option<i64>,
    /// Only that message's part of the page, not marked read (the list's
    /// script fetches it ahead, then shows it).
    fragment: Option<String>,
    /// The rows shown from the page on (`PAGE` at first, more as the list
    /// is scrolled), a multiple of `PAGE`.
    n: Option<usize>,
    /// Only the rows from that one on, and the pager (the list's script
    /// adds them as it is scrolled).
    more: Option<usize>,
    // The Filter menu's (see `Filter`); empty, no filtering.
    /// `unread` or `read`.
    #[serde(default)]
    read: String,
    #[serde(default)]
    starred: String,
    /// Part of a sender's name or address; of a recipient's (To or Cc).
    #[serde(default)]
    from: String,
    #[serde(default)]
    to: String,
    #[serde(default)]
    attachment: String,
    #[serde(default)]
    invite: String,
    /// Not the mail in Spam or Trash (for All mail).
    #[serde(default)]
    hide: String,
    /// One of `WHEN`; `custom`: from `since` to `until`, days both included.
    #[serde(default)]
    when: String,
    #[serde(default)]
    since: String,
    #[serde(default)]
    until: String,
}

/// The filters' keys in a list's address.
const FILTER_KEYS: [&str; 10] = ["read", "starred", "from", "to", "attachment", "invite", "hide", "when", "since", "until"];

/// The Filter menu's dates.
const WHEN: [(&str, &str); 6] = [
    ("", "Any time"),
    ("today", "Today"),
    ("week", "Last 7 days"),
    ("month", "Last 30 days"),
    ("year", "Last 12 months"),
    ("custom", "Between dates…"),
];

impl ListQuery {
    /// The search and the filters on, as they go in the list's address.
    fn criteria(&self) -> Vec<(&'static str, &str)> {
        let custom = self.when == "custom";
        [
            ("q", self.q.as_str()),
            ("read", &self.read),
            ("starred", &self.starred),
            ("from", &self.from),
            ("to", &self.to),
            ("attachment", &self.attachment),
            ("invite", &self.invite),
            ("hide", &self.hide),
            ("when", &self.when),
            ("since", if custom { &self.since } else { "" }),
            ("until", if custom { &self.until } else { "" }),
        ]
        .into_iter()
        .filter(|(_, v)| !v.trim().is_empty())
        .collect()
    }

    /// The list's page, with its search and filters but those of `drop`.
    fn link(&self, id: i64, page: usize, drop: &[&str]) -> String {
        let mut l = format!("/box/{id}?page={page}");
        for (k, v) in self.criteria().into_iter().filter(|(k, _)| !drop.contains(k)) {
            l.push_str(&format!("&{k}={}", percent_encoding::utf8_percent_encode(v, percent_encoding::NON_ALPHANUMERIC)));
        }
        l
    }

    /// The filters on, as said on their chips, each with the keys that drop it.
    fn chips(&self) -> Vec<(String, &'static [&'static str])> {
        let mut on: Vec<(String, &'static [&'static str])> = Vec::new();
        match self.read.as_str() {
            "unread" => on.push(("Unread".into(), &["read"])),
            "read" => on.push(("Read".into(), &["read"])),
            "new" => on.push(("New".into(), &["read"])),
            _ => {}
        }
        if !self.starred.is_empty() {
            on.push(("Starred".into(), &["starred"]));
        }
        if !self.from.trim().is_empty() {
            on.push((format!("From: {}", self.from.trim()), &["from"]));
        }
        if !self.to.trim().is_empty() {
            on.push((format!("To: {}", self.to.trim()), &["to"]));
        }
        if !self.attachment.is_empty() {
            on.push(("With attachments".into(), &["attachment"]));
        }
        if !self.invite.is_empty() {
            on.push(("Invitations".into(), &["invite"]));
        }
        if !self.hide.is_empty() {
            on.push(("Without Spam and Trash".into(), &["hide"]));
        }
        let dates: &'static [&'static str] = &["when", "since", "until"];
        if self.when == "custom" {
            let day = |s: &str| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok();
            let say = |d: chrono::NaiveDate| d.format("%-d %b %Y").to_string();
            match ordered(day(&self.since), day(&self.until)) {
                (Some(a), Some(b)) if a == b => on.push((say(a), dates)),
                (Some(a), Some(b)) => on.push((format!("{} – {}", say(a), say(b)), dates)),
                (Some(a), None) => on.push((format!("Since {}", say(a)), dates)),
                (None, Some(b)) => on.push((format!("Until {}", say(b)), dates)),
                (None, None) => {}
            }
        } else if let Some((_, label)) = WHEN.iter().find(|(w, _)| !w.is_empty() && *w == self.when) {
            on.push((label.to_string(), dates));
        }
        on
    }
}

/// Two days, the earlier first.
fn ordered(a: Option<chrono::NaiveDate>, b: Option<chrono::NaiveDate>) -> (Option<chrono::NaiveDate>, Option<chrono::NaiveDate>) {
    match (a, b) {
        (Some(x), Some(y)) if y < x => (Some(y), Some(x)),
        other => other,
    }
}

/// What a list keeps: the messages meeting every filter on.
#[derive(Default)]
struct Filter {
    seen: Option<bool>,
    /// Only those the app's badge counts (real rows; see `MailStore::badge`), for `read=new`.
    new: Option<std::collections::HashSet<i64>>,
    starred: bool,
    /// Lowercase.
    from: String,
    to: String,
    attachment: bool,
    invite: bool,
    /// Unix seconds: from (included), until (excluded).
    after: Option<i64>,
    before: Option<i64>,
}

impl Filter {
    fn of(q: &ListQuery) -> Filter {
        use chrono::{Days, Months, TimeZone};
        let zone = crate::calendar::default_zone();
        let today = chrono::Utc::now().with_timezone(&zone).date_naive();
        // A day's midnight in the settings' zone (or the first hour it has).
        let start = |d: chrono::NaiveDate| {
            (0..3).find_map(|h| zone.from_local_datetime(&d.and_hms_opt(h, 0, 0)?).earliest()).map(|t| t.timestamp())
        };
        let day = |s: &str| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok();
        let (after, before) = match q.when.as_str() {
            "today" => (start(today), None),
            "week" => (today.checked_sub_days(Days::new(7)).and_then(start), None),
            "month" => (today.checked_sub_days(Days::new(30)).and_then(start), None),
            "year" => (today.checked_sub_months(Months::new(12)).and_then(start), None),
            "custom" => {
                let (a, b) = ordered(day(&q.since), day(&q.until));
                (a.and_then(start), b.and_then(|b| b.succ_opt()).and_then(start))
            }
            _ => (None, None),
        };
        Filter {
            seen: match q.read.as_str() {
                "unread" => Some(false),
                "read" => Some(true),
                _ => None,
            },
            new: None,
            starred: !q.starred.is_empty(),
            from: q.from.trim().to_lowercase(),
            to: q.to.trim().to_lowercase(),
            attachment: !q.attachment.is_empty(),
            invite: !q.invite.is_empty(),
            after,
            before,
        }
    }

    /// On what the list knows without opening the message: flags, date.
    fn keeps(&self, m: &Msg) -> bool {
        self.seen.is_none_or(|s| has_flag(m, "\\Seen") == s)
            && self.new.as_ref().is_none_or(|n| n.contains(&m.link.unwrap_or(m.id)))
            && (!self.starred || has_flag(m, "\\Flagged"))
            && self.after.is_none_or(|a| m.date >= a)
            && self.before.is_none_or(|b| m.date < b)
    }

    /// Whether some filter reads the message itself.
    fn opens(&self) -> bool {
        !self.from.is_empty() || !self.to.is_empty() || self.attachment || self.invite
    }

    fn keeps_facts(&self, f: &Facts) -> bool {
        let names = |people: &[(String, String)], part: &str| {
            part.is_empty() || people.iter().any(|(n, a)| n.to_lowercase().contains(part) || a.to_lowercase().contains(part))
        };
        names(&f.from, &self.from) && names(&f.to, &self.to) && (!self.attachment || f.attachment) && (!self.invite || f.invite)
    }
}

/// What the filters and the address suggestions read in a message.
struct Facts {
    /// `(name, address)`: of the senders; of the recipients (To, Cc).
    from: Vec<(String, String)>,
    to: Vec<(String, String)>,
    /// Files attached (an invitation's own `.ics` isn't one).
    attachment: bool,
    invite: bool,
}

/// By blob: a blob's content never changes, so neither do its facts.
static FACTS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, Arc<Facts>>>> =
    std::sync::LazyLock::new(Default::default);

fn facts(app: &App, blob: &str) -> Option<Arc<Facts>> {
    let kept = || FACTS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(f) = kept().get(blob) {
        return Some(f.clone());
    }
    let raw = app.mail.blob(blob)?;
    let m = parse(&raw)?;
    let people = |a: Option<&mail_parser::Address>| -> Vec<(String, String)> {
        a.map(|a| a.iter().filter_map(|x| Some((x.name().unwrap_or("").trim().to_string(), x.address()?.trim().to_string()))).collect())
            .unwrap_or_default()
    };
    let mut to = people(m.to());
    to.extend(people(m.cc()));
    let html_body = has_html_body(&m);
    let f = Arc::new(Facts {
        from: people(m.from()),
        to,
        attachment: m.attachments().any(|a| shown_attachment(a, html_body) && !crate::invites::is_calendar(a)),
        invite: crate::invites::is_invitation(&m),
    });
    kept().insert(blob.to_string(), f.clone());
    Some(f)
}

/// What a list's row shows of a message.
struct Summary {
    subject: String,
    from: String,
    to: String,
    preview: String,
}

/// By blob, as `FACTS`: rows listed again (the list follows the mail, and
/// grows as it is scrolled) aren't read and parsed again.
static SUMMARIES: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, Arc<Summary>>>> =
    std::sync::LazyLock::new(Default::default);

fn summary(app: &App, blob: &str) -> Arc<Summary> {
    let kept = || SUMMARIES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(s) = kept().get(blob) {
        return s.clone();
    }
    let raw = app.mail.blob(blob).unwrap_or_default();
    let parsed = parse(&raw);
    let s = Arc::new(Summary {
        subject: parsed.as_ref().and_then(|p| p.subject().map(str::to_string)).filter(|s| !s.trim().is_empty()).unwrap_or_else(|| "(no subject)".into()),
        from: parsed.as_ref().map(|p| who(p.from())).unwrap_or_default(),
        to: parsed.as_ref().map(|p| who(p.to())).unwrap_or_default(),
        preview: parsed.as_ref().and_then(|p| p.body_text(0).map(|t| t.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(140).collect())).unwrap_or_default(),
    });
    let mut all = kept();
    // A bound, for a server listing every folder from end to end.
    if all.len() >= 100_000 {
        all.clear();
    }
    all.insert(blob.to_string(), s.clone());
    s
}

#[derive(Deserialize)]
pub struct SuggestQuery {
    /// `from` or `to`.
    #[serde(default)]
    field: String,
    #[serde(default)]
    q: String,
}

/// For the Filter menu: senders (or recipients) of the folder's mail
/// matching what is typed, those whose name or address starts so first,
/// then the most frequent.
pub async fn addresses(State(app): St, headers: HeaderMap, Path(id): Path<i64>, Query(s): Query<SuggestQuery>) -> Response {
    if !authed(&app, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let typed = s.q.trim().to_lowercase();
    // By address: a name it is given, and how often it comes.
    let mut people: std::collections::HashMap<String, (String, String, usize)> = Default::default();
    for m in app.mail.messages(id) {
        let Some(f) = facts(&app, &m.blob) else { continue };
        for (name, address) in if s.field == "to" { &f.to } else { &f.from } {
            let p = people.entry(address.to_lowercase()).or_insert_with(|| (String::new(), address.clone(), 0));
            if p.0.is_empty() {
                p.0 = name.clone();
            }
            p.2 += 1;
        }
    }
    let starts = |s: &str| s.to_lowercase().split(|c: char| !c.is_alphanumeric()).any(|w| w.starts_with(&typed));
    let mut found: Vec<(String, String, usize)> =
        people.into_values().filter(|(n, a, _)| n.to_lowercase().contains(&typed) || a.to_lowercase().contains(&typed)).collect();
    found.sort_by_cached_key(|(n, a, count)| (!(starts(n) || starts(a)), std::cmp::Reverse(*count), a.to_lowercase()));
    let found: Vec<serde_json::Value> = found.into_iter().take(8).map(|(name, address, _)| serde_json::json!({ "name": name, "address": address })).collect();
    axum::Json(found).into_response()
}

fn flash_of(q: &ListQuery) -> Markup {
    flash(&super::Msg { ok: q.ok.clone(), err: q.err.clone(), ..Default::default() })
}

/// Whether the message contains every word of the search.
fn matches(raw: &[u8], words: &[String]) -> bool {
    let hay = crate::mailstore::search_text(raw).to_lowercase();
    words.iter().all(|w| hay.contains(w.as_str()))
}

pub(super) const FUNNEL: &str = r#"<svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true"><path d="M2 3h12l-4.6 5.4V13l-2.8 1.4v-6z" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"/></svg>"#;

/// The Filter button and its menu, part of the toolbar's form: applied
/// together, with the search. `all_mail`: the list is All mail.
fn filter_menu(id: i64, q: &ListQuery, on: usize, all_mail: bool) -> Markup {
    let today = chrono::Utc::now().with_timezone(&crate::calendar::default_zone()).format("%Y-%m-%d").to_string();
    let read = if ["unread", "read", "new"].contains(&q.read.as_str()) { q.read.as_str() } else { "" };
    let person = |field: &str, label: &str, value: &str| {
        html! {
            div.person {
                label for={"filter-" (field)} { (label) }
                input #{"filter-" (field)} type="text" name=(field) value=(value) placeholder="Name or address" autocomplete="off" spellcheck="false"
                    role="combobox" aria-autocomplete="list" aria-expanded="false" aria-controls={"suggest-" (field)}
                    data-suggest={"/box/" (id) "/addresses?field=" (field)};
                ul.suggest #{"suggest-" (field)} role="listbox" aria-label={(label) " suggestions"} hidden {}
            }
        }
    };
    html! {
        details.filter {
            summary.on[on > 0] title="Filter the messages listed" data-swap="filter-summary" {
                (PreEscaped(FUNNEL)) "Filter"
                @if on > 0 { span.count aria-label={(on) " on"} { (on) } }
            }
            div.panel {
                fieldset {
                    legend { "Status" }
                    div.segmented {
                        @for (value, label, title) in [("", "All", ""), ("unread", "Unread", ""), ("read", "Read", ""), ("new", "New", "Come in since valet tells of new mail, still unread in their inbox: what the app's badge counts")] {
                            label title=[(!title.is_empty()).then_some(title)] { input type="radio" name="read" value=(value) checked[read == value]; span { (label) } }
                        }
                    }
                    label.check { input type="checkbox" name="starred" value="1" checked[!q.starred.is_empty()]; "Starred only" }
                    @if all_mail { label.check { input type="checkbox" name="hide" value="1" checked[!q.hide.is_empty()]; "Hide Spam and Trash" } }
                }
                fieldset {
                    legend { "Contains" }
                    label.check { input type="checkbox" name="attachment" value="1" checked[!q.attachment.is_empty()]; "Attachments" }
                    label.check { input type="checkbox" name="invite" value="1" checked[!q.invite.is_empty()]; "An invitation" }
                }
                fieldset {
                    legend { "People" }
                    (person("from", "From", &q.from))
                    (person("to", "To or Cc", &q.to))
                }
                fieldset {
                    legend { "Date" }
                    select name="when" aria-label="Date" {
                        @for (value, label) in WHEN { option value=(value) selected[q.when == value] { (label) } }
                    }
                    div.range {
                        label { "From" input type="date" name="since" value=(q.since) max=(today); }
                        label { "To" input type="date" name="until" value=(q.until) max=(today); }
                    }
                }
                div.foot {
                    span.small.muted { "Applied as you choose" }
                    a href=(q.link(id, 0, &FILTER_KEYS)) data-swap="filter-reset" { "Reset" }
                }
            }
        }
    }
}

/// The Filter menu: each choice applies at once, the list swapped in place
/// (names typed, and the search, after a pause), the menu staying open;
/// closed by a click elsewhere or Escape. Until the list is in, the search
/// field spins and the list is dimmed. What's left empty stays out of the list's address.
/// People are suggested as they're typed, from the folder's mail.
const FILTER_SCRIPT: &str = r#"
{
  const form = document.querySelector('form.toolbar'), menu = form.querySelector('details.filter');
  // How long typing waits (Settings → Overview → Search).
  const delay = Number(form.dataset.delay ?? 100);
  // The list comes after this script in the page.
  let typing = null, applied = 0;
  const apply = () => {
    clearTimeout(typing);
    const split = document.querySelector('.split');
    const url = `${form.getAttribute('action')}?${new URLSearchParams([['page', '0'], ...new FormData(form)])}`;
    if (url === split.dataset.list) return;
    split.dataset.list = url;
    const open = split.querySelector('.row.open a.link')?.dataset.id;
    history.replaceState(null, '', open ? `${url}&open=${open}` : url);
    // Busy until the list asked for last is in.
    const n = ++applied;
    form.classList.add('searching');
    split.classList.add('searching');
    window.valetList.update().then((r) => { if (r.gone) location = r.gone; }, () => { location = url; }).finally(() => {
      if (n !== applied) return;
      form.classList.remove('searching');
      split.classList.remove('searching');
    });
  };
  const panel = menu.querySelector('.panel');
  panel.addEventListener('change', apply);
  panel.addEventListener('input', (e) => {
    if (!e.target.matches('[data-suggest]')) return;
    clearTimeout(typing);
    typing = setTimeout(apply, delay);
  });
  document.addEventListener('click', (e) => { if (menu.open && !menu.contains(e.target)) menu.open = false; });
  menu.addEventListener('keydown', (e) => {
    if (e.key === 'Escape' && !e.defaultPrevented) { menu.open = false; menu.querySelector('summary').focus(); }
  });
  // The search: as it's typed, after a pause; at once on Enter (the form
  // has several fields and no button, so Enter wouldn't send it), or when
  // emptied with its ✕.
  const search = form.elements.q;
  search.addEventListener('input', (e) => {
    if (e.isComposing) return;
    clearTimeout(typing);
    typing = setTimeout(apply, delay);
  });
  search.addEventListener('keydown', (e) => { if (e.key === 'Enter') { e.preventDefault(); apply(); } });
  search.addEventListener('search', () => { if (!search.value) apply(); });
  const when = form.elements.when;
  when.addEventListener('change', () => { if (when.value === 'custom') form.elements.since.focus(); });
  form.addEventListener('formdata', (e) => {
    const custom = e.formData.get('when') === 'custom';
    for (const [k, v] of [...e.formData]) if (!String(v).trim() || (!custom && (k === 'since' || k === 'until'))) e.formData.delete(k);
  });
  for (const input of form.querySelectorAll('input[data-suggest]')) {
    const list = document.getElementById(input.getAttribute('aria-controls'));
    let items = [], at = -1, timer = null, asked = 0;
    const show = (open) => { list.hidden = !open; input.setAttribute('aria-expanded', String(open)); };
    const mark = (i) => {
      at = i;
      [...list.children].forEach((li, j) => li.setAttribute('aria-selected', String(j === i)));
      if (i < 0) { input.removeAttribute('aria-activedescendant'); return; }
      input.setAttribute('aria-activedescendant', list.children[i].id);
      list.children[i].scrollIntoView({ block: 'nearest' });
    };
    const draw = () => {
      list.replaceChildren(...items.map((p, i) => {
        const li = document.createElement('li'), name = document.createElement('span');
        li.id = `${list.id}-${i}`;
        li.setAttribute('role', 'option');
        li.dataset.i = i;
        name.textContent = p.name || p.address;
        li.append(name);
        if (p.name) { const a = document.createElement('span'); a.className = 'addr'; a.textContent = p.address; li.append(a); }
        return li;
      }));
      mark(-1);
      show(items.length > 0);
    };
    // Only the answer to the last question counts.
    const ask = async () => {
      const n = ++asked;
      try {
        const r = await fetch(`${input.dataset.suggest}&q=${encodeURIComponent(input.value.trim())}`);
        if (!r.ok) return;
        const found = await r.json();
        if (n !== asked || document.activeElement !== input) return;
        items = found;
        draw();
      } catch (_) {}
    };
    const pick = (i) => { input.value = items[i].address; show(false); apply(); };
    input.addEventListener('focus', ask);
    input.addEventListener('input', () => { clearTimeout(timer); timer = setTimeout(ask, 120); });
    input.addEventListener('blur', () => { asked++; show(false); });
    input.addEventListener('keydown', (e) => {
      const open = !list.hidden, n = items.length;
      if ((e.key === 'ArrowDown' || e.key === 'ArrowUp') && n) {
        e.preventDefault();
        if (!open) { show(true); mark(-1); }
        else if (e.key === 'ArrowDown') mark((at + 1) % n);
        else mark(at <= 0 ? n - 1 : at - 1);
      } else if (e.key === 'Enter' && open && at >= 0) { e.preventDefault(); pick(at); }
      else if (e.key === 'Escape' && open) { e.preventDefault(); show(false); }
    });
    // Before the field loses the focus.
    list.addEventListener('mousedown', (e) => {
      e.preventDefault();
      const li = e.target.closest('li');
      if (li) pick(+li.dataset.i);
    });
  }
}
"#;

pub async fn list(State(app): St, headers: HeaderMap, Path(id): Path<i64>, Query(q): Query<ListQuery>) -> Response {
    guard!(app, headers);
    let Some(mbox) = app.mail.mailbox(id) else { return StatusCode::NOT_FOUND.into_response() };
    let link = |page: usize| q.link(id, page, &[]);
    let rows = (q.n.unwrap_or(PAGE) / PAGE * PAGE).clamp(PAGE, MAX_ROWS);
    // This list as shown, its rows scrolled through included: what closing a message, or an action, comes back to.
    let here = if rows > PAGE { format!("{}&n={rows}", link(q.page)) } else { link(q.page) };
    if let (Some(open), Some(_)) = (q.open, &q.fragment) {
        return match reader(&app, open, &q, Some((id, here.clone()))) {
            Some((subject, body)) => maud::html! { div.readcol data-subject=(subject) { (body) } }.into_response(),
            None => StatusCode::NOT_FOUND.into_response(),
        };
    }
    // The message shown beside the list (or, on narrow screens, instead of
    // it): first, so the list shows it read.
    let reading = match q.open {
        Some(open) => match reader(&app, open, &q, Some((id, here.clone()))) {
            Some(r) => Some((open, r)),
            None => return back(&here, None, Some("That message is gone.")),
        },
        None => None,
    };
    let mut msgs = app.mail.messages(id);
    msgs.sort_by_key(|m| std::cmp::Reverse((m.date, m.id)));
    let all_mail = mbox.is_all_mail();
    let bins = bins(&app);
    // A node's rows: the account folder each message is in.
    let origins = if mbox.is_node() && (all_mail || !q.hide.is_empty()) { app.mail.origins(id) } else { Default::default() };
    let folder_of = |m: &Msg| if mbox.is_node() { origins.get(&m.id).map(|o| o.0) } else { Some(m.mailbox) };
    if all_mail {
        // A message in several folders of its account shows once: from its
        // inbox, else from out of Spam and Trash, else the newest copy.
        let boxes: std::collections::HashMap<i64, Mailbox> = app.mail.mailboxes().into_iter().map(|b| (b.id, b)).collect();
        let rank = |f: i64| if boxes.get(&f).is_some_and(Mailbox::is_inbox) { 0 } else if bins.contains_key(&f) { 2 } else { 1 };
        let key = |m: &Msg| origins.get(&m.id).filter(|(_, mid)| !mid.is_empty()).map(|(f, mid)| (boxes.get(f).and_then(|b| b.account.clone()), mid.clone()));
        let mut best: std::collections::HashMap<(Option<String>, String), (u8, i64)> = Default::default();
        for m in &msgs {
            let (Some(k), Some(f)) = (key(m), folder_of(m)) else { continue };
            let r = rank(f);
            best.entry(k).and_modify(|b| if r < b.0 { *b = (r, m.id) }).or_insert((r, m.id));
        }
        msgs.retain(|m| key(m).is_none_or(|k| best.get(&k).is_none_or(|b| b.1 == m.id)));
    }
    if !q.hide.is_empty() {
        msgs.retain(|m| folder_of(m).is_none_or(|f| !bins.contains_key(&f)));
    }
    // What the message's flags and date tell first; then what opening it does.
    let mut filter = Filter::of(&q);
    if q.read == "new" {
        filter.new = Some(app.mail.still_new());
    }
    msgs.retain(|m| filter.keeps(m));
    if filter.opens() {
        msgs.retain(|m| facts(&app, &m.blob).is_some_and(|f| filter.keeps_facts(&f)));
    }
    if !q.q.trim().is_empty() {
        match app.mail.search(&q.q) {
            Some(found) => msgs.retain(|m| found.contains(&m.blob)),
            // Until the index has every message: each read.
            None => {
                let words: Vec<String> = q.q.split_whitespace().map(str::to_lowercase).collect();
                msgs.retain(|m| app.mail.blob(&m.blob).is_some_and(|raw| matches(&raw, &words)));
            }
        }
    }
    let total = msgs.len();
    let start = (q.page * PAGE).min(total);
    let end = (start + rows).min(total);
    let shown = &msgs[start..end];
    let names: std::collections::HashMap<String, String> = app.accounts().into_iter().map(|a| (a.id, a.name)).collect();
    let account_of = |m: &Msg| {
        let src = app.mail.source(m.clone());
        app.mail.mailbox(src.mailbox).and_then(|b| b.account).and_then(|a| names.get(&a).cloned())
    };
    let title = if mbox.is_node() { short_name(&mbox) } else { place_name(&mbox) };
    let drafts = mbox.special.eq_ignore_ascii_case("\\Drafts");
    let spam = mbox.special.eq_ignore_ascii_case("\\Junk") || bins.get(&id) == Some(&"Spam");
    let chips = q.chips();
    let page_title = reading.as_ref().map(|(_, (subject, _))| subject.clone()).unwrap_or_else(|| title.clone());
    let sent = mbox.special.eq_ignore_ascii_case("\\Sent");
    let row = |m: &Msg| {
        let s = summary(&app, &m.blob);
        let from = if sent { format!("To: {}", s.to) } else { s.from.clone() };
        let is_open = reading.as_ref().is_some_and(|(o, _)| *o == m.id);
        // The open one's row closes it.
        let href = if drafts { format!("/compose?draft={}", m.id) } else if is_open { here.clone() } else { format!("{here}&open={}", m.id) };
        html! {
            div.row.unread[!has_flag(m, "\\Seen")].open[is_open] {
                label.pick { input type="checkbox" name="id" value=(m.id) aria-label="Select"; }
                a.link href=(href) title={(from) " — " (s.subject)} data-id=(m.id)
                    data-open=[(!drafts).then(|| format!("{here}&open={}", m.id))] {
                    span.star { @if has_flag(m, "\\Flagged") { "★" } }
                    span.from { (from) }
                    span.line {
                        @if mbox.is_node() { @if let Some(a) = account_of(m) { span.tag { (a) } } }
                        @if all_mail { @if let Some(b) = folder_of(m).and_then(|f| bins.get(&f)) { span.tag.bin { (b) } } }
                        span.subject { (s.subject) }
                        @if !s.preview.is_empty() { span.preview { " — " (s.preview) } }
                    }
                    span.when { (time(m.date, false)) }
                }
            }
        }
    };
    // Scrolled near its end, the list fetches its next rows (`data-more`),
    // and its address becomes `data-grown`; past `MAX_ROWS`, Older.
    let grows = end < total && rows < MAX_ROWS;
    let pager = html! {
        div.pager.small.muted
            data-more=[grows.then(|| format!("{}&n={}&more={rows}", link(q.page), rows + PAGE))]
            data-grown=[grows.then(|| format!("{}&n={}", link(q.page), rows + PAGE))] {
            span { @if q.page > 0 { a href=(link(q.page - 1)) { "← Newer" } } }
            span { (start + 1) "–" (end) " of " (total) }
            span { @if end < total { a href=(link(q.page + rows / PAGE)) { "Older →" } } }
        }
    };
    if let Some(from) = q.more {
        return html! { div.more { @for m in &shown[from.min(shown.len())..] { (row(m)) } (pager) } }.into_response();
    }
    mail_page(&page_title, id, &app, html! {
        (flash_of(&q))
        form.toolbar method="get" action={"/box/" (id)} data-delay=(super::search_delay(&app)) {
            span.normal {
                button.refresh type="button" data-folder=(id) title="Check every account for new mail now, this folder first" { "↻ Refresh" }
                span.refresh-note.small.muted role="status" {}
                span.search { input type="search" name="q" value=(q.q) placeholder={"Search " (title)}; }
                a.button href=(q.link(id, 0, &["q"])) hidden[q.q.is_empty()] title="Clear the search (the filters stay)" data-swap="clear-search" { "Clear" }
                (filter_menu(id, &q, chips.len(), all_mail))
            }
            // Shown while messages are ticked; they act on the list's form.
            span.bulk-actions {
                label.pickall title="Select every message listed" { input type="checkbox" #pickall; }
                span.count {}
                button form="bulk" name="action" value="archive" { "Archive" }
                @if all_mail || spam { button form="bulk" name="action" value="notspam" title="Back to their inbox" { "Not spam" } }
                button form="bulk" name="action" value="seen" { "Mark read" }
                button form="bulk" name="action" value="unseen" { "Mark unread" }
                button.danger form="bulk" name="action" value="delete" { "Delete" }
                button.plain type="button" #pick-none { "Cancel" }
            }
            div.filters-on data-swap="filters-on" {
                @for (label, drop) in &chips {
                    a.chip href=(q.link(id, 0, drop)) title="Remove this filter" { (label) span aria-hidden="true" { "✕" } }
                }
                @if chips.len() > 1 { a.small href=(q.link(id, 0, &FILTER_KEYS)) { "Clear filters" } }
            }
        }
        script { (PreEscaped(FILTER_SCRIPT)) }
        script { (PreEscaped(REFRESH_SCRIPT)) }
        div.split.reading[reading.is_some()] data-list=(here) data-title=(title) {
            div.listcol {
                @if shown.is_empty() {
                    p.muted { @if q.criteria().is_empty() { "Nothing here." } @else { "No message matches." } }
                } @else {
                    form #bulk method="post" action={"/box/" (id) "/bulk"} {
                        input type="hidden" name="back" value=(here);
                        div.list { @for m in shown { (row(m)) } }
                    }
                    (pager)
                }
            }
            @if let Some((_, (_, body))) = &reading { div.readcol { (body) } }
        }
        script { (PreEscaped(BULK_SCRIPT)) }
        script { (PreEscaped(PANE_SCRIPT)) }
    })
}

/// Opening a message without reloading the page: its part is fetched ahead
/// (when the pointer rests on its row, not marking it read), swapped in on
/// click, and marked read then; its body frame is fetched ahead too.
const PANE_SCRIPT: &str = r#"
{
  const split = document.querySelector('.split');
  const cache = new Map();
  const get = (url) => {
    if (!cache.has(url)) {
      const p = fetch(url + '&fragment=1', { cache: 'no-store' }).then((r) => { if (!r.ok) throw new Error(r.status); return r.text(); });
      p.catch(() => cache.delete(url));
      cache.set(url, p);
    }
    return cache.get(url);
  };
  const warm = (a) => {
    if (!a?.dataset.open || a.closest('.row.open')) return;
    get(a.dataset.open).catch(() => {});
    fetch(`/msg/${a.dataset.id}/html`).catch(() => {});
  };
  let resting = null;
  split.addEventListener('pointerover', (e) => {
    const a = e.target.closest('a.link');
    clearTimeout(resting);
    if (a) resting = setTimeout(() => warm(a), 70);
  });
  split.addEventListener('pointerdown', (e) => warm(e.target.closest('a.link')));
  const openRow = () => split.querySelector('.row.open');
  const unmark = () => { const r = openRow(); if (r) { r.classList.remove('open'); const a = r.querySelector('a.link'); a.href = a.dataset.open; } };
  const close = () => {
    unmark();
    split.querySelector('.readcol')?.remove();
    split.classList.remove('reading');
    history.pushState(null, '', split.dataset.list);
    document.title = `${split.dataset.title} · valet`;
  };
  const show = (a, html) => {
    const row = a.closest('.row');
    unmark();
    row.classList.add('open');
    a.href = split.dataset.list;
    if (row.classList.contains('unread')) {
      row.classList.remove('unread');
      fetch(`/msg/${a.dataset.id}/seen`, { method: 'POST' }).catch(() => {});
      const badge = document.querySelector('.folders a.folder.on .badge');
      if (badge) { const n = Number(badge.textContent) - 1; n > 0 ? (badge.textContent = n) : badge.remove(); }
    }
    const t = document.createElement('template');
    t.innerHTML = html;
    const col = t.content.firstElementChild;
    valetTimes(col);
    const old = split.querySelector('.readcol');
    old ? old.replaceWith(col) : split.append(col);
    split.classList.add('reading');
    history.pushState(null, '', a.dataset.open);
    document.title = `${col.dataset.subject} · valet`;
    row.scrollIntoView({ block: 'nearest' });
  };
  split.addEventListener('click', async (e) => {
    const a = e.target.closest('a.link, a.close');
    if (!a || e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
    if (a.matches('a.link') && !a.dataset.open) return;
    e.preventDefault();
    if (a.matches('a.close') || a.closest('.row.open')) return close();
    try { show(a, await get(a.dataset.open)); } catch (_) { location = a.dataset.open; }
  });
  // Back and forward: the page as its address says.
  addEventListener('popstate', () => location.reload());
}
"#;

/// Marks a message read (the list's script, opening it in place).
pub async fn mark_seen(State(app): St, headers: HeaderMap, Path(id): Path<i64>) -> Response {
    if !authed(&app, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some((_, src, folder)) = locate(&app, id) else { return StatusCode::NOT_FOUND.into_response() };
    if !has_flag(&src, "\\Seen") {
        if let Err(e) = app.mail.store(&[src.id], &StoreMode::Add, &["\\Seen".to_string()]) {
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
        if let Some(acc) = &folder.account {
            app.mail.waker(acc).notify_one();
        }
    }
    StatusCode::NO_CONTENT.into_response()
}

/// Ticking messages: the count, all of the page, a range with Shift.
const BULK_SCRIPT: &str = r#"
{
  // The list's boxes, looked up each time: Refresh swaps the list in place.
  const boxes = () => [...document.querySelectorAll('#bulk input[name=id]')], all = document.getElementById('pickall');
  const count = document.querySelector('.bulk-actions .count');
  const sync = () => {
    const bs = boxes(), n = bs.filter((b) => b.checked).length;
    count.textContent = `${n} selected`;
    all.checked = n > 0 && n === bs.length;
    all.indeterminate = n > 0 && n < bs.length;
    for (const b of bs) b.closest('.row').classList.toggle('picked', b.checked);
  };
  let last = null;
  document.querySelector('.split').addEventListener('click', (e) => {
    const b = e.target.closest('#bulk input[name=id]');
    if (!b) return;
    const bs = boxes();
    if (e.shiftKey && last && bs.includes(last)) {
      const [i, j] = [bs.indexOf(last), bs.indexOf(b)].sort((x, y) => x - y);
      for (const x of bs.slice(i, j + 1)) x.checked = b.checked;
    }
    last = b;
    sync();
  });
  document.addEventListener('valet:list', sync);
  all?.addEventListener('change', () => { for (const b of boxes()) b.checked = all.checked; sync(); });
  document.getElementById('pick-none')?.addEventListener('click', () => { for (const b of boxes()) b.checked = false; sync(); });
  document.querySelector('button[value=delete][form=bulk]')?.addEventListener('click', (e) => {
    const n = boxes().filter((b) => b.checked).length;
    if (!confirm(`Delete ${n} message${n > 1 ? 's' : ''}? They go to the Trash (from the Trash itself, for good).`)) e.preventDefault();
  });
  sync();
  // The message read stays in sight in the list.
  document.querySelector('.list .row.open')?.scrollIntoView({ block: 'nearest' });
}
"#;

/// The message row as listed, its source row (an account folder's), and that folder.
fn locate(app: &App, id: i64) -> Option<(Msg, Msg, Mailbox)> {
    let row = app.mail.message(id)?;
    let src = app.mail.source(row.clone());
    let folder = app.mail.mailbox(src.mailbox)?;
    Some((row, src, folder))
}

/// A message as read: its actions, headers and body. Marks it read.
/// `pane`: shown beside the list `(list id, list address)`, which actions
/// come back to; else on its own page. Returns its subject too.
fn reader(app: &App, id: i64, q: &ListQuery, pane: Option<(i64, String)>) -> Option<(String, Markup)> {
    let (row, src, folder) = locate(app, id)?;
    if q.fragment.is_none() && !has_flag(&src, "\\Seen") {
        let _ = app.mail.store(&[src.id], &StoreMode::Add, &["\\Seen".to_string()]);
    }
    let raw = app.mail.blob(&src.blob).unwrap_or_default();
    let m = parse(&raw)?;
    let subject = m.subject().filter(|s| !s.trim().is_empty()).unwrap_or("(no subject)").to_string();
    let html_body = has_html_body(&m);
    let remote = html_body && m.body_html(0).is_some_and(|h| has_remote(&h));
    let images = q.images.is_some();
    let back = pane.as_ref().map(|(b, _)| *b).unwrap_or(row.mailbox);
    let close = pane.as_ref().map(|(_, l)| l.clone()).unwrap_or_else(|| format!("/box/{back}"));
    let with_images = match &pane {
        Some((_, l)) => format!("{l}&open={id}&images=1"),
        None => format!("/msg/{id}?images=1"),
    };
    let (tree, _) = folders(app);
    let in_spam = bins(app).get(&folder.id) == Some(&"Spam");
    // Deleting it from the Trash (or from an account without one) is for good.
    let for_good = folder.account.as_deref().and_then(|a| app.mail.special_folder(a, "\\Trash")).is_none_or(|t| t == folder.id);
    let account_folders: Vec<Mailbox> = app.mail.mailboxes().into_iter().filter(|b| b.account == folder.account && b.id != folder.id).collect();
    // (index, name, size, kept: false when removed here to save room).
    let attachments: Vec<(usize, String, usize, bool)> = m
        .attachments()
        .enumerate()
        .filter(|(_, a)| shown_attachment(a, html_body) || removed_size(a).is_some())
        .map(|(i, a)| {
            use mail_parser::MimeHeaders;
            let name = a.attachment_name().unwrap_or("attachment").to_string();
            match removed_size(a) {
                Some(n) => (i, name, n, false),
                None => (i, name, a.contents().len(), true),
            }
        })
        .collect();
    let flagged = has_flag(&src, "\\Flagged");
    // Rules are for others' mail, not your own.
    let offer = app.mail.offer(&raw).filter(|o| !app.accounts().iter().any(|a| a.sends_as(&o.address)));
    // What each action form carries: where to come back to.
    let hidden = html! {
        input type="hidden" name="back" value=(back);
        @if let Some((_, l)) = &pane { input type="hidden" name="list" value=(l); }
    };
    let body = html! {
        div.toolbar.msg-actions {
            @if pane.is_some() { a.button.close href=(close) title="Close this message" { "✕ Close" } }
            @else { a.button href=(close) { "← Back" } }
            @if is_draft(&src, &folder) { a.button href={"/compose?draft=" (id)} { "Continue writing" } }
            a.button href={"/compose?reply=" (id)} { "Reply" }
            a.button href={"/compose?all=" (id)} { "Reply all" }
            a.button href={"/compose?forward=" (id)} { "Forward" }
            @if in_spam {
                form method="post" action={"/msg/" (id)} { (hidden)
                    button name="action" value="notspam" title="Back to its inbox" { "Not spam" } }
            }
            form method="post" action={"/msg/" (id)} { (hidden)
                button name="action" value="archive" { "Archive" } }
            form method="post" action={"/msg/" (id)} { (hidden)
                button name="action" value=(if flagged { "unflag" } else { "flag" }) { @if flagged { "★ Unflag" } @else { "☆ Flag" } } }
            form method="post" action={"/msg/" (id)} { (hidden)
                button name="action" value="unseen" { "Mark unread" } }
            form method="post" action={"/msg/" (id)} {
                (hidden)
                input type="hidden" name="action" value="move";
                select name="to" onchange="this.form.submit()" {
                    option value="" { "Move to…" }
                    optgroup label="Your folders" {
                        @for (n, b) in &tree { @if b.id != back && !b.is_all_mail() { option value=(b.id) { (indent(n.depth)) (short_name(b)) } } }
                    }
                    @if !account_folders.is_empty() {
                        optgroup label={"Accounts › " (place_name(&folder).split(" › ").next().unwrap_or(""))} {
                            @for b in &account_folders { option value=(b.id) { (short_name(b)) } }
                        }
                    }
                }
            }
            form method="post" action={"/msg/" (id)} onsubmit=[for_good.then_some("return confirm('Delete this message for good?')")] { (hidden)
                button.danger name="action" value="delete" { "Delete" } }
        }
        div.card.reader {
            div.headers {
                h1 { (subject) }
                div { b { (everyone(m.from())) } }
                @if let Some(o) = &offer {
                    @if let Some(r) = &o.rule {
                        div.meta { "Mail from " (r.sender) " "
                            @match r.node.and_then(|n| app.mail.mailbox(n)) {
                                Some(b) => { "goes to " (place_name(&b)) }
                                None => { "has a rule waiting for a folder" }
                            }
                            ". " a href="/rules" { "Edit" }
                        }
                    }
                    @if !o.senders.is_empty() {
                        details.make-rule {
                            summary { "Make a rule…" }
                            form.row method="post" action={"/msg/" (id)} {
                                (hidden)
                                input type="hidden" name="action" value="rule";
                                label { "Mail from" select name="sender" { @for s in &o.senders { option value=(s) { (s) } } } }
                                label { "Goes to" (rule_folders(&tree, app.mail.all_inboxes(), None)) }
                                label.also { input type="checkbox" name="also" value="1" checked; "Move this message too" }
                                label.also { input type="checkbox" name="inbox" value="1"; "And what's in the inbox from them" }
                                button type="submit" { "Add rule" }
                            }
                        }
                    }
                }
                div.meta { "To " (everyone(m.to()))
                    @let cc = everyone(m.cc()); @if !cc.is_empty() { br; "Cc " (cc) } }
                div.meta { (time(src.date, true)) " · " (place_name(&folder)) }
            }
            @if html_body {
                @if remote && !images {
                    div.notice { "Remote images are blocked, so the sender can't see you opened this. "
                        a href=(with_images) { "Show them" } }
                }
                iframe src=(format!("/msg/{id}/html{}", if images { "?images=1" } else { "" }))
                    sandbox="allow-same-origin allow-popups allow-popups-to-escape-sandbox"
                    referrerpolicy="no-referrer"
                    onload="const d=this.contentDocument; if (d) this.style.height = (d.documentElement.scrollHeight + 4) + 'px'" {}
            } @else {
                pre { (m.body_text(0).unwrap_or(Cow::Borrowed(""))) }
            }
            @if !attachments.is_empty() {
                div.atts {
                    @for (i, name, size, kept) in &attachments {
                        @if *kept {
                            a href={"/msg/" (id) "/att/" (i)} { "📎 " (name) span.muted.small { " " (human_size(*size)) } }
                        } @else {
                            span.removed title="Not kept in valet, to save room (the account keeps attachments from a later date): it's still at the provider." {
                                "📎 " (name) span.muted.small { " " (human_size(*size)) ", not kept" }
                            }
                        }
                    }
                }
            }
        }
    };
    Some((subject, body))
}

pub async fn view(State(app): St, headers: HeaderMap, Path(id): Path<i64>, Query(q): Query<ListQuery>) -> Response {
    guard!(app, headers);
    let Some((row, _, _)) = locate(&app, id) else { return StatusCode::NOT_FOUND.into_response() };
    let Some((subject, body)) = reader(&app, id, &q, None) else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "This message can't be read.").into_response();
    };
    mail_page(&subject, row.mailbox, &app, html! { (flash_of(&q)) (body) })
}

fn human_size(n: usize) -> String {
    match n {
        n if n < 1024 => format!("{n} B"),
        n if n < 1024 * 1024 => format!("{} KB", n / 1024),
        n => format!("{:.1} MB", n as f64 / 1024.0 / 1024.0),
    }
}

/// mail-parser turns a text-only message into HTML too: only real HTML goes in the frame.
fn has_html_body(m: &mail_parser::Message) -> bool {
    m.html_body.first().and_then(|&i| m.part(i)).is_some_and(|p| p.is_text_html())
}

/// The size an attachment had, if valet removed it (see `mime::strip`).
fn removed_size(p: &mail_parser::MessagePart) -> Option<usize> {
    p.headers.iter().find(|h| h.name.as_str().eq_ignore_ascii_case(crate::mime::REMOVED)).and_then(|h| h.value.as_text()?.trim().parse().ok())
}

/// Listed under the message: not an image its HTML shows.
fn shown_attachment(a: &mail_parser::MessagePart, html_body: bool) -> bool {
    !is_inline_image(a) || !html_body
}

fn is_inline_image(p: &mail_parser::MessagePart) -> bool {
    use mail_parser::MimeHeaders;
    p.content_id().is_some() && p.content_type().is_some_and(|c| c.ctype().eq_ignore_ascii_case("image"))
}

fn has_remote(html: &str) -> bool {
    let h = html.to_ascii_lowercase();
    ["src=\"http", "src='http", "src=http", "url(http", "url('http", "url(\"http", "background=\"http"].iter().any(|p| h.contains(p))
}

#[derive(Deserialize)]
pub struct HtmlQuery {
    #[serde(default)]
    images: Option<String>,
}

/// The HTML body, for the reader's frame: no scripts, no forms, no plugins,
/// nothing fetched from elsewhere unless images were asked for, links open
/// in a new tab.
pub async fn html_body(State(app): St, headers: HeaderMap, Path(id): Path<i64>, Query(q): Query<HtmlQuery>) -> Response {
    guard!(app, headers);
    let Some((_, src, _)) = locate(&app, id) else { return StatusCode::NOT_FOUND.into_response() };
    let raw = app.mail.blob(&src.blob).unwrap_or_default();
    let Some(m) = parse(&raw) else { return StatusCode::NOT_FOUND.into_response() };
    let body = m.body_html(0).unwrap_or_default();
    let body = cid_links(&body, id);
    let remote = if q.images.is_some() { " https: http:" } else { "" };
    let csp = format!(
        "sandbox allow-same-origin allow-popups allow-popups-to-escape-sandbox; default-src 'none'; script-src 'none'; \
         img-src 'self' data:{remote}; style-src 'unsafe-inline'{remote}; font-src data:{remote}; media-src 'none'; frame-src 'none'; form-action 'none'; base-uri 'self'"
    );
    let doc = format!(
        "<!doctype html><meta charset=\"utf-8\"><base target=\"_blank\"><style>html{{color-scheme:light}}body{{margin:12px;font:14px/1.45 system-ui,sans-serif;color:#111;background:#fff;overflow-wrap:anywhere}}img{{max-width:100%;height:auto}}</style>{body}"
    );
    Response::builder()
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header("Content-Security-Policy", csp)
        .header("X-Content-Type-Options", "nosniff")
        .header("Referrer-Policy", "no-referrer")
        .header(header::CACHE_CONTROL, "private, max-age=3600")
        .body(Body::from(doc))
        .unwrap()
}

/// `cid:` references (images sent inside the message) point to valet.
fn cid_links(html: &str, id: i64) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(i) = rest.to_ascii_lowercase().find("cid:") {
        let before = rest[..i].chars().last();
        out.push_str(&rest[..i]);
        if matches!(before, Some('"' | '\'' | '=' | '(')) {
            out.push_str(&format!("/msg/{id}/cid/"));
        } else {
            out.push_str(&rest[i..i + 4]);
        }
        rest = &rest[i + 4..];
    }
    out.push_str(rest);
    out
}

fn part_response(p: &mail_parser::MessagePart, download: bool) -> Response {
    use mail_parser::MimeHeaders;
    let ctype = p
        .content_type()
        .map(|c| format!("{}/{}", c.ctype(), c.subtype().unwrap_or("octet-stream")))
        .unwrap_or_else(|| "application/octet-stream".into());
    let name = p.attachment_name().unwrap_or("attachment").replace(['"', '\\', '\r', '\n'], "_");
    let mut b = Response::builder()
        .header(header::CONTENT_TYPE, if download { "application/octet-stream".to_string() } else { ctype })
        .header("X-Content-Type-Options", "nosniff")
        .header("Content-Security-Policy", "sandbox; default-src 'none'");
    if download {
        let enc = percent_encoding::utf8_percent_encode(&name, percent_encoding::NON_ALPHANUMERIC);
        b = b.header(header::CONTENT_DISPOSITION, format!("attachment; filename=\"{name}\"; filename*=UTF-8''{enc}"));
    }
    b.body(Body::from(p.contents().to_vec())).unwrap()
}

pub async fn attachment(State(app): St, headers: HeaderMap, Path((id, n)): Path<(i64, usize)>) -> Response {
    guard!(app, headers);
    let Some((_, src, _)) = locate(&app, id) else { return StatusCode::NOT_FOUND.into_response() };
    let raw = app.mail.blob(&src.blob).unwrap_or_default();
    let Some(m) = parse(&raw) else { return StatusCode::NOT_FOUND.into_response() };
    match m.attachments().nth(n) {
        Some(p) => part_response(p, true),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

pub async fn cid(State(app): St, headers: HeaderMap, Path((id, cid)): Path<(i64, String)>) -> Response {
    guard!(app, headers);
    use mail_parser::MimeHeaders;
    let Some((_, src, _)) = locate(&app, id) else { return StatusCode::NOT_FOUND.into_response() };
    let raw = app.mail.blob(&src.blob).unwrap_or_default();
    let Some(m) = parse(&raw) else { return StatusCode::NOT_FOUND.into_response() };
    let want = cid.trim_matches(['<', '>']);
    let found = m.parts.iter().find(|p| p.content_id().is_some_and(|c| c.trim_matches(['<', '>']).eq_ignore_ascii_case(want)));
    match found {
        Some(p) if is_inline_image(p) => part_response(p, false),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

#[derive(Deserialize)]
pub struct ActionForm {
    action: String,
    #[serde(default)]
    back: Option<i64>,
    /// The list the message was read beside (`/box/…`), to come back to.
    #[serde(default)]
    list: Option<String>,
    #[serde(default)]
    to: Option<String>,
    /// Making a rule from it: the sender, the folder, and whether it goes too.
    #[serde(default)]
    sender: Option<String>,
    #[serde(default)]
    node: Option<String>,
    #[serde(default)]
    also: Option<String>,
    #[serde(default)]
    inbox: Option<String>,
}

/// Each account's Spam and Trash folders, by what they're called.
fn bins(app: &App) -> std::collections::HashMap<i64, &'static str> {
    let mut out = std::collections::HashMap::new();
    for a in app.accounts() {
        for (special, label) in [("\\Junk", "Spam"), ("\\Trash", "Trash")] {
            if let Some(f) = app.mail.special_folder(&a.id, special) {
                out.insert(f, label);
            }
        }
    }
    out
}

/// Out of each account's Spam, into its inbox. Returns how many moved (those
/// not in Spam stay).
fn not_spam(app: &App, msgs: &[(i64, Mailbox)]) -> anyhow::Result<usize> {
    let bins = bins(app);
    let boxes = app.mail.mailboxes();
    let mut to_inbox: std::collections::BTreeMap<i64, Vec<i64>> = Default::default();
    for (src, folder) in msgs {
        if bins.get(&folder.id) != Some(&"Spam") {
            continue;
        }
        if let Some(inbox) = boxes.iter().find(|b| b.account.is_some() && b.account == folder.account && b.is_inbox()) {
            to_inbox.entry(inbox.id).or_default().push(*src);
        }
    }
    let moved = to_inbox.values().map(Vec::len).sum();
    for (inbox, ids) in to_inbox {
        app.mail.copy(&ids, inbox, true)?;
    }
    Ok(moved)
}

/// To each account's Trash, like mail apps do; from the Trash itself, for
/// good. Returns how many went to a Trash.
fn delete(app: &App, msgs: &[(i64, Mailbox)]) -> anyhow::Result<usize> {
    let mut to_trash: std::collections::BTreeMap<i64, Vec<i64>> = Default::default();
    let mut gone = Vec::new();
    for (src, folder) in msgs {
        match folder.account.as_deref().and_then(|a| app.mail.special_folder(a, "\\Trash")) {
            Some(t) if t != folder.id => to_trash.entry(t).or_default().push(*src),
            _ => gone.push(*src),
        }
    }
    let moved = to_trash.values().map(Vec::len).sum();
    for (trash, ids) in to_trash {
        app.mail.copy(&ids, trash, true)?;
    }
    if !gone.is_empty() {
        app.mail.expunge(&gone)?;
    }
    Ok(moved)
}

/// The account's Archive: the folder the provider flags so, else the
/// tree's Archive folder there, made at the provider if need be (with an
/// Archive folder in the tree first, if there's none: for Gmail, a label).
async fn archive_folder(app: &Arc<App>, account: &str) -> anyhow::Result<i64> {
    if let Some(f) = app.mail.special_folder(account, "\\Archive") {
        return Ok(f);
    }
    let boxes = app.mail.mailboxes();
    let node = boxes.iter().find(|b| b.is_node() && b.special.eq_ignore_ascii_case("\\Archive")).map(|b| b.id);
    let node = match node {
        Some(n) => n,
        None => {
            let named = boxes.iter().find(|b| b.is_node() && b.name.eq_ignore_ascii_case("Archive")).map(|b| b.id);
            let n = match named {
                Some(n) => n,
                None => app.mail.create_node(None, "Archive")?,
            };
            app.mail.set_special(n, "\\Archive")?;
            n
        }
    };
    crate::folders::member_in(app, node, account).await
}

/// Into each account's Archive; returns how many moved.
async fn archive(app: &Arc<App>, msgs: &[(i64, Mailbox)]) -> anyhow::Result<usize> {
    let mut by_target: std::collections::BTreeMap<i64, Vec<i64>> = Default::default();
    for (src, folder) in msgs {
        let Some(acc) = folder.account.as_deref() else { continue };
        let target = archive_folder(app, acc).await?;
        if target != folder.id {
            by_target.entry(target).or_default().push(*src);
        }
    }
    let moved = by_target.values().map(Vec::len).sum();
    for (target, ids) in by_target {
        app.mail.copy(&ids, target, true)?;
    }
    Ok(moved)
}

/// The list's ticked messages: archived, deleted, marked read or unread.
pub async fn bulk(State(app): St, headers: HeaderMap, Path(id): Path<i64>, body: String) -> Response {
    guard!(app, headers);
    let pairs = super::form_urlencoded_pairs(&body);
    let get = |k: &str| pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()).unwrap_or_default();
    let list = Some(get("back")).filter(|l| l.starts_with("/box/") && !l.contains("//")).unwrap_or_else(|| format!("/box/{id}"));
    let msgs: Vec<(i64, Mailbox)> =
        pairs.iter().filter(|(n, _)| n == "id").filter_map(|(_, v)| v.parse().ok()).filter_map(|m| locate(&app, m)).map(|(_, src, f)| (src.id, f)).collect();
    if msgs.is_empty() {
        return back(&list, None, Some("No message selected."));
    }
    let n = msgs.len();
    let plural = |n: usize| if n == 1 { "1 message".to_string() } else { format!("{n} messages") };
    let ids: Vec<i64> = msgs.iter().map(|(m, _)| *m).collect();
    let done: anyhow::Result<String> = match get("action").as_str() {
        "seen" => app.mail.store(&ids, &StoreMode::Add, &["\\Seen".into()]).map(|_| format!("Marked {} read.", plural(n))),
        "unseen" => app.mail.store(&ids, &StoreMode::Remove, &["\\Seen".into()]).map(|_| format!("Marked {} unread.", plural(n))),
        "delete" => delete(&app, &msgs).map(|moved| if moved == n { format!("Moved {} to the Trash.", plural(n)) } else { format!("Deleted {}.", plural(n)) }),
        "archive" => archive(&app, &msgs).await.map(|_| format!("Archived {}.", plural(n))),
        "notspam" => not_spam(&app, &msgs).and_then(|moved| match moved {
            0 => Err(anyhow::anyhow!("None of them is in Spam.")),
            m => Ok(format!("Moved {} to the inbox.", plural(m))),
        }),
        _ => return back(&list, None, Some("Unknown action.")),
    };
    let accounts: std::collections::BTreeSet<String> = msgs.iter().filter_map(|(_, f)| f.account.clone()).collect();
    for a in accounts {
        app.mail.waker(&a).notify_one();
    }
    match done {
        Ok(msg) => back(&list, Some(&msg), None),
        Err(e) => back(&list, None, Some(&format!("{e:#}"))),
    }
}

pub async fn action(State(app): St, headers: HeaderMap, Path(id): Path<i64>, axum::extract::Form(f): axum::extract::Form<ActionForm>) -> Response {
    guard!(app, headers);
    let Some((row, src, folder)) = locate(&app, id) else { return StatusCode::NOT_FOUND.into_response() };
    let pane = f.list.filter(|l| l.starts_with("/box/") && !l.contains("//"));
    let list = pane.clone().unwrap_or_else(|| format!("/box/{}", f.back.unwrap_or(row.mailbox)));
    let here = match &pane {
        Some(l) => format!("{l}&open={id}"),
        None => format!("/msg/{id}"),
    };
    let flag = |mode: StoreMode, f: &str| app.mail.store(&[src.id], &mode, &[f.to_string()]).map(|_| ());
    let done = match f.action.as_str() {
        "seen" => flag(StoreMode::Add, "\\Seen").map(|_| (here.clone(), None)),
        "unseen" => flag(StoreMode::Remove, "\\Seen").map(|_| (list.clone(), Some("Marked unread."))),
        "flag" => flag(StoreMode::Add, "\\Flagged").map(|_| (here.clone(), None)),
        "unflag" => flag(StoreMode::Remove, "\\Flagged").map(|_| (here.clone(), None)),
        "move" => {
            let target = f.to.as_deref().and_then(|t| t.parse::<i64>().ok()).and_then(|t| app.mail.mailbox(t));
            match (target, folder.account.as_deref()) {
                // Into a node: its folder in this message's account, made if it has none.
                (Some(t), Some(acc)) if t.is_node() => match crate::folders::member_in(&app, t.id, acc).await {
                    Ok(_) => app.mail.copy(&[src.id], t.id, true).map(|_| (list.clone(), Some("Moved."))),
                    Err(e) => Err(e),
                },
                (Some(t), _) if t.account == folder.account => {
                    app.mail.copy(&[src.id], t.id, true).map(|_| (list.clone(), Some("Moved.")))
                }
                _ => return back(&here, None, Some("Pick one of your folders, or a folder of the same account.")),
            }
        }
        "rule" => {
            let node = f.node.as_deref().and_then(|n| n.parse::<i64>().ok());
            let added = node.ok_or_else(|| anyhow::anyhow!("choose a folder")).and_then(|n| Ok((n, app.mail.add_rule(f.sender.as_deref().unwrap_or(""), n)?)));
            let (node, rule) = match added {
                Ok(a) => a,
                Err(e) => return back(&here, None, Some(&format!("{e:#}"))),
            };
            let (mut moved, mut trouble) = (0, None);
            if let Some(acc) = folder.account.as_deref().filter(|_| f.also.is_some()) {
                let this = match crate::folders::member_in(&app, node, acc).await {
                    Ok(_) => app.mail.copy(&[src.id], node, true),
                    Err(e) => Err(e),
                };
                match this {
                    Ok(_) => moved = 1,
                    Err(e) => trouble = Some(format!("the rule is added, but this message couldn't be moved: {e:#}")),
                }
            }
            if f.inbox.is_some() {
                let (n, e) = file_inbox(&app, rule).await;
                moved += n;
                trouble = trouble.or(e);
            }
            if let Some(acc) = &folder.account {
                app.mail.waker(acc).notify_one();
            }
            let said = match moved {
                0 => "Rule added.".to_string(),
                1 if f.inbox.is_none() => "Rule added, and this message moved.".to_string(),
                n => format!("Rule added, and {} moved.", messages(n)),
            };
            let to = if app.mail.message(id).is_some() { &here } else { &list };
            return back(to, Some(&said), trouble.as_deref());
        }
        "delete" => match delete(&app, &[(src.id, folder.clone())]) {
            Ok(0) => Ok((list.clone(), Some("Deleted."))),
            Ok(_) => Ok((list.clone(), Some("Moved to the Trash."))),
            Err(e) => Err(e),
        },
        "archive" => match archive(&app, &[(src.id, folder.clone())]).await {
            Ok(_) => Ok((list.clone(), Some("Archived."))),
            Err(e) => Err(e),
        },
        "notspam" => match not_spam(&app, &[(src.id, folder.clone())]) {
            Ok(0) => Err(anyhow::anyhow!("It isn't in Spam.")),
            Ok(_) => Ok((list.clone(), Some("Moved to the inbox."))),
            Err(e) => Err(e),
        },
        _ => return back(&here, None, Some("Unknown action.")),
    };
    if let Some(acc) = &folder.account {
        app.mail.waker(acc).notify_one();
    }
    match done {
        Ok((to, ok)) => back(&to, ok, None),
        Err(e) => back(&here, None, Some(&format!("{e:#}"))),
    }
}

// Folders

pub async fn folders_page(State(app): St, headers: HeaderMap, Path(account): Path<String>, Query(q): Query<ListQuery>) -> Response {
    guard!(app, headers);
    let Some(acc) = app.account(&account) else { return StatusCode::NOT_FOUND.into_response() };
    let (_, per) = folders(&app);
    let boxes = per.into_iter().find(|(a, _)| a.id == acc.id).map(|(_, b)| b).unwrap_or_default();
    let inbox = app.mail.all_inboxes();
    mail_page(&format!("{} folders", acc.name), inbox, &app, html! {
        (flash_of(&q))
        h1 { (acc.name) " folders" }
        div.card {
            @for b in &boxes {
                @let fixed = b.special.is_empty() && !b.remote.as_deref().is_some_and(|r| r.eq_ignore_ascii_case("INBOX"));
                div style="display:flex; gap:8px; align-items:center; flex-wrap:wrap; padding:6px 0; border-bottom:1px solid var(--line)" {
                    @if fixed {
                        form.row method="post" action={"/folders/" (acc.id) "/" (b.id) "/rename"} style="flex:1" {
                            input name="name" value=(b.name.rsplit('/').next().unwrap_or(&b.name)) aria-label="Name" style="flex:1; min-width:140px";
                            button type="submit" { "Rename" }
                        }
                        form method="post" action={"/folders/" (acc.id) "/" (b.id) "/delete"}
                            onsubmit="return confirm('Delete this folder and the mail in it, at the provider too?')" {
                            button.plain type="submit" { "Delete" }
                        }
                        span.muted.small style="flex-basis:100%" { (short_name(b)) " · " (app.mail.count(b.id).0) " messages" }
                    } @else {
                        span style="flex:1" { (short_name(b)) }
                        span.muted.small { "the provider's own folder" }
                    }
                }
            }
        }
        h2 { "New folder" }
        form.card.row method="post" action={"/folders/" (acc.id)} {
            label { "Name" input name="name" required; }
            label { "Inside" select name="parent" {
                option value="" { "(top level)" }
                @for b in &boxes { option value=(b.id) { (short_name(b)) } }
            } }
            button type="submit" { "Create" }
        }
        p.muted.small { "Folders are made, renamed and deleted at the provider too, so they match in every app. Mail apps can do the same." }
    })
}

#[derive(Deserialize)]
pub struct FolderForm {
    #[serde(default)]
    name: String,
    #[serde(default)]
    parent: String,
}

pub async fn create_folder(State(app): St, headers: HeaderMap, Path(account): Path<String>, axum::extract::Form(f): axum::extract::Form<FolderForm>) -> Response {
    guard!(app, headers);
    let here = format!("/folders/{account}");
    match crate::folders::create(&app, &account, f.parent.parse().ok(), &f.name).await {
        Ok(_) => back(&here, Some(&format!("Created {}.", f.name.trim())), None),
        Err(e) => back(&here, None, Some(&format!("{e:#}"))),
    }
}

pub async fn rename_folder(State(app): St, headers: HeaderMap, Path((account, id)): Path<(String, i64)>, axum::extract::Form(f): axum::extract::Form<FolderForm>) -> Response {
    guard!(app, headers);
    let here = format!("/folders/{account}");
    match crate::folders::rename(&app, id, &f.name).await {
        Ok(()) => back(&here, Some("Renamed."), None),
        Err(e) => back(&here, None, Some(&format!("{e:#}"))),
    }
}

pub async fn delete_folder(State(app): St, headers: HeaderMap, Path((account, id)): Path<(String, i64)>) -> Response {
    guard!(app, headers);
    let here = format!("/folders/{account}");
    match crate::folders::delete(&app, id).await {
        Ok(()) => back(&here, Some("Deleted."), None),
        Err(e) => back(&here, None, Some(&format!("{e:#}"))),
    }
}

// The tree of folders

const TREE_CSS: &str = r#"
.tree-edit .trow { display:flex; gap:6px; align-items:center; flex-wrap:wrap; padding:5px 0; border-bottom:1px solid var(--line); }
.tree-edit .trow:last-child { border-bottom:none; }
.tree-edit .trow.on { background:var(--bg); }
.tree-edit .trow form { display:flex; gap:4px; align-items:center; margin:0; flex-wrap:wrap; }
.tree-edit .trow input { width:180px; }
.tree-edit .trow .label { flex:1; min-width:120px; }
.tree-edit .handle { cursor:grab; color:var(--muted); user-select:none; padding:0 4px; }
.tree-edit button.step { background:var(--card); color:var(--fg); border:1px solid var(--line); padding:2px 8px; }
.tree-edit button.step:hover { background:var(--line); }
.tree-edit .trow.dragging { opacity:.4; }
.tree-edit .trow.drop-before { box-shadow: inset 0 2px 0 var(--accent); }
.tree-edit .trow.drop-after { box-shadow: inset 0 -2px 0 var(--accent); }
.tree-edit .trow.drop-inside { outline:2px solid var(--accent); outline-offset:-2px; }
.members fieldset { border:1px solid var(--line); border-radius:6px; margin:0 0 10px; padding:6px 12px; }
.members legend { font-weight:600; font-size:13px; }
.members label { flex-direction:row; align-items:center; gap:6px; color:var(--fg); font-size:14px; padding:2px 0; }
"#;

/// Drag and drop: a folder dropped on the top or bottom edge of another goes
/// before or after it, in its middle goes inside it. The whole tree is sent.
const TREE_SCRIPT: &str = r#"
(() => {
  const list = document.querySelector('.tree-edit');
  if (!list) return;
  const rows = () => [...list.querySelectorAll('.trow')];
  const places = () => rows().map(r => ({ id: +r.dataset.id, parent: r.dataset.parent ? +r.dataset.parent : null, label: r.dataset.label }));
  const within = (id, top) => {
    const by = new Map(places().map(p => [p.id, p]));
    for (let p = by.get(id); p; p = p.parent == null ? null : by.get(p.parent)) if (p.id === top) return true;
    return false;
  };
  const zone = (r, e) => {
    const b = r.getBoundingClientRect(), y = (e.clientY - b.top) / b.height;
    return y < .3 ? 'before' : y > .7 ? 'after' : 'inside';
  };
  const clear = () => rows().forEach(x => x.classList.remove('drop-before', 'drop-after', 'drop-inside'));
  let dragged = null;
  for (const r of rows()) {
    const h = r.querySelector('.handle');
    if (h) {
      h.addEventListener('dragstart', e => {
        dragged = +r.dataset.id;
        e.dataTransfer.effectAllowed = 'move';
        e.dataTransfer.setData('text/plain', r.dataset.label);
        e.dataTransfer.setDragImage(r, 10, 10);
        r.classList.add('dragging');
      });
      h.addEventListener('dragend', () => { r.classList.remove('dragging'); dragged = null; clear(); });
    }
    r.addEventListener('dragover', e => {
      if (dragged == null || within(+r.dataset.id, dragged)) return;
      e.preventDefault();
      clear();
      r.classList.add('drop-' + zone(r, e));
    });
    r.addEventListener('dragleave', () => r.classList.remove('drop-before', 'drop-after', 'drop-inside'));
    r.addEventListener('drop', async e => {
      const target = +r.dataset.id, where = zone(r, e);
      if (dragged == null || within(target, dragged)) return;
      e.preventDefault();
      let ps = places();
      const me = ps.find(p => p.id === dragged);
      ps = ps.filter(p => p.id !== dragged);
      const t = ps.find(p => p.id === target);
      if (where === 'inside') { me.parent = target; ps.push(me); }
      else { me.parent = t.parent; ps.splice(ps.indexOf(t) + (where === 'after' ? 1 : 0), 0, me); }
      const res = await fetch('/tree/layout', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(ps) });
      const j = await res.json().catch(() => ({}));
      const q = res.ok ? 'ok=' + encodeURIComponent('Moved.') : 'err=' + encodeURIComponent(j.error || 'Not moved.');
      location = '/tree?node=' + dragged + '&' + q;
    });
  }
})();
"#;

#[derive(Deserialize, Default)]
pub struct TreeQuery {
    node: Option<i64>,
    ok: Option<String>,
    err: Option<String>,
}

fn special_label(special: &str) -> &'static str {
    if special == ALL_MAIL {
        return "All mail";
    }
    SPECIALS.iter().find(|(s, _)| *s == special).map(|(_, l)| *l).unwrap_or("")
}

pub async fn tree_page(State(app): St, headers: HeaderMap, Query(q): Query<TreeQuery>) -> Response {
    guard!(app, headers);
    let (tree, per) = folders(&app);
    let inbox = app.mail.all_inboxes();
    let selected = q.node.and_then(|id| tree.iter().find(|(n, _)| n.id == id));
    let unplaced = app.mail.unplaced();
    // Each real folder's nodes, to tell where else a folder is.
    let mut in_nodes: std::collections::HashMap<i64, Vec<String>> = Default::default();
    for (n, b) in &tree {
        for m in &n.members {
            in_nodes.entry(*m).or_default().push(place_name(b));
        }
    }
    let flash = flash(&super::Msg { ok: q.ok.clone(), err: q.err.clone(), ..Default::default() });
    mail_page("Your folders", 0, &app, html! {
        style { (PreEscaped(TREE_CSS)) }
        h1 { "Your folders" }
        (flash)
        p.muted.small { "Each of your folders gathers folders of your accounts: one Spam can show the spam of all of them. "
            "Drag a folder by its handle to move it (onto the middle of another to put it inside), or use the arrows. Mail apps see the same folders." }
        div.card.tree-edit {
            @for (n, b) in &tree {
                div.trow.on[selected.is_some_and(|(s, _)| s.id == n.id)] data-id=(n.id) data-parent=(n.parent.map(|p| p.to_string()).unwrap_or_default()) data-label=(n.label)
                    style={"padding-left:" (n.depth * 24) "px"} {
                    @if n.id == inbox {
                        span.handle aria-hidden="true" { "\u{a0}\u{a0}" }
                        span.label { b { "INBOX" } span.muted.small { " · " (n.members.len()) " inboxes" } }
                    } @else {
                        span.handle draggable="true" title="Drag to move" { "⠿" }
                        form.label method="post" action={"/tree/" (n.id)} {
                            input name="label" value=(n.label) aria-label="Name" required;
                            button.step name="action" value="rename" title="Rename" { "Rename" }
                            span.muted.small {
                                @if !n.special.is_empty() { span.tag { (special_label(&n.special)) } }
                                @if b.is_all_mail() { "every folder" }
                                @else { (n.members.len()) @if n.members.len() == 1 { " folder" } @else { " folders" } }
                            }
                        }
                        form method="post" action={"/tree/" (n.id)} {
                            button.step name="action" value="up" title="Up" aria-label="Up" { "↑" }
                            button.step name="action" value="down" title="Down" aria-label="Down" { "↓" }
                            button.step name="action" value="indent" title="Into the folder above" aria-label="Into the folder above" { "→" }
                            button.step name="action" value="outdent" title="Out of its parent" aria-label="Out of its parent" { "←" }
                        }
                    }
                    @if !b.is_all_mail() { a.small href={"/tree?node=" (n.id) "#edit"} { "Choose folders" } }
                    a.small href={"/box/" (b.id)} { "Open" }
                }
            }
        }
        @if let Some((n, b)) = selected.filter(|(_, b)| b.is_all_mail()) {
            h2 #edit { (place_name(b)) }
            p.card.small { "It shows the mail of every folder of every account, Spam and Trash too: there are no folders to choose. "
                "Mail can't be moved into it." }
            form.card method="post" action={"/tree/" (n.id)} onsubmit="return confirm('Delete All mail? The mail stays in its folders; Reset to the default tree brings it back.')" {
                button.plain name="action" value="delete" { "Delete" }
            }
        }
        @if let Some((n, b)) = selected.filter(|(_, b)| !b.is_all_mail()) {
            h2 #edit { (place_name(b)) }
            form.card.members method="post" action={"/tree/" (n.id)} {
                input type="hidden" name="action" value="members";
                p.small.muted style="margin-top:0" { "The accounts' folders this one gathers:" }
                @for (a, boxes) in &per {
                    fieldset {
                        legend { (a.name) }
                        @for f in boxes {
                            label {
                                input type="checkbox" name="member" value=(f.id) checked[n.members.contains(&f.id)];
                                (short_name(f))
                                @let elsewhere: Vec<&String> = in_nodes.get(&f.id).map(|v| v.iter().filter(|x| **x != place_name(b)).collect()).unwrap_or_default();
                                @if !elsewhere.is_empty() { span.muted.small { " also in " (elsewhere.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")) } }
                            }
                        }
                    }
                }
                button type="submit" { "Save" }
            }
            @if n.id != inbox {
                form.card.row method="post" action={"/tree/" (n.id)} {
                    input type="hidden" name="action" value="special";
                    label { "Mail apps use it as their" select name="special" {
                        option value="" selected[n.special.is_empty()] { "(nothing special)" }
                        @for (s, l) in SPECIALS { option value=(s) selected[n.special == *s] { (l) " folder" } }
                    } }
                    button type="submit" { "Set" }
                    span.muted.small { "Each goes to one of your folders only." }
                }
                @let missing: Vec<&Account> = per.iter().map(|(a, _)| a).filter(|a| app.mail.member_of(n.id, &a.id).is_none()).collect();
                div.card style="display:flex; gap:10px; flex-wrap:wrap; align-items:center" {
                    @if !missing.is_empty() {
                        form method="post" action={"/tree/" (n.id)} {
                            button name="action" value="create_all" { "Make it in every account" }
                        }
                        span.muted.small { "Creates a folder named after it in " (missing.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", ")) ", at the provider." }
                    }
                    form method="post" action={"/tree/" (n.id)} style="margin-left:auto"
                        onsubmit="return confirm('Delete this folder? The mail in it stays in the accounts\\' folders.')" {
                        button.plain name="action" value="delete" { "Delete" }
                    }
                }
            }
        }
        h2 { "New folder" }
        form.card.row method="post" action="/tree" {
            label { "Name" input name="name" required; }
            label { "Inside" select name="parent" {
                option value="" { "(top level)" }
                @for (n, b) in &tree { option value=(n.id) selected[selected.is_some_and(|(s, _)| s.id == n.id)] { (indent(n.depth)) (short_name(b)) } }
            } }
            button type="submit" { "Create" }
        }
        p.muted.small { "A new folder is only valet's until mail is moved into it: then it is made in that message's account." }
        @if !unplaced.is_empty() {
            h2 { "Folders not in your tree" }
            p.muted.small { "Made later, at the provider or in another app: they are under Accounts." }
            div.card {
                @for f in &unplaced {
                    form method="post" action={"/tree/add/" (f.id)} style="display:flex; gap:8px; align-items:center; padding:4px 0" {
                        span style="flex:1" { (place_name(f)) }
                        button.step type="submit" { "Add to the tree" }
                    }
                }
            }
        }
        form method="post" action="/tree/reset" style="margin-top:24px"
            onsubmit="return confirm('Put back the default tree? Your folders are made again from the accounts\\' folders; the mail stays where it is.')" {
            button.plain type="submit" { "Reset to the default tree" }
        }
        script { (PreEscaped(TREE_SCRIPT)) }
    })
}

#[derive(Deserialize)]
pub struct NewNodeForm {
    #[serde(default)]
    name: String,
    #[serde(default)]
    parent: String,
}

pub async fn create_node(State(app): St, headers: HeaderMap, axum::extract::Form(f): axum::extract::Form<NewNodeForm>) -> Response {
    guard!(app, headers);
    match app.mail.create_node(f.parent.parse().ok(), &f.name) {
        Ok(id) => back(&format!("/tree?node={id}"), Some(&format!("Created {}.", f.name.trim())), None),
        Err(e) => back("/tree", None, Some(&format!("{e:#}"))),
    }
}

/// One node's buttons: rename, move, members, special use, making it in
/// every account, deleting.
pub async fn node_action(State(app): St, headers: HeaderMap, Path(id): Path<i64>, body: String) -> Response {
    guard!(app, headers);
    let pairs = form_urlencoded_pairs(&body);
    let get = |k: &str| pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()).unwrap_or_default();
    let here = format!("/tree?node={id}");
    let shift = |how| app.mail.shift_node(id, how).map(|_| None);
    let done: anyhow::Result<Option<String>> = match get("action").as_str() {
        "rename" => app.mail.rename_node(id, &get("label")).map(|_| Some("Renamed.".into())),
        "up" => shift(Shift::Up),
        "down" => shift(Shift::Down),
        "indent" => shift(Shift::Indent),
        "outdent" => shift(Shift::Outdent),
        "members" => {
            let members: Vec<i64> = pairs.iter().filter(|(n, _)| n == "member").filter_map(|(_, v)| v.parse().ok()).collect();
            app.mail.set_members(id, &members).map(|_| Some("Saved.".into()))
        }
        "special" => app.mail.set_special(id, &get("special")).map(|_| Some("Saved.".into())),
        "create_all" => {
            let mut made = Vec::new();
            let mut err = None;
            for a in app.accounts().into_iter().filter(|a| a.retired.is_none()) {
                if app.mail.member_of(id, &a.id).is_some() {
                    continue;
                }
                match crate::folders::member_in(&app, id, &a.id).await {
                    Ok(_) => made.push(a.name),
                    Err(e) => err = Some(anyhow::anyhow!("{}: {e:#}", a.name)),
                }
            }
            match err {
                Some(e) => Err(e),
                None => Ok(Some(format!("Made in {}.", made.join(", ")))),
            }
        }
        "delete" => match app.mail.delete_node(id) {
            Ok(()) => return back("/tree", Some("Deleted."), None),
            Err(e) => Err(e),
        },
        _ => Err(anyhow::anyhow!("unknown action")),
    };
    match done {
        Ok(ok) => back(&here, ok.as_deref(), None),
        Err(e) => back(&here, None, Some(&format!("{e:#}"))),
    }
}

/// The whole tree, from the drag and drop script.
pub async fn tree_layout(State(app): St, headers: HeaderMap, axum::Json(places): axum::Json<Vec<Place>>) -> Response {
    if !super::authed(&app, &headers) {
        return (StatusCode::UNAUTHORIZED, axum::Json(serde_json::json!({ "error": "logged out" }))).into_response();
    }
    match app.mail.arrange(&places) {
        Ok(()) => axum::Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => (StatusCode::UNPROCESSABLE_ENTITY, axum::Json(serde_json::json!({ "error": format!("{e:#}") }))).into_response(),
    }
}

pub async fn add_to_tree(State(app): St, headers: HeaderMap, Path(folder): Path<i64>) -> Response {
    guard!(app, headers);
    match app.mail.place_folder(folder) {
        Ok(node) => back(&format!("/tree?node={node}"), Some("Added."), None),
        Err(e) => back("/tree", None, Some(&format!("{e:#}"))),
    }
}

pub async fn reset_tree(State(app): St, headers: HeaderMap) -> Response {
    guard!(app, headers);
    match app.mail.reset_tree() {
        Ok(()) => back("/tree", Some("Back to the default tree."), None),
        Err(e) => back("/tree", None, Some(&format!("{e:#}"))),
    }
}

// Mail rules

fn messages(n: usize) -> String {
    if n == 1 { "1 message".to_string() } else { format!("{n} messages") }
}

/// Moves the mail already in live accounts' inboxes that follows this rule
/// (not the mail another, closer rule has): how many, and what went wrong.
async fn file_inbox(app: &Arc<App>, rule: i64) -> (usize, Option<String>) {
    let rules = app.mail.rules();
    let Some(node) = rules.iter().find(|r| r.id == rule).and_then(|r| r.node) else { return (0, Some("that rule has no folder".into())) };
    let (mut moved, mut trouble) = (0, None);
    for b in app.mail.mailboxes().into_iter().filter(|b| !b.is_node() && b.is_inbox()) {
        let Some(acc) = b.account.as_deref().and_then(|a| app.account(a)).filter(|a| a.retired.is_none()) else { continue };
        let follows = |m: &Msg| {
            let Some(f) = facts(app, &m.blob) else { return false };
            f.from.first().and_then(|(_, address)| crate::mailstore::rule_of(&rules, address)).is_some_and(|r| r.id == rule)
        };
        let ids: Vec<i64> = app.mail.messages(b.id).iter().filter(|m| follows(m)).map(|m| m.id).collect();
        if ids.is_empty() {
            continue;
        }
        let done = match crate::folders::member_in(app, node, &acc.id).await {
            // Its folder here is this inbox: nothing to move.
            Ok(folder) if folder == b.id => continue,
            Ok(_) => app.mail.copy(&ids, node, true),
            Err(e) => Err(e),
        };
        match done {
            Ok(_) => moved += ids.len(),
            Err(e) => trouble = Some(format!("{}: {e:#}", acc.name)),
        }
        app.mail.waker(&acc.id).notify_one();
    }
    (moved, trouble)
}

/// The folders a rule can file into, with the one it does selected.
fn rule_folders(tree: &[(Node, Mailbox)], inbox: i64, on: Option<i64>) -> Markup {
    html! {
        select name="node" aria-label="Folder" required {
            @if on.is_none() { option value="" selected { "Choose a folder…" } }
            @for (n, b) in tree.iter().filter(|(n, b)| n.id != inbox && !b.is_all_mail()) {
                option value=(n.id) selected[on == Some(n.id)] { (indent(n.depth)) (short_name(b)) }
            }
        }
    }
}

pub async fn rules_page(State(app): St, headers: HeaderMap, Query(m): Query<super::Msg>) -> Response {
    guard!(app, headers);
    let (tree, _) = folders(&app);
    let inbox = app.mail.all_inboxes();
    let rules = app.mail.rules();
    page("Mail rules", "/rules", html! {
        h1 { "Mail rules" }
        (flash(&m))
        p.muted.small { "New mail coming into an inbox from these senders goes straight to the folder, at the provider too, without a notification. "
            "Invitations in it still reach the calendar. Mail already there stays unless you ask for it to move; a message moved back from valet stays in the inbox. "
            "A rule covers every account: a folder an account doesn't have yet is made there, as when moving mail by hand." }
        div.card {
            @if rules.is_empty() { p.muted.small style="margin:0" { "No rule yet." } }
            @for r in &rules {
                form.row method="post" action={"/rules/" (r.id)} style="padding:5px 0" {
                    input name="sender" value=(r.sender) aria-label="Sender" required;
                    (rule_folders(&tree, inbox, r.node))
                    button type="submit" { "Save" }
                    @if r.node.is_some() { button.plain style="color:var(--fg)" type="submit" formaction={"/rules/" (r.id) "/inbox"} formnovalidate title="Move the mail already in the inbox that follows this rule" { "Move inbox mail" } }
                    button.plain type="submit" formaction={"/rules/" (r.id) "/delete"} formnovalidate { "Delete" }
                    @if r.node.is_none() { span.err.small { "Its folder was deleted: choose another. Until then, this mail stays in the inbox." } }
                }
            }
        }
        h2 { "New rule" }
        form.card.row method="post" action="/rules" {
            label { "From" input name="sender" placeholder="someone@acme.com or @acme.com" style="width:280px" required; }
            label { "Goes to" (rule_folders(&tree, inbox, None)) }
            label style="flex-direction:row; align-items:center; gap:4px" { input type="checkbox" name="inbox" value="1"; "Move what's in the inbox from them too" }
            button type="submit" { "Add" }
        }
        p.muted.small { "@acme.com covers the domain and its subdomains. An address's own rule comes first, then the closest domain's." }
    })
}

#[derive(Deserialize)]
pub struct RuleForm {
    #[serde(default)]
    sender: String,
    #[serde(default)]
    node: String,
    #[serde(default)]
    inbox: Option<String>,
}

impl RuleForm {
    fn node(&self) -> anyhow::Result<i64> {
        self.node.parse().map_err(|_| anyhow::anyhow!("choose a folder"))
    }
}

pub async fn create_rule(State(app): St, headers: HeaderMap, axum::extract::Form(f): axum::extract::Form<RuleForm>) -> Response {
    guard!(app, headers);
    match f.node().and_then(|node| app.mail.add_rule(&f.sender, node)) {
        Ok(rule) if f.inbox.is_some() => {
            let (n, trouble) = file_inbox(&app, rule).await;
            back("/rules", Some(&format!("Rule added, and {} moved.", messages(n))), trouble.as_deref())
        }
        Ok(_) => back("/rules", Some("Rule added."), None),
        Err(e) => back("/rules", None, Some(&format!("{e:#}"))),
    }
}

pub async fn save_rule(State(app): St, headers: HeaderMap, Path(id): Path<i64>, axum::extract::Form(f): axum::extract::Form<RuleForm>) -> Response {
    guard!(app, headers);
    match f.node().and_then(|node| app.mail.set_rule(id, &f.sender, node)) {
        Ok(()) => back("/rules", Some("Saved."), None),
        Err(e) => back("/rules", None, Some(&format!("{e:#}"))),
    }
}

/// A rule's *Move inbox mail*.
pub async fn file_rule(State(app): St, headers: HeaderMap, Path(id): Path<i64>) -> Response {
    guard!(app, headers);
    let (n, trouble) = file_inbox(&app, id).await;
    let said = if n == 0 { "No mail in the inbox follows it.".to_string() } else { format!("Moved {}.", messages(n)) };
    back("/rules", (trouble.is_none() || n > 0).then_some(said.as_str()), trouble.as_deref())
}

pub async fn delete_rule(State(app): St, headers: HeaderMap, Path(id): Path<i64>) -> Response {
    guard!(app, headers);
    match app.mail.delete_rule(id) {
        Ok(()) => back("/rules", Some("Deleted."), None),
        Err(e) => back("/rules", None, Some(&format!("{e:#}"))),
    }
}

// Writing

#[derive(Deserialize, Default)]
pub struct ComposeQuery {
    reply: Option<i64>,
    all: Option<i64>,
    forward: Option<i64>,
    draft: Option<i64>,
}

/// What the form holds: a new message, a reply, a forward, or a draft.
#[derive(Default)]
struct Draft {
    from: String,
    to: String,
    cc: String,
    bcc: String,
    subject: String,
    /// The text, as HTML (sanitized when it comes from the form).
    html: String,
    in_reply_to: String,
    references: String,
    /// The message whose attachments go along (the forwarded one, or the
    /// draft last saved), minus those in `drop`.
    carry: Option<i64>,
    drop: Vec<usize>,
    /// The saved draft this replaces.
    draft_id: Option<i64>,
    replying: Option<i64>,
}

fn prefixed(prefix: &str, subject: &str) -> String {
    let s = subject.trim();
    if s.to_lowercase().starts_with(&prefix.to_lowercase()) { s.to_string() } else { format!("{prefix} {s}") }
}

/// What a message reads as, in HTML fit to quote or edit: its HTML part
/// cleaned up, or its text.
fn quotable_html(m: &mail_parser::Message) -> String {
    let real_html = m.html_body.first().and_then(|&i| m.part(i)).is_some_and(|p| p.is_text_html());
    match (real_html, m.body_html(0)) {
        (true, Some(h)) => clean(&h),
        _ => mail_parser::decoders::html::text_to_html(&m.body_text(0).unwrap_or_default()),
    }
}

/// HTML written in the editor (or pasted into it), reduced to what mail needs.
fn clean(html: &str) -> String {
    ammonia::Builder::default()
        .add_tags(["font", "center"])
        .add_generic_attributes(["align"])
        .url_schemes(["http", "https", "mailto", "tel"].into_iter().collect())
        .clean(html)
        .to_string()
}

fn escape(s: &str) -> String {
    maud::html! { (s) }.into_string()
}

/// The address of mine the message was sent to, for answering from it.
fn my_address(app: &App, m: &mail_parser::Message, account: Option<&str>) -> Option<String> {
    let accounts = app.accounts();
    let mut to: Vec<String> = addresses_of(m.to());
    to.extend(addresses_of(m.cc()));
    to.extend(m.header_values("Delivered-To").filter_map(|v| v.as_text().map(str::to_string)));
    for addr in &to {
        if accounts.iter().any(|a| a.sends_now(addr)) {
            return Some(addr.to_lowercase());
        }
    }
    account.and_then(|id| accounts.iter().find(|a| a.id == id && a.retired.is_none())).map(|a| a.address.clone())
}

fn is_draft(m: &Msg, folder: &Mailbox) -> bool {
    folder.special.eq_ignore_ascii_case("\\Drafts") || has_flag(m, "\\Draft")
}

fn header_ids(v: &mail_parser::HeaderValue) -> Vec<String> {
    match v {
        mail_parser::HeaderValue::Text(t) => vec![format!("<{t}>")],
        mail_parser::HeaderValue::TextList(l) => l.iter().map(|t| format!("<{t}>")).collect(),
        _ => vec![],
    }
}

fn draft_for(app: &App, q: &ComposeQuery) -> Draft {
    let (id, mode) = match (q.reply, q.all, q.forward, q.draft) {
        (Some(id), ..) => (id, "reply"),
        (_, Some(id), ..) => (id, "all"),
        (_, _, Some(id), _) => (id, "forward"),
        (.., Some(id)) => (id, "draft"),
        _ => return Draft { html: "<div><br></div>".into(), ..Default::default() },
    };
    let Some((_, src, folder)) = locate(app, id) else { return Draft::default() };
    let raw = app.mail.blob(&src.blob).unwrap_or_default();
    let Some(m) = parse(&raw) else { return Draft::default() };
    let subject = m.subject().unwrap_or("");
    if mode == "draft" {
        return Draft {
            from: addresses_of(m.from()).into_iter().next().unwrap_or_default(),
            to: everyone(m.to()),
            cc: everyone(m.cc()),
            bcc: everyone(m.bcc()),
            subject: subject.to_string(),
            html: quotable_html(&m),
            in_reply_to: header_ids(m.in_reply_to()).join(" "),
            references: header_ids(m.references()).join(" "),
            carry: (m.attachment_count() > 0).then_some(id),
            draft_id: is_draft(&src, &folder).then_some(id),
            ..Default::default()
        };
    }
    let from = my_address(app, &m, folder.account.as_deref()).unwrap_or_default();
    let when = m.date().map(|d| d.to_rfc822()).unwrap_or_default();
    let mut d = Draft { from: from.clone(), ..Default::default() };
    if mode == "forward" {
        d.subject = prefixed("Fwd:", subject);
        d.html = format!(
            "<div><br></div><div>---------- Forwarded message ----------<br>From: {}<br>Date: {}<br>Subject: {}<br>To: {}</div><br>{}",
            escape(&everyone(m.from())),
            escape(&when),
            escape(subject),
            escape(&everyone(m.to())),
            quotable_html(&m)
        );
        d.carry = (m.attachment_count() > 0).then_some(id);
        return d;
    }
    d.to = if m.reply_to().is_some() { everyone(m.reply_to()) } else { everyone(m.from()) };
    if mode == "all" {
        let mine = |a: &str| a.eq_ignore_ascii_case(&from) || app.accounts().iter().any(|x| x.sends_as(a));
        let mut others: Vec<String> = Vec::new();
        for x in [m.to(), m.cc()].into_iter().flatten().flat_map(|l| l.iter()) {
            let Some(addr) = x.address() else { continue };
            if mine(addr) || d.to.to_lowercase().contains(&addr.to_lowercase()) {
                continue;
            }
            others.push(match x.name().filter(|n| !n.trim().is_empty()) {
                Some(n) => format!("{n} <{addr}>"),
                None => addr.to_string(),
            });
        }
        d.cc = others.join(", ");
    }
    d.subject = prefixed("Re:", subject);
    d.html = format!(
        "<div><br></div><div>On {}, {} wrote:</div><blockquote>{}</blockquote>",
        escape(&when),
        escape(&who(m.from())),
        quotable_html(&m)
    );
    if let Some(mid) = m.message_id() {
        d.in_reply_to = format!("<{mid}>");
        d.references = header_ids(m.references()).into_iter().chain(std::iter::once(d.in_reply_to.clone())).collect::<Vec<_>>().join(" ");
    }
    d.replying = Some(id);
    d
}

/// The names of the attachments a draft carries along.
fn carried(app: &App, d: &Draft) -> Vec<(usize, String)> {
    use mail_parser::MimeHeaders;
    let Some((_, src, _)) = d.carry.and_then(|c| locate(app, c)) else { return vec![] };
    let raw = app.mail.blob(&src.blob).unwrap_or_default();
    let Some(m) = parse(&raw) else { return vec![] };
    m.attachments().enumerate().filter(|(i, _)| !d.drop.contains(i)).map(|(i, a)| (i, a.attachment_name().unwrap_or("attachment").to_string())).collect()
}

const EDITOR_CSS: &str = r#"
.editor-bar { display:flex; gap:4px; flex-wrap:wrap; border:1px solid var(--line); border-bottom:none; border-radius:6px 6px 0 0; padding:4px; background:var(--bg); }
.editor-bar button { background:none; color:var(--fg); border:1px solid transparent; border-radius:4px; padding:3px 9px; min-width:32px; }
.editor-bar button:hover { background:var(--line); }
.editor { min-height:320px; padding:10px 12px; border:1px solid var(--line); border-radius:0 0 6px 6px; background:var(--bg); color:var(--fg); overflow-wrap:anywhere; }
.editor:focus { outline:2px solid var(--accent); outline-offset:-1px; }
.editor blockquote { margin:0 0 0 4px; padding-left:10px; border-left:3px solid var(--line); color:var(--muted); }
.editor img { max-width:100%; }
.write-actions { display:flex; gap:10px; align-items:center; flex-wrap:wrap; }
.write-actions .status { color:var(--muted); font-size:13px; }
.carried label { display:inline-flex; flex-direction:row; align-items:center; gap:4px; margin-right:12px; color:var(--fg); }
"#;

const EDITOR_SCRIPT: &str = r#"
(() => {
  const form = document.querySelector('form.write'), ed = form.querySelector('.editor');
  // Enter goes to the next line, as in mail apps; a <p> would leave a blank line's space.
  document.execCommand('defaultParagraphSeparator', false, 'div');
  const status = form.querySelector('.status');
  let dirty = false, saving = null, sending = false;
  for (const b of form.querySelectorAll('.editor-bar button')) {
    b.addEventListener('mousedown', e => e.preventDefault());
    b.addEventListener('click', () => {
      const cmd = b.dataset.cmd;
      if (cmd === 'createLink') {
        const url = prompt('Link to');
        if (url) document.execCommand('createLink', false, /^[a-z]+:/i.test(url) ? url : 'https://' + url);
      } else if (cmd === 'formatBlock') {
        document.execCommand('formatBlock', false, b.dataset.arg);
      } else {
        document.execCommand(cmd, false, null);
      }
      ed.focus(); dirty = true;
    });
  }
  const sync = () => { form.elements.html.value = ed.innerHTML; };
  form.addEventListener('input', () => { dirty = true; });
  form.addEventListener('change', () => { dirty = true; });
  const carried = form.querySelector('.carried');
  const show = names => {
    carried.innerHTML = '';
    names.forEach(([i, n]) => {
      const l = document.createElement('label');
      const c = document.createElement('input');
      c.type = 'checkbox'; c.name = 'drop'; c.value = i; c.title = 'Leave out';
      l.append('📎 ' + n + ' ', c, ' leave out');
      carried.append(l);
    });
  };
  async function save() {
    if (!dirty || saving || sending) return saving;
    dirty = false; sync();
    const data = new FormData(form);
    const hadFiles = data.getAll('files').some(f => f.size);
    saving = (async () => {
      try {
        const r = await fetch('/compose/draft', { method: 'POST', body: data });
        const j = await r.json();
        if (!r.ok) throw new Error(j.error || 'not saved');
        form.elements.draft_id.value = j.draft;
        form.elements.carry.value = j.draft;
        if (hadFiles) form.elements.files.value = '';
        show(j.attachments);
        status.textContent = 'Draft saved ' + new Date().toLocaleTimeString([], valetTime.opts({ hour: '2-digit', minute: '2-digit' }));
        status.title = valetTime.note(Date.now());
      } catch (e) {
        dirty = true;
        status.textContent = 'Draft not saved: ' + e.message;
      } finally {
        saving = null;
      }
    })();
    return saving;
  }
  setInterval(save, 8000);
  form.querySelector('[data-save]').addEventListener('click', () => { dirty = true; save(); });
  form.addEventListener('submit', async e => {
    sync();
    if (e.submitter && e.submitter.name === 'discard') { sending = true; return; }
    sending = true;
    if (saving) { e.preventDefault(); await saving; form.submit(); }
  });
  window.addEventListener('pagehide', () => {
    if (dirty && !sending) { sync(); navigator.sendBeacon('/compose/draft', new FormData(form)); }
  });
})();
"#;

fn compose_view(app: &App, d: &Draft, err: Option<&str>) -> Response {
    let accounts: Vec<Account> = app.accounts().into_iter().filter(|a| a.retired.is_none()).collect();
    let senders: Vec<(String, String)> = accounts
        .iter()
        .flat_map(|a| std::iter::once(&a.address).chain(&a.aliases).map(move |addr| (addr.clone(), a.name.clone())))
        .collect();
    let inbox = app.mail.all_inboxes();
    let tool = |cmd: &str, arg: &str, label: Markup, title: &str| {
        html! { button type="button" data-cmd=(cmd) data-arg=(arg) title=(title) { (label) } }
    };
    mail_page("Write", inbox, app, html! {
        style { (PreEscaped(EDITOR_CSS)) }
        @if let Some(e) = err { p.err { (e) } }
        @if senders.is_empty() {
            p.muted { "Add a mail account first, on the " a href="/mail" { "Accounts" } " page." }
        } @else {
            form.card.write method="post" action="/compose" enctype="multipart/form-data" {
                label { "From" select name="from" {
                    @for (addr, name) in &senders { option value=(addr) selected[addr.eq_ignore_ascii_case(&d.from)] { (addr) " (" (name) ")" } }
                } }
                label { "To" input name="to" value=(d.to) placeholder="name@example.com, …" autofocus[d.to.is_empty()]; }
                label { "Cc" input name="cc" value=(d.cc); }
                details open[!d.bcc.is_empty()] { summary.small { "Bcc" } label { input name="bcc" value=(d.bcc); } }
                label { "Subject" input name="subject" value=(d.subject); }
                div {
                    div.editor-bar {
                        (tool("bold", "", html! { b { "B" } }, "Bold"))
                        (tool("italic", "", html! { i { "I" } }, "Italic"))
                        (tool("underline", "", html! { u { "U" } }, "Underline"))
                        (tool("strikeThrough", "", html! { s { "S" } }, "Strikethrough"))
                        (tool("createLink", "", html! { "Link" }, "Link"))
                        (tool("insertUnorderedList", "", html! { "• List" }, "Bulleted list"))
                        (tool("insertOrderedList", "", html! { "1. List" }, "Numbered list"))
                        (tool("formatBlock", "blockquote", html! { "Quote" }, "Quote"))
                        (tool("formatBlock", "h3", html! { "Heading" }, "Heading"))
                        (tool("formatBlock", "div", html! { "Text" }, "Normal text"))
                        (tool("removeFormat", "", html! { "Clear" }, "Clear formatting"))
                    }
                    div.editor contenteditable="true" autofocus[!d.to.is_empty()] { (PreEscaped(&d.html)) }
                }
                div.carried.small {
                    @for (i, name) in carried(app, d) {
                        label { "📎 " (name) " " input type="checkbox" name="drop" value=(i) title="Leave out"; " leave out" }
                    }
                }
                label { "Attach files" input type="file" name="files" multiple; }
                input type="hidden" name="html";
                input type="hidden" name="in_reply_to" value=(d.in_reply_to);
                input type="hidden" name="references" value=(d.references);
                input type="hidden" name="carry" value=(d.carry.map(|c| c.to_string()).unwrap_or_default());
                input type="hidden" name="draft_id" value=(d.draft_id.map(|c| c.to_string()).unwrap_or_default());
                input type="hidden" name="replying" value=(d.replying.map(|c| c.to_string()).unwrap_or_default());
                div.write-actions {
                    button type="submit" name="send" value="1" { "Send" }
                    button.plain type="button" data-save style="color:var(--fg)" { "Save draft" }
                    button.plain type="submit" name="discard" value="1" formnovalidate
                        onclick="return confirm('Discard this message?')" { "Discard" }
                    span.status { @if d.draft_id.is_some() { "Draft" } }
                }
            }
            script { (PreEscaped(EDITOR_SCRIPT)) }
        }
    })
}

pub async fn compose(State(app): St, headers: HeaderMap, Query(q): Query<ComposeQuery>) -> Response {
    guard!(app, headers);
    compose_view(&app, &draft_for(&app, &q), None)
}

/// Whether the provider puts mail sent through it in Sent by itself (then
/// valet doesn't, or it would be there twice).
fn saves_sent(acc: &Account) -> bool {
    let smtp = acc.smtp.to_ascii_lowercase();
    smtp.contains("gmail.com") || smtp.contains("office365.com") || smtp.contains("outlook.com")
}

type Files = Vec<(String, String, Vec<u8>)>;

/// The compose form, and the buttons pressed.
async fn read_form(mut form: Multipart) -> (Draft, Files, std::collections::HashMap<String, String>) {
    let mut fields: std::collections::HashMap<String, String> = Default::default();
    let mut drop = Vec::new();
    let mut files: Files = Vec::new();
    while let Ok(Some(field)) = form.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        if name == "files" {
            let filename = field.file_name().unwrap_or("").to_string();
            let ctype = field.content_type().unwrap_or("application/octet-stream").to_string();
            let Ok(data) = field.bytes().await else { continue };
            if !filename.is_empty() && !data.is_empty() {
                files.push((filename, ctype, data.to_vec()));
            }
        } else if let Ok(v) = field.text().await {
            if name == "drop" {
                drop.extend(v.parse::<usize>().ok());
            } else {
                fields.insert(name, v);
            }
        }
    }
    let get = |k: &str| fields.get(k).cloned().unwrap_or_default();
    let draft = Draft {
        from: get("from"),
        to: get("to"),
        cc: get("cc"),
        bcc: get("bcc"),
        subject: get("subject"),
        html: clean(&get("html")),
        in_reply_to: get("in_reply_to"),
        references: get("references"),
        carry: get("carry").parse().ok(),
        drop,
        draft_id: get("draft_id").parse().ok(),
        replying: get("replying").parse().ok(),
    };
    (draft, files, fields)
}

pub async fn send(State(app): St, headers: HeaderMap, form: Multipart) -> Response {
    guard!(app, headers);
    let (draft, files, fields) = read_form(form).await;
    let inbox = format!("/box/{}", app.mail.all_inboxes());
    if fields.contains_key("discard") {
        if let Some(d) = draft.draft_id {
            let _ = forget_draft(&app, d);
        }
        return back(&inbox, Some("Discarded."), None);
    }
    match deliver(&app, &draft, files).await {
        Ok(name) => {
            if let Some(r) = draft.replying.and_then(|r| locate(&app, r)) {
                let _ = app.mail.store(&[r.1.id], &StoreMode::Add, &["\\Answered".to_string()]);
            }
            if let Some(d) = draft.draft_id {
                let _ = forget_draft(&app, d);
            }
            back(&inbox, Some(&format!("Sent from {name}.")), None)
        }
        Err(e) => compose_view(&app, &draft, Some(&format!("Not sent: {e:#}"))),
    }
}

fn forget_draft(app: &App, id: i64) -> anyhow::Result<()> {
    let Some((row, src, folder)) = locate(app, id) else { return Ok(()) };
    if is_draft(&src, &folder) {
        app.mail.expunge(&[row.id])?;
    }
    Ok(())
}

/// Tells an open page that some mail changed (the webmail then fetches its
/// list again, every page the app's badge), as server-sent events; ends when
/// valet stops.
pub async fn events(State(app): St, headers: HeaderMap) -> Response {
    use axum::response::sse::{Event, KeepAlive, Sse};
    use tokio::sync::broadcast::error::{RecvError, TryRecvError};
    if !authed(&app, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let changes = app.mail.changed.subscribe();
    let stopping = app.stopping.subscribe();
    let stream = futures::stream::unfold((changes, stopping), |(mut changes, mut stopping)| async move {
        if *stopping.borrow() {
            return None;
        }
        tokio::select! {
            r = changes.recv() => if let Err(RecvError::Closed) = r { return None },
            _ = stopping.changed() => return None,
        }
        // Let a burst settle (a sync brings messages 25 at a time).
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        while !matches!(changes.try_recv(), Err(TryRecvError::Empty | TryRecvError::Closed)) {}
        // (An event without data isn't dispatched.)
        Some((Ok::<_, std::convert::Infallible>(Event::default().event("changed").data("1")), (changes, stopping)))
    });
    Sse::new(stream).keep_alive(KeepAlive::default()).into_response()
}

#[derive(Deserialize)]
pub struct RefreshQuery {
    /// The folder shown, synced right after the inboxes.
    folder: Option<i64>,
}

/// Checks every account for new mail now: wakes their syncs and answers
/// once each has been through its inbox and the real folders of `folder`
/// (with none given, or All mail, through all its folders), or has failed,
/// or after 30 seconds. Says which failed, and which it stopped waiting for.
pub async fn refresh(State(app): St, headers: HeaderMap, Query(q): Query<RefreshQuery>) -> Response {
    if !authed(&app, &headers) {
        return (StatusCode::UNAUTHORIZED, axum::Json(serde_json::json!({ "error": "logged out" }))).into_response();
    }
    let shown: Vec<Mailbox> = match q.folder.and_then(|id| app.mail.mailbox(id)) {
        Some(b) if b.is_node() => app.mail.node(b.id).map(|n| n.members).unwrap_or_default().into_iter().filter_map(|m| app.mail.mailbox(m)).collect(),
        Some(b) => vec![b],
        None => Vec::new(),
    };
    for b in shown {
        if let (Some(account), Some(remote)) = (b.account, b.remote) {
            app.mail.hurry(&account, [remote]);
        }
    }
    let whole = q.folder.is_none() || q.folder.and_then(|id| app.mail.mailbox(id)).is_some_and(|b| b.is_all_mail());
    let mut passed = app.mail.passed();
    // A pass started after this is one that started later than any had
    // started now (and took the folders asked for).
    let accounts: Vec<(Account, crate::mailstore::Passes)> = app.accounts().into_iter().filter(|a| a.retired.is_none()).map(|a| { let then = app.mail.passes(&a.id); (a, then) }).collect();
    for (a, _) in &accounts {
        app.mail.waker(&a.id).notify_one();
    }
    let synced = |a: &Account, then: &crate::mailstore::Passes| {
        let p = app.mail.passes(&a.id);
        if whole { p.finished > then.started } else { p.ahead > then.started }
    };
    let failed = |a: &Account, then: &crate::mailstore::Passes| !synced(a, then) && app.mail.passes(&a.id).failed > then.failed;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    while !accounts.iter().all(|(a, then)| synced(a, then) || failed(a, then)) {
        if !matches!(tokio::time::timeout_at(deadline, passed.changed()).await, Ok(Ok(()))) {
            break;
        }
    }
    let failures: Vec<_> = accounts
        .iter()
        .filter(|(a, then)| failed(a, then))
        .map(|(a, _)| serde_json::json!({ "account": a.name, "error": app.mail.status(&a.id).map(|s| s.1).unwrap_or_default() }))
        .collect();
    let waiting: Vec<&str> = accounts.iter().filter(|(a, then)| !synced(a, then) && !failed(a, then)).map(|(a, _)| a.name.as_str()).collect();
    axum::Json(serde_json::json!({ "done": waiting.is_empty(), "failed": failures, "waiting": waiting })).into_response()
}

/// Saves the form as a draft in the From account's Drafts folder, replacing
/// the one saved before. Answers the new draft's id and its attachments.
pub async fn save_draft(State(app): St, headers: HeaderMap, form: Multipart) -> Response {
    if !super::authed(&app, &headers) {
        return (StatusCode::UNAUTHORIZED, axum::Json(serde_json::json!({ "error": "logged out" }))).into_response();
    }
    let (draft, files, _) = read_form(form).await;
    let saved = async {
        use anyhow::Context;
        let acc = app.accounts().into_iter().find(|a| a.sends_now(&draft.from)).context("pick an address to send from")?;
        let message = build(&app, &draft, files, true)?;
        let folder = match app.mail.special_folder(&acc.id, "\\Drafts") {
            Some(b) => b,
            None => match app.mail.mailboxes().into_iter().find(|b| b.account.as_deref() == Some(acc.id.as_str()) && short_name(b) == "Drafts") {
                Some(b) => b.id,
                None => crate::folders::create(&app, &acc.id, None, "Drafts").await?,
            },
        };
        let id = app.mail.append_row(folder, &message.formatted(), &["\\Draft".into(), "\\Seen".into()], crate::mailstore::now())?;
        if let Some(old) = draft.draft_id.filter(|&o| o != id) {
            forget_draft(&app, old)?;
        }
        app.mail.waker(&acc.id).notify_one();
        anyhow::Ok(id)
    }
    .await;
    match saved {
        Ok(id) => {
            let names: Vec<(usize, String)> = carried(&app, &Draft { carry: Some(id), ..Default::default() });
            axum::Json(serde_json::json!({ "draft": id, "attachments": names })).into_response()
        }
        Err(e) => (StatusCode::UNPROCESSABLE_ENTITY, axum::Json(serde_json::json!({ "error": format!("{e:#}") }))).into_response(),
    }
}

/// The message the form describes. A draft may have no recipient yet and
/// keeps its Bcc.
fn build(app: &App, d: &Draft, mut files: Files, draft: bool) -> anyhow::Result<lettre::Message> {
    use anyhow::Context;
    use lettre::message::header::ContentType;
    use lettre::message::{Attachment, Mailboxes, MultiPart};
    let parse_list = |s: &str| -> anyhow::Result<Mailboxes> {
        let s = s.trim().trim_end_matches(',');
        if s.is_empty() { Ok(Mailboxes::new()) } else { s.parse::<Mailboxes>().map_err(|e| anyhow::anyhow!("{s}: {e}")) }
    };
    let (to, cc, bcc) = (parse_list(&d.to)?, parse_list(&d.cc)?, parse_list(&d.bcc)?);
    let from: lettre::message::Mailbox = d.from.parse().context("the From address")?;
    let mut b = lettre::Message::builder().from(from.clone()).subject(d.subject.clone()).date_now();
    for m in to.iter() {
        b = b.to(m.clone());
    }
    for m in cc.iter() {
        b = b.cc(m.clone());
    }
    for m in bcc.iter() {
        b = b.bcc(m.clone());
    }
    if draft {
        b = b.keep_bcc();
        if to.iter().chain(cc.iter()).chain(bcc.iter()).next().is_none() {
            b = b.envelope(lettre::address::Envelope::new(Some(from.email.clone()), vec![from.email.clone()])?);
        }
    } else {
        anyhow::ensure!(to.iter().chain(cc.iter()).chain(bcc.iter()).next().is_some(), "who is it for?");
    }
    if !d.in_reply_to.is_empty() {
        b = b.in_reply_to(d.in_reply_to.clone()).references(d.references.clone());
    }
    let domain = d.from.rsplit_once('@').map(|(_, h)| h.to_string()).unwrap_or_else(|| "valet".into());
    b = b.message_id(Some(format!("<{}@{domain}>", uuid::Uuid::new_v4().simple())));
    let mut all: Files = Vec::new();
    if let Some((_, src, _)) = d.carry.and_then(|c| locate(app, c)) {
        let raw = app.mail.blob(&src.blob).unwrap_or_default();
        if let Some(m) = parse(&raw) {
            use mail_parser::MimeHeaders;
            for (i, a) in m.attachments().enumerate() {
                if d.drop.contains(&i) {
                    continue;
                }
                let ct = a.content_type().map(|c| format!("{}/{}", c.ctype(), c.subtype().unwrap_or("octet-stream"))).unwrap_or_default();
                all.push((a.attachment_name().unwrap_or("attachment").to_string(), ct, a.contents().to_vec()));
            }
        }
    }
    all.append(&mut files);
    let text = mail_parser::decoders::html::html_to_text(&d.html);
    let body = MultiPart::alternative_plain_html(text.trim_end().to_string(), format!("<!doctype html><html><body>{}</body></html>", d.html));
    let message = if all.is_empty() {
        b.multipart(body)?
    } else {
        let mut mp = MultiPart::mixed().multipart(body);
        for (name, ct, data) in all {
            let ct = ContentType::parse(&ct).unwrap_or(ContentType::parse("application/octet-stream").unwrap());
            mp = mp.singlepart(Attachment::new(name).body(data, ct));
        }
        b.multipart(mp)?
    };
    Ok(message)
}

/// Sends through the account the message is from, and keeps a copy in that
/// account's Sent folder. Returns the account's name.
async fn deliver(app: &App, d: &Draft, files: Files) -> anyhow::Result<String> {
    use anyhow::Context;
    let acc = app.accounts().into_iter().find(|a| a.sends_now(&d.from)).context("pick an address to send from")?;
    let message = build(app, d, files, false)?;
    let recipients: Vec<String> = message.envelope().to().iter().map(|a| a.to_string()).collect();
    let raw = crate::smtpd::strip_bcc(&message.formatted());
    crate::accounts::send(&acc, &app.mail, &d.from, &recipients, &raw).await?;
    if !saves_sent(&acc)
        && let Some(sent) = app.mail.special_folder(&acc.id, "\\Sent")
    {
        app.mail.append(sent, &raw, &["\\Seen".to_string()], crate::mailstore::now())?;
        app.mail.waker(&acc.id).notify_one();
    }
    Ok(acc.name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(cid_links(r#"<img src="cid:abc@x"> cid: text"#, 7), r#"<img src="/msg/7/cid/abc@x"> cid: text"#);
        assert!(has_remote(r#"<img SRC="https://t.example/p.gif">"#) && !has_remote("<p>http://x</p>"));
        assert_eq!(prefixed("Re:", "re: hi"), "re: hi");
        assert_eq!(prefixed("Re:", "hi"), "Re: hi");
        assert_eq!(human_size(2048), "2 KB");
    }

    #[test]
    fn reads_names_and_search() {
        let raw = b"From: \"Ann B\" <ann@x.fr>\r\nTo: me@y.fr, Bob <bob@z.fr>\r\nSubject: =?utf-8?q?R=C3=A9union?=\r\n\r\nOrdre du jour\r\n";
        let m = parse(raw).unwrap();
        assert_eq!(who(m.from()), "Ann B");
        assert_eq!(everyone(m.to()), "me@y.fr, Bob <bob@z.fr>");
        assert!(matches(raw, &["réunion".into(), "jour".into()]));
        assert!(!matches(raw, &["absent".into()]));
    }

    fn query(pairs: &str) -> ListQuery {
        Query::<ListQuery>::try_from_uri(&format!("/box/1?{pairs}").parse().unwrap()).unwrap().0
    }

    #[test]
    fn filters_in_the_address() {
        let q = query("q=r%C3%A9union&read=unread&from=Ann+B&when=week&since=2026-01-01&page=2");
        assert_eq!(q.link(4, 0, &[]), "/box/4?page=0&q=r%C3%A9union&read=unread&from=Ann%20B&when=week");
        assert_eq!(q.link(4, 1, &["q"]), "/box/4?page=1&read=unread&from=Ann%20B&when=week");
        assert_eq!(q.link(4, 0, &FILTER_KEYS), "/box/4?page=0&q=r%C3%A9union");
        let chips: Vec<String> = q.chips().into_iter().map(|(l, _)| l).collect();
        assert_eq!(chips, ["Unread", "From: Ann B", "Last 7 days"]);
        let chips = |s: &str| query(s).chips().into_iter().map(|(l, _)| l).collect::<Vec<_>>();
        assert_eq!(chips("when=custom&since=2026-10-20&until=2026-10-12"), ["12 Oct 2026 – 20 Oct 2026"]);
        assert_eq!(chips("when=custom&until=2026-10-12"), ["Until 12 Oct 2026"]);
        assert!(chips("when=custom&read=everything&from=+").is_empty());
    }

    #[test]
    fn filter_keeps() {
        let msg = |flags: &[&str], date: i64| Msg {
            id: 1, mailbox: 1, uid: 1, blob: String::new(), flags: flags.iter().map(|f| f.to_string()).collect(), date, size: 0, modseq: 0, remote_uid: None, link: None,
        };
        let f = Filter::of(&query("read=unread&starred=1"));
        assert!(f.keeps(&msg(&["\\Flagged"], 0)));
        assert!(!f.keeps(&msg(&["\\Flagged", "\\Seen"], 0)) && !f.keeps(&msg(&[], 0)));
        assert!(Filter::of(&query("read=read")).keeps(&msg(&["\\seen"], 0)));
        // Days in the settings' zone (Paris), both included, either order.
        let f = Filter::of(&query("when=custom&since=2026-10-12&until=2026-10-10"));
        assert!(f.keeps(&msg(&[], 1_791_583_200)), "10 Oct, 00:00 in Paris");
        assert!(!f.keeps(&msg(&[], 1_791_583_199)));
        assert!(f.keeps(&msg(&[], 1_791_842_399)) && !f.keeps(&msg(&[], 1_791_842_400)), "until the end of 12 Oct");
        let now = crate::mailstore::now();
        let week = Filter::of(&query("when=week"));
        assert!(week.keeps(&msg(&[], now - 6 * 86400)) && !week.keeps(&msg(&[], now - 9 * 86400)));
        assert!(!Filter::of(&query("")).opens() && Filter::of(&query("to=x")).opens());
    }

    #[test]
    fn filter_reads_people_and_parts() {
        let facts = Facts {
            from: vec![("Ann B".into(), "ann@x.fr".into())],
            to: vec![("".into(), "me@y.fr".into()), ("Bob".into(), "bob@z.fr".into())],
            attachment: true,
            invite: false,
        };
        assert!(Filter::of(&query("from=ANN")).keeps_facts(&facts) && Filter::of(&query("from=x.fr&to=bob")).keeps_facts(&facts));
        assert!(!Filter::of(&query("from=bob")).keeps_facts(&facts));
        assert!(Filter::of(&query("attachment=1")).keeps_facts(&facts) && !Filter::of(&query("invite=1")).keeps_facts(&facts));
        let invite = b"From: o@x\r\nContent-Type: multipart/mixed; boundary=b\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nhi\r\n--b\r\nContent-Type: text/calendar; method=REQUEST\r\n\r\nBEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:u\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n--b--\r\n";
        let m = parse(invite).unwrap();
        assert!(crate::invites::is_invitation(&m));
        assert!(!m.attachments().any(|a| shown_attachment(a, has_html_body(&m)) && !crate::invites::is_calendar(a)), "its .ics isn't an attachment");
        let reply = String::from_utf8_lossy(invite).replace("REQUEST", "REPLY");
        assert!(!crate::invites::is_invitation(&parse(reply.as_bytes()).unwrap()), "an answer isn't an invitation");
    }
}
