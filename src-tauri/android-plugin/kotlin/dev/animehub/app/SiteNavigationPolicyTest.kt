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
