package dev.animehub.app

import java.net.Inet6Address
import java.net.InetAddress

/**
 * Saf IP politika mantığı – Android bağımlılığı yok.
 * Bu dosya APK derlemesi ve unit test tarafından ortak kullanılır.
 * SiteNavigationPolicy bu dosyaya delege eder; aynı kaynak dosya testte de kullanılır.
 * IANA IPv4/IPv6 Special-Purpose registries (2025-10-09) ile uyumlu.
 * Rust src-tauri/src/sites/url_policy.rs ile aynı vektörleri verir.
 */
object IpPolicy {
    fun isPublicIp(ip: String): Boolean =
        if (ip.contains(':')) isPublicIpv6(ip) else isPublicIpv4(ip)

    // Test accessor – üretimdeki gerçek işlevi çağırır
    fun isPublicIpForTest(ip: String): Boolean = isPublicIp(ip)

    /**
     * WHATWG IPv4 parser (URL Standard, "IPv4 parser"): the same algorithm the
     * Rust `url` crate and Chromium run. It accepts decimal, `0x`-hex and
     * leading-zero octal labels plus N-part shorthand, and returns the canonical
     * dotted quad. `null` means "this host is not an IPv4 literal" — callers
     * must treat that as blocked, never as public.
     *
     * Needed because `android.net.Uri` does NOT normalise hosts the way the
     * desktop parser does: without this, `https://010.1/` would be public on
     * desktop (8.0.0.1) and blocked on Android, and `https://0177.0.0.1/` would
     * be loopback on desktop (127.0.0.1) and something else here.
     */
    fun parseIpv4(host: String): String? {
        val rawParts = host.split('.')
        val parts = if (rawParts.size > 1 && rawParts.last().isEmpty()) rawParts.dropLast(1) else rawParts
        if (parts.isEmpty() || parts.size > 4) return null
        val numbers = mutableListOf<Long>()
        for (part in parts) {
            numbers.add(parseIpv4Part(part) ?: return null)
        }
        for (i in 0 until numbers.size - 1) {
            if (numbers[i] > 255L) return null
        }
        val limit = when (numbers.size) {
            1 -> 4294967296L
            2 -> 16777216L
            3 -> 65536L
            else -> 256L
        }
        if (numbers.last() >= limit) return null
        var value = numbers.last()
        for (i in 0 until numbers.size - 1) {
            value += numbers[i] shl (8 * (3 - i))
        }
        return "${(value shr 24) and 0xFF}.${(value shr 16) and 0xFF}.${(value shr 8) and 0xFF}.${value and 0xFF}"
    }

    private fun parseIpv4Part(part: String): Long? {
        if (part.isEmpty()) return null
        val lower = part.lowercase()
        if (lower.startsWith("0x")) {
            val rest = lower.substring(2)
            if (rest.isEmpty()) return 0L
            if (!rest.all { it in '0'..'9' || it in 'a'..'f' }) return null
            return rest.toLongOrNull(16)
        }
        if (part.length > 1 && part[0] == '0') {
            if (!part.all { it in '0'..'7' }) return null
            return part.toLongOrNull(8)
        }
        if (!part.all { it in '0'..'9' }) return null
        return part.toLongOrNull(10)
    }

    /**
     * WHATWG "ends in a number": the last non-empty label is all ASCII digits,
     * or a `0x`-prefixed hex string. Mirrors Rust `ends_in_a_number` exactly.
     */
    fun endsInANumber(host: String): Boolean {
        val rawParts = host.split('.')
        val parts = if (rawParts.size > 1 && rawParts.last().isEmpty()) rawParts.dropLast(1) else rawParts
        val last = parts.lastOrNull() ?: return false
        if (last.isEmpty()) return false
        if (last.all { it in '0'..'9' }) return true
        val lower = last.lowercase()
        return lower.startsWith("0x") && lower.substring(2).all { it in '0'..'9' || it in 'a'..'f' }
    }

    fun isPublicIpv4(ip: String): Boolean {
        // A leading zero makes the octet ambiguous: browsers read "0177" as
        // octal (127) while `toIntOrNull` reads it as 177. Chrome refuses such
        // hosts outright, and so do we — otherwise `0177.0.0.1` would be public
        // here and loopback on the desktop side.
        if (ip.split('.').any { it.length > 1 && it.startsWith("0") }) return false
        val octets = ip.split('.').map { it.toIntOrNull() ?: return false }
        if (octets.size != 4 || octets.any { it !in 0..255 }) return false
        val a = octets[0]
        val b = octets[1]
        val c = octets[2]
        val d = octets[3]
        return !(
            a == 0 || // 0.0.0.0/8 This network
                a == 10 || // 10.0.0.0/8 private
                a == 127 || // 127.0.0.0/8 loopback
                (a == 100 && b in 64..127) || // 100.64.0.0/10 CGNAT
                (a == 169 && b == 254) || // 169.254.0.0/16 link-local
                (a == 172 && b in 16..31) || // 172.16.0.0/12 private
                (a == 192 && b == 0 && c == 0 && d != 9 && d != 10) || // 192.0.0.0/24 IANA (except .9/.10)
                (a == 192 && b == 0 && c == 2) || // 192.0.2.0/24 TEST-NET-1
                (a == 192 && b == 88 && c == 99) || // 192.88.99.0/24 6to4 relay
                (a == 192 && b == 168) || // 192.168.0.0/16 private
                (a == 198 && b == 51 && c == 100) || // 198.51.100.0/24 TEST-NET-2
                (a == 203 && b == 0 && c == 113) || // 203.0.113.0/24 TEST-NET-3
                (a == 198 && (b == 18 || b == 19)) || // 198.18.0.0/15 benchmarking
                a >= 224 // 224.0.0.0/4 multicast, 240.0.0.0/4 reserved + broadcast
            )
    }

