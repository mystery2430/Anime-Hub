package dev.animehub.app

import org.junit.Assert.*
import org.junit.Test

/**
 * Unit test for IP policy – runs the real production logic from IpPolicy.kt,
 * which is the same file APK derlemesi kullanıyor. SiteNavigationPolicy de
 * bu dosyaya delege eder, dolayısıyla bu test gerçek uygulama akışını doğrular.
 * CI bu testi Gradle ile çalıştırır.
 * Gerekli vektörler: ::ffff:8.8.8.8, ::ffff:192.168.1.1, ::ffff:10.0.0.1,
 * 3fff:0fff::1, 3fff:1234::1, 2001::1, 2001:1::1, 64:ff9b::8.8.8.8, 64:ff9b::10.0.0.1
 */
class SiteNavigationPolicyTest {

    @Test
    fun `mapped public is allowed`() {
        // IpPolicy – APK'nın kullandığı aynı kaynak dosya
        assertTrue(IpPolicy.isPublicIpForTest("::ffff:8.8.8.8"))
        assertTrue(IpPolicy.isPublicIpForTest("8.8.8.8"))
        // SiteNavigationPolicy de aynı IpPolicy'ye delege ettiğini doğrula
        assertTrue(SiteNavigationPolicy.isPublicIpForTest("::ffff:8.8.8.8"))
    }

    @Test
    fun `mapped private is blocked`() {
        assertFalse(IpPolicy.isPublicIpForTest("::ffff:192.168.1.1"))
        assertFalse(IpPolicy.isPublicIpForTest("::ffff:10.0.0.1"))
        assertFalse(IpPolicy.isPublicIpForTest("192.168.1.1"))
        assertFalse(SiteNavigationPolicy.isPublicIpForTest("::ffff:192.168.1.1"))
    }

    @Test
    fun `other vectors unchanged`() {
        // 3fff::/20
        assertFalse(IpPolicy.isPublicIpForTest("3fff:0fff::1"))
        assertTrue(IpPolicy.isPublicIpForTest("3fff:1234::1"))
        // Teredo N/A → blocked
        assertFalse(IpPolicy.isPublicIpForTest("2001::1"))
        assertTrue(IpPolicy.isPublicIpForTest("2001:1::1"))
        // NAT64
        assertTrue(IpPolicy.isPublicIpForTest("64:ff9b::8.8.8.8"))
        assertFalse(IpPolicy.isPublicIpForTest("64:ff9b::10.0.0.1"))
        assertFalse(IpPolicy.isPublicIpForTest("64:ff9b::192.168.1.1"))
        assertFalse(IpPolicy.isPublicIpForTest("64:ff9b::192.0.2.1"))
        // Aynı vektörler SiteNavigationPolicy üzerinden de doğrulanıyor
        assertFalse(SiteNavigationPolicy.isPublicIpForTest("3fff:0fff::1"))
        assertTrue(SiteNavigationPolicy.isPublicIpForTest("3fff:1234::1"))
    }

    // -----------------------------------------------------------------------
    // Shared policy vectors (AH-14): the SAME file the Rust side loads in
    // `url_policy.rs::navigation_policy_vectors_match_the_android_policy`.
    // Neither implementation keeps its own copy of the expected answers.
    // -----------------------------------------------------------------------

