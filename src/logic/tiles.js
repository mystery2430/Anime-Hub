/**
 * Pure view-model logic for the launcher grid.
 *
 * Nothing in this module touches the DOM, which is what makes it unit-testable
 * in Node and — more importantly — makes the rendering contract explicit:
 * the renderer receives only validated, plain-string data.
 */

/** Launcher groups, in display order. */
export const CATEGORIES = [
  { id: "tracking", title: "Takip / Veritabanı" },
  { id: "watching", title: "İzleme" },
];

const FALLBACK_COLOR = "#0ea5e9";

/**
 * Shipped logos. The path is an allowlist, not a prefix check: a stored
 * `../` or `https://` value must never become an `img` source.
 */
const BUNDLED_LOGOS = {
  "logos/openanime.png": "/logos/openanime.png",
  "logos/animecix.png": "/logos/animecix.png",
};

/** Largest data URL the tile renderer will accept. Matches the backend byte cap. */
const DATA_IMAGE_MAX = 280_000;

/**
 * Validate a `#rgb` / `#rrggbb` colour before it is used anywhere.
 *
 * The colour ends up in an inline `style.background`, which is a real
 * injection surface if taken raw from a stored site entry. Returning `null`
 * (rather than the input) on failure means a bad value can never reach the
 * DOM at all.
 */
export function safeColor(value) {
  if (typeof value !== "string") return null;
  const hex = value.trim().replace(/^#/, "");
  if (!/^([0-9a-fA-F]{3}|[0-9a-fA-F]{6})$/.test(hex)) return null;
  return `#${hex.toLowerCase()}`;
}

/**
 * Shorten a name for the icon disc.
 *
 * Two rules, in order:
 * 1. if the text is two or more *real* words (each starting with a letter or
 *    digit), contribute the initial of the first two, so "My Site" reads as
 *    "MS" rather than "MY";
 * 2. otherwise contribute the first two characters, so "OpenAnime" reads as
 *    "OP" and `<img src=x>` reads as "IM" — whitespace inside a
 *    punctuation-led string is not word separation.
 *
 * An `explicit` value that holds no letters or digits at all is ignored in
 * favour of the name, so a stray "!!" cannot blank the disc. Everything that
 * is not a letter or digit is dropped, so the disc never overflows and never
 * carries markup.
 */
/**
 * A tile image the renderer may assign to `img.src`.
 *
 * Bundled logos must be on the allowlist. User photos must be a PNG, JPEG or
 * WebP data URL. Remote URLs are rejected: fetching one would tell that host
 * which tiles this install has.
 */
export function safeTileImage(icon) {
  if (!icon || typeof icon !== "object") return null;
  if (icon.kind === "bundled") {
    return BUNDLED_LOGOS[String(icon.path || "")] || null;
  }
  if (icon.kind === "image") {
    return safeDataImage(icon.data);
  }
  return null;
}

/** Canonical `data:image/(png|jpeg|webp);base64,...` or null. */
export function safeDataImage(value) {
  if (typeof value !== "string" || value.length === 0 || value.length > DATA_IMAGE_MAX) {
    return null;
  }
  if (!/^data:image\/(png|jpeg|webp);base64,[A-Za-z0-9+/]+={0,2}$/.test(value)) {
    return null;
  }
  return value;
}

export function iconLetters(name, explicit) {
  const strip = (v) => String(v ?? "").replace(/[^\p{L}\p{N}]/gu, "");

  const explicitClean = strip(explicit);
  const source = explicitClean === "" ? String(name ?? "") : String(explicit ?? "").trim();

  const words = source.split(/\s+/).filter(Boolean);
  const startsWord = /^\p{L}|^\p{N}/u;

  let letters;
  if (words.length >= 2 && words.slice(0, 2).every((w) => startsWord.test(w))) {
    letters = words
      .slice(0, 2)
      .map((w) => [...w][0])
      .join("");
  } else {
    letters = [...strip(source)].slice(0, 2).join("");
  }

  return (letters || "?").toUpperCase();
}

/**
 * Build the tile view-model for one site.
 *
 * Every field is a validated string: the renderer assigns them with
 * `textContent` / `style.background` and never with `innerHTML`.
 */
export function tileModel(site) {
  const icon = site.icon && site.icon.kind === "letter" ? site.icon : {};
  const name = String(site.name || "").trim() || "(isimsiz)";
  return {
    id: String(site.id),
    name,
    url: String(site.url || ""),
    host: String(site.host || ""),
    category: site.category === "tracking" ? "tracking" : "watching",
    letters: iconLetters(name, icon.text),
    color: safeColor(icon.color) || FALLBACK_COLOR,
    image: safeTileImage(site.icon),
    builtin: Boolean(site.builtin),
    native: Boolean(site.native),
  };
}

/**
 * Group tiles by category, preserving the order the backend returned.
 *
 * Always returns an entry for every known category so the UI can render a
 * stable empty state instead of a missing section.
 */
export function groupTiles(sites) {
  const models = sites.map(tileModel);
  const out = new Map(CATEGORIES.map((c) => [c.id, []]));
  for (const tile of models) {
    const bucket = out.get(tile.category);
    if (bucket) bucket.push(tile);
  }
  return CATEGORIES.map((c) => ({
    id: c.id,
    title: c.title,
    tiles: out.get(c.id) || [],
  }));
}

/**
 * Client-side mirror of the Rust URL policy.
 *
 * Defence in depth only: the backend re-validates and is the authority. This
 * exists so the user gets an error at the field instead of after a round trip.
 */
export function validateSiteUrl(raw) {
  if (typeof raw !== "string" || raw.trim() === "") {
    return { ok: false, code: "invalid_url", message: "Bir adres girin." };
  }
  const trimmed = raw.trim();
  if (trimmed.length > 2048) {
    return { ok: false, code: "invalid_url", message: "Adres çok uzun." };
  }

  let url;
  try {
    url = new URL(trimmed);
  } catch {
    return { ok: false, code: "invalid_url", message: "Adres çözümlenemedi." };
  }

  if (url.protocol === "http:") {
    return { ok: false, code: "insecure_scheme", message: "Yalnızca HTTPS adresler kabul edilir." };
  }
  if (url.protocol !== "https:") {
    return { ok: false, code: "invalid_url", message: "Adres https:// ile başlamalı." };
  }

  const host = url.hostname.toLowerCase();
  if (!host) {
    return { ok: false, code: "invalid_url", message: "Adresin bir alan adı yok." };
  }
  if (url.username || url.password) {
    return { ok: false, code: "invalid_url", message: "Adrese kullanıcı adı/parola gömülemez." };
  }
  if (isPrivateHost(host)) {
    return { ok: false, code: "invalid_url", message: "Yerel/ağ içi adresler kabul edilmez." };
  }

  return { ok: true, normalized: url.toString() };
}

/** Loopback / private / link-local, mirroring `url_policy::is_private_host`. */
export function isPrivateHost(host) {
  const h = host.replace(/\.$/, "");
  if (h === "localhost" || h.endsWith(".localhost")) return true;
  if (h.endsWith(".local") || h.endsWith(".internal") || h.endsWith(".lan") || h.endsWith(".home")) {
    return true;
  }
  if (h === "tauri.localhost" || h === "asset.localhost") return true;

  const v4 = h.match(/^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})$/);
  if (v4) {
    const [a, b] = [Number(v4[1]), Number(v4[2])];
    if (a === 127 || a === 10 || a === 0) return true;
    if (a === 192 && b === 168) return true;
    if (a === 172 && b >= 16 && b <= 31) return true;
    if (a === 169 && b === 254) return true; // link-local / cloud metadata
    if (a === 100 && b >= 64 && b <= 127) return true; // CGNAT
    if (a >= 224) return true; // multicast + reserved
    return false;
  }

  // IPv6 literals arrive bracketed from `URL#hostname`.
  const v6 = h.replace(/^\[|\]$/g, "");
  if (v6.includes(":")) {
    const lower = v6.toLowerCase();
    if (lower === "::1" || lower === "::") return true;
    if (lower.startsWith("fe8") || lower.startsWith("fe9") || lower.startsWith("fea") || lower.startsWith("feb")) {
      return true; // fe80::/10
    }
    if (lower.startsWith("fc") || lower.startsWith("fd")) return true; // fc00::/7
    return false;
  }

  return false;
}

