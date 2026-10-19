// SPDX-License-Identifier: MIT
//
// Behavioral tests for PiP transfer diagnostics using injected native actions.

package dev.animehub.app

import org.junit.Assert
import org.junit.Before
import org.junit.Test

class PipTransferFlowTest {
  @Before
  fun resetDiagnostics() {
    AnimeHubDebugLog.resetForTests()
  }

  @Test
  fun `attach records a safe successful transfer`() {
    AnimeHubDebugLog.setEnabled(true)
    var attached = false

    val result = PipTransferFlow.attach("k-41") { attached = true }

    Assert.assertTrue(result)
    Assert.assertTrue(attached)
    Assert.assertTrue(AnimeHubDebugLog.snapshot().events.any {
      it.area == "pip" && it.phase == "pip_transfer" &&
        it.result == "attached" && it.operationId == "k-41"
    })
  }

  @Test
  fun `attach failure is classified and provider text is not logged`() {
    AnimeHubDebugLog.setEnabled(true)
    val privateFailure = "https://private.example/?token=SECRET; Cookie: value"

    val result = PipTransferFlow.attach("r-42") {
      throw IllegalStateException(privateFailure)
    }

    Assert.assertFalse(result)
    val snapshot = AnimeHubDebugLog.snapshot()
    Assert.assertTrue(snapshot.events.any {
      it.area == "pip" && it.phase == "pip_transfer" &&
        it.result == "failed" && it.errorClass == "provider" && it.operationId == "r-42"
    })
    val exported = snapshot.toString()
    Assert.assertFalse(exported.contains("private.example"))
    Assert.assertFalse(exported.contains("SECRET"))
    Assert.assertFalse(exported.contains("Cookie"))
  }

  @Test
  fun `launcher transfer still requests focus after attach failure`() {
    AnimeHubDebugLog.setEnabled(true)
    var focusRequested = false

    val result = PipTransferFlow.transferToLauncher(
      operationId = "r-43",
      launcherAvailable = true,
      attach = { false },
      focus = { focusRequested = true; true },
    )

    Assert.assertFalse(result)
    Assert.assertTrue("focus remains best-effort even if PiP attach fails", focusRequested)
    Assert.assertTrue(AnimeHubDebugLog.snapshot().events.any {
      it.area == "pip" && it.phase == "transfer_to_launcher" &&
        it.result == "failed" && it.errorClass == "provider" && it.operationId == "r-43"
    })
  }

  @Test
  fun `launcher focus refusal is a failed transfer even when attach succeeds`() {
    AnimeHubDebugLog.setEnabled(true)
    var attachRequested = false

    val result = PipTransferFlow.transferToLauncher(
      operationId = "r-44",
      launcherAvailable = true,
      attach = { attachRequested = true; true },
      focus = { false },
    )

    Assert.assertFalse(result)
    Assert.assertTrue(attachRequested)
    Assert.assertTrue(AnimeHubDebugLog.snapshot().events.any {
      it.area == "pip" && it.phase == "transfer_to_launcher" &&
        it.result == "failed" && it.errorClass == "provider" && it.operationId == "r-44"
    })
  }

  @Test
  fun `launcher focus exception is safely classified`() {
    AnimeHubDebugLog.setEnabled(true)
    val privateFailure = "https://private.example/?token=SECRET; Cookie: value"

    val result = PipTransferFlow.transferToLauncher(
      operationId = "r-45",
      launcherAvailable = true,
      attach = { true },
      focus = { throw IllegalStateException(privateFailure) },
    )

    Assert.assertFalse(result)
    val snapshot = AnimeHubDebugLog.snapshot()
    Assert.assertTrue(snapshot.events.any {
      it.area == "pip" && it.phase == "transfer_to_launcher" &&
        it.result == "failed" && it.errorClass == "provider" && it.operationId == "r-45"
    })
    Assert.assertFalse(snapshot.toString().contains("private.example"))
    Assert.assertFalse(snapshot.toString().contains("SECRET"))
    Assert.assertFalse(snapshot.toString().contains("Cookie"))
  }

  @Test
  fun `missing launcher is reported without invoking attach`() {
    AnimeHubDebugLog.setEnabled(true)
    var attachRequested = false

    val result = PipTransferFlow.transferToLauncher(
      operationId = "r-46",
      launcherAvailable = false,
      attach = { attachRequested = true; true },
      focus = { false },
    )

    Assert.assertFalse(result)
    Assert.assertFalse(attachRequested)
    Assert.assertTrue(AnimeHubDebugLog.snapshot().events.any {
      it.area == "pip" && it.phase == "transfer_to_launcher" &&
        it.result == "failed" && it.errorClass == "activity_gone" && it.operationId == "r-46"
    })
  }
}