    @Test
    fun `shared vectors agree with the rust policy`() {
        var urls = 0
        var hosts = 0
        var ips = 0
        for (raw in readFixture()) {
            val line = raw.trimEnd('\r')
            if (line.isEmpty() || line.startsWith("#")) continue
            val cols = line.split('\t')
            when (cols[0]) {
                "url" -> {
                    val target = parseVector(cols[1])
                    val reason = SiteNavigationPolicy.rejection(target)
                    val wantAllow = cols[2] == "allow"
                    val wantReason = if (wantAllow) null else cols[3]
                    assertEquals("url ${cols[1]}", wantReason, reason)
                    assertEquals("url ${cols[1]}", wantAllow, reason == null)
                    urls++
                }

                "host" -> {
                    val wantPublic = cols[2] == "public"
                    assertEquals(
                        "host '${cols[1]}'",
                        wantPublic,
                        SiteNavigationPolicy.hostAllowedForTest(cols[1]),
                    )
                    hosts++
                }

                "ip" -> {
                    val wantPublic = cols[2] == "public"
                    assertEquals("ip ${cols[1]}", wantPublic, IpPolicy.isPublicIp(cols[1]))
                    ips++
                }

                else -> fail("bilinmeyen vektör türü: ${cols[0]} ($line)")
            }
        }
        // A missing, truncated or emptied fixture must fail loudly rather than
        // pass with zero vectors.
        assertTrue("url vektörleri kısalmış: $urls", urls >= 30)
        assertTrue("host vektörleri kısalmış: $hosts", hosts >= 30)
        assertTrue("ip vektörleri kısalmış: $ips", ips >= 60)
    }

    @Test
    fun `top level data navigation is refused but blob playback is not`() {
        // The two checkpoints are different: a data: DOCUMENT is a fake page
        // with an opaque origin, while blob: is how MSE players start playback,
        // and data:image/… logos are sub-resources that never reach this guard.
        assertEquals(
            SiteNavigationPolicy.DATA_NAVIGATION,
            SiteNavigationPolicy.rejection(parseVector("data:text/html,<b>sahte sayfa</b>")),
        )
        assertEquals(
            SiteNavigationPolicy.DATA_NAVIGATION,
            SiteNavigationPolicy.rejection(parseVector("data:image/png;base64,iVBORw0KGgo=")),
        )
        assertNull(SiteNavigationPolicy.rejection(parseVector("blob:https://openani.me/media-1")))
        assertEquals(
            SiteNavigationPolicy.INSECURE_SCHEME,
            SiteNavigationPolicy.rejection(parseVector("blob:http://openani.me/media-1")),
        )
        assertNull(SiteNavigationPolicy.rejection(parseVector("about:blank")))
        assertEquals(
            SiteNavigationPolicy.UNKNOWN_ABOUT,
            SiteNavigationPolicy.rejection(parseVector("about:srcdoc")),
        )
    }

    @Test
    fun `blocked navigation is logged without host path or query`() {
        val target = parseVector("https://192.168.1.1/router/admin?token=abc123#frag")
        val reason = SiteNavigationPolicy.rejection(target)
        assertNotNull("bu adres engellenmeliydi", reason)
        val message = SiteNavigationPolicy.blockedMessage(target.scheme, reason!!, true)
        // logcat is world-readable on older releases and trivially readable over
        // adb: scheme, reason and frame kind only.
        for (leak in listOf("192.168.1.1", "router", "admin", "token", "abc123", "frag", "//")) {
            assertFalse("log sızıntısı '$leak': $message", message.contains(leak))
        }
        assertTrue(message.contains("reason=private_host"))
        assertTrue(message.contains("scheme=https"))
        assertTrue(message.contains("frame=main"))
        // A null scheme still has to produce a complete, host-free line.
        val unknown = SiteNavigationPolicy.blockedMessage(null, SiteNavigationPolicy.UNSUPPORTED_SCHEME, false)
        assertTrue(unknown.contains("scheme=none"))
        assertTrue(unknown.contains("frame=sub"))
    }

