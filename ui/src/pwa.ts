// Service worker registration. The worker only precaches the app shell (see
// public/sw.js); cluster data is always fetched live, so an installed console
// opens offline but never shows stale state.

export function registerServiceWorker(onUpdateReady: (apply: () => void) => void) {
  if (!('serviceWorker' in navigator)) return;
  // The daemon serves the UI from whatever path it is mounted at, so resolve
  // the worker relative to the current document rather than from the root.
  const url = new URL('sw.js', document.baseURI).href;

  window.addEventListener('load', () => {
    navigator.serviceWorker
      .register(url, { scope: './' })
      .then((registration) => {
        const notifyIfWaiting = () => {
          const waiting = registration.waiting;
          if (!waiting || !navigator.serviceWorker.controller) return;
          onUpdateReady(() => {
            waiting.postMessage('skip-waiting');
          });
        };

        notifyIfWaiting();
        registration.addEventListener('updatefound', () => {
          const installing = registration.installing;
          installing?.addEventListener('statechange', () => {
            if (installing.state === 'installed') notifyIfWaiting();
          });
        });
      })
      .catch(() => {
        // No service worker just means no offline shell; the app still runs.
      });

    let reloading = false;
    navigator.serviceWorker.addEventListener('controllerchange', () => {
      if (reloading) return;
      reloading = true;
      window.location.reload();
    });
  });
}
