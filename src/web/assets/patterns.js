// The alert pattern form: one row per alert (how long before or after the
// start, or a time of day for all-day events) instead of a typed list. It
// writes the list valet reads ("12m, 1d 18:00") into the form's field.
(() => {
  'use strict';
  const UNITS = [['m', 1, 'minutes'], ['h', 60, 'hours'], ['d', 1440, 'days'], ['w', 10080, 'weeks']];
  const pad = (n) => String(n).padStart(2, '0');
  const el = (tag, attrs = {}, ...kids) => {
    const e = document.createElement(tag);
    for (const [k, v] of Object.entries(attrs)) {
      if (k === 'class') e.className = v;
      else if (v !== false && v != null) e.setAttribute(k, v === true ? '' : v);
    }
    e.append(...kids);
    return e;
  };
  const option = (value, label, selected) => el('option', { value, selected }, label);

  for (const box of document.querySelectorAll('[data-alerts]')) {
    const input = box.querySelector('input[name="alerts"]');
    const rows = el('div', { class: 'alert-rows' });

    // An alert relative to the start: 12 minutes before.
    const relative = (minutes = -10) => {
      const m = Math.abs(minutes);
      const unit = [...UNITS].reverse().find(([, n]) => m && m % n === 0) || UNITS[0];
      const n = el('input', { type: 'number', min: '0', max: '999', value: m / unit[1], required: true, style: 'width:5em' });
      const u = el('select', {}, ...UNITS.map(([k, , label]) => option(k, label, k === unit[0])));
      const side = el('select', {}, option('before', 'before the start', minutes <= 0), option('after', 'after the start', minutes > 0));
      const row = el('div', { class: 'alert-row' }, n, u, side);
      row.spec = () => `${side.value === 'after' ? '+' : ''}${Number(n.value) || 0}${u.value}`;
      return row;
    };
    // An alert at a time of day, for all-day events: 18:00 the day before.
    const clock = (minutes = 9 * 60) => {
      const days = Math.max(0, Math.ceil(-minutes / 1440));
      const time = minutes + days * 1440;
      const t = el('input', { type: 'time', value: `${pad(Math.floor(time / 60))}:${pad(time % 60)}`, required: true });
      const d = el('select', {}, ...[0, 1, 2, 3, 7].map((k) => option(k, k === 0 ? 'the same day' : k === 1 ? 'the day before' : `${k} days before`, k === days)));
      if (![0, 1, 2, 3, 7].includes(days)) d.append(option(days, `${days} days before`, true));
      const row = el('div', { class: 'alert-row' }, 'at ', t, d);
      row.spec = () => (Number(d.value) ? `${d.value}d ` : '') + (t.value || '09:00');
      return row;
    };
    const add = (row) => {
      const remove = el('button', { type: 'button', class: 'plain', title: 'Remove this alert', 'aria-label': 'Remove this alert' }, '×');
      remove.addEventListener('click', () => row.remove());
      row.append(remove);
      rows.append(row);
    };
    for (const m of JSON.parse(box.dataset.alerts || '[]')) add(m > 0 && m < 1440 ? clock(m) : relative(m));
    const addRel = el('button', { type: 'button', class: 'plain', style: 'color:var(--fg)' }, '+ Before or after the start');
    const addClock = el('button', { type: 'button', class: 'plain', style: 'color:var(--fg)' }, '+ At a time of day (all-day events)');
    addRel.addEventListener('click', () => add(relative()));
    addClock.addEventListener('click', () => add(clock()));
    input.closest('label').hidden = true;
    box.append(el('span', { class: 'small muted' }, 'Alerts'), rows, el('div', { class: 'alert-add' }, addRel, addClock),
      el('span', { class: 'small muted' }, 'No alert at all: a silent pattern.'));
    input.form.addEventListener('submit', () => { input.value = [...rows.children].map((r) => r.spec()).join(', '); });
  }
})();
