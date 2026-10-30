/**
 * Pure view-model logic for the "hidden sites" settings list.
 *
 * «Başlatıcıdan gizle» keeps the registry entry, its icon and its profile, so
 * it must be reversible. `list_sites` intentionally returns the visible grid
 * only, which used to make the action a one-way door: the hidden entry could
 * not be named again from the UI. This module decides what the restore list
 * shows, with no DOM access, so the rule is unit-tested in Node.
 */

/** Longest name the settings list will print, in code points. */
const NAME_MAX = 80;

/**
 * Rows for the restore list: hidden entries only, one plain string each.
 *
 * Anything malformed (a stored document can be older than this code) is
 * dropped rather than rendered, and the name is length-capped and stripped of
 * control characters so the row can never carry markup or break the layout.
 * Sorting compares the raw strings, so the order does not depend on the host
 * locale.
 */
export function hiddenSiteRows(sites) {
  if (!Array.isArray(sites)) return [];
  return sites
    .filter(
      (site) =>
        site &&
        site.hidden === true &&
        typeof site.id === "string" &&
        site.id.length > 0 &&
        typeof site.name === "string",
    )
    .map((site) => ({
      id: site.id,
      name: displayName(site.name),
      builtin: site.builtin === true,
    }))
    .sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
}

/** `true` when the restore list has nothing to show. */
export function hasNoHiddenSites(sites) {
  return hiddenSiteRows(sites).length === 0;
}

/**
 * Characters that may not appear in a rendered row.
 *
 * C0/DEL/C1 controls are invisible but break the layout; the Unicode
 * bidirectional controls are worse: U+202E (RIGHT-TO-LEFT OVERRIDE) reorders
 * everything after it, so a stored name such as `Kötü\u202E...` can render as
 * a different site's name next to the "Geri göster" button. The row is written
 * with `textContent` (no markup risk), but reordering is a spoofing risk the
 * restore list does not need to carry. Line/paragraph separators and the BOM
 * are stripped for the same layout reason.
 */
function isDisplayable(code) {
  if (code < 0x20 || code === 0x7f) return false; // C0, DEL
  if (code >= 0x80 && code <= 0x9f) return false; // C1
  if (code === 0x061c) return false; // ARABIC LETTER MARK
  if (code >= 0x200b && code <= 0x200f) return false; // zero-width + LRM/RLM
  if (code === 0x2028 || code === 0x2029) return false; // line/paragraph separator
  if (code >= 0x202a && code <= 0x202e) return false; // bidi embeddings/overrides
  if (code >= 0x2060 && code <= 0x2064) return false; // word joiner, invisible ops
  if (code >= 0x2066 && code <= 0x206f) return false; // bidi isolates + deprecated formats
  if (code === 0xfeff) return false; // BOM / zero-width no-break space
  return true;
}

function displayName(raw) {
  const cleaned = [...raw]
    .filter((char) => isDisplayable(char.codePointAt(0)))
    .slice(0, NAME_MAX)
    .join("")
    .trim();
  return cleaned.length > 0 ? cleaned : "Adsız site";
}