    @Test
    fun `numeric host forms follow the whatwg ipv4 parser`() {
        // android.net.Uri does not normalise hosts, so the parser lives in
        // IpPolicy and both platforms run the same algorithm.
        assertEquals("8.0.0.1", IpPolicy.parseIpv4("010.0.0.1"))
        assertEquals("127.0.0.1", IpPolicy.parseIpv4("0177.0.0.1"))
        assertEquals("127.0.0.1", IpPolicy.parseIpv4("0x7f000001"))
        assertEquals("127.0.0.1", IpPolicy.parseIpv4("2130706433"))
        assertEquals("127.0.0.1", IpPolicy.parseIpv4("127.1"))
        assertEquals("0.0.0.0", IpPolicy.parseIpv4("0x"))
        assertNull(IpPolicy.parseIpv4("example.0x7f"))
        assertNull(IpPolicy.parseIpv4("1.2.3.4.5"))
        assertNull(IpPolicy.parseIpv4("999.1.1.1"))
        assertNull(IpPolicy.parseIpv4(""))

        assertTrue(IpPolicy.endsInANumber("1.2.3.4"))
        assertTrue(IpPolicy.endsInANumber("example.0x7f"))
        assertTrue(IpPolicy.endsInANumber("0x"))
        assertFalse(IpPolicy.endsInANumber("openani.me"))
        assertFalse(IpPolicy.endsInANumber(""))

        // ... and the decision follows the parsed value, failing closed when the
        // parser refuses the host.
        assertTrue(SiteNavigationPolicy.hostAllowedForTest("010.0.0.1"))
        assertFalse(SiteNavigationPolicy.hostAllowedForTest("0177.0.0.1"))
        assertFalse(SiteNavigationPolicy.hostAllowedForTest("example.0x7f"))
    }

    // -----------------------------------------------------------------------
    // Sub-resource checkpoint (AH-01 / AH-02 / AH-08 / AH-P7)
    // -----------------------------------------------------------------------

    @Test
    fun `subresources refuse cleartext and private hosts but keep media and inline assets`() {
        val cases =
            listOf(
                Triple("https://openani.me/segment-0007.ts", null, "public media segment"),
                Triple("https://openani.me/api/episode?id=1", null, "public xhr"),
                Triple("wss://openani.me/live", null, "secure websocket"),
                Triple("data:image/png;base64,iVBORw0KGgo=", null, "inline asset"),
                Triple("blob:https://openani.me/media-1", null, "MSE object URL"),
                Triple("about:blank", null, "blank frame"),
                Triple("https://192.168.1.1/admin", SiteNavigationPolicy.PRIVATE_HOST, "router"),
                Triple("https://127.0.0.1:8080/", SiteNavigationPolicy.PRIVATE_HOST, "loopback"),
                Triple(
                    "https://169.254.169.254/latest/meta-data/",
                    SiteNavigationPolicy.PRIVATE_HOST,
                    "cloud metadata",
                ),
                Triple("https://[::1]/x", SiteNavigationPolicy.PRIVATE_HOST, "ipv6 loopback"),
                Triple("https://tauri.localhost/", SiteNavigationPolicy.PRIVATE_HOST, "launcher origin"),
                Triple("https://0x7f000001/", SiteNavigationPolicy.PRIVATE_HOST, "hex loopback"),
                Triple("http://openani.me/image.png", SiteNavigationPolicy.INSECURE_SCHEME, "cleartext"),
                Triple("ws://openani.me/live", SiteNavigationPolicy.INSECURE_SCHEME, "cleartext socket"),
                Triple("file:///etc/passwd", SiteNavigationPolicy.UNSUPPORTED_SCHEME, "local file"),
                Triple("javascript:alert(1)", SiteNavigationPolicy.UNSUPPORTED_SCHEME, "script url"),
                Triple("intent://x/#Intent;end", SiteNavigationPolicy.UNSUPPORTED_SCHEME, "intent"),
            )
        for ((url, want, why) in cases) {
            assertEquals("$why: $url", want, SiteNavigationPolicy.subresourceRejection(parseVector(url)))
        }
    }

