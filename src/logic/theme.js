/**
 * Launcher theme preference: system / dark / light.
 *
 * Kept dependency-free and DOM-agnostic so `node --test` can cover the
 * logic. `main.js` owns the listeners and the backend round-trip; this
 * module only decides what a stored value *means*.
 */

export const THEMES = Object.freeze(["system", "dark", "light"]);

/** localStorage mirror so the first paint matches before the backend answers. */
export const THEME_CACHE_KEY = "animehub-theme";

/**
 * Coerce any persisted value into a valid preference.
 * Unknown, missing or legacy values degrade to "system".
 */
export function normalizeTheme(value) {
  return THEMES.includes(value) ? value : "system";
}

/**
 * The value written to `<html data-theme>`. "system" is represented by
 * *removing* the attribute so the `prefers-color-scheme` media query in
 * styles.css keeps driving — that is also what makes live OS theme changes
 * apply without any JavaScript.
 */
export function attributeFor(theme) {
  const t = normalizeTheme(theme);
  return t === "system" ? null : t;
}

/** Effective rendering scheme for a preference. Pure; media arrives mocked in tests. */
export function resolveTheme(theme, prefersDark) {
  const t = normalizeTheme(theme);
  if (t === "system") return prefersDark ? "dark" : "light";
  return t;
}

/** Human-readable label for the three-switch control. */
export function themeLabel(theme) {
  switch (normalizeTheme(theme)) {
    case "dark":
      return "Koyu";
    case "light":
      return "Açık";
    default:
      return "Sistem";
  }
}

/**
 * Apply the preference to the document root. Kept side-effecty but tiny;
 * the callers pass a real document.
 */
export function applyThemeToDom(theme, doc = document) {
  const attr = attributeFor(theme);
  if (attr === null) doc.documentElement.removeAttribute("data-theme");
  else doc.documentElement.setAttribute("data-theme", attr);
  return attr;
}
