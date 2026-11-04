import { defineConfig } from "vite";

// Fixed port: `tauri.conf.json > build.devUrl` points at 1420, and Tauri
// refuses to start if something else took it.
//
// `devUrl` is a localhost origin, so the Tauri dev webview only ever talks to
// the dev server over loopback; bind 127.0.0.1 for both `dev` and `preview`
// and keep Vite's default Host check (no allowedHosts override). Binding to
// all interfaces with the Host check off turned dev into a LAN- and
// rebinding-reachable origin exposing /@fs and VITE_*/TAURI_* env.
export default defineConfig({
  clearScreen: false,
  server: {
    host: "127.0.0.1",
    port: 1420,
    strictPort: true,
  },
  preview: {
    host: "127.0.0.1",
    port: 1420,
    strictPort: true,
  },
  envPrefix: ["VITE_", "TAURI_"],
  build: {
    target: ["es2021", "chrome100", "safari13"],
    // Android's system WebView lags behind desktop Chrome; keep the output
    // conservative so nothing needs a runtime polyfill. Vite 8 minifies with
    // its own (Rust) minifier, so no separate esbuild dependency is needed.
    minify: true,
    sourcemap: false,
    rollupOptions: {
      output: {
        // Stable file names make the AppImage/deb diffs reviewable.
        entryFileNames: "assets/[name].js",
        chunkFileNames: "assets/[name].js",
        assetFileNames: "assets/[name][extname]",
      },
    },
  },
});
