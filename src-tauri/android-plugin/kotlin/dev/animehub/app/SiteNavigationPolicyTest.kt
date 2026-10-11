package dev.animehub.app

import org.junit.Assert.*
import org.junit.Test

/**
 * Unit test for SiteNavigationPolicy – runs the real Kotlin policy for
 * IPv4-mapped vectors. This is not a synthetic helper; it calls
 * SiteNavigationPolicy.isPublicIpForTest which delegates to the actual
 * isPublicIp used by the WebView guard. CI runs this via Gradle.
 */
class SiteNavigationPolicyTest {

    @Test
    fun `mapped public is allowed`() {
        assertTrue(SiteNavigationPolicy.isPublicIpForTest("::ffff:8.8.8.8"))
        assertTrue(SiteNavigationPolicy.isPublicIpForTest("8.8.8.8"))
    }

    @Test
    fun `mapped private is blocked`() {
        assertFalse(SiteNavigationPolicy.isPublicIpForTest("::ffff:192.168.1.1"))
        assertFalse(SiteNavigationPolicy.isPublicIpForTest("::ffff:10.0.0.1"))
        assertFalse(SiteNavigationPolicy.isPublicIpForTest("192.168.1.1"))
    }

    @Test
    fun `other vectors unchanged`() {
        // 3fff::/20
        assertFalse(SiteNavigationPolicy.isPublicIpForTest("3fff:0fff::1"))
        assertTrue(SiteNavigationPolicy.isPublicIpForTest("3fff:1234::1"))
        // Teredo N/A → blocked
        assertFalse(SiteNavigationPolicy.isPublicIpForTest("2001::1"))
        assertTrue(SiteNavigationPolicy.isPublicIpForTest("2001:1::1"))
        // NAT64
        assertTrue(SiteNavigationPolicy.isPublicIpForTest("64:ff9b::8.8.8.8"))
        assertFalse(SiteNavigationPolicy.isPublicIpForTest("64:ff9b::10.0.0.1"))
        assertFalse(SiteNavigationPolicy.isPublicIpForTest("64:ff9b::192.168.1.1"))
        assertFalse(SiteNavigationPolicy.isPublicIpForTest("64:ff9b::192.0.2.1"))
    }
}
