// SPDX-License-Identifier: MIT
//
// Real-socket tests for the connection-pinning proxy.
//
// Nothing here talks to the internet: a fake resolver supplies the addresses
// and a real loopback ServerSocket stands in for the origin, so the CONNECT
// parser, the classification, the pinned connect, the tunnel and the refusal
// paths all run as production code. What a JVM test cannot prove is which
// socket the WebView engine would open — that is why the proxy exists, and why
// the device checklist insists on a packet capture.
package dev.animehub.app

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.BufferedReader
import java.io.InputStream
import java.io.InputStreamReader
import java.io.OutputStream
import java.net.InetAddress
import java.net.InetSocketAddress
import java.net.ServerSocket
import java.net.Socket
import java.util.Collections

class DnsPinnedProxyTest {

  // -------------------------------------------------------------------------
  // Fixtures
  // -------------------------------------------------------------------------

  /** A public address no test ever connects to for real. */
  private val publicA =
    InetAddress.getByAddress(byteArrayOf(93.toByte(), 184.toByte(), 216.toByte(), 34.toByte()))
  private val publicB =
    InetAddress.getByAddress(byteArrayOf(93.toByte(), 184.toByte(), 216.toByte(), 35.toByte()))

  private fun address(vararg octets: Int): InetAddress =
    InetAddress.getByAddress(byteArrayOf(octets[0].toByte(), octets[1].toByte(), octets[2].toByte(), octets[3].toByte()))

  /** Answers one hostname, records how many times it was asked. */
  private class FakeResolver(vararg answers: List<InetAddress>) {
    private val queue = ArrayDeque(answers.toList())
    var calls = 0
      private set
    var lastHost: String? = null
      private set

    fun resolve(host: String): List<InetAddress> {
      calls++
      lastHost = host
      return if (queue.isEmpty()) emptyList() else queue.removeFirst()
    }
  }

  /**
   * An origin that answers every byte it reads, upper-cased, and records the
   * socket it saw. Bound to loopback so the test never leaves the machine.
   */
  private class EchoOrigin {
    val server = ServerSocket(0, 8, InetAddress.getByName("127.0.0.1"))
    val port: Int get() = server.localPort
    val accepted = Collections.synchronizedList(mutableListOf<Socket>())

    /**
     * Sockets whose reader saw EOF or a reset, i.e. the proxy really tore the
     * upstream side down. `Socket.isClosed` on the server side only reports a
     * LOCAL close, so it would answer `false` for a peer hangup.
     */
    val ended = Collections.synchronizedList(mutableListOf<Socket>())
    private val thread = Thread {
      while (!server.isClosed) {
        val socket = try {
          server.accept()
        } catch (_: Exception) {
          return@Thread
        }
        accepted.add(socket)
        Thread {
          try {
            val input = socket.getInputStream()
            val output = socket.getOutputStream()
            val buffer = ByteArray(1024)
            while (true) {
              val read = input.read(buffer)
              if (read < 0) break
              output.write(buffer, 0, read)
              output.flush()
            }
          } catch (_: Exception) {
            // Closed by the proxy.
          } finally {
            ended.add(socket)
          }
        }.apply { isDaemon = true }.start()
      }
    }.apply { isDaemon = true; start() }

    fun close() {
      try {
        server.close()
      } catch (_: Exception) {
      }
      accepted.forEach { DnsPinnedProxy.closeQuietly(it) }
    }
  }

  /**
   * A socket to the loopback echo origin that REPORTS the pinned address.
   *
   * Production verification compares the socket's own remote address with the
   * classified one, and a loopback origin can never satisfy that for a public
   * address. This fake reports the pinned address while the bytes really reach
   * the local origin, so the tunnel tests keep the production verification ON.
   *
   * It does not weaken it: `a_socket_that_landed_elsewhere_is_never_tunnelled
   * _through` proves a socket that reports a different address is refused, and
   * `the_production_connector_pins_and_verifies_on_real_sockets` runs the real
   * `PinnedConnect` against real sockets.
   */
  private class PinnedFakeSocket(
    private val real: Socket,
    private val reported: InetAddress,
  ) : Socket() {
    override fun getInputStream(): InputStream = real.getInputStream()
    override fun getOutputStream(): OutputStream = real.getOutputStream()
    override fun getInetAddress(): InetAddress = reported
    override fun setSoTimeout(timeout: Int) {
      real.soTimeout = timeout
    }

    override fun getSoTimeout(): Int = real.soTimeout
    override fun close() = real.close()
    override fun isClosed(): Boolean = real.isClosed
  }

