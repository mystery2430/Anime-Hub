import java.net.Inet6Address
import java.net.InetAddress

/** Standalone copy of SiteNavigationPolicy.isPublicIp for JVM testing (no Android deps). */
object Policy {
    private fun isPublicIpv4(ip: String): Boolean {
        val octets = ip.split('.').map { it.toIntOrNull() ?: return false }
        if (octets.size != 4 || octets.any { it !in 0..255 }) return false
        val a = octets[0]; val b = octets[1]; val c = octets[2]; val d = octets[3]
        return !(
            a == 0 || a == 10 || a == 127 ||
            (a == 100 && b in 64..127) ||
            (a == 169 && b == 254) ||
            (a == 172 && b in 16..31) ||
            (a == 192 && b == 0 && c == 0 && d != 9 && d != 10) ||
            (a == 192 && b == 0 && c == 2) ||
            (a == 192 && b == 88 && c == 99) ||
            (a == 192 && b == 168) ||
            (a == 198 && b == 51 && c == 100) ||
            (a == 203 && b == 0 && c == 113) ||
            (a == 198 && (b == 18 || b == 19)) ||
            a >= 224
        )
    }

    private fun isPublicIpv6(ip: String): Boolean {
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
                bytes[4] == 0x00.toByte() && bytes[5] == 0x00.toByte() && bytes[6] == 0x00.toByte() && bytes[7] == 0x00.toByte()) return false
            if (bytes[0] == 0x01.toByte() && bytes[1] == 0x00.toByte() && bytes[2] == 0x00.toByte() && bytes[3] == 0x00.toByte() &&
                bytes[4] == 0x00.toByte() && bytes[5] == 0x00.toByte() && bytes[6] == 0x00.toByte() && bytes[7] == 0x01.toByte()) return false
            if ((bytes[0].toInt() and 0xFF) == 0x3F && (bytes[1].toInt() and 0xFF) == 0xFF && (bytes[2].toInt() and 0xF0) == 0x00) return false
            if ((bytes[0].toInt() and 0xFF) == 0x5F && (bytes[1].toInt() and 0xFF) == 0x00) return false
            if (bytes[0] == 0x00.toByte() && bytes[1] == 0x64.toByte() && bytes[2] == 0xFF.toByte() && bytes[3] == 0x9B.toByte() &&
                bytes[4] == 0x00.toByte() && bytes[5] == 0x01.toByte()) return false
            if (bytes[0] == 0x20.toByte() && bytes[1] == 0x01.toByte() && bytes[2] == 0x00.toByte() && bytes[3] == 0x02.toByte() &&
                bytes[4] == 0x00.toByte() && bytes[5] == 0x00.toByte()) return false
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

    fun isPublicIp(ip: String): Boolean = if (ip.contains(":")) isPublicIpv6(ip) else isPublicIpv4(ip)
}

fun main() {
    fun check(ip: String, expected: Boolean) {
        val res = Policy.isPublicIp(ip)
        val status = if (res == expected) "PASS" else "FAIL"
        println("$status: isPublicIp($ip) = $res, expected $expected")
        if (res != expected) throw AssertionError("isPublicIp($ip) = $res, expected $expected")
    }
    check("8.8.8.8", true)
    check("192.168.1.1", false)
    check("::ffff:8.8.8.8", true)
    check("::ffff:192.168.1.1", false)
    check("::ffff:10.0.0.1", false)
    check("3fff:0fff::1", false)
    check("3fff:1234::1", true)
    check("2001::1", false)
    check("2001:1::1", true)
    check("64:ff9b::8.8.8.8", true)
    check("64:ff9b::10.0.0.1", false)
    check("64:ff9b::192.168.1.1", false)
    check("2002::1", false)
    check("fec0::1", false)
    check("4000::1", false)
    println("All SiteNavigationPolicy checks passed")
}
