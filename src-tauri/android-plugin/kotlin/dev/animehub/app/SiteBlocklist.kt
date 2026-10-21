// SPDX-License-Identifier: MIT
//
// The user-editable ad/redirect blocklist, evaluated inside the Android site
// WebView. Rust owns the list and pushes a snapshot before every open; this
// file is pure (no android.* import) so the matching rules are unit-tested.
//
// MATCHING MUST STAY IDENTICAL to src-tauri/src/sites/blocklist.rs:
//   example.com    → the apex and every subdomain
//   *.example.com  → subdomains only (the apex stays allowed)
//   =example.com   → the apex only
//   # ...          → comment, ignored
// A naive `endsWith(rule)` is rejected by the shared tests because it would
// block `notdoubleclick.net` for the `doubleclick.net` rule.

package dev.animehub.app

import java.util.concurrent.atomic.AtomicReference

/** Immutable parsed rule set; [isBlocked] is the pure matcher. */
data class BlocklistRules(
  val suffixes: Set<String> = emptySet(),
  val wildcardOnly: Set<String> = emptySet(),
  val exact: Set<String> = emptySet(),
) {
  val size: Int get() = suffixes.size + wildcardOnly.size + exact.size

  fun isBlocked(rawHost: String?): Boolean {
    val host = normalizeHost(rawHost) ?: return false
    if (host in exact) return true
    if (host in suffixes) return true
    var dot = host.indexOf('.')
    while (dot >= 0) {
      val parent = host.substring(dot + 1)
      if (parent.isEmpty()) break
      if (parent in suffixes) return true
      if (parent in wildcardOnly) return true
      dot = host.indexOf('.', dot + 1)
    }
    return false
  }

  companion object {
    /** Lower-case, trim trailing dot, strip IPv6 brackets; null when empty. */
    fun normalizeHost(raw: String?): String? {
      if (raw == null) return null
      var host = raw.trim().lowercase().trimEnd('.')
      if (host.startsWith("[") && host.endsWith("]")) {
        host = host.substring(1, host.length - 1)
      }
      return host.ifEmpty { null }
    }

    fun parse(raw: List<String>): BlocklistRules {
      val suffixes = linkedSetOf<String>()
      val wildcard = linkedSetOf<String>()
      val exact = linkedSetOf<String>()
      for (line in raw) {
        val trimmed = line.trim()
        if (trimmed.isEmpty() || trimmed.startsWith("#")) continue
        val entry = trimmed.lowercase().trimEnd('.')
        when {
          entry.startsWith("*") -> {
            val rest = entry.substring(1).trimStart('.')
            if (rest.isNotEmpty()) wildcard += rest
          }
          entry.startsWith("=") -> {
            val rest = entry.substring(1)
            if (rest.isNotEmpty()) exact += rest
          }
          else -> suffixes += entry
        }
      }
      return BlocklistRules(suffixes.toSet(), wildcard.toSet(), exact.toSet())
    }
  }
}

/**
 * Process-wide snapshot. Rust replaces it before every open and after every
 * settings change or reset. The reference swap is atomic, so an already-open
 * WebView sees the new rules on its next checkpoint without being recreated.
 *
 * A transfer that fails before this object is reached is NOT represented as an
 * empty snapshot: Rust aborts the open / closes the active site and returns an
 * error. This object cannot observe the bridge failure itself, so it must never
 * be described as making a failed transfer safe.
 *
 * A DISABLED blocklist arrives as an empty snapshot: the rule file lives in
 * Rust, and this object deliberately has no separate `enabled` bit that could
 * disagree with the desktop matcher (`Blocklist::is_blocked` returns false
 * when disabled).
 */
object SiteBlocklist {
  private val current = AtomicReference(BlocklistRules())

  fun update(raw: List<String>) {
    current.set(BlocklistRules.parse(raw))
  }

  fun isBlocked(host: String?): Boolean = current.get().isBlocked(host)

  fun size(): Int = current.get().size

  internal fun resetForTests() {
    current.set(BlocklistRules())
  }
}
