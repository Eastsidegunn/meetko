import { defineConfig } from "vite";
import pkg from "./package.json";

// Tauri expects a fixed port in dev; the webview on macOS is WebKit, so target Safari.
export default defineConfig({
  clearScreen: false,
  server: { port: 1420, strictPort: true, host: "localhost" },
  build: { target: "safari15", outDir: "dist", emptyOutDir: true },
  // the version shown at the bottom of 설정, taken from package.json (kept equal to tauri.conf.json)
  define: { __APP_VERSION__: JSON.stringify(pkg.version) },
});