  private fun awaitCondition(timeoutMs: Long = 2_000, condition: () -> Boolean): Boolean {
    val deadline = System.currentTimeMillis() + timeoutMs
    while (System.currentTimeMillis() < deadline) {
      if (condition()) return true
      Thread.sleep(10)
    }
    return condition()
  }

  private fun statusLine(socket: Socket): String {
    socket.soTimeout = 2_000
    val reader = BufferedReader(InputStreamReader(socket.getInputStream(), Charsets.ISO_8859_1))
    return reader.readLine() ?: ""
  }

  private fun send(socket: Socket, text: String) {
    socket.getOutputStream().write(text.toByteArray(Charsets.ISO_8859_1))
    socket.getOutputStream().flush()
  }

  // -------------------------------------------------------------------------
  // 1. The connection goes to the checked address, resolved once
  // -------------------------------------------------------------------------

  @Test
  fun a_public_answer_is_tunnelled_to_exactly_the_checked_address() {
    val origin = EchoOrigin()
    val resolver = FakeResolver(listOf(publicA))
    val seen = Collections.synchronizedList(mutableListOf<Pair<InetAddress, Int>>())
    val proxy = DnsPinnedProxy(
      resolver = resolver::resolve,
      connector = { address, port, _ ->
        // What the proxy asks for is what a real connector would dial. The
        // fake then reaches a loopback origin instead of the internet, and
        // reports where it was sent.
        seen.add(address to port)
        PinnedFakeSocket(
          Socket().apply { connect(InetSocketAddress(InetAddress.getByName("127.0.0.1"), origin.port), 2_000) },
          address,
        )
      },
    )
    val port = proxy.start()
    try {
      val client = Socket("127.0.0.1", port)
      client.soTimeout = 2_000
      send(client, "CONNECT public.example:443 HTTP/1.1\r\nHost: public.example:443\r\n\r\n")
      assertEquals("HTTP/1.1 200 Connection Established", statusLine(client))

      // The proxy dialled the CLASSIFIED address and the real port.
      assertEquals(listOf(publicA to 443), seen.toList())
      // Exactly one resolution for this connection.
      assertEquals(1, resolver.calls)
      assertEquals("public.example", resolver.lastHost)

      // Bytes flow in both directions through the tunnel.
      send(client, "ping")
      val echoed = ByteArray(4)
      client.getInputStream().read(echoed)
      assertEquals("ping", String(echoed))

      client.close()
      // The upstream socket is closed when the client hangs up: no leak.
      assertTrue(
        "the upstream connection must be torn down after the client disconnects",
        awaitCondition { origin.ended.isNotEmpty() },
      )
    } finally {
      proxy.stop()
      origin.close()
    }
  }

  @Test
  fun a_literal_target_is_never_resolved() {
    val origin = EchoOrigin()
    val resolver = FakeResolver(listOf(publicA))
    val seen = Collections.synchronizedList(mutableListOf<InetAddress>())
    val proxy = DnsPinnedProxy(
      resolver = resolver::resolve,
      connector = { address, _, _ ->
        seen.add(address)
        PinnedFakeSocket(
          Socket().apply { connect(InetSocketAddress(InetAddress.getByName("127.0.0.1"), origin.port), 2_000) },
          address,
        )
      },
    )
    val port = proxy.start()
    try {
      val client = Socket("127.0.0.1", port)
      send(client, "CONNECT 93.184.216.34:8443 HTTP/1.1\r\n\r\n")
      assertEquals("HTTP/1.1 200 Connection Established", statusLine(client))
      assertEquals(0, resolver.calls)
      assertEquals(listOf(publicA), seen.toList())
      client.close()
    } finally {
      proxy.stop()
      origin.close()
    }
  }

  // -------------------------------------------------------------------------
  // 2/3/4. Rebinding, mixed answers and empty answers
  // -------------------------------------------------------------------------

