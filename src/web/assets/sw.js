// valet's service worker: pages and calendar months seen online stay
// readable offline (as they were then). Nothing is sent or changed offline.
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
    if (url.pathname === '/logout') e.respondWith(forget().then(() => fetch(req)));
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
