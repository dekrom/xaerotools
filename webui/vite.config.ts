import { defineConfig } from 'vite';

export default defineConfig({
  base: './',
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    target: 'es2020',
  },
  server: {
    proxy: {
      '/api': 'http://127.0.0.1:45746',
      '/tiles': 'http://127.0.0.1:45746',
      '/atlas': 'http://127.0.0.1:45746',
      '/hl': 'http://127.0.0.1:45746',
      '/preview': 'http://127.0.0.1:45746',
      '/ingest': 'http://127.0.0.1:45746',
      '/ws': { target: 'ws://127.0.0.1:45746', ws: true },
    },
  },
});
