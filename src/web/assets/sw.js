// valet's service worker: pages and calendar months seen online stay
// readable offline (as they were then). Nothing is sent or changed offline.
// Where notifications are on, it shows new mail valet pushes, and the app's
// badge counts it until read.
const VERSION = '__VERSION__';
const SHELL = `valet-shell-${VERSION}`;
const PAGES = 'valet-pages';
const DATA = 'valet-data';
const PRECACHE = __PRECACHE__;
// Never kept: logins, CalDAV, live status, writing, attachments, changes.
const NEVER = [/^\/dav(\/|$)/, /^\/\.well-known\//, /^\/login/, /^\/logout/, /^\/mail\/oauth/, /^\/mail\/status/, /^\/compose/, /\/att\//, /^\/sw\.js$/, /^\/calendar\/event/, /^\/calendar\/alarms/];

self.addEventListener('install', (e) => {
  e.waitUntil(caches.open(SHELL).then((c) => c.addAll(PRECACHE)).then(() => self.skipWaiting()));
});

self.addEventListener('activate', (e) => {
  e.waitUntil((async () => {
    for (const k of await caches.keys()) if (k.startsWith('valet-shell-') && k !== SHELL) await caches.delete(k);
    await self.clients.claim();
  })());
});

const forget = () => Promise.all([caches.delete(PAGES), caches.delete(DATA)]);

// The app's badge (a Mac's Dock): new mail still unread, as valet counts it.
const badge = (n) => {
  const nav = self.navigator;
  if (!nav.setAppBadge) return Promise.resolve();
  return (n > 0 ? nav.setAppBadge(n) : nav.clearAppBadge()).catch(() => {});
};
const recount = () => fetch('/mail/badge.json', { cache: 'no-store' })
  .then((r) => (r.ok ? r.json().then((j) => badge(j.badge)) : r.status === 401 && badge(0)))
  .catch(() => {});

const post = (url, body) => fetch(url, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) });

// Logged out: this device isn't told of new mail any more.
async function unsubscribe() {
  const sub = await self.registration.pushManager?.getSubscription();
  if (!sub) return;
  await post('/push/unsubscribe', { endpoint: sub.endpoint }).catch(() => {});
  await sub.unsubscribe();
}

async function trim(cache, max) {
  const keys = await cache.keys();
  for (const k of keys.slice(0, Math.max(0, keys.length - max))) await cache.delete(k);
}

// A kept copy, marked with when it was fetched.
async function marked(resp, page) {
  const at = Math.floor(Date.parse(resp.headers.get('Date') || '') / 1000) || '';
  const headers = new Headers(resp.headers);
  headers.set('X-Valet-Offline', String(at));
  if (!page) return new Response(await resp.blob(), { status: resp.status, headers });
  const html = (await resp.text()).replace('<head>', `<head><meta name="valet-offline" content="${at}">`);
  return new Response(html, { status: resp.status, headers });
}

function timeout(promise, ms) {
  return new Promise((ok, fail) => {
    const t = setTimeout(() => fail(new Error('timeout')), ms);
    promise.then((r) => { clearTimeout(t); ok(r); }, (e) => { clearTimeout(t); fail(e); });
  });
}

async function networkFirst(req, name, page, max) {
  const cache = await caches.open(name);
  try {
    const r = await timeout(fetch(req), 8000);
    if (r.ok && r.type === 'basic') {
      await cache.put(req, r.clone());
      trim(cache, max);
    } else if (r.status === 401) {
      forget();
    }
    return r;
  } catch (_) {
    const hit = await cache.match(req);
    if (hit) return marked(hit, page);
    if (page) return (await caches.match('/offline')) || new Response('Offline', { status: 503 });
    return new Response(JSON.stringify({ error: 'offline' }), { status: 503, headers: { 'Content-Type': 'application/json' } });
  }
}

self.addEventListener('fetch', (e) => {
  const req = e.request;
  const url = new URL(req.url);
  if (url.origin !== self.location.origin) return;
  if (req.method !== 'GET') {
    // Logging out forgets what this device kept.
    if (url.pathname === '/logout') e.respondWith(Promise.all([forget(), unsubscribe().catch(() => {}), badge(0)]).then(() => fetch(req)));
    return;
  }
  if (NEVER.some((r) => r.test(url.pathname))) return;
  if (url.pathname.startsWith('/assets/')) {
    e.respondWith(caches.match(req).then((hit) => hit || fetch(req)));
    return;
  }
  if (url.pathname.startsWith('/calendar/month/')) {
    e.respondWith(networkFirst(req, DATA, false, 60));
    return;
  }
  if (req.mode === 'navigate' || /^\/msg\/\d+\/html$/.test(url.pathname)) {
    e.respondWith(networkFirst(req, PAGES, req.mode === 'navigate', 200));
  }
});

// New mail (see notify.rs): who it's from, the subject and first words.
const ICON = PRECACHE.find((u) => u.includes('icon-192.png'));

self.addEventListener('push', (e) => {
  let n = {};
  try { n = e.data ? e.data.json() : {}; } catch (_) {}
  const opts = { body: n.body || '', tag: n.tag || 'mail', icon: ICON, timestamp: Date.now(), data: { url: n.url || '/inbox', message: n.message } };
  if (n.message) opts.actions = [{ action: 'read', title: 'Mark as read' }];
  e.waitUntil(Promise.all([
    self.registration.showNotification(n.account ? `${n.title || 'valet'} · ${n.account}` : n.title || 'valet', opts),
    typeof n.badge === 'number' ? badge(n.badge) : null,
  ]));
});

// Opens the message in valet's window if there is one, else in a new one.
self.addEventListener('notificationclick', (e) => {
  const n = e.notification, d = n.data || {};
  n.close();
  if (e.action === 'read') {
    e.waitUntil(fetch(`/msg/${d.message}/seen`, { method: 'POST' }).catch(() => {}).then(recount));
    return;
  }
  e.waitUntil((async () => {
    const url = new URL(d.url || '/inbox', self.location.origin).href;
    const wins = await self.clients.matchAll({ type: 'window', includeUncontrolled: true });
    const w = wins.find((c) => new URL(c.url).origin === self.location.origin);
    if (w) {
      try {
        await w.focus();
        await w.navigate(url);
        return;
      } catch (_) {}
    }
    await self.clients.openWindow(url);
  })());
});

// The browser renewed the subscription: valet learns the new one.
self.addEventListener('pushsubscriptionchange', (e) => {
  e.waitUntil((async () => {
    const old = e.oldSubscription, key = old?.options?.applicationServerKey;
    const sub = e.newSubscription || (key && await self.registration.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: key }));
    if (old) await post('/push/unsubscribe', { endpoint: old.endpoint }).catch(() => {});
    if (sub) await post('/push/subscribe', sub.toJSON());
  })());
});
