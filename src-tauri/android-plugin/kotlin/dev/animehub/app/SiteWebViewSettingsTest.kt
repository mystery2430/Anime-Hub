// SPDX-License-Identifier: MIT
//
// Unit tests for the site WebView settings plan. They drive the REAL
// `applySiteSettings` through a recording sink, so what is asserted here is the
// production mapping, not a copy of it. The only part that needs a device is
// `WebSettingsSink` in AnimeHubSiteController.kt (one property assignment per
// method) — everything the plan decides is covered below.

package dev.animehub.app

import android.webkit.WebSettings
import org.junit.Assert.*
import org.junit.Test

class SiteWebViewSettingsTest {

    /** Records every write `applySiteSettings` performs, in order. */
    private class RecordingSink : SiteSettingsSink {
        val writes = mutableListOf<Pair<String, Any>>()

        override fun javaScriptEnabled(value: Boolean) {
            writes += "javaScriptEnabled" to value
        }

        override fun domStorageEnabled(value: Boolean) {
            writes += "domStorageEnabled" to value
        }

        override fun databaseEnabled(value: Boolean) {
            writes += "databaseEnabled" to value
        }

        override fun geolocationEnabled(value: Boolean) {
            writes += "geolocationEnabled" to value
        }

        override fun mediaPlaybackRequiresUserGesture(value: Boolean) {
            writes += "mediaPlaybackRequiresUserGesture" to value
        }

        override fun javaScriptCanOpenWindowsAutomatically(value: Boolean) {
            writes += "javaScriptCanOpenWindowsAutomatically" to value
        }

        override fun supportMultipleWindows(value: Boolean) {
            writes += "supportMultipleWindows" to value
        }

        override fun allowFileAccess(value: Boolean) {
            writes += "allowFileAccess" to value
        }

        override fun allowContentAccess(value: Boolean) {
            writes += "allowContentAccess" to value
        }

        override fun allowFileAccessFromFileURLs(value: Boolean) {
            writes += "allowFileAccessFromFileURLs" to value
        }

        override fun allowUniversalAccessFromFileURLs(value: Boolean) {
            writes += "allowUniversalAccessFromFileURLs" to value
        }

        override fun mixedContentMode(value: Int) {
            writes += "mixedContentMode" to value
        }

        override fun safeBrowsingEnabled(value: Boolean) {
            writes += "safeBrowsingEnabled" to value
        }

        override fun cacheMode(value: Int) {
            writes += "cacheMode" to value
        }
    }

    @Test
    fun `production plan turns the unsafe defaults off`() {
        val plan = SiteWebViewSettings.PRODUCTION
        // Every one of these is NOT the platform default (or is only default
        // from a later API level), which is why it is decided explicitly.
        assertFalse("file access must be off", plan.allowFileAccess)
        assertFalse("content access must be off", plan.allowContentAccess)
        assertFalse(plan.allowFileAccessFromFileURLs)
        assertFalse(plan.allowUniversalAccessFromFileURLs)
        assertEquals(MIXED_CONTENT_NEVER_ALLOW, plan.mixedContentMode)
        assertFalse("no geolocation without a consent flow", plan.geolocationEnabled)
        assertFalse("no window we do not police", plan.supportMultipleWindows)
        assertTrue(plan.safeBrowsingEnabled)
    }

    @Test
    fun `production plan keeps playback and site behaviour working`() {
        val plan = SiteWebViewSettings.PRODUCTION
        // PiP / video and ordinary site logins must not regress: JavaScript,
        // DOM storage, autoplay and same-view window.open stay on.
        assertTrue(plan.javaScriptEnabled)
        assertTrue(plan.domStorageEnabled)
        assertTrue(plan.databaseEnabled)
        assertFalse("autoplay must not need a second tap", plan.mediaPlaybackRequiresUserGesture)
        assertTrue(plan.javaScriptCanOpenWindowsAutomatically)
        assertEquals(CACHE_LOAD_DEFAULT, plan.cacheMode)
    }