  @Test
  fun an_answer_that_turned_private_is_refused_for_the_new_connection() {
    val origin = EchoOrigin()
    // First connection: public. Second answer for the SAME name: loopback —
    // the rebinding move. Each CONNECT is a fresh decision.
    val resolver = FakeResolver(listOf(publicA), listOf(address(127, 0, 0, 1)))
    var dialled = 0
    val proxy = DnsPinnedProxy(
      resolver = resolver::resolve,
      connector = { address, _, _ ->
        dialled++
        PinnedFakeSocket(
          Socket().apply { connect(InetSocketAddress(InetAddress.getByName("127.0.0.1"), origin.port), 2_000) },
          address,
        )
      },
    )
    val port = proxy.start()
    try {
      val first = Socket("127.0.0.1", port)
      send(first, "CONNECT rebind.example:443 HTTP/1.1\r\n\r\n")
      assertEquals("HTTP/1.1 200 Connection Established", statusLine(first))
      first.close()

      val second = Socket("127.0.0.1", port)
      send(second, "CONNECT rebind.example:443 HTTP/1.1\r\n\r\n")
      assertEquals("HTTP/1.1 403 Forbidden", statusLine(second))
      assertEquals("the second connection must never be dialled", 1, dialled)
      assertEquals(1L, proxy.refusalCount(DnsPinnedProxy.Refusal.NON_PUBLIC))
      second.close()
    } finally {
      proxy.stop()
      origin.close()
    }
  }

  @Test
  fun a_mixed_public_and_private_answer_is_refused_as_a_whole() {
    val resolver = FakeResolver(listOf(publicA, address(10, 0, 0, 7)))
    var dialled = 0
    val proxy = DnsPinnedProxy(
      resolver = resolver::resolve,
      connector = { _, _, _ -> dialled++; throw java.io.IOException("must not be called") },
    )
    val port = proxy.start()
    try {
      val client = Socket("127.0.0.1", port)
      send(client, "CONNECT mixed.example:443 HTTP/1.1\r\n\r\n")
      assertEquals("HTTP/1.1 403 Forbidden", statusLine(client))
      // The public half must NOT be used as a fallback.
      assertEquals(0, dialled)
      assertEquals(1L, proxy.refusalCount(DnsPinnedProxy.Refusal.NON_PUBLIC))
      client.close()
    } finally {
      proxy.stop()
    }
  }

  @Test
  fun an_empty_answer_or_a_throwing_resolver_is_refused() {
    val resolver = FakeResolver(emptyList())
    var dialled = 0
    val proxy = DnsPinnedProxy(
      resolver = resolver::resolve,
      connector = { _, _, _ -> dialled++; throw java.io.IOException("must not be called") },
    )
    val port = proxy.start()
    try {
      val client = Socket("127.0.0.1", port)
      send(client, "CONNECT nothing.example:443 HTTP/1.1\r\n\r\n")
      assertEquals("HTTP/1.1 502 Bad Gateway", statusLine(client))
      client.close()
    } finally {
      proxy.stop()
    }

    val throwing = DnsPinnedProxy(
      resolver = { throw java.net.UnknownHostException("nope") },
      connector = { _, _, _ -> dialled++; throw java.io.IOException("must not be called") },
    )
    val other = throwing.start()
    try {
      val client = Socket("127.0.0.1", other)
      send(client, "CONNECT broken.example:443 HTTP/1.1\r\n\r\n")
      assertEquals("HTTP/1.1 502 Bad Gateway", statusLine(client))
      client.close()
    } finally {
      throwing.stop()
    }
    assertEquals(0, dialled)
  }

  // -------------------------------------------------------------------------
  // 5. The private-address matrix
  // -------------------------------------------------------------------------

  @Test
  fun every_non_public_address_shape_is_refused_before_connecting() {
    val cases = listOf(
      "127.0.0.1" to address(127, 0, 0, 1),
      "10.0.0.1" to address(10, 0, 0, 1),
      "172.16.0.1" to address(172, 16, 0, 1),
      "192.168.0.1" to address(192, 168, 0, 1),
      "169.254.169.254" to address(169, 254, 169, 254),
      "0.0.0.0" to address(0, 0, 0, 0),
      "100.64.0.1" to address(100, 64, 0, 1),
      "::1" to InetAddress.getByName("::1"),
      "fd00::1" to InetAddress.getByName("fd00::1"),
      "fe80::1" to InetAddress.getByName("fe80::1"),
      "::ffff:127.0.0.1" to InetAddress.getByName("::ffff:127.0.0.1"),
    )
    for ((label, resolved) in cases) {
      var dialled = 0
      val proxy = DnsPinnedProxy(
        resolver = { listOf(resolved) },
        connector = { _, _, _ -> dialled++; throw java.io.IOException("must not be called") },
      )
      val port = proxy.start()
      try {
        val client = Socket("127.0.0.1", port)
        send(client, "CONNECT site.example:443 HTTP/1.1\r\n\r\n")
        assertEquals("$label must be refused", "HTTP/1.1 403 Forbidden", statusLine(client))
        client.close()
      } finally {
        proxy.stop()
      }
      assertEquals("$label must never be dialled", 0, dialled)
    }
  }