    fun isPublicIpv6(ip: String): Boolean {
        val lower = ip.lowercase()
        if (lower == "::1" || lower == "::") return false
        try {
            val inet = InetAddress.getByName(ip)
            if (inet !is Inet6Address) {
                if (ip.contains(":") && ip.contains(".")) {
                    val v4Part = ip.substringAfterLast(':')
                    if (v4Part.contains(".")) return isPublicIpv4(v4Part)
                    return isPublicIpv4(inet.hostAddress ?: return false)
                }
                return false
            }
            val addr = inet
            if (addr.isAnyLocalAddress || addr.isLoopbackAddress || addr.isMulticastAddress) return false
            val bytes = addr.address

            if ((bytes[0].toInt() and 0xFF) == 0xFE && (bytes[1].toInt() and 0xC0) == 0x80) return false
            if ((bytes[0].toInt() and 0xFE) == 0xFC) return false
            if ((bytes[0].toInt() and 0xFF) == 0xFF) return false
            if (bytes[0] == 0x20.toByte() && bytes[1] == 0x01.toByte() && bytes[2] == 0x0D.toByte() && bytes[3] == 0xB8.toByte()) return false
            if (bytes[0] == 0x01.toByte() && bytes[1] == 0x00.toByte() && bytes[2] == 0x00.toByte() && bytes[3] == 0x00.toByte() &&
                bytes[4] == 0x00.toByte() && bytes[5] == 0x00.toByte() && bytes[6] == 0x00.toByte() && bytes[7] == 0x00.toByte()
            ) return false
            if (bytes[0] == 0x01.toByte() && bytes[1] == 0x00.toByte() && bytes[2] == 0x00.toByte() && bytes[3] == 0x00.toByte() &&
                bytes[4] == 0x00.toByte() && bytes[5] == 0x00.toByte() && bytes[6] == 0x00.toByte() && bytes[7] == 0x01.toByte()
            ) return false
            if ((bytes[0].toInt() and 0xFF) == 0x3F && (bytes[1].toInt() and 0xFF) == 0xFF && (bytes[2].toInt() and 0xF0) == 0x00) return false
            if ((bytes[0].toInt() and 0xFF) == 0x5F && (bytes[1].toInt() and 0xFF) == 0x00) return false
            if (bytes[0] == 0x00.toByte() && bytes[1] == 0x64.toByte() && bytes[2] == 0xFF.toByte() && bytes[3] == 0x9B.toByte() &&
                bytes[4] == 0x00.toByte() && bytes[5] == 0x01.toByte()
            ) return false
            if (bytes[0] == 0x20.toByte() && bytes[1] == 0x01.toByte() && bytes[2] == 0x00.toByte() && bytes[3] == 0x02.toByte() &&
                bytes[4] == 0x00.toByte() && bytes[5] == 0x00.toByte()
            ) return false
            if (bytes[0] == 0x20.toByte() && bytes[1] == 0x02.toByte()) return false
            if ((bytes[0].toInt() and 0xFF) == 0xFE && (bytes[1].toInt() and 0xC0) == 0xC0) return false
            val in2001_23 = (bytes[0].toInt() and 0xFF) == 0x20 && (bytes[1].toInt() and 0xFF) == 0x01 && (bytes[2].toInt() and 0xFE) == 0x00
            if (in2001_23) {
                val isPcp1 = bytes[0]==0x20.toByte() && bytes[1]==0x01.toByte() && bytes[2]==0x00.toByte() && bytes[3]==0x01.toByte() &&
                    bytes[4]==0x00.toByte() && bytes[5]==0x00.toByte() && bytes[6]==0x00.toByte() && bytes[7]==0x00.toByte() &&
                    bytes[8]==0x00.toByte() && bytes[9]==0x00.toByte() && bytes[10]==0x00.toByte() && bytes[11]==0x00.toByte() &&
                    bytes[12]==0x00.toByte() && bytes[13]==0x00.toByte() && bytes[14]==0x00.toByte() && bytes[15]==0x01.toByte()
                val isPcp2 = bytes[0]==0x20.toByte() && bytes[1]==0x01.toByte() && bytes[2]==0x00.toByte() && bytes[3]==0x01.toByte() &&
                    bytes[4]==0x00.toByte() && bytes[5]==0x00.toByte() && bytes[6]==0x00.toByte() && bytes[7]==0x00.toByte() &&
                    bytes[8]==0x00.toByte() && bytes[9]==0x00.toByte() && bytes[10]==0x00.toByte() && bytes[11]==0x00.toByte() &&
                    bytes[12]==0x00.toByte() && bytes[13]==0x00.toByte() && bytes[14]==0x00.toByte() && bytes[15]==0x02.toByte()
                val isPcp3 = bytes[0]==0x20.toByte() && bytes[1]==0x01.toByte() && bytes[2]==0x00.toByte() && bytes[3]==0x01.toByte() &&
                    bytes[4]==0x00.toByte() && bytes[5]==0x00.toByte() && bytes[6]==0x00.toByte() && bytes[7]==0x00.toByte() &&
                    bytes[8]==0x00.toByte() && bytes[9]==0x00.toByte() && bytes[10]==0x00.toByte() && bytes[11]==0x00.toByte() &&
                    bytes[12]==0x00.toByte() && bytes[13]==0x00.toByte() && bytes[14]==0x00.toByte() && bytes[15]==0x03.toByte()
                val isAmt = bytes[2] == 0x00.toByte() && bytes[3] == 0x03.toByte()
                val isAs112 = bytes[2] == 0x00.toByte() && bytes[3] == 0x04.toByte() && bytes[4] == 0x01.toByte() && bytes[5] == 0x12.toByte()
                val seg1 = ((bytes[2].toInt() and 0xFF) shl 8) or (bytes[3].toInt() and 0xFF)
                val isOrchid = (seg1 and 0xFFF0) == 0x0020 || (seg1 and 0xFFF0) == 0x0030
                val allowedIn23 = isPcp1 || isPcp2 || isPcp3 || isAmt || isAs112 || isOrchid
                if (!allowedIn23) return false
            }

            val isMapped = bytes[0] == 0.toByte() && bytes[1] == 0.toByte() && bytes[2] == 0.toByte() && bytes[3] == 0.toByte() &&
                bytes[4] == 0.toByte() && bytes[5] == 0.toByte() && bytes[6] == 0.toByte() && bytes[7] == 0.toByte() &&
                bytes[8] == 0.toByte() && bytes[9] == 0.toByte() && (bytes[10].toInt() and 0xFF) == 0xFF && (bytes[11].toInt() and 0xFF) == 0xFF
            val isCompatible = bytes[0] == 0.toByte() && bytes[1] == 0.toByte() && bytes[2] == 0.toByte() && bytes[3] == 0.toByte() &&
                bytes[4] == 0.toByte() && bytes[5] == 0.toByte() && bytes[6] == 0.toByte() && bytes[7] == 0.toByte() &&
                bytes[8] == 0.toByte() && bytes[9] == 0.toByte() && bytes[10] == 0.toByte() && bytes[11] == 0.toByte()
            if (isMapped || isCompatible) {
                val v4 = "${bytes[12].toInt() and 0xFF}.${bytes[13].toInt() and 0xFF}.${bytes[14].toInt() and 0xFF}.${bytes[15].toInt() and 0xFF}"
                return isPublicIpv4(v4)
            }
            if ((bytes[0].toInt() and 0xFF) == 0x00 && (bytes[1].toInt() and 0xFF) == 0x64 &&
                (bytes[2].toInt() and 0xFF) == 0xFF && (bytes[3].toInt() and 0xFF) == 0x9B &&
                (bytes[4].toInt() and 0xFF) == 0x00 && (bytes[5].toInt() and 0xFF) == 0x00 &&
                (bytes[6].toInt() and 0xFF) == 0x00 && (bytes[7].toInt() and 0xFF) == 0x00 &&
                (bytes[8].toInt() and 0xFF) == 0x00 && (bytes[9].toInt() and 0xFF) == 0x00 &&
                (bytes[10].toInt() and 0xFF) == 0x00 && (bytes[11].toInt() and 0xFF) == 0x00) {
                val v4 = "${bytes[12].toInt() and 0xFF}.${bytes[13].toInt() and 0xFF}.${bytes[14].toInt() and 0xFF}.${bytes[15].toInt() and 0xFF}"
                if (!isPublicIpv4(v4)) return false
            }
            if ((bytes[0].toInt() and 0xFF) == 0x00 && (bytes[1].toInt() and 0xFF) == 0x64 &&
                (bytes[2].toInt() and 0xFF) == 0xFF && (bytes[3].toInt() and 0xFF) == 0x9B &&
                (bytes[4].toInt() and 0xFF) == 0x00 && (bytes[5].toInt() and 0xFF) == 0x00 &&
                (bytes[6].toInt() and 0xFF) == 0x00 && (bytes[7].toInt() and 0xFF) == 0x00 &&
                (bytes[8].toInt() and 0xFF) == 0x00 && (bytes[9].toInt() and 0xFF) == 0x00 &&
                (bytes[10].toInt() and 0xFF) == 0x00 && (bytes[11].toInt() and 0xFF) == 0x00) {
                return true
            }
            val seg0 = ((bytes[0].toInt() and 0xFF) shl 8) or (bytes[1].toInt() and 0xFF)
            if ((seg0 and 0xE000) != 0x2000) return false
            return true
        } catch (e: Exception) {
            return false
        }
    }
}
