// SPDX-License-Identifier: MIT
//
// Safe diagnostic outcomes for transferring PiP ownership between WebViews.
// Kept Android-free so the production decision is exercised with fake actions.

package dev.animehub.app

/**
 * Converts PiP attach/launcher-transfer actions into safe, gated events. The
 * actions are injected so tests can exercise failure classification without an
 * Activity, a real BroadcastReceiver, or provider-specific behavior.
 */
object PipTransferFlow {
  /** Attach the PiP controller to the visible WebView; failures are non-fatal. */
  fun attach(operationId: String, action: () -> Unit): Boolean {
    val safeId = AnimeHubDebugLog.safeOperationId(operationId) ?: AnimeHubDebugLog.newOperationId()
    return try {
      action()
      AnimeHubDebugLog.record("pip", "pip_transfer", "attached", safeId)
      true
    } catch (_: Exception) {
      AnimeHubDebugLog.record(
        "pip", "pip_transfer", "failed", safeId, errorClass = "provider",
      )
      false
    }
  }

  /** Attach/focus the launcher while keeping close successful on PiP failure. */
  fun transferToLauncher(
    operationId: String,
    launcherAvailable: Boolean,
    attach: () -> Boolean,
    focus: () -> Boolean,
  ): Boolean {
    val safeId = AnimeHubDebugLog.safeOperationId(operationId) ?: AnimeHubDebugLog.newOperationId()
    val attached = if (launcherAvailable) {
      try {
        attach()
      } catch (_: Exception) {
        false
      }
    } else {
      false
    }
    val focusFailed = try {
      !focus()
    } catch (_: Exception) {
      true
    }
    val transferred = launcherAvailable && attached && !focusFailed
    AnimeHubDebugLog.record(
      "pip",
      "transfer_to_launcher",
      if (transferred) "ok" else "failed",
      safeId,
      errorClass = if (transferred) null else if (!launcherAvailable) "activity_gone" else "provider",
    )
    return transferred
  }
}