    @Test
    fun `the two checkpoints differ exactly where they should`() {
        // A data: DOCUMENT is a fake page; a data: image is a normal
        // sub-resource. Same string family, opposite answers, by design.
        assertEquals(
            SiteNavigationPolicy.DATA_NAVIGATION,
            SiteNavigationPolicy.rejection(parseVector("data:text/html,<b>x</b>")),
        )
        assertNull(SiteNavigationPolicy.subresourceRejection(parseVector("data:text/html,<b>x</b>")))
        assertNull(SiteNavigationPolicy.rejection(parseVector("blob:https://openani.me/m")))
        assertNull(SiteNavigationPolicy.subresourceRejection(parseVector("blob:https://openani.me/m")))
        // Both checkpoints refuse a private host and cleartext.
        for (url in listOf("https://10.0.0.1/", "http://openani.me/")) {
            assertNotNull("top-level: $url", SiteNavigationPolicy.rejection(parseVector(url)))
            assertNotNull("sub-resource: $url", SiteNavigationPolicy.subresourceRejection(parseVector(url)))
        }
        // A websocket is a sub-resource the top-level rule does not allow, which
        // is fine: nothing navigates the main frame to wss:.
        assertEquals(
            SiteNavigationPolicy.UNSUPPORTED_SCHEME,
            SiteNavigationPolicy.rejection(parseVector("wss://openani.me/live")),
        )
        assertNull(SiteNavigationPolicy.subresourceRejection(parseVector("wss://openani.me/live")))
    }

    @Test
    fun `subresource blocking is host and scheme only - no dns and no blocklist claims`() {
        // A hostname that merely RESOLVES to a private address cannot be caught
        // here, and the user blocklist is not plumbed to the WebView. The test
        // pins that honest behaviour so nobody mistakes it for a firewall.
        assertNull(SiteNavigationPolicy.subresourceRejection(parseVector("https://router.local.evil.example/")))
        assertNull(SiteNavigationPolicy.subresourceRejection(parseVector("https://127.0.0.1.nip.io/")))
        assertNull(SiteNavigationPolicy.subresourceRejection(parseVector("https://ads.example-tracker.com/x.js")))
        // ... while the literal forms ARE caught.
        assertEquals(
            SiteNavigationPolicy.PRIVATE_HOST,
            SiteNavigationPolicy.subresourceRejection(parseVector("https://127.0.0.1/")),
        )
        assertEquals(
            SiteNavigationPolicy.PRIVATE_HOST,
            SiteNavigationPolicy.subresourceRejection(parseVector("https://printer.local/")),
        )
    }

    /**
     * Reads the shared fixture from the test classpath. CI copies
     * `tests/fixtures/navigation-policy.tsv` into `app/src/test/resources/`;
     * a missing copy fails here instead of silently shrinking the suite.
     */
    private fun readFixture(): List<String> {
        val stream =
            javaClass.classLoader?.getResourceAsStream(FIXTURE)
                ?: throw AssertionError(
                    "test kaynağı bulunamadı: $FIXTURE — app/src/test/resources altına kopyalanmalı",
                )
        return stream.bufferedReader().use { it.readLines() }
    }

    /**
     * Stand-in for `android.net.Uri`, which is a stub in unit tests (calling it
     * throws "Stub!"), used only to build [SiteNavigationPolicy.NavTarget] from
     * a fixture line. It reproduces the parts of Uri this policy depends on:
     * lower-cased scheme, bracketed IPv6 host, authority without user info or
     * port, and the scheme-specific part of an opaque URI. The real extraction
     * (`Uri.getScheme/getHost/getSchemeSpecificPart`) is three property reads in
     * `SiteNavigationPolicy.rejection(Uri)` and needs a device to verify.
     */
    // ---------------------------------------------------------------------
    // Top-level DNS verdicts (redirect targets)
    // ---------------------------------------------------------------------

