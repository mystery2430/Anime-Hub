// SPDX-License-Identifier: MIT
//
// The site WebView's settings, as data.
//
// WHY THIS FILE EXISTS: `buildWebView` used to assign six settings inline and
// leave every other `WebSettings` field at its platform default. Two problems
// with that: the defaults are NOT all safe (file and content access, mixed
// content) and an inline assignment cannot be tested — the only "proof" was a
// source-text pin. So the decision lives here as a pure, unit-tested plan, and
// `applySiteSettings` writes exactly these fields and nothing else. The one
// platform-touching line is [WebSettingsSink] in AnimeHubSiteController.kt.
//
// No Android types in this file (it is compiled into the pure unit-test source
// set); the two integer constants mirror `android.webkit.WebSettings` and
// `SiteWebViewSettingsTest` pins them against the platform values.

package dev.animehub.app

/**
 * `WebSettings.MIXED_CONTENT_NEVER_ALLOW` (API 21+). The value is 1: 0 is
 * ALWAYS_ALLOW and 2 is COMPATIBILITY_MODE, which is why
 * `SiteWebViewSettingsTest` pins this literal against the platform constant —
 * writing 2 here would have shipped the compatibility mode under a
 * "never allow" comment.
 */
const val MIXED_CONTENT_NEVER_ALLOW = 1

/** `WebSettings.LOAD_DEFAULT` — the normal network-with-revalidation mode. */
const val CACHE_LOAD_DEFAULT = -1

/**
 * Every WebView setting this app decides for a SITE WebView, with the reason it
 * has that value. Anything not listed here is deliberately left at the platform
 * default (see [leftAtDefault]).
 *
 * NON-PROXY EGRESS: this plan is NOT a network confinement control, and
 * `WebSettings` has no supported field that disables WebRTC or WebTransport, or
 * that forces ICE/STUN/TURN, raw UDP or QUIC through the connection-pinning
 * `CONNECT` proxy. `ProxyController` (see `SiteProxyController`) governs URL
 * requests made through WebView's network stack; it is not a UDP firewall and
 * cannot prove that the Chromium network service made no direct socket. The
 * page-level API removal is in `src-tauri/src/web/webrtc_guard.js` and is
 * defence in depth only. Do not add a property here that claims to enforce
 * kernel- or network-level egress; a user-consented `VpnService` would be a
 * separate control with its own lifecycle and consent.
 *
 * MEDIA: this plan keeps JavaScript, MSE/DOM storage and autoplay available;
 * the egress guard deliberately does not remove `MediaSource`, `<video>`, or
 * the PiP controller. Sites whose playback actually requires WebRTC or
 * main-thread WebTransport will fail by design and need a documented exception,
 * not a silent downgrade of the network policy.
 */