  @Test
  fun alternative_ipv4_spellings_and_ip_literals_are_refused() {
    // Every one of these is loopback/link-local in the WHATWG parser, so a
    // CONNECT must be refused even though the engine would normalise it.
    for (literal in listOf("0177.0.0.1", "0x7f000001", "2130706433", "127.1", "169.254.169.254")) {
      var dialled = 0
      val proxy = DnsPinnedProxy(
        resolver = { emptyList() },
        connector = { _, _, _ -> dialled++; throw java.io.IOException("must not be called") },
      )
      val port = proxy.start()
      try {
        val client = Socket("127.0.0.1", port)
        send(client, "CONNECT $literal:443 HTTP/1.1\r\n\r\n")
        assertEquals("$literal must be refused", "HTTP/1.1 403 Forbidden", statusLine(client))
        client.close()
      } finally {
        proxy.stop()
      }
      assertEquals(0, dialled)
    }
  }

  // -------------------------------------------------------------------------
  // 7. The socket is verified to be where it was sent
  // -------------------------------------------------------------------------

  @Test
  fun a_socket_that_landed_elsewhere_is_never_tunnelled_through() {
    val origin = EchoOrigin()
    val proxy = DnsPinnedProxy(
      resolver = { listOf(publicA) },
      connector = { _, _, _ ->
        // A connector that (wrongly) connects somewhere else: the proxy's own
        // verification must catch it, close the socket and refuse.
        Socket().apply { connect(InetSocketAddress(InetAddress.getByName("127.0.0.1"), origin.port), 2_000) }
      },
    )
    val port = proxy.start()
    try {
      val client = Socket("127.0.0.1", port)
      send(client, "CONNECT pinned.example:443 HTTP/1.1\r\n\r\n")
      assertEquals("HTTP/1.1 403 Forbidden", statusLine(client))
      assertEquals(1L, proxy.refusalCount(DnsPinnedProxy.Refusal.ADDRESS_CHANGED))
      client.close()
    } finally {
      proxy.stop()
      origin.close()
    }
  }

  @Test
  fun the_production_connector_pins_and_verifies_on_real_sockets() {
    val origin = EchoOrigin()
    try {
      val loopback = InetAddress.getByName("127.0.0.1")
      // It connects, and it accepts the address it really reached.
      val socket = PinnedConnect.connect(loopback, origin.port, 2_000)
      PinnedConnect.verify(socket, loopback)
      DnsPinnedProxy.closeQuietly(socket)

      // The same socket, verified against a different address, must be
      // rejected AND closed: that is the guard against a future edit that
      // dialled by name.
      val other = PinnedConnect.connect(loopback, origin.port, 2_000)
      var refused = false
      try {
        PinnedConnect.verify(other, publicA)
      } catch (e: PinnedConnect.AddressChangedException) {
        refused = true
      }
      assertTrue("a landed-elsewhere socket must be refused", refused)
      assertTrue("and closed", other.isClosed)
    } finally {
      origin.close()
    }
  }

  // -------------------------------------------------------------------------
  // 8. Malformed requests, and cleartext HTTP through the proxy
  // -------------------------------------------------------------------------

  @Test
  fun a_cleartext_request_through_the_proxy_is_refused() {
    var dialled = 0
    val proxy = DnsPinnedProxy(
      resolver = { listOf(publicA) },
      connector = { _, _, _ -> dialled++; throw java.io.IOException("must not be called") },
    )
    val port = proxy.start()
    try {
      val client = Socket("127.0.0.1", port)
      send(client, "GET http://example.com/ HTTP/1.1\r\nHost: example.com\r\n\r\n")
      assertEquals("HTTP/1.1 405 Method Not Allowed", statusLine(client))
      assertEquals(0, dialled)
      assertEquals(1L, proxy.refusalCount(DnsPinnedProxy.Refusal.METHOD))
      client.close()
    } finally {
      proxy.stop()
    }
  }

