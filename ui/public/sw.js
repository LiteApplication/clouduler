// Precaches the app shell so the console opens offline, but never caches API
// traffic: cluster state must always come from the daemon, and a stale cached
// /api response would show a cluster that no longer exists.
const CACHE = 'clouduler-shell-v1';
const SHELL = ['./', './index.html', './manifest.webmanifest', './icon.svg', './icon-192.png', './icon-512.png'];

self.addEventListener('install', (event) => {
  event.waitUntil(
    caches.open(CACHE).then((cache) => cache.addAll(SHELL)).then(() => self.skipWaiting()),
  );
});

self.addEventListener('activate', (event) => {
  event.waitUntil(
    caches
      .keys()
      .then((keys) => Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k))))
      .then(() => self.clients.claim()),
  );
});

self.addEventListener('message', (event) => {
  if (event.data === 'skip-waiting') self.skipWaiting();
});

self.addEventListener('fetch', (event) => {
  const { request } = event;
  if (request.method !== 'GET') return;

  const url = new URL(request.url);
  if (url.origin !== self.location.origin) return;
  // Live data and auth flows always go to the network, never the cache.
  if (url.pathname.startsWith('/api') || url.pathname.startsWith('/auth')) return;
  // SSE streams must not be buffered by the cache layer at all.
  if (request.headers.get('accept') === 'text/event-stream') return;

  // Navigations: network first so a redeployed UI is picked up, shell on failure.
  if (request.mode === 'navigate') {
    event.respondWith(
      fetch(request).catch(() => caches.match('./index.html').then((r) => r || Response.error())),
    );
    return;
  }

  // Hashed build assets: cache first, they never change under the same name.
  event.respondWith(
    caches.match(request).then(
      (hit) =>
        hit ||
        fetch(request).then((response) => {
          if (response.ok && response.type === 'basic') {
            const copy = response.clone();
            caches.open(CACHE).then((cache) => cache.put(request, copy));
          }
          return response;
        }),
    ),
  );
});
