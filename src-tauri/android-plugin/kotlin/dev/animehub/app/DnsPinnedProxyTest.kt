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
      // Round 22: the forms the desktop parser used to accept (and tunnel) while
      // this one refused them: a signed port, a bracketed IPv4 and an
      // unbracketed a:b host.
      "example.com:+443", "[8.8.8.8]:443", "example.com:443:443",
    )) {
      assertNull("$raw must not parse", DnsPinnedProxy.parseAuthority(raw))
    }
    assertNull(DnsPinnedProxy.parseConnectHead("GET / HTTP/1.1"))
    assertNull(DnsPinnedProxy.parseConnectHead("CONNECT example.com:443 HTTP/2"))
    assertEquals(443, DnsPinnedProxy.parsePort("443"))
    assertNull(DnsPinnedProxy.parsePort("-1"))
    assertNull(DnsPinnedProxy.parsePort("1e3"))
  }

  /**
   * The CONNECT grammar shared with the Rust `parse_connect`.
   *
   * One fixture, two row kinds, and each row runs the **production** parser —
   * no test-local reimplementation:
   *
   *  * `request`   — the whole line, through `parseConnectHead`;
   *  * `authority` — the authority alone, through `parseAuthority`.
   *
   * The Rust suite includes the identical file, so a method, version,
   * character, bracket or port rule added to one parser alone fails the other
   * suite. `accept` means the parser accepts the line; the destination policy
   * still runs afterwards (`127.0.0.1:443` parses and is then refused).
   */
  @Test
  fun `connect vectors agree with the rust parser`() {
    val fixture =
      javaClass.classLoader?.getResourceAsStream(CONNECT_FIXTURE)
        ?: throw AssertionError(
          "test kaynağı bulunamadı: $CONNECT_FIXTURE — app/src/test/resources altına kopyalanmalı",
        )
    var requests = 0
    var authorities = 0
    for (raw in fixture.bufferedReader().use { it.readLines() }) {
      val line = raw.trimEnd('\r')
      if (line.isEmpty() || line.startsWith("#")) continue
      val cols = line.split('\t')
      assertEquals("üç sütun (tür, değer, karar): $line", 3, cols.size)
      val parsed =
        when (cols[0]) {
          "request" -> {
            requests++
            DnsPinnedProxy.parseConnectHead("${cols[1]}\r\n\r\n")
          }
          "authority" -> {
            authorities++
            DnsPinnedProxy.parseAuthority(cols[1])
          }
          else -> throw AssertionError("bilinmeyen vektör türü: ${cols[0]} ($line)")
        }
      when (cols[2]) {
        "accept" -> assertTrue("${cols[1]} ayrıştırılmalı", parsed != null)
        "reject" -> assertNull("${cols[1]} ayrıştırılmamalı", parsed)
        else -> throw AssertionError("bilinmeyen karar: ${cols[2]} ($line)")
      }
    }
    // A truncated or emptied fixture must not pass with zero vectors, and both
    // row kinds have to survive: dropping every `request` row would otherwise
    // leave the method and version rules untested.
    assertTrue("request vektörleri kısalmış: $requests", requests >= 30)
    assertTrue("authority vektörleri kısalmış: $authorities", authorities >= 30)
  }

  /**
   * RFC 9112 §3.1 (method is case-sensitive) and §2.3 (`HTTP/DIGIT.DIGIT`,
   * case-sensitive), stated next to the fixture so a failure names the rule.
   * `HTTP/1.2` is valid syntax and is still refused: this proxy is HTTP/1.0 and
   * 1.1 only, which is an application choice shared with the desktop parser.
   */
  @Test
  fun `only the connect method and http 1x are accepted`() {
    assertTrue(DnsPinnedProxy.parseConnectHead("CONNECT example.test:443 HTTP/1.1\r\n\r\n") != null)
    assertTrue(DnsPinnedProxy.parseConnectHead("CONNECT example.test:443 HTTP/1.0\r\n\r\n") != null)
    for (line in
      listOf(
        "connect example.test:443 HTTP/1.1",
        "Connect example.test:443 HTTP/1.1",
        "CONNECt example.test:443 HTTP/1.1",
        "CONNECT example.test:443 HTTP/1.2",
        "CONNECT example.test:443 HTTP/1.",
        "CONNECT example.test:443 HTTP/1.foo",
        "CONNECT example.test:443 HTTP/1.1x",
        "CONNECT example.test:443 http/1.1",
        "CONNECT example.test:443 HTTP/2",
      )) {
      assertNull("$line reddedilmeli", DnsPinnedProxy.parseConnectHead("$line\r\n\r\n"))
    }
  }

  /**
   * Bytes the TSV cannot carry: a raw control byte and a non-ASCII byte in the
   * target. Both must die in the parser, before the resolver or a socket.
   */
  @Test
  fun `control and non ascii bytes never reach the resolver`() {
    for (line in
      listOf(
        "CONNECT a\tb.test:443 HTTP/1.1",
        "CONNECT a\u0001b.test:443 HTTP/1.1",
        "CONNECT \u00e9xample.test:443 HTTP/1.1",
      )) {
      assertNull("$line reddedilmeli", DnsPinnedProxy.parseConnectHead("$line\r\n\r\n"))
    }
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

  // -------------------------------------------------------------------------
  // 11. Resolver timeout, shutdown and who else can reach the listener
  // -------------------------------------------------------------------------

  /**
   * A resolver that never answers, the way a captive portal or a dead DNS
   * server behaves: `InetAddress.getAllByName` has no timeout of its own.
   */
  private class StuckResolver {
    val calls = java.util.concurrent.atomic.AtomicInteger(0)

    @Suppress("UNUSED_PARAMETER")
    fun resolve(host: String): List<InetAddress> {
      calls.incrementAndGet()
      val latch = java.util.concurrent.CountDownLatch(1)
      try {
        // Released in teardown via interruption of the daemon thread when the
        // proxy's pool is shut down; until then it blocks like a real lookup.
        latch.await()
      } catch (_: InterruptedException) {
        Thread.currentThread().interrupt()
      }
      return emptyList()
    }
  }

  /**
   * A resolver call that **cannot** be interrupted, the way a stuck
   * `getAllByName` on a dead resolver really behaves.
   *
   * [StuckResolver] above waits on a latch, so `future.cancel(true)` releases
   * it — that models a slow lookup, not a wedged one. This one swallows the
   * interrupt and keeps waiting until [release], which is the only honest way
   * to measure what happens when the pool threads are genuinely owned by the
   * platform call. [hangCalls] limits which calls hang; `null` means all.
   */
  private class UninterruptibleResolver(vararg answers: List<InetAddress>, private val hangCalls: Int? = null) {
    val calls = java.util.concurrent.atomic.AtomicInteger(0)

    /** Lookups that are past the hang and have really returned. */
    val completed = java.util.concurrent.atomic.AtomicInteger(0)
    val release = java.util.concurrent.atomic.AtomicBoolean(false)
    private val queue = ArrayDeque(answers.toList())

    fun resolve(host: String): List<InetAddress> {
      val call = calls.incrementAndGet()
      val hangs = hangCalls == null || call <= hangCalls
      if (hangs) {
        while (!release.get()) {
          try {
            Thread.sleep(10)
          } catch (_: InterruptedException) {
            // Ignored on purpose: the platform call is not interruptible.
          }
        }
      }
      val answer = if (queue.isEmpty()) listOf(PUBLIC_ANSWER) else queue.removeFirst()
      completed.incrementAndGet()
      return answer
    }
  }

  private companion object {
    /** A public address no test ever connects to for real. */
    val PUBLIC_ANSWER: InetAddress =
      InetAddress.getByAddress(byteArrayOf(93.toByte(), 184.toByte(), 216.toByte(), 34.toByte()))

    /**
     * The shared CONNECT request/authority vectors. `.github/workflows/ci.yml`
     * copies `tests/fixtures/connect-request.tsv` into
     * `app/src/test/resources/`; a missing copy fails the test instead of
     * shrinking the suite, and the Rust suite includes the very same file.
     */
    const val CONNECT_FIXTURE = "connect-request.tsv"
  }

  /**
   * A resolver whose calls are gated one by one: the first [gated] calls wait
   * for their own gate and ignore interrupts, later calls answer immediately.
   *
   * This is what makes the "old generation becomes usable again" case
   * measurable: releasing call 1 frees generation 1 while generation 2 is still
   * wedged.
   */
  private class GatedResolver(private val gated: Int) {
    val calls = java.util.concurrent.atomic.AtomicInteger(0)
    private val gates = java.util.concurrent.ConcurrentHashMap<Int, java.util.concurrent.CountDownLatch>()

    private fun gate(call: Int) = gates.getOrPut(call) { java.util.concurrent.CountDownLatch(1) }

    fun release(call: Int) {
      gate(call).countDown()
    }

    @Suppress("UNUSED_PARAMETER")
    fun resolve(host: String): List<InetAddress> {
      val call = calls.incrementAndGet()
      if (call <= gated) {
        val gate = gate(call)
        while (gate.count > 0L) {
          try {
            gate.await(20, java.util.concurrent.TimeUnit.MILLISECONDS)
          } catch (_: InterruptedException) {
            // Not interruptible on purpose.
          }
        }
      }
      return listOf(PUBLIC_ANSWER)
    }
  }

  private fun resolverThreads(): Int =
    Thread.getAllStackTraces().keys.count { it.name.startsWith("AnimeHubProxyResolve") && it.isAlive }

  private fun proxyThreads(): List<String> =
    Thread.getAllStackTraces().keys.filter { it.isAlive && it.name.startsWith("AnimeHubProxy") }
      .map { it.name }

  /**
   * Live threads with exactly this name.
   *
   * The pools are named `AnimeHubProxy`, `AnimeHubProxyTunnel`,
   * `AnimeHubProxyAccept` and `AnimeHubProxyResolve`, and several tests create
   * proxies, so a `startsWith` count mixes instances that are still winding
   * down. An exact name is what "when a phase fails, no accept/handler/tunnel
   * thread was left behind" can be asserted against.
   */
  private fun threadsNamed(name: String): Int =
    Thread.getAllStackTraces().keys.count { it.isAlive && it.name == name }

  /**
   * Resolver threads that are *inside a lookup right now*.
   *
   * Unlike the global thread count this cannot be confused by idle pool threads
   * from an earlier test that are still inside their keep-alive: only the
   * thread executing this suite's fake resolver has these frames on its stack.
   */
  private fun resolverThreadsInsideLookup(): Int =
    Thread.getAllStackTraces().entries.count { (thread, stack) ->
      thread.name.startsWith("AnimeHubProxyResolve") &&
        stack.any { it.className.contains("DnsPinnedProxyTest") }
    }

  /**
   * True once a resolver thread is parked waiting for work, i.e. its generation
   * can accept a submission again. Waiting for this instead of sleeping makes
   * the reuse test deterministic: the pool hands a finished task back to the
   * queue a moment after the callable returns.
   */
  private fun awaitIdleResolverThread(timeoutMs: Long = 3_000): Boolean = awaitCondition(timeoutMs) {
    Thread.getAllStackTraces().entries.any { (thread, stack) ->
      thread.name.startsWith("AnimeHubProxyResolve") &&
        thread.state == Thread.State.TIMED_WAITING &&
        stack.any { it.className.startsWith("java.util.concurrent.SynchronousQueue") }
    }
  }

  @Test
  fun a_stuck_resolver_is_refused_after_a_bounded_wait() {
    val resolver = StuckResolver()
    var dialled = 0
    val proxy = DnsPinnedProxy(
      limits = DnsPinnedProxy.Limits(resolveMs = 250),
      resolver = resolver::resolve,
      connector = { _, _, _ -> dialled++; throw java.io.IOException("must not be called") },
    )
    val port = proxy.start()
    try {
      val client = Socket("127.0.0.1", port)
      client.soTimeout = 5_000
      send(client, "CONNECT slow.example:443 HTTP/1.1\r\n\r\n")
      val started = System.currentTimeMillis()
      val line = statusLine(client)
      val elapsed = System.currentTimeMillis() - started

      // A resolver that never answers is a gateway failure — never a 200, and
      // never a public verdict, because no address was ever classified.
      assertEquals("HTTP/1.1 502 Bad Gateway", line)
      assertTrue("the wait must be bounded, took $elapsed ms", elapsed < 2_500)
      assertEquals("nothing may be dialled without an address", 0, dialled)
      assertEquals(1L, proxy.refusalCount(DnsPinnedProxy.Refusal.RESOLVE_FAILED))
      // The handler slot is released even though the lookup is still running.
      assertTrue(
        "the handler must not stay occupied",
        awaitCondition { proxy.activeConnections() == 0 },
      )
      client.close()
    } finally {
      proxy.stop()
    }
  }

  // ---------------------------------------------------------------------
  // start(): all-or-nothing
  // ---------------------------------------------------------------------

  @Test
  fun an_invalid_limits_value_cannot_even_be_constructed() {
    // Every one of these ends up as a socket option, a pool size or an array
    // length. A zero/negative value would make the proxy useless or throw from
    // inside start() with the listener already bound, and an oversized one is an
    // allocation: the proxy therefore rejects them at construction, before a
    // socket or a thread can exist.
    val invalid = listOf<() -> DnsPinnedProxy.Limits>(
      { DnsPinnedProxy.Limits(handshakeMs = 0) },
      { DnsPinnedProxy.Limits(handshakeMs = -1) },
      { DnsPinnedProxy.Limits(connectMs = 0) },
      { DnsPinnedProxy.Limits(resolveMs = 0) },
      { DnsPinnedProxy.Limits(idleMs = 0) },
      { DnsPinnedProxy.Limits(maxConnections = 0) },
      { DnsPinnedProxy.Limits(maxConnections = -4) },
      { DnsPinnedProxy.Limits(backlog = 0) },
      { DnsPinnedProxy.Limits(headerBytes = 0) },
      { DnsPinnedProxy.Limits(headerBytes = DnsPinnedProxy.Limits.MAX_HEADER_BYTES + 1) },
      { DnsPinnedProxy.Limits(bufferBytes = 0) },
      { DnsPinnedProxy.Limits(bufferBytes = DnsPinnedProxy.Limits.MAX_BUFFER_BYTES + 1) },
      { DnsPinnedProxy.Limits(resolveThreads = 0) },
      // `2 * resolveThreads` must not overflow into a negative bound.
      { DnsPinnedProxy.Limits(resolveThreads = Int.MAX_VALUE / 2 + 1, resolveThreadCap = -1) },
    )
    for ((index, build) in invalid.withIndex()) {
      var threw = false
      try {
        build()
      } catch (_: IllegalArgumentException) {
        threw = true
      }
      assertTrue("invalid Limits #$index must be rejected", threw)
      // `copy()` re-runs the same validation, so a valid value cannot be edited
      // into an invalid one either.
      var copyThrew = false
      try {
        DnsPinnedProxy.Limits().copy(maxConnections = 0)
      } catch (_: IllegalArgumentException) {
        copyThrew = true
      }
      if (index == 0) assertTrue("copy() must validate too", copyThrew)
    }
  }

  @Test
  fun a_failed_start_leaves_nothing_behind_and_can_be_retried() {
    // Each phase of start() is made to fail once, in order. After every failure
    // the object must be exactly as it was before: no port, no listener, no
    // pool thread, no resolver generation spent. The same object must then be
    // able to start cleanly.
    for (phase in DnsPinnedProxy.StartPhase.entries) {
      // The previous iteration's stop() is asynchronous for its accept thread.
      assertTrue(
        "the previous proxy must be gone before the next phase",
        awaitCondition { threadsNamed("AnimeHubProxyAccept") == 0 },
      )
      var fail = true
      val proxy = DnsPinnedProxy(
        limits = DnsPinnedProxy.Limits(resolveThreads = 1, resolveThreadCap = 2),
        resolver = { listOf(publicA) },
        connector = { _, _, _ -> throw java.io.IOException("must not be called") },
        startHook = { failingPhase ->
          if (fail && failingPhase == phase) throw java.io.IOException("phase $phase failed")
        },
      )
      try {
        var failure: Exception? = null
        try {
          proxy.start()
        } catch (e: Exception) {
          failure = e
        }
        assertTrue("$phase: a failing start must throw, got $failure", failure is java.io.IOException)
        assertNull("$phase: a failed start must not leave a port", proxy.port)
        assertEquals(
          "$phase: a failed start must return its resolver generation",
          0,
          proxy.resolverGenerationCount(),
        )
        // No accept, handler or tunnel thread: the pools never ran a task and
        // the accept thread must not have been started.
        assertEquals("$phase: no accept thread may survive", 0, threadsNamed("AnimeHubProxyAccept"))
        assertEquals("$phase: no handler thread may survive", 0, threadsNamed("AnimeHubProxy"))
        assertEquals("$phase: no tunnel thread may survive", 0, threadsNamed("AnimeHubProxyTunnel"))

        // The object is still usable, and it starts cleanly on the second
        // attempt: nothing from the failed attempt is remembered or leaked.
        fail = false
        val port = proxy.start()
        assertTrue("$phase: the retry must bind, got $port", port > 0)
        assertEquals("$phase: exactly one generation per lifecycle", 1, proxy.resolverGenerationCount())
        assertEquals("$phase: the retry must publish one port", port, proxy.port)
        assertEquals("$phase: start() is idempotent", port, proxy.start())

        // A real CONNECT still works after the failed attempt: the resolver
        // generation that was released was replaced, not exhausted.
        val client = Socket("127.0.0.1", port)
        client.soTimeout = 3_000
        send(client, "CONNECT example.com:443 HTTP/1.1\r\n\r\n")
        assertEquals("$phase: the proxy must work after the retry", "HTTP/1.1 502 Bad Gateway", statusLine(client))
        assertEquals("$phase: the retry must have resolved", 1L, proxy.resolutionCount())
        client.close()
      } finally {
        proxy.stop()
        assertNull("$phase: stop() must clear the port", proxy.port)
        assertTrue(
          "$phase: stop() must end the accept thread, saw ${proxyThreads()}",
          awaitCondition { threadsNamed("AnimeHubProxyAccept") == 0 },
        )
      }
    }
  }

  @Test
  fun a_failed_start_returns_its_resolver_generation_to_the_budget() {
    // The generation is taken before the listener exists, so a failed start has
    // to put it back — otherwise every failed attempt would consume one unit of
    // a cap that is only two generations wide, and a start that failed twice
    // would leave the object unable to start at all.
    var fail = true
    val proxy = DnsPinnedProxy(
      limits = DnsPinnedProxy.Limits(resolveThreads = 1, resolveThreadCap = 2),
      resolver = { listOf(publicA) },
      connector = { _, _, _ -> throw java.io.IOException("must not be called") },
      startHook = { phase -> if (fail && phase == DnsPinnedProxy.StartPhase.PUMP_POOL) throw java.io.IOException("no pump") },
    )
    try {
      // Four failures in a row: a leaked generation would exhaust the budget at
      // the third one, because the cap only fits two generations.
      repeat(4) {
        var threw = false
        try {
          proxy.start()
        } catch (_: java.io.IOException) {
          threw = true
        }
        assertTrue("failure #$it must be reported", threw)
        assertNull("failure #$it must not leave a port", proxy.port)
      }
      fail = false
      val port = proxy.start()
      assertTrue("a clean start must still be possible, got $port", port > 0)
    } finally {
      proxy.stop()
    }
  }

  @Test
  fun a_head_of_exactly_header_bytes_is_accepted_and_one_more_byte_is_not() {
    // The read loop tests `out.size() < headerBytes` BEFORE reading, so it can
    // never store more than headerBytes bytes: the audit's "reads
    // headerBytes + 1" claim is wrong. This pins the boundary from both sides.
    val limit = 128
    val base = "CONNECT example.com:443 HTTP/1.1\r\n"
    fun head(total: Int): String {
      val pad = total - base.length - 2 - 2 // trailing CRLF + blank line
      return base + "X-Pad: " + "a".repeat(pad - 7) + "\r\n" + "\r\n"
    }
    val exact = head(limit)
    assertEquals("the fixture must be exactly the limit", limit, exact.length)

    for ((label, payload, expected) in listOf(
      Triple("exactly headerBytes", exact, "HTTP/1.1 502 Bad Gateway"),
      Triple("headerBytes + 1", head(limit + 1), "HTTP/1.1 403 Forbidden"),
    )) {
      val proxy = DnsPinnedProxy(
        limits = DnsPinnedProxy.Limits(headerBytes = limit),
        resolver = { listOf(publicA) },
        connector = { _, _, _ -> throw java.io.IOException("must not be called") },
      )
      val port = proxy.start()
      try {
        val client = Socket("127.0.0.1", port)
        client.soTimeout = 3_000
        send(client, payload)
        assertEquals("$label must be classified, not truncated", expected, statusLine(client))
        client.close()
        if (label.startsWith("exactly")) {
          assertEquals(
            "a head of exactly headerBytes must reach the resolver",
            1L,
            proxy.resolutionCount(),
          )
        } else {
          assertEquals(
            "one byte more must be refused before resolution",
            0L,
            proxy.resolutionCount(),
          )
          assertEquals(1L, proxy.refusalCount(DnsPinnedProxy.Refusal.HANDSHAKE))
        }
      } finally {
        proxy.stop()
      }
    }
  }

  @Test
  fun the_cap_must_leave_room_for_a_full_spare_generation() {
    // 4/5 used to be accepted and then could never form a spare generation:
    // the configuration read as a bound while it silently disabled the only
    // recovery this design has. It is rejected at construction instead.
    for (cap in listOf(0, 4, 5, 7)) {
      var threw = false
      try {
        DnsPinnedProxy.Limits(resolveThreads = 4, resolveThreadCap = cap)
      } catch (_: IllegalArgumentException) {
        threw = true
      }
      assertTrue("resolveThreads = 4 with resolveThreadCap = $cap must be rejected", threw)
    }
    // The defaults are valid and leave room for exactly one spare generation.
    val defaults = DnsPinnedProxy.Limits()
    assertEquals(4, defaults.resolveThreads)
    assertEquals(8, defaults.resolveThreadCap)
    // A larger cap is accepted; the part that is not a whole generation is
    // documented as unused rather than turned into a half generation.
    assertEquals(10, DnsPinnedProxy.Limits(resolveThreads = 4, resolveThreadCap = 10).resolveThreadCap)
  }

  @Test
  fun a_wedged_resolver_gets_one_spare_generation_and_service_returns() {
    // Two lookups that cannot be interrupted own both threads of the first
    // generation. Without a spare, every later CONNECT would be refused until
    // the process restarted — the availability bug this test exists for. The
    // third call answers, so it runs on a fresh generation.
    val resolver = UninterruptibleResolver(hangCalls = 2)
    val baseline = resolverThreads()
    val origin = EchoOrigin()
    val proxy = DnsPinnedProxy(
      limits = DnsPinnedProxy.Limits(resolveMs = 200, resolveThreads = 2, resolveThreadCap = 4),
      resolver = resolver::resolve,
      connector = { address, _, _ ->
        PinnedFakeSocket(
          Socket().apply { connect(InetSocketAddress(InetAddress.getByName("127.0.0.1"), origin.port), 2_000) },
          address,
        )
      },
    )
    val port = proxy.start()
    try {
      for (host in listOf("one.example", "two.example")) {
        val client = Socket("127.0.0.1", port)
        send(client, "CONNECT $host:443 HTTP/1.1\r\n\r\n")
        assertEquals("HTTP/1.1 502 Bad Gateway", statusLine(client))
        client.close()
      }
      assertEquals("both lookups must really be running", 2, resolver.calls.get())
      assertEquals(2L, proxy.refusalCount(DnsPinnedProxy.Refusal.RESOLVE_FAILED))

      // Third attempt: the generation is full, so a new one is created and the
      // waiting network is used again — no app restart, no cached failure.
      val client = Socket("127.0.0.1", port)
      client.soTimeout = 5_000
      send(client, "CONNECT three.example:443 HTTP/1.1\r\n\r\n")
      assertEquals("HTTP/1.1 200 Connection Established", statusLine(client))
      client.close()

      assertEquals("the third lookup must have run", 3, resolver.calls.get())
      assertEquals(3L, proxy.resolutionCount())
      assertEquals(0L, proxy.refusalCount(DnsPinnedProxy.Refusal.RESOLVE_EXHAUSTED))
      assertTrue(
        "resolver threads must stay within the cap, saw ${resolverThreads() - baseline}",
        // Idle pool threads from earlier tests can still be shutting down, so
        // the delta is clamped: the claim is "no more than the cap", never
        // "exactly the cap".
        (resolverThreads() - baseline).coerceAtLeast(0) <= 4,
      )
    } finally {
      // `stop()` interrupts every generation, but a call that ignores the
      // interrupt keeps its thread until the platform returns — which is the
      // documented residual, not something stop() can fix. Releasing the fake
      // lookup is what proves the threads are then released and not leaked.
      resolver.release.set(true)
      proxy.stop()
      origin.close()
      assertTrue(
        "every generation must be released once the lookups return, saw ${resolverThreads() - baseline}",
        awaitCondition(3_000) { resolverThreads() <= baseline },
      )
    }
  }

  @Test
  fun when_every_generation_is_stuck_dns_is_refused_until_the_platform_returns() {
    // resolveThreads = 1 and a cap of 2: one wedged lookup, one spare thread,
    // then the ceiling. This is the documented availability residual, measured
    // instead of asserted in prose.
    val resolver = UninterruptibleResolver()
    val baseline = resolverThreads()
    val proxy = DnsPinnedProxy(
      limits = DnsPinnedProxy.Limits(resolveMs = 200, resolveThreads = 1, resolveThreadCap = 2),
      resolver = resolver::resolve,
      connector = { _, _, _ -> throw java.io.IOException("must not be called") },
    )
    val port = proxy.start()
    try {
      // 1 and 2 spend the two threads the cap allows, each on its own timeout.
      repeat(2) {
        val client = Socket("127.0.0.1", port)
        send(client, "CONNECT stuck$it.example:443 HTTP/1.1\r\n\r\n")
        assertEquals("HTTP/1.1 502 Bad Gateway", statusLine(client))
        client.close()
      }
      assertEquals(2, resolver.calls.get())
      assertTrue(
        "threads must stay within the cap, saw ${resolverThreads() - baseline}",
        (resolverThreads() - baseline).coerceAtLeast(0) <= 2,
      )

      // From here the refusal is immediate and global: a different host is
      // refused just as fast, because the shortage is in this process, not in
      // that host's answer. Nothing is queued and nothing is dialled.
      val started = System.currentTimeMillis()
      for (host in listOf("another.example", "third.example")) {
        val client = Socket("127.0.0.1", port)
        send(client, "CONNECT $host:443 HTTP/1.1\r\n\r\n")
        assertEquals("HTTP/1.1 502 Bad Gateway", statusLine(client))
        client.close()
      }
      val elapsed = System.currentTimeMillis() - started
      assertTrue("an exhausted resolver must refuse at once, took $elapsed ms", elapsed < 500)
      assertEquals("no new lookup may start past the cap", 2, resolver.calls.get())
      assertEquals(2L, proxy.refusalCount(DnsPinnedProxy.Refusal.RESOLVE_EXHAUSTED))
      assertTrue(awaitCondition { proxy.activeConnections() == 0 })
      assertTrue(
        "the handler slots must not leak while DNS is down",
        proxy.activeConnections() == 0,
      )

      // When the platform calls finally return, capacity comes back on its own
      // and the very next CONNECT resolves again: no failure was cached.
      resolver.release.set(true)
      assertTrue(
        "the wedged lookups must finish",
        awaitCondition(3_000) { resolver.completed.get() == 2 },
      )
      // A freed worker is handed back to the pool a moment after the callable
      // returns, so this loop tolerates a refusal or two while it settles.
      // Nothing is restarted and nothing was cached: what matters is that the
      // name is resolved again as soon as the platform returns.
      var status: String? = null
      val deadline = System.currentTimeMillis() + 5_000
      while (System.currentTimeMillis() < deadline && resolver.calls.get() < 3) {
        val client = Socket("127.0.0.1", port)
        send(client, "CONNECT after.example:443 HTTP/1.1\r\n\r\n")
        status = statusLine(client)
        client.close()
        if (resolver.calls.get() >= 3) break
        Thread.sleep(25)
      }
      assertEquals("the next attempt must resolve again", 3, resolver.calls.get())
      // Public answer, connector throws: CONNECT_FAILED, not a resolver refusal.
      assertEquals("HTTP/1.1 502 Bad Gateway", status)
      assertTrue(
        "the refusal must be the connector, not the resolver",
        proxy.refusalCount(DnsPinnedProxy.Refusal.CONNECT_FAILED) >= 1L,
      )
      assertTrue(
        "resolver threads must stay within the cap after the lookups return",
        (resolverThreads() - baseline).coerceAtLeast(0) <= 2,
      )
    } finally {
      resolver.release.set(true)
      proxy.stop()
      assertTrue(
        "every generation must be released once the lookups return, saw ${resolverThreads() - baseline}",
        awaitCondition(3_000) { resolverThreads() <= baseline },
      )
    }
  }

  @Test
  fun an_older_generation_is_reused_once_its_lookup_returns() {
    // resolveThreads = 1, cap = 2: one generation, one spare. The case that
    // used to stay broken: generation 1's stuck call returns while generation 2
    // is still stuck. The next CONNECT must run on generation 1 — the budget is
    // already spent, so no third generation can be created, and refusing there
    // would be a DNS outage with a perfectly usable thread idle.
    val resolver = GatedResolver(gated = 2)
    val baseline = resolverThreads()
    val origin = EchoOrigin()
    val proxy = DnsPinnedProxy(
      limits = DnsPinnedProxy.Limits(resolveMs = 200, resolveThreads = 1, resolveThreadCap = 2),
      resolver = resolver::resolve,
      connector = { address, _, _ ->
        PinnedFakeSocket(
          Socket().apply { connect(InetSocketAddress(InetAddress.getByName("127.0.0.1"), origin.port), 2_000) },
          address,
        )
      },
    )
    val port = proxy.start()
    try {
      // Call 1 wedges generation 1.
      val first = Socket("127.0.0.1", port)
      send(first, "CONNECT one.example:443 HTTP/1.1\r\n\r\n")
      assertEquals("HTTP/1.1 502 Bad Gateway", statusLine(first))
      first.close()
      assertEquals(1, resolver.calls.get())

      // Call 2 wedges the spare generation; generation 1 is still stuck.
      val second = Socket("127.0.0.1", port)
      send(second, "CONNECT two.example:443 HTTP/1.1\r\n\r\n")
      assertEquals("HTTP/1.1 502 Bad Gateway", statusLine(second))
      second.close()
      assertEquals("the spare generation must have been used", 2, resolver.calls.get())
      assertTrue(
        "two generations must be inside a lookup, saw ${resolverThreadsInsideLookup()}",
        awaitCondition { resolverThreadsInsideLookup() == 2 },
      )

      // Generation 1's call returns, so its thread is idle again.
      resolver.release(1)
      assertTrue("generation 1 must become usable again", awaitIdleResolverThread())

      // The budget is spent (2 of 2), so this can only work by reusing
      // generation 1. It used to be refused with RESOLVE_EXHAUSTED.
      val third = Socket("127.0.0.1", port)
      third.soTimeout = 5_000
      send(third, "CONNECT three.example:443 HTTP/1.1\r\n\r\n")
      assertEquals("HTTP/1.1 200 Connection Established", statusLine(third))
      third.close()

      assertEquals("the reused generation must have resolved", 3, resolver.calls.get())

      // Reuse must not turn into a cache: the same host resolves AGAIN on the
      // next connection, because a stored answer would be a stored verdict.
      val fourth = Socket("127.0.0.1", port)
      fourth.soTimeout = 5_000
      send(fourth, "CONNECT three.example:443 HTTP/1.1\r\n\r\n")
      assertEquals("HTTP/1.1 200 Connection Established", statusLine(fourth))
      fourth.close()
      assertEquals("the same host must be resolved again", 4, resolver.calls.get())
      assertEquals("a lookup was possible, so nothing may be exhausted", 0L, proxy.refusalCount(DnsPinnedProxy.Refusal.RESOLVE_EXHAUSTED))
      assertTrue(
        "no third generation may exist: cap is 2, saw ${resolverThreads() - baseline}",
        (resolverThreads() - baseline).coerceAtLeast(0) <= 2,
      )
    } finally {
      resolver.release(1)
      resolver.release(2)
      proxy.stop()
      origin.close()
      assertTrue(
        "resolver threads must drain once the calls return, saw ${resolverThreads() - baseline}",
        awaitCondition(3_000) { resolverThreads() <= baseline },
      )
    }
  }

  @Test
  fun a_stop_racing_a_lookup_leaves_no_proxy_thread_behind() {
    // stop() takes the rotation lock and shuts every generation down while
    // handlers are inside `future.get`. Nothing may block (a stuck platform call
    // is not waited for) and nothing may survive: a handler that refuses into a
    // closed socket must not keep its thread.
    val resolver = UninterruptibleResolver()
    val baseline = resolverThreads()
    val proxy = DnsPinnedProxy(
      limits = DnsPinnedProxy.Limits(resolveMs = 5_000, resolveThreads = 2, resolveThreadCap = 4),
      resolver = resolver::resolve,
      connector = { _, _, _ -> throw java.io.IOException("must not be called") },
    )
    val port = proxy.start()
    val clients = (0 until 4).map { index ->
      Thread {
        try {
          val client = Socket("127.0.0.1", port)
          client.soTimeout = 8_000
          send(client, "CONNECT stop$index.example:443 HTTP/1.1\r\n\r\n")
          statusLine(client)
          client.close()
        } catch (_: Exception) {
          // EOF or a refused socket is fine: stop() is allowed to win.
        }
      }.apply { isDaemon = true; start() }
    }
    try {
      assertTrue(
        "the lookups must be inside the resolver before the stop, saw ${resolver.calls.get()}",
        awaitCondition { resolver.calls.get() >= 2 },
      )
      val started = System.nanoTime()
      proxy.stop()
      val elapsedMs = (System.nanoTime() - started) / 1_000_000
      assertTrue("stop() must not wait for a stuck lookup, took $elapsedMs ms", elapsedMs < 2_000)
      clients.forEach { it.join(8_000) }
      // The accept, handler and tunnel threads are interruptible and must be
      // gone. The resolver calls are not: they are expected to survive the stop,
      // and counting them as gone is precisely the bug this round is about.
      assertTrue(
        "only the uncancellable resolver calls may survive a stop, saw ${proxyThreads()}",
        awaitCondition(5_000) { proxyThreads().all { it == "AnimeHubProxyResolve" } },
      )
      assertEquals(
        "one surviving resolver thread per call still inside the platform",
        resolver.calls.get(),
        proxyThreads().count { it == "AnimeHubProxyResolve" },
      )
      assertTrue(
        "the cap must hold during a stop race, saw ${resolverThreads() - baseline}",
        (resolverThreads() - baseline).coerceAtLeast(0) <= 4,
      )
      // With the stuck calls released and the lifecycle over, the object can be
      // started again: nothing was leaked into the budget.
      resolver.release.set(true)
      assertTrue(
        "the abandoned lookup must free its thread, saw ${proxyThreads()}",
        awaitCondition(5_000) { proxyThreads().isEmpty() && resolverThreads() <= baseline },
      )
      assertTrue("a fresh start must bind", proxy.start() > 0)
    } finally {
      resolver.release.set(true)
      proxy.stop()
    }
  }

  @Test
  fun a_restart_cannot_spend_the_resolver_budget_twice() {
    // `stop()` cannot end a call that ignores the interrupt. If clearing the
    // generation list made `start()` forget those threads, each restart would
    // mint a fresh generation and the "cap" would only bound a single
    // lifecycle. The budget therefore spans lifecycles, and a start that cannot
    // get a generation must not bind at all.
    val resolver = UninterruptibleResolver()
    val baseline = resolverThreads()
    val proxy = DnsPinnedProxy(
      limits = DnsPinnedProxy.Limits(resolveMs = 150, resolveThreads = 1, resolveThreadCap = 2),
      resolver = resolver::resolve,
      connector = { _, _, _ -> throw java.io.IOException("must not be called") },
    )
    val firstPort = proxy.start()
    try {
      val client = Socket("127.0.0.1", firstPort)
      send(client, "CONNECT l1.example:443 HTTP/1.1\r\n\r\n")
      assertEquals("HTTP/1.1 502 Bad Gateway", statusLine(client))
      client.close()
      assertEquals(1, resolver.calls.get())

      proxy.stop()
      // The stuck call outlives the stop: one thread, still inside the platform
      // lookup. Clearing this from the accounting is exactly the bug.
      assertTrue(
        "stop() must not be able to reclaim an uncancellable call, saw ${resolverThreadsInsideLookup()}",
        awaitCondition { resolverThreadsInsideLookup() == 1 },
      )

      // One generation still fits (1 stuck + 1 = cap), so this restart works.
      val secondPort = proxy.start()
      val second = Socket("127.0.0.1", secondPort)
      send(second, "CONNECT l2.example:443 HTTP/1.1\r\n\r\n")
      assertEquals("HTTP/1.1 502 Bad Gateway", statusLine(second))
      second.close()
      assertEquals(2, resolver.calls.get())
      assertTrue(
        "both lifecycles' calls must still be running, saw ${resolverThreadsInsideLookup()}",
        awaitCondition { resolverThreadsInsideLookup() == 2 },
      )
      assertTrue(
        "the cap must include the previous lifecycle, saw ${resolverThreads() - baseline}",
        (resolverThreads() - baseline).coerceAtLeast(0) <= 2,
      )

      proxy.stop()
      // Both threads are still owned by calls that have not returned, so the
      // budget is spent. A third start must say so instead of binding a
      // listener that can only answer 502.
      var failure: Exception? = null
      try {
        proxy.start()
      } catch (e: Exception) {
        failure = e
      }
      assertTrue("a start without a resolver thread must fail, got $failure", failure is java.io.IOException)
      assertNull("a refused start must not leave a listener behind", proxy.port)
      assertEquals(
        "a refused start must not start another lookup",
        2,
        resolverThreadsInsideLookup(),
      )
      assertTrue(
        "a refused start must not add threads, saw ${resolverThreads() - baseline}",
        (resolverThreads() - baseline).coerceAtLeast(0) <= 2,
      )

      // When the platform calls finally return, the budget frees and a normal
      // start works again: the refusal was about real threads, not bookkeeping.
      resolver.release.set(true)
      assertTrue(
        "the abandoned calls must drain",
        awaitCondition(3_000) { resolverThreads() <= baseline },
      )
      val thirdPort = proxy.start()
      assertTrue("a start must work once the threads are gone", thirdPort > 0)
      proxy.stop()
    } finally {
      resolver.release.set(true)
      proxy.stop()
    }
  }

  @Test
  fun the_budget_counts_real_threads_not_the_capacity_of_a_dead_generation() {
    // Two threads per generation, cap four. Each lifecycle leaves exactly one
    // call behind, so the third start still fits (1 + 1 + 2 = 4). This fails if
    // the accounting charges a shut-down generation its full capacity instead
    // of the calls that are actually still running, and it fails if `stop()`
    // forgets the generations altogether (a fourth start would then succeed
    // while three stuck calls are alive).
    val resolver = UninterruptibleResolver()
    val baseline = resolverThreads()
    val proxy = DnsPinnedProxy(
      limits = DnsPinnedProxy.Limits(resolveMs = 120, resolveThreads = 2, resolveThreadCap = 4),
      resolver = resolver::resolve,
      connector = { _, _, _ -> throw java.io.IOException("must not be called") },
    )
    fun wedgeOnce(label: String) {
      val port = proxy.start()
      val client = Socket("127.0.0.1", port)
      send(client, "CONNECT $label.example:443 HTTP/1.1\r\n\r\n")
      assertEquals("HTTP/1.1 502 Bad Gateway", statusLine(client))
      client.close()
    }
    try {
      wedgeOnce("first")
      proxy.stop()
      wedgeOnce("second")
      proxy.stop()
      // 1 + 1 stuck + one full generation (2) = 4: exactly the cap.
      wedgeOnce("third")
      assertEquals(3, resolver.calls.get())
      assertTrue(
        "the cap must hold across lifecycles, saw ${resolverThreads() - baseline}",
        (resolverThreads() - baseline).coerceAtLeast(0) <= 4,
      )
      proxy.stop()
      // 1 + 1 + 1 stuck leaves room for 1 more generation? No: 3 + 2 > 4.
      var failure: Exception? = null
      try {
        proxy.start()
      } catch (e: Exception) {
        failure = e
      }
      assertTrue("the fourth start must be refused, got $failure", failure is java.io.IOException)
      assertNull("a refused start must not leave a listener behind", proxy.port)
    } finally {
      resolver.release.set(true)
      proxy.stop()
      assertTrue(
        "resolver threads must drain, saw ${resolverThreads() - baseline}",
        awaitCondition(3_000) { resolverThreads() <= baseline },
      )
    }
  }

  @Test
  fun parallel_wedges_cannot_create_more_generations_than_the_cap_allows() {
    // Eight clients hit a resolver that never returns at the same moment. Two
    // generations of two threads are all the cap allows; the rotation lock and
    // the budget check are what stop the parallel handlers from each minting
    // their own spare generation.
    val resolver = UninterruptibleResolver()
    val baseline = resolverThreads()
    val proxy = DnsPinnedProxy(
      limits = DnsPinnedProxy.Limits(resolveMs = 150, resolveThreads = 2, resolveThreadCap = 4),
      resolver = resolver::resolve,
      connector = { _, _, _ -> throw java.io.IOException("must not be called") },
    )
    val port = proxy.start()
    val start = java.util.concurrent.CountDownLatch(1)
    val clients = (0 until 8).map { index ->
      Thread {
        try {
          start.await()
          val client = Socket("127.0.0.1", port)
          client.soTimeout = 5_000
          send(client, "CONNECT parallel$index.example:443 HTTP/1.1\r\n\r\n")
          statusLine(client)
          client.close()
        } catch (_: Exception) {
          // A refusal is the expected answer for most of them.
        }
      }.apply { isDaemon = true; start() }
    }
    try {
      start.countDown()
      clients.forEach { it.join(10_000) }
      assertTrue(
        "no more lookups than the cap allows may start, saw ${resolver.calls.get()}",
        resolver.calls.get() <= 4,
      )
      assertTrue(
        "the thread count must never pass the cap, saw ${resolverThreads() - baseline}",
        (resolverThreads() - baseline).coerceAtLeast(0) <= 4,
      )
      assertTrue(
        "some connections must have been refused, saw ${proxy.refusalCount(DnsPinnedProxy.Refusal.RESOLVE_EXHAUSTED)}",
        proxy.refusalCount(DnsPinnedProxy.Refusal.RESOLVE_EXHAUSTED) >= 1L,
      )
      assertEquals(
        "every refusal must be a resolver refusal, not a tunnel attempt",
        0L,
        proxy.refusalCount(DnsPinnedProxy.Refusal.CONNECT_FAILED),
      )
    } finally {
      resolver.release.set(true)
      proxy.stop()
      assertTrue(
        "resolver threads must drain, saw ${resolverThreads() - baseline}",
        awaitCondition(3_000) { resolverThreads() <= baseline },
      )
    }
  }

  @Test
  fun a_cap_that_is_not_a_whole_number_of_generations_leaves_the_remainder_unused() {
    // 4 threads per generation, cap 10: exactly two generations can exist (8
    // threads). The two spare units are not turned into a half generation,
    // because a generation is the unit that can actually run a lookup.
    val resolver = UninterruptibleResolver()
    val baseline = resolverThreads()
    val proxy = DnsPinnedProxy(
      limits = DnsPinnedProxy.Limits(resolveMs = 120, resolveThreads = 4, resolveThreadCap = 10),
      resolver = resolver::resolve,
      connector = { _, _, _ -> throw java.io.IOException("must not be called") },
    )
    val port = proxy.start()
    try {
      repeat(8) {
        val client = Socket("127.0.0.1", port)
        send(client, "CONNECT wedge$it.example:443 HTTP/1.1\r\n\r\n")
        assertEquals("HTTP/1.1 502 Bad Gateway", statusLine(client))
        client.close()
      }
      assertEquals("two full generations may be created", 8, resolver.calls.get())
      assertTrue(
        "the thread count must never pass the cap, saw ${resolverThreads() - baseline}",
        (resolverThreads() - baseline).coerceAtLeast(0) <= 8,
      )
      val client = Socket("127.0.0.1", port)
      send(client, "CONNECT ninth.example:443 HTTP/1.1\r\n\r\n")
      assertEquals("HTTP/1.1 502 Bad Gateway", statusLine(client))
      client.close()
      assertEquals("the ninth lookup must not start", 8, resolver.calls.get())
      assertEquals(1L, proxy.refusalCount(DnsPinnedProxy.Refusal.RESOLVE_EXHAUSTED))
    } finally {
      resolver.release.set(true)
      proxy.stop()
      assertTrue(
        "resolver threads must drain, saw ${resolverThreads() - baseline}",
        awaitCondition(3_000) { resolverThreads() <= baseline },
      )
    }
  }

  @Test
  fun a_late_answer_after_a_timeout_is_never_classified_or_reused() {
    // The first lookup times out and then answers LOOPBACK. If that late answer
    // were cached or reused, the second CONNECT would be a NON_PUBLIC refusal
    // (or worse, a tunnel to 127.0.0.1). It must resolve again and use the
    // second, public answer.
    val release = java.util.concurrent.CountDownLatch(1)
    var calls = 0
    val proxy = DnsPinnedProxy(
      limits = DnsPinnedProxy.Limits(resolveMs = 200),
      resolver = { _ ->
        calls++
        if (calls == 1) {
          while (release.count > 0L) {
            try {
              release.await()
            } catch (_: InterruptedException) {
              // Not interruptible: models the platform call.
            }
          }
          listOf(address(127, 0, 0, 1))
        } else {
          listOf(publicA)
        }
      },
      connector = { _, _, _ -> throw java.io.IOException("must not be called") },
    )
    val port = proxy.start()
    try {
      val first = Socket("127.0.0.1", port)
      send(first, "CONNECT late.example:443 HTTP/1.1\r\n\r\n")
      assertEquals("HTTP/1.1 502 Bad Gateway", statusLine(first))
      first.close()
      assertEquals(1, calls)

      // The stale lookup now completes with a loopback address.
      release.countDown()
      assertTrue(awaitCondition(3_000) { calls == 1 })

      val second = Socket("127.0.0.1", port)
      send(second, "CONNECT late.example:443 HTTP/1.1\r\n\r\n")
      assertEquals("HTTP/1.1 502 Bad Gateway", statusLine(second))
      second.close()

      assertEquals("the name must be resolved again", 2, calls)
      assertEquals(2L, proxy.resolutionCount())
      assertEquals(
        "a timed-out answer must never be classified",
        0L,
        proxy.refusalCount(DnsPinnedProxy.Refusal.NON_PUBLIC),
      )
      // publicA was classified and dialled, so the failure is the connector.
      assertEquals(1L, proxy.refusalCount(DnsPinnedProxy.Refusal.CONNECT_FAILED))
    } finally {
      proxy.stop()
    }
  }

  @Test
  fun a_failed_resolution_is_not_remembered_as_a_verdict() {
    // First attempt fails, the second answer for the SAME name is loopback.
    // A cache of "this host was allowed/failed" would either let the second
    // connection through or refuse it without asking again; both would hide a
    // rebinding answer behind a stale decision.
    var calls = 0
    val proxy = DnsPinnedProxy(
      resolver = { _ ->
        calls++
        if (calls == 1) throw java.io.IOException("resolver down") else listOf(address(127, 0, 0, 1))
      },
      connector = { _, _, _ -> throw java.io.IOException("must not be called") },
    )
    val port = proxy.start()
    try {
      val first = Socket("127.0.0.1", port)
      send(first, "CONNECT again.example:443 HTTP/1.1\r\n\r\n")
      assertEquals("HTTP/1.1 502 Bad Gateway", statusLine(first))
      first.close()

      val second = Socket("127.0.0.1", port)
      send(second, "CONNECT again.example:443 HTTP/1.1\r\n\r\n")
      assertEquals("HTTP/1.1 403 Forbidden", statusLine(second))
      second.close()

      assertEquals("the name must be resolved again", 2, calls)
      assertEquals(1L, proxy.refusalCount(DnsPinnedProxy.Refusal.RESOLVE_FAILED))
      assertEquals(1L, proxy.refusalCount(DnsPinnedProxy.Refusal.NON_PUBLIC))
      // One resolution attempt per connection, never a verdict carried over.
      assertEquals(2L, proxy.resolutionCount())
    } finally {
      proxy.stop()
    }
  }

  @Test
  fun stop_closes_live_handshakes_and_tunnels() {
    val origin = EchoOrigin()
    val proxy = DnsPinnedProxy(
      limits = DnsPinnedProxy.Limits(idleMs = 120_000),
      resolver = { listOf(publicA) },
      connector = { address, _, _ ->
        PinnedFakeSocket(
          Socket().apply { connect(InetSocketAddress(InetAddress.getByName("127.0.0.1"), origin.port), 2_000) },
          address,
        )
      },
    )
    val port = proxy.start()
    // One established tunnel and one client parked mid-handshake.
    val tunnelled = Socket("127.0.0.1", port)
    send(tunnelled, "CONNECT live.example:443 HTTP/1.1\r\n\r\n")
    assertEquals("HTTP/1.1 200 Connection Established", statusLine(tunnelled))
    assertTrue(awaitCondition { origin.accepted.isNotEmpty() })
    val silent = Socket("127.0.0.1", port)
    silent.soTimeout = 5_000
    assertTrue("the proxy must have accepted the silent client", awaitCondition { proxy.activeConnections() == 2 })

    proxy.stop()

    // `shutdownNow()` interrupts threads but not a blocked `read()`: the
    // sockets must actually be closed. EOF on the page side is the proof.
    tunnelled.soTimeout = 3_000
    silent.soTimeout = 3_000
    assertTrue("the tunnel must end", tunnelled.getInputStream().read() < 0)
    assertTrue("the parked handshake must end", silent.getInputStream().read() < 0)
    assertTrue(
      "the upstream socket must be closed too",
      awaitCondition(3_000) { origin.ended.isNotEmpty() },
    )
    assertTrue(
      "every handler slot must be released",
      awaitCondition(3_000) { proxy.activeConnections() == 0 },
    )
    tunnelled.close()
    silent.close()
    origin.close()
  }

  @Test
  fun another_app_cannot_use_the_proxy_to_reach_local_or_private_services() {
    // The listener is on 127.0.0.1, so any app on the device that finds the
    // port can talk to it (documented residual: see
    // docs/security-residual-risks.md). What it must NOT become is a pivot
    // into the device or the user's LAN — every one of these is refused
    // before a socket is opened.
    var dialled = 0
    val proxy = DnsPinnedProxy(
      resolver = { host ->
        when (host) {
          "router.example" -> listOf(address(192, 168, 1, 1))
          "meta.example" -> listOf(InetAddress.getByName("169.254.169.254"))
          else -> emptyList()
        }
      },
      connector = { _, _, _ -> dialled++; throw java.io.IOException("must not be called") },
    )
    val port = proxy.start()
    try {
      for (target in listOf("127.0.0.1:22", "[::1]:443", "10.0.0.5:80", "router.example:443", "meta.example:80")) {
        val client = Socket("127.0.0.1", port)
        send(client, "CONNECT $target HTTP/1.1\r\n\r\n")
        assertEquals("$target must be refused", "HTTP/1.1 403 Forbidden", statusLine(client))
        client.close()
      }
      assertEquals("no socket may be opened for a non-public target", 0, dialled)
      assertEquals(5L, proxy.refusalCount(DnsPinnedProxy.Refusal.NON_PUBLIC))
    } finally {
      proxy.stop()
    }
  }

  @Test
  fun an_idle_handshake_flood_is_bounded_by_the_handshake_timeout() {
    val origin = EchoOrigin()
    val proxy = DnsPinnedProxy(
      limits = DnsPinnedProxy.Limits(maxConnections = 2, handshakeMs = 300),
      resolver = { listOf(publicA) },
      connector = { address, _, _ ->
        PinnedFakeSocket(
          Socket().apply { connect(InetSocketAddress(InetAddress.getByName("127.0.0.1"), origin.port), 2_000) },
          address,
        )
      },
    )
    val port = proxy.start()
    val parked = mutableListOf<Socket>()
    try {
      repeat(2) { parked.add(Socket("127.0.0.1", port)) }
      assertTrue(awaitCondition { proxy.activeConnections() == 2 })

      // While both slots are held by clients that never speak, a real request
      // is refused outright (503) rather than queued behind them.
      val blocked = Socket("127.0.0.1", port)
      blocked.soTimeout = 3_000
      send(blocked, "CONNECT site.example:443 HTTP/1.1\r\n\r\n")
      assertEquals("HTTP/1.1 503 Service Unavailable", statusLine(blocked))
      blocked.close()

      // The hold is bounded by the handshake deadline, so the page recovers on
      // its own: no state to clear, no restart needed.
      assertTrue(
        "the parked handshakes must expire",
        awaitCondition(3_000) { proxy.activeConnections() == 0 },
      )
      val client = Socket("127.0.0.1", port)
      send(client, "CONNECT site.example:443 HTTP/1.1\r\n\r\n")
      assertEquals("HTTP/1.1 200 Connection Established", statusLine(client))
      client.close()
    } finally {
      parked.forEach { DnsPinnedProxy.closeQuietly(it) }
      proxy.stop()
      origin.close()
    }
  }

  private fun DnsPinnedProxy.canAccept(port: Int): Boolean = try {
    Socket().use { it.connect(InetSocketAddress("127.0.0.1", port), 500) }
    true
  } catch (_: Exception) {
    false
  }
}