  @Test
  fun malformed_connect_head_is_refused_and_the_socket_is_closed() {
    val bad = listOf(
      "CONNECT example.com HTTP/1.1", // no port
      "CONNECT example.com:0 HTTP/1.1",
      "CONNECT example.com:70000 HTTP/1.1",
      "CONNECT example.com:+443 HTTP/1.1",
      "CONNECT :443 HTTP/1.1",
      "CONNECT user@example.com:443 HTTP/1.1",
      "CONNECT example.com:443/../x HTTP/1.1",
      "CONNECT [::1:443 HTTP/1.1", // unbalanced bracket
      "CONNECT []:443 HTTP/1.1",
      "CONNECT [fe80::1%wlan0]:443 HTTP/1.1", // zone id
      "CONNECT example.com:443 HTTP/2.0",
      "CONNECT  example.com:443 HTTP/1.1", // double space
      "PRI * HTTP/2.0",
    )
    for (head in bad) {
      var dialled = 0
      val proxy = DnsPinnedProxy(
        resolver = { listOf(publicA) },
        connector = { _, _, _ -> dialled++; throw java.io.IOException("must not be called") },
      )
      val port = proxy.start()
      try {
        val client = Socket("127.0.0.1", port)
        client.soTimeout = 3_000
        send(client, "$head\r\n\r\n")
        val line = statusLine(client)
        assertTrue("$head must not be accepted, got $line", line.startsWith("HTTP/1.1 4"))
        assertEquals("$head must not be dialled", 0, dialled)
        client.close()
      } finally {
        proxy.stop()
      }
    }
  }

  @Test
  fun an_oversized_connect_head_is_refused() {
    val proxy = DnsPinnedProxy(
      limits = DnsPinnedProxy.Limits(headerBytes = 512),
      resolver = { listOf(publicA) },
      connector = { _, _, _ -> throw java.io.IOException("must not be called") },
    )
    val port = proxy.start()
    try {
      val client = Socket("127.0.0.1", port)
      client.soTimeout = 3_000
      val padding = "X-Pad: " + "a".repeat(600) + "\r\n"
      send(client, "CONNECT example.com:443 HTTP/1.1\r\n$padding\r\n")
      assertEquals("HTTP/1.1 403 Forbidden", statusLine(client))
      assertEquals(1L, proxy.refusalCount(DnsPinnedProxy.Refusal.HANDSHAKE))
      client.close()
    } finally {
      proxy.stop()
    }
  }

  @Test
  fun a_silent_client_is_dropped_after_the_handshake_timeout() {
    val proxy = DnsPinnedProxy(
      limits = DnsPinnedProxy.Limits(handshakeMs = 200),
      resolver = { listOf(publicA) },
      connector = { _, _, _ -> throw java.io.IOException("must not be called") },
    )
    val port = proxy.start()
    try {
      val client = Socket("127.0.0.1", port)
      client.soTimeout = 3_000
      // Send nothing at all: the proxy gives up on its OWN timeout and closes.
      val started = System.currentTimeMillis()
      val response = client.getInputStream().readBytes().toString(Charsets.ISO_8859_1)
      val elapsed = System.currentTimeMillis() - started
      assertTrue("the proxy must answer and close, got: $response", response.startsWith("HTTP/1.1 403"))
      assertTrue("it must not wait for the full idle timeout ($elapsed ms)", elapsed < 2_500)
      client.close()
    } finally {
      proxy.stop()
    }
  }

  // -------------------------------------------------------------------------
  // 9/10. Teardown and concurrency
  // -------------------------------------------------------------------------

  @Test
  fun a_client_that_leaves_mid_tunnel_closes_the_upstream_socket() {
    val origin = EchoOrigin()
    val proxy = DnsPinnedProxy(
      resolver = { listOf(publicA) },
      connector = { address, _, _ ->
        PinnedFakeSocket(
          Socket().apply { connect(InetSocketAddress(InetAddress.getByName("127.0.0.1"), origin.port), 2_000) },
          address,
        )
      },
    )
    val port = proxy.start()
    try {
      val client = Socket("127.0.0.1", port)
      send(client, "CONNECT drop.example:443 HTTP/1.1\r\n\r\n")
      assertEquals("HTTP/1.1 200 Connection Established", statusLine(client))
      assertTrue(awaitCondition { origin.accepted.isNotEmpty() })
      client.close()
      assertTrue(
        "the upstream connection must be torn down when the client leaves",
        awaitCondition { origin.ended.isNotEmpty() },
      )
      assertTrue(
        "the handler slot must be released",
        awaitCondition { proxy.activeConnections() == 0 },
      )
    } finally {
      proxy.stop()
      origin.close()
    }
  }

