// SPDX-License-Identifier: MIT
//
// The ONE place a TCP connection is opened for a proxied site request.
//
// `Socket(hostname, port)` would resolve the name a second time and hand the
// attack back: the proxy classifies one address, and the socket races to
// another. This object therefore only ever connects to an already-resolved
// [InetAddress] (`InetSocketAddress(address, port)` performs no lookup) and
// then verifies what the socket actually connected to.
//
// Pure JVM so the real socket path — including the verification — is exercised
// in CI against real sockets.
package dev.animehub.app

import java.io.IOException
import java.net.InetAddress
import java.net.InetSocketAddress
import java.net.Socket

object PinnedConnect {
  /** The socket did not end up where it was sent: never tunnel through it. */
  class AddressChangedException(message: String) : IOException(message)

  /**
   * Connect to [address]:[port] with no name resolution.
   *
   * @throws AddressChangedException when the socket's own remote address is not
   *   [address]. The socket is closed first; the caller must not use it.
   */
  fun connect(address: InetAddress, port: Int, timeoutMs: Int): Socket {
    val socket = Socket()
    try {
      // `InetSocketAddress(InetAddress, Int)` is the address, not a name: the
      // JVM cannot resolve here even if it wanted to.
      socket.tcpNoDelay = true
      socket.connect(InetSocketAddress(address, port), timeoutMs)
      verify(socket, address)
      return socket
    } catch (e: Throwable) {
      DnsPinnedProxy.closeQuietly(socket)
      throw e
    }
  }

  /**
   * Assert that [socket] is connected to [expected].
   *
   * Defence in depth: with `InetSocketAddress(address, …)` this cannot differ,
   * but the check is what makes the guarantee verifiable instead of implied —
   * a future edit that switched to a name-based overload would fail here
   * (loudly, and closed) instead of silently reopening the rebinding window.
   */
  fun verify(socket: Socket, expected: InetAddress) {
    val actual = socket.inetAddress
      ?: throw AddressChangedException("socket has no remote address")
    if (!actual.address.contentEquals(expected.address)) {
      DnsPinnedProxy.closeQuietly(socket)
      throw AddressChangedException("socket connected to an unclassified address")
    }
  }
}