    @Test
    fun `top-level redirect to a resolved private address is blocked`() {
        val gate = NavigationDnsGate()
        // First sight of the host: the UI thread must not resolve, so the
        // navigation is allowed AND a probe is requested.
        assertEquals(
            NavigationDnsGate.Decision.ALLOW_AND_PROBE,
            gate.decide("redirector.example", 1_000L),
        )
        // The probe answers with loopback: the next navigation is cancelled.
        assertEquals(
            NavigationDnsGate.Decision.BLOCK,
            gate.record("redirector.example", listOf("127.0.0.1"), 1_100L),
        )
        assertEquals(
            NavigationDnsGate.Decision.BLOCK,
            gate.decide("redirector.example", 1_200L),
        )
        // Same for a private range and a link-local metadata endpoint.
        gate.record("lan.example", listOf("192.168.1.10"), 1_100L)
        assertEquals(NavigationDnsGate.Decision.BLOCK, gate.decide("LAN.example.", 1_200L))
        gate.record("meta.example", listOf("169.254.169.254"), 1_100L)
        assertEquals(NavigationDnsGate.Decision.BLOCK, gate.decide("meta.example", 1_200L))
    }

    @Test
    fun `public answers allow and a changed answer is blocked`() {
        val gate = NavigationDnsGate()
        gate.decide("site.example", 0L)
        assertEquals(
            NavigationDnsGate.Decision.ALLOW,
            gate.record("site.example", listOf("93.184.216.34"), 10L),
        )
        assertEquals(NavigationDnsGate.Decision.ALLOW, gate.decide("site.example", 20L))
        // Rebinding: a different public set after the first answer is refused.
        assertEquals(
            NavigationDnsGate.Decision.BLOCK,
            gate.record("site.example", listOf("93.184.216.35"), 30L),
        )
        assertEquals(NavigationDnsGate.Decision.BLOCK, gate.decide("site.example", 40L))
    }

    @Test
    fun `unknown hosts fail open once and are probed once`() {
        val gate = NavigationDnsGate()
        assertEquals(NavigationDnsGate.Decision.ALLOW_AND_PROBE, gate.decide("a.example", 0L))
        // A second navigation while the probe is in flight must not stampede.
        assertEquals(NavigationDnsGate.Decision.ALLOW, gate.decide("a.example", 1L))
        // A resolver outage clears the in-flight mark and caches nothing, so a
        // DNS problem never bricks the app.
        gate.recordLookupFailure("a.example")
        assertEquals(NavigationDnsGate.Decision.ALLOW_AND_PROBE, gate.decide("a.example", 2L))
    }

    @Test
    fun `missing, ip literal and malformed hosts are blocked at the top level`() {
        val gate = NavigationDnsGate()
        assertEquals(NavigationDnsGate.Decision.BLOCK, gate.decide(null, 0L))
        assertEquals(NavigationDnsGate.Decision.BLOCK, gate.decide("", 0L))
        // IP literals are decided by SiteNavigationPolicy, not resolved here.
        assertNull(NavigationDnsGate.normalize("127.0.0.1"))
        assertNull(NavigationDnsGate.normalize("0x7f000001"))
        assertNull(NavigationDnsGate.normalize("[::1]"))
        assertEquals("site.example", NavigationDnsGate.normalize("SITE.example."))
    }

    @Test
    fun `an empty resolver answer is never cached as a safe verdict`() {
        val gate = NavigationDnsGate()
        assertEquals(NavigationDnsGate.Decision.ALLOW_AND_PROBE, gate.decide("empty.example", 0L))

        // Regression: `record(host, emptyList())` used to be classified as
        // ALLOW and stored as Verdict.PUBLIC with an empty `pinned` set, so the
        // NEXT navigation was allowed from the cache — a "safe" verdict for a
        // host whose address was never established. The answer proves nothing,
        // so nothing may be cached.
        assertEquals(
            NavigationDnsGate.Decision.ALLOW,
            gate.record("empty.example", emptyList(), 10L),
        )
        assertNull(gate.cachedVerdict("empty.example", 20L))
        assertEquals(0, gate.size())
        assertEquals(0, gate.probingCount())

        // A later navigation therefore probes again instead of trusting a
        // fabricated PUBLIC entry.
        assertEquals(NavigationDnsGate.Decision.ALLOW_AND_PROBE, gate.decide("empty.example", 30L))
    }