  @Test
  fun the_connection_ceiling_refuses_instead_of_queueing() {
    val origin = EchoOrigin()
    val proxy = DnsPinnedProxy(
      limits = DnsPinnedProxy.Limits(maxConnections = 2),
      resolver = { listOf(publicA) },
      connector = { address, _, _ ->
        PinnedFakeSocket(
          Socket().apply { connect(InetSocketAddress(InetAddress.getByName("127.0.0.1"), origin.port), 2_000) },
          address,
        )
      },
    )
    val port = proxy.start()
    val held = mutableListOf<Socket>()
    try {
      repeat(2) {
        val client = Socket("127.0.0.1", port)
        send(client, "CONNECT busy.example:443 HTTP/1.1\r\n\r\n")
        assertEquals("HTTP/1.1 200 Connection Established", statusLine(client))
        held.add(client)
      }
      // Third connection: the ceiling is reached, so it is refused outright.
      val third = Socket("127.0.0.1", port)
      third.soTimeout = 3_000
      val line = statusLine(third)
      assertTrue("expected a refusal, got $line", line.startsWith("HTTP/1.1 5"))
      third.close()
      assertTrue(proxy.refusalCount(DnsPinnedProxy.Refusal.BUSY) >= 1L)
    } finally {
      held.forEach { DnsPinnedProxy.closeQuietly(it) }
      proxy.stop()
      origin.close()
    }
  }

  @Test
  fun the_proxy_binds_loopback_only() {
    val proxy = DnsPinnedProxy(resolver = { listOf(publicA) })
    val port = proxy.start()
    try {
      // Reachable on loopback, and the bind constant is a literal: the proxy
      // never listens on a wildcard or a routable interface.
      val probe = Socket()
      probe.connect(InetSocketAddress("127.0.0.1", port), 1_000)
      assertTrue(probe.isConnected)
      probe.close()
      assertEquals("127.0.0.1", DnsPinnedProxy.LOOPBACK)
    } finally {
      proxy.stop()
    }
  }

  // -------------------------------------------------------------------------
  // The pure parser, exercised directly
  // -------------------------------------------------------------------------

  @Test
  fun the_connect_parser_accepts_only_what_the_policy_can_reason_about() {
    assertEquals(
      DnsPinnedProxy.Target("example.com", 443),
      DnsPinnedProxy.parseConnectHead("CONNECT example.com:443 HTTP/1.1\r\n\r\n"),
    )
    assertEquals(
      DnsPinnedProxy.Target("example.com", 8443),
      DnsPinnedProxy.parseAuthority("EXAMPLE.com.:8443"),
    )
    assertEquals(
      DnsPinnedProxy.Target("2001:db8::1", 443),
      DnsPinnedProxy.parseAuthority("[2001:db8::1]:443"),
    )
    for (raw in listOf(
      "", "example.com", "example.com:", ":443", "example.com:0", "example.com:65536",
      "example.com:44 3", "user:pw@example.com:443", "example.com:443?x=1", "[::1]",
      "[::1]443", "[]:443", "exa mple.com:443", "example.com:443\r\nX: y",
    )) {
      assertNull("$raw must not parse", DnsPinnedProxy.parseAuthority(raw))
    }
    assertNull(DnsPinnedProxy.parseConnectHead("GET / HTTP/1.1"))
    assertNull(DnsPinnedProxy.parseConnectHead("CONNECT example.com:443 HTTP/2"))
    assertEquals(443, DnsPinnedProxy.parsePort("443"))
    assertNull(DnsPinnedProxy.parsePort("-1"))
    assertNull(DnsPinnedProxy.parsePort("1e3"))
  }

  @Test
  fun the_proxy_says_which_address_a_literal_denotes() {
    assertEquals(
      "127.0.0.1",
      DnsPinnedProxy.literal("0177.0.0.1")?.hostAddress,
    )
    assertEquals(
      "127.0.0.1",
      DnsPinnedProxy.literal("0x7f000001")?.hostAddress,
    )
    assertEquals(InetAddress.getByName("::1"), DnsPinnedProxy.literal("::1"))
    assertNull("a hostname is not a literal", DnsPinnedProxy.literal("example.com"))
    assertNull(DnsPinnedProxy.literal("a:b"))
  }

  @Test
  fun a_stopped_proxy_releases_its_port() {
    val proxy = DnsPinnedProxy(resolver = { listOf(publicA) })
    val port = proxy.start()
    proxy.stop()
    assertNull(proxy.port)
    assertFalse("a stopped proxy must not accept", proxy.canAccept(port))
  }

  private fun DnsPinnedProxy.canAccept(port: Int): Boolean = try {
    Socket().use { it.connect(InetSocketAddress("127.0.0.1", port), 500) }
    true
  } catch (_: Exception) {
    false
  }
}
