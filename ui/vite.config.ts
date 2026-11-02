import { defineConfig } from 'vitest/config';
import react from '@vitejs/plugin-react';

// The Rust side always serves ui/dist from the domain root (see
// cl-api/src/ui.rs's SPA fallback), never off a sub-path, so assets must be
// referenced absolutely: a relative reference resolves against whatever
// client-side route the browser is on, which breaks a hard reload of any
// path but `/` (e.g. `/devices` would look for `/devices/assets/...`).
//
// `vite build --mode mock` is the docs-site build: the same app wired to the
// in-memory fake backend (src/mock), emitted to docs/static/ui-mock/ with
// relative asset urls so it works from any sub-path.
export default defineConfig(({ mode }) => ({
  base: mode === 'mock' ? './' : '/',
  plugins: [react()],
  build:
    mode === 'mock'
      ? {
          outDir: '../docs/static/ui-mock',
          emptyOutDir: true,
          sourcemap: false,
          rollupOptions: { input: 'mock.html' },
        }
      : {
          outDir: 'dist',
          emptyOutDir: true,
          sourcemap: true,
        },
  server: {
    proxy: {
      '/api': 'http://localhost:8443',
      '/auth': 'http://localhost:8443',
    },
  },
  test: {
    environment: 'jsdom',
    globals: true,
    setupFiles: ['./src/test/setup.ts'],
  },
}));
