import { defineConfig } from 'vite'
import { svelte } from '@sveltejs/vite-plugin-svelte'

const BACKEND = 'http://127.0.0.1:8080'

// https://vite.dev/config/
export default defineConfig({
  plugins: [svelte()],
  build: {
    outDir: 'dist',
  },
  server: {
    proxy: {
      '/ws': { target: BACKEND, ws: true },
      '/stream': { target: BACKEND, ws: true },
    },
  },
})
