import { defineConfig } from 'vitest/config';
import react from '@vitejs/plugin-react';

// The Rust side always serves ui/dist from the domain root (see
// cl-api/src/ui.rs's SPA fallback), never off a sub-path, so assets must be
// referenced absolutely: a relative reference resolves against whatever
// client-side route the browser is on, which breaks a hard reload of any
// path but `/` (e.g. `/devices` would look for `/devices/assets/...`).
export default defineConfig({
  base: '/',
  plugins: [react()],
  build: {
    outDir: 'dist',
    emptyOutDir: true,
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
});
