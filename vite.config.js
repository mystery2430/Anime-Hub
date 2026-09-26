import { defineConfig } from "vite";

// Fixed port: `tauri.conf.json > build.devUrl` points at 1420, and Tauri
// refuses to start if something else took it.
export default defineConfig({
  clearScreen: false,
  server: {
    host: "0.0.0.0",
    port: 1420,
    strictPort: true,
    // The preview proxy uses a non-localhost Host header.
    allowedHosts: true,
  },
  // Allow the Tauri preview origin so the sandboxed preview works too.
  preview: {
    host: "0.0.0.0",
    port: 1420,
    strictPort: true,
    allowedHosts: true,
  },
  envPrefix: ["VITE_", "TAURI_"],
  build: {
    target: ["es2021", "chrome100", "safari13"],
    // Android's system WebView lags behind desktop Chrome; keep the output
    // conservative so nothing needs a runtime polyfill.
    minify: "esbuild",
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