    @Test
    fun `apply writes every decided setting and nothing else`() {
        val sink = RecordingSink()
        applySiteSettings(sink)
        val plan = SiteWebViewSettings.PRODUCTION

        assertEquals(
            listOf(
                "javaScriptEnabled" to plan.javaScriptEnabled,
                "domStorageEnabled" to plan.domStorageEnabled,
                "databaseEnabled" to plan.databaseEnabled,
                "geolocationEnabled" to plan.geolocationEnabled,
                "mediaPlaybackRequiresUserGesture" to plan.mediaPlaybackRequiresUserGesture,
                "javaScriptCanOpenWindowsAutomatically" to plan.javaScriptCanOpenWindowsAutomatically,
                "supportMultipleWindows" to plan.supportMultipleWindows,
                "allowFileAccess" to plan.allowFileAccess,
                "allowContentAccess" to plan.allowContentAccess,
                "allowFileAccessFromFileURLs" to plan.allowFileAccessFromFileURLs,
                "allowUniversalAccessFromFileURLs" to plan.allowUniversalAccessFromFileURLs,
                "mixedContentMode" to plan.mixedContentMode,
                "safeBrowsingEnabled" to plan.safeBrowsingEnabled,
                "cacheMode" to plan.cacheMode,
            ),
            sink.writes,
        )
        // No setting is written twice, and nothing outside the plan is touched.
        assertEquals(sink.writes.size, sink.writes.map { it.first }.distinct().size)
    }

    @Test
    fun `apply passes a non default plan through unchanged`() {
        // Proves the adapter really reads the plan instead of hard-coding the
        // production values.
        val sink = RecordingSink()
        val plan = SiteWebViewSettings(geolocationEnabled = true, allowFileAccess = true)
        applySiteSettings(sink, plan)
        assertTrue(sink.writes.contains("geolocationEnabled" to true))
        assertTrue(sink.writes.contains("allowFileAccess" to true))
        // ... and the rest of the plan is untouched by that override.
        assertTrue(sink.writes.contains("mixedContentMode" to MIXED_CONTENT_NEVER_ALLOW))
        assertTrue(sink.writes.contains("javaScriptEnabled" to true))
    }

    @Test
    fun `integer constants match the platform values`() {
        // The plan file stays free of Android types, so the two integer settings
        // are literals here. If the platform ever changes them, this fails
        // instead of silently writing the wrong mode.
        assertEquals(WebSettings.MIXED_CONTENT_NEVER_ALLOW, MIXED_CONTENT_NEVER_ALLOW)
        assertEquals(WebSettings.LOAD_DEFAULT, CACHE_LOAD_DEFAULT)
    }

    @Test
    fun `the settings plan cannot be mistaken for non proxy egress enforcement`() {
        // REFLECTION/SOURCE-LEVEL ONLY. There is no supported WebSettings field
        // that disables WebRTC or WebTransport, or that forces UDP/QUIC through
        // ProxyController. This test prevents a future field from being added
        // with a name that implies such a guarantee without a platform API and
        // a real device/network test behind it.
        val sinkMethods = SiteSettingsSink::class.java.declaredMethods.map { it.name }
        for (part in listOf("proxy", "webrtc", "webtransport", "quic", "udp")) {
            assertFalse(
                "SiteSettingsSink must not grow a fake '$part' egress switch without a supported platform API",
                sinkMethods.any { it.contains(part, ignoreCase = true) },
            )
        }

        // Blocking WebRTC at the document-start layer must not be "fixed" by
        // disabling media/autoplay at the settings layer. The second test above
        // pins playback; this keeps the reason visible next to the egress gap.
        val plan = SiteWebViewSettings.PRODUCTION
        assertFalse("playback must stay independent of the WebRTC guard", plan.mediaPlaybackRequiresUserGesture)
        assertTrue("the page still runs its own MSE/DASH player", plan.javaScriptEnabled)
    }

    @Test
    fun `left at default list does not claim a decided setting`() {
        val sink = RecordingSink()
        applySiteSettings(sink)
        val decided = sink.writes.map { it.first }.toSet()
        for (name in SiteWebViewSettings.leftAtDefault) {
            assertFalse("$name is decided, so it is not 'left at default'", decided.contains(name))
        }
        assertTrue(SiteWebViewSettings.leftAtDefault.isNotEmpty())
    }
}
