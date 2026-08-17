import { defineConfig } from 'vitest/config';
import react from '@vitejs/plugin-react';

// The Rust side serves ui/dist as static files off whatever path the daemon
// is mounted at, so assets must be referenced relatively, not from root.
export default defineConfig({
  base: './',
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
