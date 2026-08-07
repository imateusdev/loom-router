import path from "path"
import react from "@vitejs/plugin-react"
import { defineConfig } from "vite"
import { inspectAttr } from 'kimi-plugin-inspect-react'
import pkg from './package.json'

// https://vite.dev/config/
export default defineConfig({
  base: './',
  plugins: [inspectAttr(), react()],
  define: {
    // Browser-mock fallback for the sidebar version stamp; inside Tauri the
    // running binary's own version (getVersion) wins, so a stale dev build
    // never misreports itself.
    __APP_VERSION__: JSON.stringify(pkg.version),
  },
  server: {
    // Uncommon port (3000 clashes with everything); strict so a busy port
    // fails loudly instead of silently mismatching Tauri's devUrl.
    port: 4783,
    strictPort: true,
  },
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src"),
    },
  },
});
