import { defineConfig } from "vite";

export default defineConfig({
  // Tauri dev server parity with tauri.conf.json > build.devUrl
  server: {
    port: 1420,
    strictPort: true,
  },
  build: {
    outDir: "dist",
    target: "es2021",
  },
  // Keep the Tauri command/event boundary framework-independent:
  // plain TypeScript shell; a framework (e.g. Svelte 5) can replace
  // src/main.ts later without touching the command contracts.
  clearScreen: false,
});
