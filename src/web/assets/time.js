// Every time valet shows is in one zone (Settings, Overview: Paris unless
// changed), and says so on hover; on a device set to another zone, the
// hover gives the device's time too. Loaded first, for the pages' scripts.
(() => {
  'use strict';
  const here = Intl.DateTimeFormat().resolvedOptions().timeZone;
  const valid = (z) => { try { new Intl.DateTimeFormat('en', { timeZone: z }); return true; } catch (_) { return false; } };
  const set = document.querySelector('meta[name="valet-tz"]')?.content;
  const zone = set && valid(set) ? set : 'Europe/Paris';
  const label = (z) => z === 'UTC' ? 'UTC' : `${z.slice(z.lastIndexOf('/') + 1).replace(/_/g, ' ')} time`;
  const part = (f, t, type) => f.formatToParts(t).find((p) => p.type === type)?.value || '';
  const fShort = new Intl.DateTimeFormat('en-GB', { timeZone: zone, timeZoneName: 'short' });
  const fOffset = new Intl.DateTimeFormat('en', { timeZone: zone, timeZoneName: 'longOffset' });
  const full = (t, z) => new Date(t).toLocaleString([], { timeZone: z, dateStyle: 'full', timeStyle: 'short' });
  // `Paris time (Europe/Paris, CEST, UTC+02:00)`, as the server says it; `t` in milliseconds.
  const zoneNote = (t) => {
    if (zone === 'UTC') return 'UTC';
    const abbr = part(fShort, t, 'timeZoneName'), offset = part(fOffset, t, 'timeZoneName').replace('GMT', 'UTC');
    return `${label(zone)} (${zone}, ${abbr && !abbr.startsWith('GMT') ? abbr + ', ' : ''}${offset === 'UTC' ? 'UTC+00:00' : offset})`;
  };
  const elsewhere = (t) => here && here !== zone ? `\nOn this device (${label(here)}): ${full(t, here)}` : '';
  // The hover of a time: the moment in full, its zone, this device's time.
  const note = (t) => `${full(t, zone)}\n${zoneNote(t)}${elsewhere(t)}`;
  // A Date whose own fields (getHours…) are the wall clock in valet's zone.
  const fWall = new Intl.DateTimeFormat('en-US', { timeZone: zone, hourCycle: 'h23', year: 'numeric', month: 'numeric', day: 'numeric', hour: 'numeric', minute: 'numeric', second: 'numeric' });
  const wall = (t) => {
    const p = Object.fromEntries(fWall.formatToParts(t).map((x) => [x.type, x.value]));
    return new Date(+p.year, p.month - 1, +p.day, +p.hour % 24, +p.minute, +p.second);
  };
  // Options for toLocale…String, in valet's zone.
  const opts = (o = {}) => ({ ...o, timeZone: zone });
  window.valetTime = { zone, here, label: label(zone), note, zoneNote, wall, opts, full: (t) => full(t, zone) };
  // Times the server wrote (<time datetime>): the full hover, when first hovered.
  document.addEventListener('mouseover', (e) => {
    const el = e.target.closest?.('time[datetime]');
    if (!el || el.dataset.noted) return;
    const t = Date.parse(el.getAttribute('datetime'));
    if (!isNaN(t)) { el.title = note(t); el.dataset.noted = '1'; }
  });
})();
