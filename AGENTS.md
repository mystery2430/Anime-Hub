# AGENTS.md — AnimeHub

Instructions for coding agents working in this repository.

## Project

AnimeHub — a Tauri 2 desktop + Android launcher for anime streaming/tracking
sites. Rust core (`src-tauri/`), vanilla JS frontend (`src/`, no framework),
Kotlin Android plugin (`src-tauri/android-plugin/`).

## Where things live

- `src-tauri/src/commands.rs` — the IPC command surface (`invoke` boundary).
- `src-tauri/src/anilist.rs` — OAuth + GraphQL client; secrets read from env
  (`ANIMEHUB_ANILIST_*`), never committed.
- `src-tauri/src/secure/` — encrypted store (`crypto.rs`, `store.rs`,
  `master_key.rs`, `mod.rs`).
- `src-tauri/src/web/windows.rs` — WebView session lifecycle. This is the
  **only** place a site URL is handed to a WebView and the only place the
  navigation policy is attached; do not add new URL entry points anywhere else.
- `src-tauri/src/web/session.rs` — per-site profile dirs, cookie model,
  `decide_navigation`.
- `src-tauri/src/web/dns.rs` — DNS-rebinding classification for the nav guard.
- `src-tauri/src/sites/` — `url_policy.rs` (HTTPS-only validation choke point),
  `registry.rs` (site registry, sanitize_name/parse_icon_image),
  `blocklist.rs`.
- `src-tauri/src/android_bridge.rs` — Android-only JNI bridge with desktop
  stubs.
- `src-tauri/capabilities/default.json` — the single capability file: grants
  are scoped to the launcher webview (`main`), **no remote origins**. Any
  permission change belongs in this file only.
- `src/` — frontend: `main.js` + `src/logic/*.js`; no HTML sink may receive an
  unvalidated string (see `tests/security.test.js`).
- `ARCHITECTURE_REPORT.md` — authoritative module map. `MAIN_TREE_AUDIT.md` —
  the latest audit (read before touching lifecycle/cookie code).

## Commands

- Install / run: `npm install`, `npm run dev` (Vite + `tauri dev`), release
  build via the CI workflows; local build: `npm run tauri build` equivalent in
  `src-tauri`.
- Rust tests: `cargo test` in `src-tauri/` (unit tests live at the bottom of
  each module).
- JS tests: `npm test` at the repo root (includes `tests/security.test.js`,
  which asserts the capability surface — update it in the same change when
  you touch `capabilities/`).
- Dependency audit: `cargo audit --file src-tauri/Cargo.lock`.

## Hard rules (broken these = broken review)

1. **Errors never leak**: paths, tokens, secrets, raw JVM exception strings
   must not reach the UI. `AppError` variants carry user-presentable Turkish
   strings; `error.rs` conversions scrub details (log the detail, return the
   generic).
2. **Lock hygiene**: in IPC-reachable and native-callback code use
   `lock().map_err(... )?`, never `.expect()`. Never hold a guard across an
   `.await`. Serialize desktop/Android session mutations through
   `DESKTOP_SESSION_LOCK` / `ANDROID_SESSION_LOCK`.
3. **All user URLs** pass `validate_site_url` first; in-WebView navigation
   stays in `decide_navigation`; blob/data allowances are scoped there only.
4. **Turkish user-facing strings**; command errors map `code()` + message for
   the frontend.
5. **Tauri command threads**: long/blocking bodies must be `#[tauri::command(async)]`
   or move their blocking core into `tauri::async_runtime::spawn_blocking`.
6. **Commits**: short conventional subjects (`fix:`, `feat:`, `security:`),
   one concern per commit.
7. Worktree etiquette: never `git reset --hard`, `git clean`, or force-push on
   this branch's history.

## When in doubt

- Transport/cookie/profile changes: re-run the full test suites and read
  `web/windows.rs` first — the serialized lifecycle contract is the fragile
  part of this app.
- Adding a capability or a new IPC command: update `tests/security.test.js`
  in the same change.
