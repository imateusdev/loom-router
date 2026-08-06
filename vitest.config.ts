// Test config kept separate from vite.config.ts so the app build never
// pulls in jsdom or the setup file.
//
// `inspectAttr()` from the app's vite config is deliberately absent: it
// rewrites JSX to add inspector attributes, which is a dev-server affordance
// and only adds noise to rendered output under test.

import { defineConfig } from 'vitest/config'
import react from '@vitejs/plugin-react'
import path from 'path'

export default defineConfig({
  plugins: [react()],
  resolve: {
    alias: { '@': path.resolve(__dirname, './src') },
  },
  test: {
    environment: 'jsdom',
    globals: true,
    setupFiles: ['./src/test/setup.ts'],
    include: ['src/**/*.test.{ts,tsx}'],
    restoreMocks: true,
  },
})
