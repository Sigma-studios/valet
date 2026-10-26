// On every page: the service worker (offline reading, notifications), and a
// word when a page is shown offline. In valet's Android app, window.ValetApp is there too.
(() => {
  'use strict';
  if ('serviceWorker' in navigator) navigator.serviceWorker.register('/sw.js', { scope: '/' }).catch(() => {});
  // Notifications on here: valet keeps knowing this device's subscription
  // (browsers renew them; told again daily in case valet lost it).
  if (window.Notification?.permission === 'granted' && 'PushManager' in window && !location.pathname.startsWith('/login')) {
    navigator.serviceWorker.ready.then(async (reg) => {
      const sub = await reg.pushManager.getSubscription();
      if (!sub) return;
      const day = new Date().toDateString();
      if (localStorage.getItem('valet.push') === sub.endpoint && localStorage.getItem('valet.push.day') === day) return;
      const r = await fetch('/push/subscribe', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(sub.toJSON()) });
      if (r.ok) { localStorage.setItem('valet.push', sub.endpoint); localStorage.setItem('valet.push.day', day); }
    }).catch(() => {});
  }
  const kept = document.querySelector('meta[name="valet-offline"]');
  const banner = (text) => {
    let b = document.getElementById('offline-banner');
    if (!b) {
      b = document.createElement('div');
      b.id = 'offline-banner';
      b.className = 'offline-banner';
      document.querySelector('main')?.prepend(b);
    }
    b.textContent = text;
  };
  if (kept) {
    const at = Number(kept.content) ? new Date(Number(kept.content) * 1000).toLocaleString([], valetTime.opts({ dateStyle: 'medium', timeStyle: 'short' })) + ` (${valetTime.label})` : '';
    banner(`Offline: this page is as it was${at ? ' on ' + at : ''}. Nothing can be sent or changed until you're back online.`);
  } else if (!navigator.onLine) {
    banner("You're offline: pages you opened before can still be read.");
  }
  window.addEventListener('offline', () => banner("You're offline: pages you opened before can still be read."));
  window.addEventListener('online', () => { if (!kept) document.getElementById('offline-banner')?.remove(); });
  // Forms need valet: say so instead of failing.
  document.addEventListener('submit', (e) => {
    if (navigator.onLine) return;
    e.preventDefault();
    e.stopImmediatePropagation();
    alert("You're offline: this will work once you're back online.");
  }, true);
  // Forms that take a while (an answer emailed): a spinner in the button
  // pressed, and no second press meanwhile.
  document.addEventListener('submit', (e) => {
    const f = e.target;
    if (!f.matches('form[data-busy]')) return;
    if (f.classList.contains('waiting')) { e.preventDefault(); return; }
    f.classList.add('waiting');
    e.submitter?.classList.add('busy');
    // Once the form's values are taken: a disabled button would leave its own out.
    setTimeout(() => f.querySelectorAll('button, select').forEach((b) => { b.disabled = true; }));
  });
  // Back on the page from the browser's cache: usable again.
  window.addEventListener('pageshow', (e) => {
    if (!e.persisted) return;
    document.querySelectorAll('form.waiting').forEach((f) => {
      f.classList.remove('waiting');
      f.querySelectorAll('button, select').forEach((b) => { b.disabled = false; b.classList.remove('busy'); });
    });
  });
})();
