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
}