    @Test
    fun `a failed lookup caches no verdict and frees the probe slot`() {
        val gate = NavigationDnsGate(maxProbing = 1)
        assertEquals(NavigationDnsGate.Decision.ALLOW_AND_PROBE, gate.decide("outage.example", 0L))
        assertEquals(1, gate.probingCount())

        gate.recordLookupFailure("OUTAGE.example.") // what the probe's catch does
        assertEquals(0, gate.probingCount())
        assertNull(gate.cachedVerdict("outage.example", 1L))
        // Fail-open: the navigation is not blocked by a resolver outage, and the
        // freed slot lets the next navigation probe again.
        assertEquals(NavigationDnsGate.Decision.ALLOW_AND_PROBE, gate.decide("outage.example", 2L))
    }

    @Test
    fun `the probe budget bounds the resolver queue and recovers when a slot frees`() {
        val gate = NavigationDnsGate(maxProbing = 2)

        // Two distinct hosts take the budget; each asks for exactly one probe.
        assertEquals(NavigationDnsGate.Decision.ALLOW_AND_PROBE, gate.decide("one.example", 0L))
        assertEquals(NavigationDnsGate.Decision.ALLOW_AND_PROBE, gate.decide("two.example", 0L))
        assertEquals(2, gate.probingCount())

        // Further hosts are still ALLOWED (fail-open, the same decision as a
        // cache miss) but get NO probe, so neither `probing` nor the
        // single-threaded resolver queue can grow with the page's host count.
        for (i in 0 until 100) {
            assertEquals(NavigationDnsGate.Decision.ALLOW, gate.decide("flood$i.example", 1L))
        }
        assertEquals(2, gate.probingCount())

        // A settled probe frees its slot, and the next host gets one again.
        assertEquals(
            NavigationDnsGate.Decision.ALLOW,
            gate.record("one.example", listOf("93.184.216.34"), 2L),
        )
        assertEquals(NavigationDnsGate.Decision.ALLOW_AND_PROBE, gate.decide("three.example", 3L))
        assertEquals(2, gate.probingCount())

        // An answer for a host that never held a slot is still recorded (the
        // synchronous request checkpoint records every host it resolves) but
        // must not push the set over the cap.
        assertEquals(
            NavigationDnsGate.Decision.ALLOW,
            gate.record("flood7.example", listOf("93.184.216.34"), 4L),
        )
        assertEquals(2, gate.probingCount())
    }

    @Test
    fun `cached verdicts expire and honour the entry cap`() {
        val gate = NavigationDnsGate(cacheMs = 1_000L, maxHosts = 3)
        gate.decide("x.example", 0L)
        gate.record("x.example", listOf("127.0.0.1"), 0L)
        assertNull(gate.cachedVerdict("x.example", 1_000L))
        assertEquals(NavigationDnsGate.Decision.ALLOW_AND_PROBE, gate.decide("x.example", 1_000L))
        for (i in 0 until 10) {
            gate.record("h$i.example", listOf("93.184.216.34"), 2_000L)
        }
        assertTrue(gate.size() <= 3)
    }

    private fun parseVector(raw: String): SiteNavigationPolicy.NavTarget {
        val colon = raw.indexOf(':')
        if (colon <= 0) return SiteNavigationPolicy.NavTarget(null, null, null)
        val scheme = raw.substring(0, colon)
        val rest = raw.substring(colon + 1)
        if (!rest.startsWith("//")) return SiteNavigationPolicy.NavTarget(scheme, null, rest)
        val afterSlashes = rest.substring(2)
        val authEnd = afterSlashes.indexOfFirst { it == '/' || it == '?' || it == '#' }
        val authority = if (authEnd < 0) afterSlashes else afterSlashes.substring(0, authEnd)
        val withoutUser = authority.substringAfterLast('@')
        val host =
            if (withoutUser.startsWith("[")) withoutUser.substringBefore(']') + "]"
            else withoutUser.substringBefore(':')
        return SiteNavigationPolicy.NavTarget(scheme, host, rest)
    }

    private companion object {
        const val FIXTURE = "navigation-policy.tsv"
    }
}
