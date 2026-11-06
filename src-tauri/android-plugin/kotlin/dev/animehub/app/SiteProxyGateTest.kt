// SPDX-License-Identifier: MIT
//
// The gate that decides whether a site may open at all.
//
// These are the states a provider can put us in (installing, ready,
// unsupported, wedged callback), driven directly — no WebView, no device.
package dev.animehub.app

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class SiteProxyGateTest {

  @Test
  fun nothing_may_open_before_the_install_is_confirmed() {
    val gate = SiteProxyGate()
    assertEquals(SiteProxyGate.State.NOT_STARTED, gate.state)
    assertFalse("a site must not open unpinned", gate.canOpen())
    assertEquals(SiteProxyGate.WAIT, gate.openDecision())
  }

  @Test
  fun ready_is_the_only_state_that_allows_an_open() {
    val gate = SiteProxyGate()
    assertTrue(gate.begin(now = 1_000))
    assertEquals(SiteProxyGate.State.INSTALLING, gate.state)
    assertEquals("still installing, so still refusing", SiteProxyGate.WAIT, gate.openDecision())
    assertFalse(gate.canOpen())

    gate.markReady()
    assertEquals(SiteProxyGate.State.READY, gate.state)
    assertTrue(gate.canOpen())
    assertNull("ready means no refusal", gate.openDecision())
  }

  @Test
  fun an_unsupported_provider_refuses_instead_of_opening_directly() {
    val gate = SiteProxyGate()
    gate.begin(now = 0)
    gate.markUnsupported()
    assertEquals(SiteProxyGate.State.UNSUPPORTED, gate.state)
    assertFalse(gate.canOpen())
    assertEquals(SiteProxyPolicy.ERR_PROXY_UNAVAILABLE, gate.openDecision())
  }

  @Test
  fun a_failed_install_refuses_and_a_later_success_cannot_flip_it() {
    val gate = SiteProxyGate()
    gate.begin(now = 0)
    gate.markFailed()
    assertEquals(SiteProxyGate.State.FAILED, gate.state)
    assertEquals(SiteProxyPolicy.ERR_PROXY_UNAVAILABLE, gate.openDecision())
    // A late callback (or a second attempt) must not turn a refusal into a
    // ready state behind the user's back.
    gate.markReady()
    assertEquals(SiteProxyGate.State.FAILED, gate.state)
    assertFalse(gate.canOpen())
    assertFalse("a terminal state is not retried", gate.begin(now = 10))
  }

  @Test
  fun only_one_caller_wins_the_install() {
    val gate = SiteProxyGate()
    assertTrue(gate.begin(now = 0))
    assertFalse("the second caller must wait, not reinstall", gate.begin(now = 1))
  }

  @Test
  fun a_callback_that_never_arrives_expires_and_refuses() {
    val gate = SiteProxyGate(deadlineMs = 5_000)
    gate.begin(now = 1_000)
    assertTrue(gate.isInstalling(now = 1_001))
    assertFalse("not before the deadline", gate.expireIfOverdue(now = 5_999))
   assertTrue("a wedged provider must not hold the open forever", gate.expireIfOverdue(now = 6_000))
    assertEquals(SiteProxyGate.State.FAILED, gate.state)
    assertEquals(SiteProxyPolicy.ERR_PROXY_UNAVAILABLE, gate.openDecision())
    assertFalse("expiry happens once", gate.expireIfOverdue(now = 60_000))
  }

  @Test
  fun waiters_are_notified_exactly_once_and_only_on_settle() {
    val gate = SiteProxyGate()
    val seen = mutableListOf<SiteProxyGate.State>()
    assertNull("registered, not settled", gate.observe { seen.add(it) })
    gate.begin(now = 0)
    assertTrue(seen.isEmpty())
    gate.markReady()
    assertEquals(listOf(SiteProxyGate.State.READY), seen)
    gate.markFailed()
    assertEquals("a settled gate notifies nobody again", listOf(SiteProxyGate.State.READY), seen)
  }

  @Test
  fun observing_a_settled_gate_answers_immediately() {
    val gate = SiteProxyGate()
    gate.begin(now = 0)
    gate.markReady()
    var called = false
    val settled = gate.observe { called = true }
    assertEquals(SiteProxyGate.State.READY, settled)
    assertFalse("nothing may be registered once settled", called)
  }

  @Test
  fun the_waiter_list_is_bounded_and_overflow_is_a_refusal() {
    val gate = SiteProxyGate()
    gate.begin(now = 0)
    repeat(SiteProxyGate.MAX_WAITERS) { assertNull(gate.observe { }) }
    // The next observer is not queued: it is told FAILED, which refuses.
    assertEquals(SiteProxyGate.State.FAILED, gate.observe { })
  }

  @Test
  fun the_proxy_rule_is_loopback_with_an_explicit_port() {
    assertEquals("127.0.0.1:41234", SiteProxyPolicy.proxyRule(41_234))
    assertNull(SiteProxyPolicy.proxyRule(null))
    assertNull(SiteProxyPolicy.proxyRule(0))
    assertNull(SiteProxyPolicy.proxyRule(-1))
    assertNull(SiteProxyPolicy.proxyRule(65_536))

    assertTrue(SiteProxyPolicy.isLoopbackRule("127.0.0.1:41234"))
    for (rule in listOf(
      "0.0.0.0:41234", // wildcard: reachable from the network
      "localhost:41234", // may resolve to ::1, which the proxy does not bind
      "example.com:41234", // not our proxy at all
      "127.0.0.1:", // no port
      "127.0.0.1:0", // never a real listener
      "127.0.0.1:65536",
      "DIRECT",
      "127.0.0.1:41234; DIRECT", // a fallback list
    )) {
      assertFalse("$rule must never be accepted as the proxy rule", SiteProxyPolicy.isLoopbackRule(rule))
    }
  }

  @Test
  fun the_refusal_sentinel_is_the_one_rust_classifies() {
    // The literal must stay byte-identical to
    // `animehub_android::PROXY_SENTINEL`; the Rust test pins the same pair and
    // asserts the Android adapter re-exports exactly this constant.
    assertEquals("proxy-unavailable", SiteProxyPolicy.ERR_PROXY_UNAVAILABLE)
  }
}