data class SiteWebViewSettings(
    /**
     * Sites are JavaScript applications; without this nothing renders. This is
     * the one setting that cannot be narrowed — the isolation comes from the
     * profile, the navigation policy and the absent IPC bridge, not from
     * switching JavaScript off.
     */
    val javaScriptEnabled: Boolean = true,
    /** Login sessions and player state live here. Profile-scoped, so it is isolated per site. */
    val domStorageEnabled: Boolean = true,
    /** Legacy WebSQL; a few players still use it. Profile-scoped like DOM storage. */
    val databaseEnabled: Boolean = true,
    /**
     * DISABLED on purpose (was `true`). There is no geolocation permission flow:
     * `SiteChromeClient` does not override `onGeolocationPermissionsShowPrompt`,
     * so the WebView's own default denies every request and the setting bought
     * nothing but surface. It is now denied explicitly as well, so a future
     * provider default cannot silently start granting. Re-enabling it means
     * implementing a real consent prompt first.
     */
    val geolocationEnabled: Boolean = false,
    /**
     * Autoplay: an episode has to start without a second tap, and PiP keeps
     * playing when the player view moves. Same value wry uses for the launcher
     * WebView, so site behaviour does not change between the two.
     */
    val mediaPlaybackRequiresUserGesture: Boolean = false,
    /**
     * Kept `true`, but pinned together with [supportMultipleWindows]: with
     * multiple windows OFF there is no `onCreateWindow` handler, so a
     * `window.open` becomes an ordinary same-view navigation — which goes
     * through [SiteNavigationPolicy] like any other. Allowing popups into a
     * second window we never police would be the worse option.
     */
    val javaScriptCanOpenWindowsAutomatically: Boolean = true,
    /** No new windows: nothing implements `onCreateWindow`, so a popup would be dropped silently. */
    val supportMultipleWindows: Boolean = false,
    /**
     * DISABLED explicitly (the platform default is `false` only from API 30 up).
     * A site never needs `file://`; with it off, a compromised page cannot read
     * app-private files through a file URL it managed to navigate to.
     */
    val allowFileAccess: Boolean = false,
    /** DISABLED explicitly (default `true`). There is no file-picker flow, so `content://` is dead weight. */
    val allowContentAccess: Boolean = false,
    /** Both file-URL escape hatches off; they only matter with [allowFileAccess], which is off. */
    val allowFileAccessFromFileURLs: Boolean = false,
    val allowUniversalAccessFromFileURLs: Boolean = false,
    /**
     * EXPLICIT DECISION (the WebView default is `MIXED_CONTENT_ALWAYS_ALLOW`):
     * an HTTPS site must not pull HTTP sub-resources. This matches the
     * navigation policy, which refuses top-level `http://` outright instead of
     * upgrading it. Trade-off, documented in `docs/android-storage-isolation.md`
     * and the README: a site that serves images or media segments over plain
     * HTTP will lose them. If that turns out to break a real site, the fix is a
     * per-site setting, not a global downgrade.
     */
    val mixedContentMode: Int = MIXED_CONTENT_NEVER_ALLOW,
    /** Google Safe Browsing lookups for known-malicious URLs (platform default, stated explicitly). */
    val safeBrowsingEnabled: Boolean = true,
    /**
     * Normal network-with-revalidation caching. Stated explicitly because the
     * HTTP cache is the one piece of site data `clear_site_data` does NOT remove:
     * `WebView.clearCache` is app-wide, not profile-scoped, so calling it from a
     * site operation would wipe the launcher's cache too. The UI wording and the
     * docs say so instead of overclaiming.
     */
    val cacheMode: Int = CACHE_LOAD_DEFAULT,
) {
    companion object {
        /** The single plan every site WebView is built with. */
        val PRODUCTION = SiteWebViewSettings()

        /**
         * Settings this plan deliberately leaves at the platform default, listed
         * so a reviewer can see they were considered rather than missed:
         *
         *  - `allowFileAccessFromFileURLs` / `allowUniversalAccessFromFileURLs`
         *    ARE listed above (off);
         *  - `loadsImagesAutomatically`, `blockNetworkImage`: images on, no blocking;
         *  - `builtInZoomControls` / `setSupportZoom`: provider defaults, not security-relevant;
         *  - `userAgentString`: wry's, so sites see the same UA as before;
         *  - `javaScriptCanOpenWindowsAutomatically`: listed above;
         *  - `mediaPlaybackRequiresUserGesture`: listed above.
         *
         * Downloads, file choosers and permission prompts are not settings but
         * callbacks: `setDownloadListener` is never installed, `onShowFileChooser`
         * and `onPermissionRequest` are denied explicitly by `SiteChromeClient`,
         * so none of them can hand a site anything silently.
         */
        val leftAtDefault: List<String> =
            listOf(
                "loadsImagesAutomatically",
                "blockNetworkImage",
                "builtInZoomControls",
                "userAgentString",
            )
    }
}

/**
 * Where a [SiteWebViewSettings] plan is written. One method per decided setting
 * and nothing else, so a test can drive the real [applySiteSettings] with a fake
 * and check both the values AND that no undecided setting was touched.
 */
interface SiteSettingsSink {
    fun javaScriptEnabled(value: Boolean)

    fun domStorageEnabled(value: Boolean)

    fun databaseEnabled(value: Boolean)

    fun geolocationEnabled(value: Boolean)

    fun mediaPlaybackRequiresUserGesture(value: Boolean)

    fun javaScriptCanOpenWindowsAutomatically(value: Boolean)

    fun supportMultipleWindows(value: Boolean)

    fun allowFileAccess(value: Boolean)

    fun allowContentAccess(value: Boolean)

    fun allowFileAccessFromFileURLs(value: Boolean)

    fun allowUniversalAccessFromFileURLs(value: Boolean)

    fun mixedContentMode(value: Int)

    fun safeBrowsingEnabled(value: Boolean)

    fun cacheMode(value: Int)
}

/**
 * Apply the plan. This is the production path `buildWebView` takes (through
 * `WebSettingsSink`), so the unit test exercises the real function and not a
 * copy of it.
 */
fun applySiteSettings(sink: SiteSettingsSink, plan: SiteWebViewSettings = SiteWebViewSettings.PRODUCTION) {
    sink.javaScriptEnabled(plan.javaScriptEnabled)
    sink.domStorageEnabled(plan.domStorageEnabled)
    sink.databaseEnabled(plan.databaseEnabled)
    sink.geolocationEnabled(plan.geolocationEnabled)
    sink.mediaPlaybackRequiresUserGesture(plan.mediaPlaybackRequiresUserGesture)
    sink.javaScriptCanOpenWindowsAutomatically(plan.javaScriptCanOpenWindowsAutomatically)
    sink.supportMultipleWindows(plan.supportMultipleWindows)
    sink.allowFileAccess(plan.allowFileAccess)
    sink.allowContentAccess(plan.allowContentAccess)
    sink.allowFileAccessFromFileURLs(plan.allowFileAccessFromFileURLs)
    sink.allowUniversalAccessFromFileURLs(plan.allowUniversalAccessFromFileURLs)
    sink.mixedContentMode(plan.mixedContentMode)
    sink.safeBrowsingEnabled(plan.safeBrowsingEnabled)
    sink.cacheMode(plan.cacheMode)
}