/**
 * Validate the add/edit form.
 *
 * Returns `{ ok: true, draft }` or `{ ok: false, field, message }`, where
 * `field` names the input to focus.
 */
export function validateDraft(input) {
  const name = String(input.name || "").trim();
  if (name === "") {
    return { ok: false, field: "name", message: "Bir isim girin." };
  }
  if (name.length > 40) {
    return { ok: false, field: "name", message: "İsim en fazla 40 karakter olabilir." };
  }

  const urlCheck = validateSiteUrl(input.url);
  if (!urlCheck.ok) {
    return { ok: false, field: "url", message: urlCheck.message };
  }

  const letter = String(input.letter || "").trim();
  if (letter && [...letter.replace(/[^\p{L}\p{N}]/gu, "")].length === 0) {
    return { ok: false, field: "letter", message: "Kısaltma harf veya rakam içermeli." };
  }

  const color = String(input.color || "").trim();
  if (color && !safeColor(color)) {
    return { ok: false, field: "color", message: "Renk #rrggbb biçiminde olmalı." };
  }

  const image = imageForDraft(input.image);
  if (image === false) {
    return {
      ok: false,
      field: "photo",
      message: "Fotoğraf PNG, JPEG veya WebP olmalı.",
    };
  }

  return {
    ok: true,
    draft: {
      name,
      url: urlCheck.normalized,
      category: input.category === "tracking" ? "tracking" : "watching",
      letter: letter || null,
      color: color ? safeColor(color) : null,
      image,
    },
  };
}

/**
 * `undefined` / `null` means "leave the stored photo alone".
 * `""` clears it. A data URL replaces it. Anything else is `false`.
 */
function imageForDraft(image) {
  if (image == null) return null;
  if (image === "") return "";
  return safeDataImage(image) || false;
}
