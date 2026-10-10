# Android: site navigation policy

Status: **partial guard, not device-verified.** Read this with
`docs/android-storage-isolation.md`.

## Why Android needs its own guard

On desktop each site is a child WebView, and Tauri's `on_navigation` and
`on_new_window` hooks run `decide_navigation_dns` (HTTPS only, private and local
hosts, blocklist, DNS rebinding) for every navigation.

On Android there is one WebView, the launcher, and the site is loaded into it
with `window.navigate` (`open_on_mobile`). Wry's navigation handler is fixed when
the WebView is created and is keyed by WebView id (`URL_LOADING_OVERRIDE` in wry
`src/android/binding.rs`), so app code cannot install the desktop hook later.

## What was done

`AnimeHubPlugin.load()` wraps the WebView's client in `SiteNavigationGuard`
(`src-tauri/android-plugin/kotlin/dev/animehub/app/AnimeHubPlugin.kt`):

- `shouldOverrideUrlLoading` cancels navigations whose scheme is not `https`
  (apart from `blob`, `data`, `about`) or whose host is local or private. The
  rules mirror `navigation_allowed` and `is_private_host`. A node test keeps the
  host suffixes in step.
- Every other callback goes to wry's original client, so the custom protocol,
  page-started tracking (used by IPC) and error recovery are unchanged.
- The launcher's own origin (`tauri.localhost`) is refused like any other local
  name, so a site cannot load the launcher into its own WebView. Returning to
  the launcher uses `loadUrl`, which this guard does not see.

## Ordering

Tauri calls `load()` from `on_webview_created`, which runs after wry calls
`setWebViewClient(RustWebViewClient)` (wry `src/android/main_pipe.rs`). So the
guard wraps wry's client, not a default one. `WebView.getWebViewClient()` is
public from API 26, which matches `minSdkVersion` 26.

## IPC and the site-facing `ipc` object (verified in wry 0.55.1 source)

- wry's `main_pipe.rs` calls `addJavascriptInterface(Ipc, "ipc")` on the WebView.
  The object is therefore visible to every page loaded in it, including remote sites.
  This conflicts with the project rule against site-facing `JavascriptInterface`.
  It comes from wry, not from AnimeHub code. Removing it needs a wry change or a
  different IPC transport, and is not done here.
- `Ipc.postMessage` sends `webViewClient.currentUrl` with each message. `Ipc` holds
  the original `RustWebViewClient`, and the delegate forwards `onPageStarted`, so
  `currentUrl` keeps updating under the guard.
- Access decisions are Tauri's, from the capability files (see the comment in
  `src-tauri/capabilities/default.json`). Verified in Tauri 2.11.6 source, not on a device.

## What is not covered

| Gap | Why |
| --- | --- |
| Blocklist | Lives in Rust. Running it needs a Rust round-trip inside a UI-thread callback. Not done. |
| DNS rebinding | DNS on the UI thread would stall the WebView. Not done. |
| Subframes and sub-resources | The guard only sees top-level navigations. |
| App-initiated `loadUrl` | Not routed through `shouldOverrideUrlLoading`; `open_on_mobile` checks the first URL with `validate_site_url`. |
| Redirects | Whether the WebView calls `shouldOverrideUrlLoading` for every server redirect is not verified. |
| `window.open` | Wry does not enable multiple windows, and the init script overrides `window.open` when popup blocking is on. The Android default is that multi-window is off; not device-verified. |
| The back affordance (`animehub://close-site`) | Desktop handles it in `on_navigation`. On Android it is a non-HTTP scheme, so the guard refuses it. Whether the Android back path works without it is not verified. |

## Verification status

- Read from source: wry 0.55.1 navigation and client code; Tauri 2.11.6
  `on_webview_created` ordering; Android `WebView.getWebViewClient()` (API 26).
- Compiled: not compiled locally (no Android SDK or Kotlin compiler in the
  sandbox). The CI "Android compile (aarch64)" job is the compile check.
- Behaviour: **not tested on a device.** The policy has no unit test that runs
  Kotlin; the node test only checks the shared host rules.

## Next steps

1. Device test: open a site, tap a link to `http://`, a `192.168.x.x` URL and a
   normal same-site page. Check `AnimeHubNavGuard` logs.
2. Decide whether the blocklist should reach Android (for example a snapshot
   pushed to Kotlin when the list changes).
3. Check the back affordance on Android and give it a native path if needed.
