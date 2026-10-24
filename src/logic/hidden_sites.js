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

function displayName(raw) {
  const cleaned = [...raw]
    .filter((char) => char.codePointAt(0) >= 0x20 && char.codePointAt(0) !== 0x7f)
    .slice(0, NAME_MAX)
    .join("")
    .trim();
  return cleaned.length > 0 ? cleaned : "Adsız site";
}
