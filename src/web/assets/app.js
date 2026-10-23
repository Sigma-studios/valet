// On every page: the service worker (offline reading), and a word when a
// page is shown offline. In valet's Android app, window.ValetApp is there too.
(() => {
  'use strict';
  if ('serviceWorker' in navigator) navigator.serviceWorker.register('/sw.js', { scope: '/' }).catch(() => {});
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
})();
