import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import { readFileSync } from 'node:fs'

// Версия панели = версия программы (Cargo.toml): по ней панель замечает, что сервер уже обновлён, а вкладка — старая.
const VERSION = /^version\s*=\s*"([^"]+)"/m.exec(readFileSync(new URL('../hopebackup-core/Cargo.toml', import.meta.url), 'utf8'))?.[1] || 'dev'

// В разработке API проксируется на сервер управления (https://127.0.0.1:4443, самоподписанный сертификат).
export default defineConfig({
  plugins: [react()],
  define: { __PANEL_VERSION__: JSON.stringify(VERSION) },
  server: {
    port: 3002,
    proxy: { '/api': { target: process.env.HOPEBACKUP_API || 'https://127.0.0.1:4443', secure: false, changeOrigin: true } },
  },
  build: { outDir: 'dist', emptyOutDir: true, chunkSizeWarningLimit: 1500 },
})
