// The calendar page: month, week and day views drawn from valet's month
// feeds (/calendar/month/YYYY-MM). The service worker keeps those feeds,
// so months seen or fetched ahead stay readable offline. Times are in the
// zone of the settings (time.js), whatever the device's.
(() => {
  'use strict';
  const root = document.getElementById('cal');
  const lang = navigator.language || 'en';
  const T = window.valetTime;
  const zone = T.zone;
  const months = new Map(); // 'YYYY-MM' -> { data, offline } or { missing: true }
  let calendars = [], patterns = [];
  const hidden = new Set(JSON.parse(localStorage.getItem('valet.cal.hidden') || '[]'));
  const fmt = (o) => new Intl.DateTimeFormat(lang, o);
  const fTime = fmt({ hour: '2-digit', minute: '2-digit' });
  const fDay = fmt({ weekday: 'long', day: 'numeric', month: 'long' });
  const fDayShort = fmt({ weekday: 'short', day: 'numeric', month: 'short' });
  const fMonth = fmt({ month: 'long', year: 'numeric' });
  const fWd = fmt({ weekday: 'short' });
  const fAt = fmt({ hour: '2-digit', minute: '2-digit' });

  // Dates are local midnights, and times wall clocks in valet's zone
  // (T.wall): the Date's own fields are what the page shows.
  const pad = (n) => String(n).padStart(2, '0');
  const ymd = (d) => `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
  const hm = (d) => `${pad(d.getHours())}:${pad(d.getMinutes())}`;
  const parseYmd = (s) => { const [y, m, d] = s.split('-').map(Number); return new Date(y, m - 1, d); };
  const addDays = (d, n) => new Date(d.getFullYear(), d.getMonth(), d.getDate() + n);
  const today = () => { const n = T.wall(Date.now()); return new Date(n.getFullYear(), n.getMonth(), n.getDate()); };
  const weekStart = (d) => addDays(d, -((d.getDay() + 6) % 7));
  const monthKey = (d) => `${d.getFullYear()}-${pad(d.getMonth() + 1)}`;
  const sameDay = (a, b) => ymd(a) === ymd(b);
  // Hours from b to a on the clock (a day of daylight saving still has 24).
  const clockHours = (a, b) => {
    const at = (x) => Date.UTC(x.getFullYear(), x.getMonth(), x.getDate(), x.getHours(), x.getMinutes(), x.getSeconds());
    return (at(a) - at(b)) / 3600000;
  };
  const el = (tag, attrs = {}, ...kids) => {
    const e = document.createElement(tag);
    for (const [k, v] of Object.entries(attrs)) {
      if (k === 'class') e.className = v;
      else if (k.startsWith('on')) e.addEventListener(k.slice(2), v);
      else if (v !== false && v != null) e.setAttribute(k, v === true ? '' : v);
    }
    for (const k of kids.flat()) if (k != null) e.append(k);
    return e;
  };

  // State lives in the address: #week/2026-10-23
  function readHash() {
    const m = /^#(month|week|day)\/(\d{4}-\d{2}-\d{2})$/.exec(location.hash);
    return m ? { view: m[1], date: parseYmd(m[2]) } : null;
  }
  let state = readHash() || { view: localStorage.getItem('valet.cal.view') || (innerWidth < 720 ? 'day' : 'week'), date: today() };
  const go = (view, date) => { location.hash = `${view}/${ymd(date)}`; };

  async function loadMonth(key, fresh) {
    if (!fresh && months.has(key) && !months.get(key).missing) return months.get(key);
    try {
      const r = await fetch(`/calendar/month/${key}`, { cache: 'no-store' });
      if (r.status === 401) { location = '/login'; return null; }
      if (!r.ok) throw new Error(r.status);
      const data = await r.json();
      const m = { data, offline: r.headers.get('X-Valet-Offline') };
      months.set(key, m);
      calendars = data.calendars; patterns = data.patterns;
      return m;
    } catch (_) {
      months.set(key, { missing: true });
      return months.get(key);
    }
  }

  const color = (id) => (calendars.find((c) => c.id === id) || {}).color || '#3b82f6';
  const calName = (id) => (calendars.find((c) => c.id === id) || {}).name || id;

  // The events overlapping [from, to), from the months covering it.
  async function eventsIn(from, to) {
    const keys = [];
    for (let d = new Date(from.getFullYear(), from.getMonth(), 1); d < to; d = new Date(d.getFullYear(), d.getMonth() + 1, 1)) keys.push(monthKey(d));
    const got = await Promise.all(keys.map((k) => loadMonth(k)));
    const seen = new Set(), out = [];
    const status = { offline: null, missing: false };
    for (const m of got) {
      if (!m || m.missing) { status.missing = true; continue; }
      if (m.offline) status.offline = m.offline;
      for (const e of m.data.events) {
        const s = e.allDay ? parseYmd(e.start) : T.wall(e.start * 1000);
        const f = e.allDay ? parseYmd(e.end) : T.wall(e.end * 1000);
        const key = `${e.calendar}/${e.name}/${s.getTime()}`;
        if (seen.has(key) || hidden.has(e.calendar)) continue;
        seen.add(key);
        const overlap = f > s ? s < to && f > from : s >= from && s < to;
        if (overlap) out.push({ ...e, s, f });
      }
    }
    out.sort((a, b) => (b.allDay - a.allDay) || (a.s - b.s) || a.title.localeCompare(b.title));
    return { events: out, status };
  }

  function bar(title) {
    const views = el('span', { class: 'cal-views' },
      ...['month', 'week', 'day'].map((v) => el('button', { class: state.view === v ? 'on' : '', onclick: () => { localStorage.setItem('valet.cal.view', v); go(v, state.date); } }, v[0].toUpperCase() + v.slice(1))));
    const step = (n) => {
      const d = state.date;
      if (state.view === 'month') return go('month', new Date(d.getFullYear(), d.getMonth() + n, 1));
      go(state.view, addDays(d, n * (state.view === 'week' ? 7 : 1)));
    };
    return el('div', { class: 'cal-bar' },
      el('button', { onclick: () => go(state.view, today()) }, 'Today'),
      el('button', { onclick: () => step(-1), 'aria-label': 'Previous' }, '‹'),
      el('button', { onclick: () => step(1), 'aria-label': 'Next' }, '›'),
      el('h1', {}, title),
      el('span', { class: 'cal-zone', 'data-zone': '', title: `Times are in ${T.zoneNote(Date.now())}: change it in Settings` }, T.label),
      views,
      el('button', { class: 'primary', onclick: () => edit(null, { date: state.view === 'month' ? today() : state.date }) }, 'New event'));
  }

  function calendarFilters() {
    return el('div', { class: 'cal-cals' }, calendars.map((c) => el('label', {},
      el('input', { type: 'checkbox', checked: !hidden.has(c.id), onchange: (e) => {
        e.target.checked ? hidden.delete(c.id) : hidden.add(c.id);
        localStorage.setItem('valet.cal.hidden', JSON.stringify([...hidden]));
        render();
      } }),
      el('span', { class: 'dot', style: `background:${c.color}` }), c.name)));
  }

  function note(status) {
    if (status.missing) return el('div', { class: 'cal-note' }, navigator.onLine ? 'Some of these days could not be loaded.' : "Offline: these days weren't kept on this device.");
    // A page shown offline says so already.
    if (status.offline && document.getElementById('offline-banner')) return null;
    if (status.offline) return el('div', { class: 'cal-note' }, `Offline: as of ${fAt.format(T.wall(Number(status.offline) * 1000))} (${T.label}). Changes wait until you're back online.`);
    return null;
  }

  // An event's hover: its title, and for timed ones when, in which zone.
  const hover = (e) => (e.allDay ? e.title : `${e.title}\n${T.note(e.start * 1000)}`) + (e.unanswered ? '\nNot answered yet' : '');

  // Invitations not answered yet are drawn hollow, in their calendar's color.
  function chip(e, day) {
    const startsHere = !e.allDay && sameDay(e.s, day);
    return el('span', {
      class: 'chip' + (e.allDay ? ' allday' : '') + (e.unanswered ? ' unanswered' : ''),
      style: `--c:${color(e.calendar)}`,
      title: hover(e),
      onclick: (ev) => { ev.stopPropagation(); show(e); },
    }, e.allDay ? null : el('span', { class: 'd' }), startsHere ? el('span', { class: 't' }, fTime.format(e.s)) : null, e.title || '(no title)');
  }

  async function month() {
    const first = new Date(state.date.getFullYear(), state.date.getMonth(), 1);
    const start = weekStart(first);
    const last = new Date(first.getFullYear(), first.getMonth() + 1, 0);
    const end = addDays(weekStart(last), 7);
    const { events, status } = await eventsIn(start, end);
    const grid = el('div', { class: 'cal-month' });
    for (let i = 0; i < 7; i++) grid.append(el('div', { class: 'wd' }, fWd.format(addDays(start, i))));
    const max = innerWidth < 720 ? 2 : 3;
    for (let d = start; d < end; d = addDays(d, 1)) {
      const day = d, next = addDays(d, 1);
      const here = events.filter((e) => e.s < next && (e.f > day || (+e.f === +e.s && e.s >= day)));
      const cell = el('div', {
        class: 'cell' + (day.getMonth() !== first.getMonth() ? ' other' : '') + (sameDay(day, today()) ? ' today' : ''),
        onclick: () => edit(null, { date: day }),
      }, el('span', { class: 'num', onclick: (ev) => { ev.stopPropagation(); go('day', day); } }, day.getDate()));
      here.slice(0, max).forEach((e) => cell.append(chip(e, day)));
      if (here.length > max) cell.append(el('div', { class: 'more', onclick: (ev) => { ev.stopPropagation(); go('day', day); } }, `+${here.length - max} more`));
      grid.append(cell);
    }
    return [bar(fMonth.format(first)), calendarFilters(), note(status), grid];
  }

  // Side by side when they overlap: each event gets a column of its cluster.
  function layout(list) {
    const out = [];
    let cluster = [], end = 0;
    const flush = () => {
      const cols = [];
      for (const e of cluster) {
        let c = cols.findIndex((last) => last <= e.top);
        if (c < 0) { c = cols.length; cols.push(0); }
        cols[c] = e.top + e.height;
        e.col = c;
      }
      cluster.forEach((e) => { e.cols = cols.length; out.push(e); });
      cluster = [];
    };
    for (const e of list.sort((a, b) => a.top - b.top || b.height - a.height)) {
      if (cluster.length && e.top >= end) flush();
      cluster.push(e);
      end = Math.max(end, e.top + e.height);
    }
    flush();
    return out;
  }

  async function week(days) {
    const start = days === 7 ? weekStart(state.date) : state.date;
    const end = addDays(start, days);
    const { events, status } = await eventsIn(start, end);
    const cols = `48px repeat(${days}, minmax(0, 1fr))`;
    const dates = [...Array(days).keys()].map((i) => addDays(start, i));
    const head = el('div', { class: 'head', style: `grid-template-columns:${cols}` }, el('div'),
      dates.map((d) => el('div', { class: sameDay(d, today()) ? 'today' : '', onclick: () => go('day', d) }, fWd.format(d), el('b', {}, d.getDate()))));
    const alldays = el('div', { class: 'alldays', style: `grid-template-columns:${cols}` }, el('div', {}, 'all day'),
      dates.map((d) => el('div', { onclick: () => edit(null, { date: d, allDay: true }) },
        events.filter((e) => e.allDay && e.s < addDays(d, 1) && e.f > d).map((e) => chip(e, d)))));
    const H = parseFloat(getComputedStyle(root).getPropertyValue('--hour')) || 44;
    const hours = el('div', { class: 'hours', title: `Hours in ${T.zoneNote(Date.now())}` }, [...Array(24).keys()].map((h) => el('div', {}, h ? `${pad(h)}:00` : '')));
    const grid = el('div', { class: 'grid', style: `grid-template-columns:${cols}; height:${24 * H}px` }, hours);
    for (const d of dates) {
      const next = addDays(d, 1);
      const col = el('div', {
        class: 'col' + (sameDay(d, today()) ? ' today' : ''),
        onclick: (ev) => {
          const y = ev.offsetY, min = Math.max(0, Math.min(23 * 60 + 30, Math.floor(y / H * 2) * 30));
          edit(null, { date: d, minutes: min });
        },
      });
      const placed = events.filter((e) => !e.allDay && e.s < next && (e.f > d || (+e.f === +e.s && e.s >= d))).map((e) => {
        const s = e.s < d ? d : e.s, f = e.f > next ? next : e.f;
        const top = clockHours(s, d) * H;
        return { e, top, height: Math.max(clockHours(f, s) * H, H / 2.2) };
      });
      for (const p of layout(placed)) {
        const w = 100 / p.cols;
        col.append(el('div', {
          class: 'ev' + (p.e.unanswered ? ' unanswered' : ''),
          style: `top:${p.top}px; height:${p.height - 2}px; left:calc(${p.col * w}% + 1px); width:calc(${w}% - 3px); --c:${color(p.e.calendar)}`,
          title: hover(p.e),
          onclick: (ev) => { ev.stopPropagation(); show(p.e); },
        }, el('div', {}, p.e.title || '(no title)'), el('div', { class: 't' }, `${fTime.format(p.e.s)} – ${fTime.format(p.e.f)}`)));
      }
      if (sameDay(d, today())) {
        const n = T.wall(Date.now());
        col.append(el('div', { class: 'now', style: `top:${clockHours(n, d) * H}px` }));
      }
      grid.append(col);
    }
    const scroll = el('div', { class: 'scroll', 'data-at': `${state.view}/${ymd(state.date)}` }, grid);
    const title = days === 7
      ? `${fmt({ day: 'numeric', month: 'short' }).format(start)} – ${fmt({ day: 'numeric', month: 'short', year: 'numeric' }).format(addDays(start, 6))}`
      : fDay.format(state.date);
    // Where it was when drawn again; else from the first event or 7:00.
    const before = root.querySelector('.cal-week .scroll');
    const keep = before && before.dataset.at === scroll.dataset.at ? before.scrollTop : null;
    requestAnimationFrame(() => {
      scroll.scrollTop = keep ?? Math.max(0, Math.min(placedTop(events, dates, H) ?? 7 * H, 7 * H) - 8);
    });
    return [bar(title), calendarFilters(), note(status), el('div', { class: 'cal-week' }, head, alldays, scroll)];
  }

  function placedTop(events, dates, H) {
    const tops = events.filter((e) => !e.allDay && dates.some((d) => sameDay(d, e.s))).map((e) => (e.s.getHours() + e.s.getMinutes() / 60) * H);
    return tops.length ? Math.min(...tops) : null;
  }

  let drawing = 0;
  async function render() {
    const mine = ++drawing;
    const parts = state.view === 'month' ? await month() : await week(state.view === 'week' ? 7 : 1);
    if (mine !== drawing) return;
    root.replaceChildren(...parts.filter(Boolean));
  }

  // The card of one event.
  const dlg = el('dialog', { class: 'cal-dlg' });
  document.body.append(dlg);
  dlg.addEventListener('click', (e) => { if (e.target === dlg) dlg.close(); });

  function whenText(e) {
    if (e.allDay) {
      const last = addDays(e.f, -1);
      return sameDay(e.s, last) ? fDay.format(e.s) : `${fDayShort.format(e.s)} – ${fDayShort.format(last)}`;
    }
    if (sameDay(e.s, e.f) || +e.f === +e.s) return `${fDay.format(e.s)}, ${fTime.format(e.s)} – ${fTime.format(e.f)}`;
    return `${fDayShort.format(e.s)} ${fTime.format(e.s)} – ${fDayShort.format(e.f)} ${fTime.format(e.f)}`;
  }

  function show(e) {
    const online = navigator.onLine;
    dlg.replaceChildren(...[
      el('h2', {}, e.title || '(no title)'),
      el('div', { class: 'meta' }, whenText(e),
        e.allDay ? null : [' · ', el('span', { 'data-zone': '', title: T.note(e.start * 1000) }, T.label)],
        e.repeats ? ' · repeats' : '', el('br'),
        el('span', { class: 'dot', style: `display:inline-block;width:9px;height:9px;border-radius:50%;background:${color(e.calendar)};margin-right:5px` }), calName(e.calendar),
        e.location ? [el('br'), '📍 ', e.location] : null),
      e.unanswered ? answering(e, online) : null,
      e.notes ? el('div', { class: 'notes' }, e.notes) : null,
      el('div', { class: 'actions' },
        el('button', { class: 'plain left', disabled: !online, onclick: () => remove(e) }, 'Delete'),
        el('button', { type: 'button', style: 'background:var(--card);color:var(--fg);border:1px solid var(--line)', onclick: () => dlg.close() }, 'Close'),
        el('button', { disabled: !online, onclick: () => edit(e) }, e.repeats ? 'Edit the series' : 'Edit')),
      online ? null : el('p', { class: 'muted small' }, 'Offline: editing waits until you are back online.')].filter(Boolean));
    if (!dlg.open) dlg.showModal();
  }

  // An invitation not answered yet: answered here when valet got it by mail
  // (the organizer gets the reply), else from where it came.
  function answering(e, online) {
    if (!e.invite) return el('p', { class: 'small' }, 'Not answered yet: answer it from your mail or calendar app.');
    const mine = calendars.filter((c) => c.id !== 'invitations');
    const cal = el('select', { name: 'calendar' },
      mine.map((c) => el('option', { value: c.id, selected: c.id === e.calendar }, c.name)), el('option', { value: '' }, 'Invitations'));
    const err = el('p', { class: 'err small' });
    const answer = async (a) => {
      try {
        const r = await post('/calendar/invite', { invite: e.invite, answer: a, calendar: cal.value });
        // The bar's count of invitations waiting.
        const count = document.querySelector('header a[href="/invitations"] .count');
        if (count) r.waiting ? (count.textContent = r.waiting) : count.remove();
        await changed();
      }
      catch (x) { err.textContent = x.message; }
    };
    const button = (a, label, attrs = {}) => el('button', { type: 'button', disabled: !online, onclick: () => answer(a), ...attrs }, label);
    return el('div', { class: 'answer' },
      el('p', { class: 'small' }, 'Not answered yet: the organizer gets your answer by email.'),
      el('div', { class: 'row' }, el('label', {}, 'Put it in', cal),
        button('ACCEPTED', 'Accept'), button('TENTATIVE', 'Maybe'), button('DECLINED', 'Decline', { class: 'plain' })),
      err);
  }

  async function post(url, data) {
    const r = await fetch(url, { method: 'POST', body: new URLSearchParams(data) });
    const text = await r.text();
    let j = {};
    try { j = JSON.parse(text); } catch (_) {}
    if (r.status === 401) { location = '/login'; throw new Error('logged out'); }
    // Whatever valet (or what stands before it) said, rather than nothing.
    if (!r.ok) throw new Error(j.error || `not saved: ${text.trim().slice(0, 200) || r.statusText || 'error ' + r.status}`);
    return j;
  }

  async function changed() {
    months.clear();
    dlg.close();
    await render();
    prefetch();
    // valet's Android app: its alarms follow.
    if (window.ValetApp && window.ValetApp.refreshAlarms) window.ValetApp.refreshAlarms();
  }

  async function remove(e) {
    const what = e.repeats ? 'Delete every occurrence of this event?' : 'Delete this event?';
    if (!confirm(what)) return;
    try { await post('/calendar/event/delete', { calendar: e.calendar, name: e.name }); await changed(); }
    catch (err) { alert(err.message); }
  }

  // The form: a new event (at `at`), or the series of `e`.
  async function edit(e, at = {}) {
    if (!navigator.onLine) { alert("You're offline: adding and editing wait until you are back online."); return; }
    if (!calendars.length) await loadMonth(monthKey(today()));
    let ev = null;
    if (e) {
      const r = await fetch(`/calendar/event?calendar=${encodeURIComponent(e.calendar)}&name=${encodeURIComponent(e.name)}`, { cache: 'no-store' });
      if (!r.ok) { alert('That event could not be opened.'); return; }
      ev = await r.json();
    }
    const allDay = ev ? ev.allDay : !!at.allDay;
    let s, f;
    if (ev) {
      s = ev.allDay ? parseYmd(ev.start) : T.wall(ev.start * 1000);
      f = ev.allDay ? addDays(parseYmd(ev.end), -1) : T.wall(ev.end * 1000);
    } else {
      const base = at.date || today();
      const min = at.minutes ?? 9 * 60;
      s = new Date(base.getFullYear(), base.getMonth(), base.getDate(), Math.floor(min / 60), min % 60);
      f = allDay ? s : new Date(s.getFullYear(), s.getMonth(), s.getDate(), s.getHours() + 1, s.getMinutes());
    }
    const field = (label, input) => el('label', {}, label, input);
    const input = (name, value, attrs = {}) => el('input', { name, value: value ?? '', ...attrs });
    const err = el('p', { class: 'err' });
    const choices = calendars.filter((c) => c.id !== 'invitations' || (ev && ev.calendar === 'invitations'));
    // None of the user's own yet: saving makes one.
    const cal = el('select', { name: 'calendar' }, choices.length
      ? choices.map((c) => el('option', { value: c.id, selected: ev ? c.id === ev.calendar : c.id === localStorage.getItem('valet.cal.last') }, c.name))
      : el('option', { value: '' }, 'Calendar (new)'));
    const repeat = el('select', { name: 'repeat' },
      [['none', 'Never'], ['daily', 'Every day'], ['weekly', 'Every week'], ['monthly', 'Every month'], ['yearly', 'Every year']]
        .concat(ev && ev.repeat === 'custom' ? [['custom', 'As it is (set in another app)']] : [])
        .map(([v, l]) => el('option', { value: v, selected: (ev ? ev.repeat : 'none') === v }, l)));
    const pattern = el('select', { name: 'pattern' }, el('option', { value: '' }, 'Automatic'),
      patterns.map((p) => el('option', { value: p.id, selected: ev && ev.pattern === p.id }, `${p.name}${p.ring ? ' ⏰' : ''}`)));
    const allDayBox = el('input', { type: 'checkbox', name: 'all_day', value: '1', checked: allDay });
    const times = [input('start_time', hm(s), { type: 'time', required: true }), input('end_time', hm(f), { type: 'time', required: true })];
    const sync = () => times.forEach((t) => { t.closest('label').style.display = allDayBox.checked ? 'none' : ''; t.disabled = allDayBox.checked; });
    allDayBox.addEventListener('change', sync);
    const startDate = input('start_date', ymd(s), { type: 'date', required: true });
    const endDate = input('end_date', ymd(f), { type: 'date', required: true });
    // Moving the start moves the end with it.
    let lastStart = startDate.value;
    startDate.addEventListener('change', () => {
      if (!startDate.value || !lastStart) return;
      const shift = (parseYmd(startDate.value) - parseYmd(lastStart)) / 86400000;
      if (endDate.value) endDate.value = ymd(addDays(parseYmd(endDate.value), Math.round(shift)));
      lastStart = startDate.value;
    });
    const form = el('form', {},
      el('h2', {}, ev ? (ev.repeat !== 'none' ? 'Edit the series' : 'Edit event') : 'New event'),
      field('Title', input('title', ev ? ev.title : '', { required: true, autofocus: true })),
      el('label', { class: 'row-check' }, allDayBox, 'All day'),
      el('div', { class: 'two' }, field('Starts', startDate), field(`at (${T.label})`, times[0])),
      el('div', { class: 'two' }, field('Ends', endDate), field(`at (${T.label})`, times[1])),
      el('div', { class: 'two' }, field('Repeats', repeat), field('Calendar', cal)),
      field('Alerts', pattern),
      field('Place', input('location', ev ? ev.location : '')),
      field('Notes', el('textarea', { name: 'notes' }, ev ? ev.notes : '')),
      ev && ev.meeting ? el('p', { class: 'muted small' }, "This one has guests: changes made here stay on your calendars, the others aren't told.") : null,
      err,
      el('div', { class: 'actions' },
        el('button', { type: 'button', style: 'background:var(--card);color:var(--fg);border:1px solid var(--line)', onclick: () => dlg.close() }, 'Cancel'),
        el('button', { type: 'submit' }, 'Save')));
    form.addEventListener('submit', async (x) => {
      x.preventDefault();
      const data = new FormData(form);
      data.set('tz', zone);
      if (ev) { data.set('from_calendar', ev.calendar); data.set('name', ev.name); }
      try {
        const saved = await post('/calendar/event', data);
        localStorage.setItem('valet.cal.last', saved.calendar);
        await changed();
      } catch (e2) { err.textContent = e2.message; }
    });
    dlg.replaceChildren(form);
    sync();
    if (!dlg.open) dlg.showModal();
    form.elements.title.focus();
  }

  // Months around today, kept by the service worker for offline reading.
  let prefetching = false;
  async function prefetch() {
    if (prefetching || !navigator.onLine) return;
    prefetching = true;
    const t = today();
    for (let i = -1; i <= 6; i++) await loadMonth(monthKey(new Date(t.getFullYear(), t.getMonth() + i, 1)), true);
    prefetching = false;
  }

  window.addEventListener('hashchange', () => { state = readHash() || state; render(); });
  window.addEventListener('online', () => { months.clear(); render(); });
  document.addEventListener('keydown', (e) => {
    if (dlg.open || e.target.closest('input, textarea, select') || e.metaKey || e.ctrlKey || e.altKey) return;
    const keys = { m: 'month', w: 'week', d: 'day' };
    if (keys[e.key]) go(keys[e.key], state.date);
    else if (e.key === 't') go(state.view, today());
    else if (e.key === 'ArrowLeft' || e.key === 'ArrowRight') root.querySelectorAll('.cal-bar button')[e.key === 'ArrowLeft' ? 1 : 2].click();
  });
  // Swipe to the previous or next page on phones.
  let touch = null;
  root.addEventListener('touchstart', (e) => { touch = e.touches.length === 1 ? [e.touches[0].clientX, e.touches[0].clientY] : null; }, { passive: true });
  root.addEventListener('touchend', (e) => {
    if (!touch || dlg.open) return;
    const dx = e.changedTouches[0].clientX - touch[0], dy = e.changedTouches[0].clientY - touch[1];
    if (Math.abs(dx) > 70 && Math.abs(dx) > 2 * Math.abs(dy)) root.querySelectorAll('.cal-bar button')[dx > 0 ? 1 : 2].click();
  });
  // The red line follows the time.
  setInterval(() => { if (state.view !== 'month' && !dlg.open) render(); }, 5 * 60 * 1000);

  render().then(prefetch);
})();
