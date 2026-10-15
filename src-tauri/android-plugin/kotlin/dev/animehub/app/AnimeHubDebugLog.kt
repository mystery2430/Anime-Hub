// SPDX-License-Identifier: MIT
//
// In-app diagnostics buffer shared by the plugin, PiP and site controllers.
// Extracted from AnimeHubPlugin.kt (pure JVM code, no Tauri dependencies) so
// the CI unit-test source set can compile every file that logs.

package dev.animehub.app

import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

/**
 * In-app diagnostics for the launcher's developer panel, read through the
 * `debug_log` plugin command. Holds the last 200 lines of status text only:
 * a short area tag, the outcome, and error codes. Never site URLs or hosts,
 * and never message bodies or cookie values.
 */
object AnimeHubDebugLog {
  private const val MAX_LINES = 200
  private val lines = ArrayDeque<String>()
  private val stamp = SimpleDateFormat("HH:mm:ss.SSS", Locale.ROOT)

  @Synchronized
  fun add(area: String, message: String) {
    if (lines.size >= MAX_LINES) lines.removeFirst()
    lines.addLast(stamp.format(Date()) + " [" + area + "] " + message)
  }

  @Synchronized
  fun snapshot(): List<String> = lines.toList()
}
