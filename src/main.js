/**
 * Launcher UI.
 *
 * Rendering rule that the whole file follows: **no `innerHTML` anywhere**.
 * Site names, letters and hosts are user-controlled, so every dynamic value
 * goes through `textContent`, and colours are re-validated by `safeColor()`
 * before touching a style. See `logic/tiles.js` for the validation.
 */

import { call, on, ApiError, isTauri } from "./logic/bridge.js";
import { wrapFocusIndex } from "./logic/focus.js";
import {
  groupTiles,
  validateDraft,
  safeColor,
  safeDataImage,
  CATEGORIES,
} from "./logic/tiles.js";
import {
  THEMES,
  THEME_CACHE_KEY,
  normalizeTheme,
  applyThemeToDom,
  themeLabel,
} from "./logic/theme.js";
import { createThemeFlow } from "./logic/theme_flow.js";
import {
  heroModel,
  displayHost,
  bannerStyle,
  bannerRgb,
} from "./logic/hero.js";
import {
  bindSwitch,
  createSettingsFlow,
  switchStates,
} from "./logic/settings_flow.js";
import { createBackSession } from "./logic/back_close.js";
import { copyTextFallback } from "./logic/copy_text.js";
import { clearResultMessage, takeMigrationNotice } from "./logic/storage_scope.js";
import {
  sanitizeDebugLogSnapshot,
  serializeDebugLogSnapshot,
} from "./logic/debug_log.js";

const el = {
  gridTracking: document.getElementById("grid-tracking"),
  gridWatching: document.getElementById("grid-watching"),
  notice: document.getElementById("notice"),
  btnSettings: document.getElementById("btn-settings"),
  btnAbout: document.getElementById("btn-about"),
  modalSite: document.getElementById("modal-site"),
  modalSettings: document.getElementById("modal-settings"),
  modalAbout: document.getElementById("modal-about"),
  formSite: document.getElementById("form-site"),
  siteError: document.getElementById("site-error"),
  siteTitle: document.getElementById("site-modal-title"),
  letterRow: document.getElementById("letter-row"),
  photoField: document.getElementById("photo-field"),
  photoPreview: document.getElementById("photo-preview"),
  photoHint: document.getElementById("photo-hint"),
  btnPickPhoto: document.getElementById("btn-pick-photo"),
  btnClearPhoto: document.getElementById("btn-clear-photo"),
  sitePhoto: document.getElementById("site-photo"),
  dev: {
    panel: document.getElementById("dev-panel"),
    refresh: document.getElementById("dev-refresh"),
    copy: document.getElementById("dev-copy"),
    download: document.getElementById("dev-download"),
    reset: document.getElementById("dev-reset"),
    summary: document.getElementById("dev-summary"),
    status: document.getElementById("dev-status"),
    log: document.getElementById("dev-log"),
  },
  settings: {
    blockPopups: document.getElementById("set-block-popups"),
    cosmetic: document.getElementById("set-cosmetic"),
    blocklistEnabled: document.getElementById("set-blocklist-enabled"),
    blocklist: document.getElementById("set-blocklist"),
    fullscreen: document.getElementById("set-fullscreen"),
    pip: document.getElementById("set-pip"),
    developer: document.getElementById("set-developer"),
    anilistId: document.getElementById("set-anilist-id"),
    anilistSecret: document.getElementById("set-anilist-secret"),
    anilistRedirect: document.getElementById("set-anilist-redirect"),
    anilistStatus: document.getElementById("anilist-status"),
    keyHint: document.getElementById("key-backend-hint"),
  },
  btnSaveBlocklist: document.getElementById("btn-save-blocklist"),
  btnResetBlocklist: document.getElementById("btn-reset-blocklist"),
  btnSaveAnilist: document.getElementById("btn-save-anilist"),
  btnAnilistLogout: document.getElementById("btn-anilist-logout"),
  btnAnilistLogin: document.getElementById("btn-anilist-login"),
  linkAnilistDev: document.getElementById("link-anilist-dev"),
  aboutList: document.getElementById("about-list"),
  themeBtns: THEMES.map((t) =>
    document.querySelector(`.theme-btn[data-theme="${t}"]`),
  ),
};

el.settings.theme = document.getElementById("set-theme");

el.hero = {
  episodes: document.getElementById("hero-episodes"),
  current: document.getElementById("hero-current"),
  completed: document.getElementById("hero-completed"),
  cta: document.getElementById("hero-cta"),
};

// ---------------------------------------------------------------------------
// Hero bar (AniList istatistikleri)
// ---------------------------------------------------------------------------

function applyHero(model) {
  el.hero.episodes.textContent = model.episodes;
  el.hero.current.textContent = model.current;
  el.hero.completed.textContent = model.completed;
  el.hero.cta.hidden = model.loggedIn;
}

async function refreshHero() {
  try {
    const stats = await call("anilist_hero_stats");
    applyHero(heroModel(stats));
  } catch {
    // Offline ya da rate limit: bağlı görünüm bozmasın diye sadece tireler.
    applyHero(heroModel(null));
  }
}

el.hero.cta.addEventListener("click", () => {
  // Ayarlar → AniList bölümü her yerden aynı kapıya çıkar.
  el.btnSettings.click();
});

/** Grid element for a category id. */
const gridFor = (category) =>
  category === "tracking" ? el.gridTracking : el.gridWatching;

/** Site currently being edited, or `null` when adding. */
let editingId = null;
/** Last site opened, so "back" knows what to close. */
let openSiteLabel = null;

/**
 * Close session driving the Android back handshake
 * (`window.__animehubBackToLauncher` / `__animehubBackStatus`). Keeps
 * `openSiteLabel` honest across pending/failed closes — see
 * `logic/back_close.js` for the contract with Kotlin's `SiteBackStatus`.
 */
const backSession = createBackSession({
  call,
  notify: (message) => showNotice(`Site kapatılamadı: ${message}`, "warn"),
  errorText: errMessage,
});

// ---------------------------------------------------------------------------
// Notices
// ---------------------------------------------------------------------------

function showNotice(message, kind = "warn") {
  el.notice.hidden = false;
  el.notice.dataset.kind = kind;
  el.notice.textContent = "";
  el.notice.appendChild(document.createTextNode(message));

  const close = document.createElement("button");
  close.type = "button";
  close.setAttribute("aria-label", "Kapat");
  close.textContent = "\u00d7";
  close.addEventListener("click", () => {
    el.notice.hidden = true;
  });
  el.notice.appendChild(close);
}

function showFormError(message) {
  if (!message) {
    el.siteError.hidden = true;
    el.siteError.textContent = "";
    return;
  }
  el.siteError.hidden = false;
  el.siteError.textContent = message;
}

// ---------------------------------------------------------------------------
// Tiles
// ---------------------------------------------------------------------------

/** `undefined` keeps the stored photo, `""` clears it, a data URL replaces it. */
let pendingImage;

/**
 * One site card: a wrapper holding two sibling buttons. The card button opens
 * the site; the menu button is a sibling, not a child, so the two never nest
 * (nested interactive elements are invalid and confuse keyboard and screen
 * reader users). Clicking the menu cannot reach the card's handler.
 */
function renderTile(tile) {
  const wrap = document.createElement("div");
  wrap.className = "tile-wrap";

  const button = document.createElement("button");
  button.type = "button";
  button.className = "tile";
  button.dataset.siteId = tile.id;

  const color = safeColor(tile.color) || "#6366f1";
  button.style.setProperty("--site", bannerRgb(color));

  const banner = document.createElement("span");
  banner.className = "tile-banner";
  banner.setAttribute("aria-hidden", "true");
  if (tile.image) {
    banner.classList.add("has-image");
    const img = document.createElement("img");
    img.alt = "";
    img.draggable = false;
    img.src = tile.image;
    banner.appendChild(img);
  } else {
    banner.style.background = bannerStyle(color);
    const mark = document.createElement("span");
    mark.className = "tile-mark";
    mark.textContent = tile.letters;
    banner.appendChild(mark);
  }

  const body = document.createElement("span");
  body.className = "tile-body";
  const name = document.createElement("span");
  name.className = "tile-name";
  name.textContent = tile.name;
  name.title = tile.url;
  const host = document.createElement("span");
  host.className = "tile-host";
  host.textContent = displayHost(tile.url);
  body.append(name, host);
  if (!host.textContent) body.classList.add("no-host");

  button.append(banner, body);

  if (tile.native) {
    const badge = document.createElement("span");
    badge.className = "tile-badge";
    badge.textContent = "NATIVE";
    badge.title = "AniList ile yerel entegrasyon";
    button.appendChild(badge);
  }

  button.addEventListener("click", () => openSite(tile));
  wrap.appendChild(button);

  const menu = document.createElement("button");
  menu.type = "button";
  menu.className = "tile-menu";
  menu.setAttribute("aria-label", `${tile.name} seçenekleri`);
  menu.setAttribute("aria-haspopup", "dialog");
  menu.textContent = "\u22ee";
  menu.addEventListener("click", () => siteMenu(tile));
  wrap.appendChild(menu);

  return wrap;
}

function renderAddTile() {
  const button = document.createElement("button");
  button.type = "button";
  button.className = "tile tile-add";

  const banner = document.createElement("span");
  banner.className = "tile-banner";
  banner.setAttribute("aria-hidden", "true");
  const plus = document.createElement("span");
  plus.className = "tile-mark";
  plus.textContent = "+";
  banner.appendChild(plus);

  const body = document.createElement("span");
  body.className = "tile-body";
  const name = document.createElement("span");
  name.className = "tile-name";
  name.textContent = "Site ekle";
  const host = document.createElement("span");
  host.className = "tile-host";
  host.textContent = "Yeni bir siteyi hub'ına ekle";
  body.append(name, host);

  button.append(banner, body);
  button.addEventListener("click", () => openSiteForm(null));
  return button;
}

function renderLauncher(groups) {
  for (const group of groups) {
    const grid = gridFor(group.id);
    grid.textContent = "";

    const count = document.getElementById(`count-${group.id}`);
    if (count) count.textContent = String(group.tiles.length);

    if (group.tiles.length === 0) {
      const note = document.createElement("p");
      note.className = "empty-note";
      note.textContent =
        group.id === "tracking"
          ? "Takip sitesi eklenmedi. AniList client ID girerek yerel entegrasyonu açabilirsiniz."
          : "Henüz izleme sitesi yok.";
      grid.appendChild(note);
    }

    for (const tile of group.tiles) {
      grid.appendChild(renderTile(tile));
    }

    // "Add" always lives at the end of the İzleme group.
    if (group.id === "watching") {
      grid.appendChild(renderAddTile());
    }
  }
}

async function refreshLauncher() {
  try {
    const sites = await call("list_sites");
    renderLauncher(groupTiles(sites));
  } catch (e) {
    showNotice(errMessage(e));
  }
}

// ---------------------------------------------------------------------------
// Opening a site
// ---------------------------------------------------------------------------

async function openSite(tile) {
  try {
    const opened = await call("open_site", { id: tile.id });
    openSiteLabel = opened.label;
    backSession.opened(opened.label, tile.id);
  } catch (e) {
    showNotice(errMessage(e));
  }
}

/**
 * Ask the backend to close the site WebView and return to the launcher.
 *
 * The label is intentionally NOT cleared here: `close_site` may still be
 * pending or may fail, and claiming success in a `finally` is exactly the
 * A.5 divergence (JS idle, Rust still open). The session settles to
 * `closed` only after the invoke resolves, to `failed:<reason>` otherwise,
 * and the `animehub://site-closed` event performs the actual label clear.
 */
export async function backToLauncher() {
  backSession.requestClose();
}

/**
 * Android system-back hook (`AnimeHubSiteController.closeViaLauncher`).
 * When the site WebView has no history left, the native side evaluates this
 * in the launcher so the normal `close_site` flow — Rust state, the
 * `animehub://site-closed` event, bookkeeping — runs exactly as if the user
 * had asked for it here.
 *
 * Returns whether a close session actually accepted the request (a label
 * exists). Kotlin then polls `__animehubBackStatus()` for the real outcome:
 * `pending` while the flow runs, `closed` on success, `failed:<reason>` on
 * error (which it surfaces with a Toast — the launcher's own notice would
 * sit behind the site view), or `idle` when there was nothing to close and
 * it should fall back to a native close.
 */
window.__animehubBackToLauncher = (operationId) => backSession.requestClose(operationId).accepted;

/** Status string consumed by Kotlin's `SiteBackStatus` poll. */
window.__animehubBackStatus = () => backSession.statusString();

// ---------------------------------------------------------------------------
// Site add / edit / remove
// ---------------------------------------------------------------------------

function setPhotoPreview(src, clearable = true) {
  el.photoPreview.textContent = "";
  if (!src) {
    el.photoPreview.classList.remove("has-image");
    el.btnClearPhoto.hidden = true;
    return;
  }
  const img = document.createElement("img");
  img.alt = "";
  img.src = src;
  el.photoPreview.appendChild(img);
  el.photoPreview.classList.add("has-image");
  el.btnClearPhoto.hidden = !clearable;
}

function openSiteForm(site) {
  editingId = site ? site.id : null;
  el.siteTitle.textContent = site ? "Siteyi düzenle" : "Site ekle";
  showFormError(null);

  const form = el.formSite;
  form.elements.name.value = site ? site.name : "";
  form.elements.url.value = site ? site.url : "";
  form.elements.letter.value = site && !site.image ? site.letters || "" : "";
  form.elements.color.value = site && !site.image ? site.color || "" : "";
  const category = site && site.category === "tracking" ? "tracking" : "watching";
  form.elements.category.value = category;

  const builtin = Boolean(site && site.builtin);
  el.letterRow.hidden = builtin;
  el.photoField.hidden = false;
  el.btnPickPhoto.hidden = builtin;
  el.btnClearPhoto.hidden = true;
  el.sitePhoto.disabled = builtin;
  el.photoHint.textContent = builtin
    ? "Varsayılan sitelerin logosu sabittir."
    : "PNG, JPEG veya WebP. Kare bir simge olarak kaydedilir.";
  pendingImage = undefined;
  el.sitePhoto.value = "";
  setPhotoPreview(site && site.image, !builtin);

  openModal(el.modalSite);
  form.elements.name.focus();
}

function readPhotoFile(file) {
  return new Promise((resolve, reject) => {
    if (!file) {
      resolve(null);
      return;
    }
    if (!["image/png", "image/jpeg", "image/webp"].includes(file.type)) {
      reject(new Error("Yalnızca PNG, JPEG veya WebP."));
      return;
    }
    if (file.size > 8 * 1024 * 1024) {
      reject(new Error("Fotoğraf 8 MB'dan küçük olmalı."));
      return;
    }
    const reader = new FileReader();
    reader.onerror = () => reject(new Error("Fotoğraf okunamadı."));
    reader.onload = () => {
      const img = new Image();
      img.onerror = () => reject(new Error("Fotoğraf okunamadı."));
      img.onload = () => {
        const side = 128;
        const canvas = document.createElement("canvas");
        canvas.width = side;
        canvas.height = side;
        const ctx = canvas.getContext("2d");
        const scale = Math.max(side / img.width, side / img.height);
        const w = img.width * scale;
        const h = img.height * scale;
        ctx.drawImage(img, (side - w) / 2, (side - h) / 2, w, h);
        resolve(canvas.toDataURL("image/jpeg", 0.85));
      };
      img.src = String(reader.result || "");
    };
    reader.readAsDataURL(file);
  });
}

function siteMenu(tile) {
  const actions = [];
  actions.push({ label: "Düzenle", run: () => openSiteForm(tile) });
  actions.push({
    label: "Çerezleri ve site verilerini temizle",
    run: () => clearSiteData(tile.id),
  });
  actions.push({
    label: tile.builtin ? "Başlatıcıdan gizle" : "Sil",
    danger: !tile.builtin,
    run: () => removeSite(tile),
  });
  showActionSheet(tile.name, actions);
}

/**
 * Minimal action sheet.
 *
 * Kept dependency-free; labels are text nodes, so a hostile site name cannot
 * inject markup through the menu either.
 */
function showActionSheet(title, actions) {
  const backdrop = document.createElement("div");
  backdrop.className = "modal";
  backdrop.dataset.transient = "true";

  const card = document.createElement("div");
  card.className = "modal-card";
  card.setAttribute("role", "dialog");
  card.setAttribute("aria-modal", "true");
  card.setAttribute("aria-labelledby", "action-sheet-title");

  const heading = document.createElement("h2");
  heading.id = "action-sheet-title";
  heading.textContent = title;
  card.appendChild(heading);

  const list = document.createElement("div");
  list.className = "settings-body";
  for (const action of actions) {
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = action.danger ? "btn danger" : "btn";
    btn.style.width = "100%";
    btn.style.marginBottom = "8px";
    btn.style.textAlign = "left";
    btn.textContent = action.label;
    btn.addEventListener("click", () => {
      // Close first so focus returns to the menu button, then run the action
      // (which may open another modal and must see a clean stack).
      closeModal(backdrop);
      action.run();
    });
    list.appendChild(btn);
  }
  card.appendChild(list);

  const footer = document.createElement("div");
  footer.className = "modal-actions";
  const cancel = document.createElement("button");
  cancel.type = "button";
  cancel.className = "btn ghost";
  cancel.textContent = "Vazgeç";
  cancel.addEventListener("click", () => closeModal(backdrop));
  footer.appendChild(cancel);
  card.appendChild(footer);

  backdrop.appendChild(card);
  backdrop.addEventListener("click", (e) => {
    if (e.target === backdrop) closeModal(backdrop);
  });
  document.body.appendChild(backdrop);
  showOverlay(backdrop);
  // Start on "Vazgeç": destructive actions must not be one Enter away.
  cancel.focus();
}

async function clearSiteData(id) {
  try {
    await call("clear_site_data", { id });
    // Platform-honest scope: on Android the HTTP cache and service workers are
    // NOT profile-scoped and are not cleared. See src/logic/storage_scope.js.
    showNotice(clearResultMessage(currentPlatform), "info");
  } catch (e) {
    showNotice(errMessage(e));
  }
}

async function removeSite(tile) {
  const verb = tile.builtin ? "gizlemek" : "silmek";
  if (!window.confirm(`"${tile.name}" kaydını ${verb} istiyor musunuz?`)) return;
  try {
    const result = await call("remove_site", { id: tile.id });
    await refreshLauncher();
    // `remove_site` reports the profile outcome explicitly (A.8 contract):
    // `deferred` is progress-with-retry (in-use profile, drained at next
    // start) — say so instead of letting it pass as an instant delete.
    // `unsupported` arrives as a rejection with the MULTI_PROFILE guidance
    // (handled in catch), never as a silent success.
    const outcome = result && typeof result === "object" ? result.profile : null;
    if (outcome === "deferred") {
      showNotice(
        "Profil verisi şu an kullanımda; bir sonraki açılışta otomatik silinecek.",
        "info",
      );
    }
  } catch (e) {
    showNotice(errMessage(e));
  }
}

// ---------------------------------------------------------------------------
// Modals
// ---------------------------------------------------------------------------

/**
 * Open overlays, top of stack last. Only the top overlay is interactive: the
 * rest of the document, including lower overlays, is made `inert`, so Tab,
 * clicks and screen readers cannot reach background controls.
 */
const overlayStack = [];

const FOCUSABLE =
  'a[href], button:not([disabled]), input:not([disabled]):not([type="hidden"]), ' +
  'select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])';

function syncInert() {
  const top = overlayStack.length ? overlayStack[overlayStack.length - 1].el : null;
  for (const child of document.body.children) {
    child.inert = top !== null && child !== top;
  }
}

/** Show an overlay element and make it the only interactive one. */
function showOverlay(el) {
  // Remember where focus came from so closing can return it there.
  const opener = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  const existing = overlayStack.findIndex((o) => o.el === el);
  if (existing >= 0) overlayStack.splice(existing, 1);
  overlayStack.push({ el, opener });
  el.hidden = false;
  syncInert();
  const first = el.querySelector("input, textarea, button");
  if (first) first.focus();
}

/**
 * Hide an overlay, pop it from the stack and restore focus to its opener.
 * Transient overlays (action sheets) are removed from the DOM instead.
 */
function hideOverlay(el) {
  const index = overlayStack.findIndex((o) => o.el === el);
  const entry = index >= 0 ? overlayStack.splice(index, 1)[0] : null;
  if (el.dataset.transient === "true") el.remove();
  else el.hidden = true;
  syncInert();
  if (entry && entry.opener && entry.opener.isConnected) entry.opener.focus();
}

function openModal(modal) {
  showOverlay(modal);
}

function closeModal(modal) {
  hideOverlay(modal);
}

/** Keep Tab and Shift+Tab inside the top overlay. */
function trapFocus(event) {
  if (event.key !== "Tab" || overlayStack.length === 0) return;
  const top = overlayStack[overlayStack.length - 1].el;
  const items = [...top.querySelectorAll(FOCUSABLE)].filter((n) => !n.disabled && n.offsetParent !== null);
  if (items.length === 0) {
    event.preventDefault();
    return;
  }
  const target = wrapFocusIndex(items.length, items.indexOf(document.activeElement), event.shiftKey);
  if (target !== null) {
    event.preventDefault();
    items[target].focus();
  }
}

function wireModalClose(modal) {
  modal.querySelectorAll("[data-close]").forEach((btn) =>
    btn.addEventListener("click", () => closeModal(modal)),
  );
  modal.addEventListener("click", (e) => {
    if (e.target === modal) closeModal(modal);
  });
}

// ---------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------

/** Currently applied preference; mirrored to localStorage for the first paint. */
let themePref = normalizeTheme(
  (() => {
    try {
      return localStorage.getItem(THEME_CACHE_KEY);
    } catch {
      return null;
    }
  })(),
);

function applyTheme(pref) {
  themePref = normalizeTheme(pref);
  applyThemeToDom(themePref);
  for (const btn of el.themeBtns) {
    if (!btn) continue;
    btn.setAttribute("aria-pressed", String(btn.dataset.theme === themePref));
    btn.title = `${themeLabel(btn.dataset.theme)} tema`;
  }
  if (el.settings.theme) el.settings.theme.value = themePref;
}

// Apply the cached choice immediately so the first paint matches; the
// backend reconciliation in boot() may correct it afterwards.
applyTheme(themePref);

let currentSettings = null;
let currentDevLog = sanitizeDebugLogSnapshot(null);

// Theme flow: optimistic draw + queue + rollback. `applyTheme` updates DOM +
// themePref + selector/buttons; cache is the localStorage mirror.
let themeFlow = null;
function ensureThemeFlow() {
  if (themeFlow) return themeFlow;
  themeFlow = createThemeFlow({
    call,
    notify: (msg, kind) => showNotice(msg, kind),
    errorText: errMessage,
    getCurrent: () => currentSettings,
    setCurrent: (value) => {
      currentSettings = value;
    },
    draw: (pref) => applyTheme(pref),
    cacheSet: (pref) => {
      try {
        localStorage.setItem(THEME_CACHE_KEY, normalizeTheme(pref));
      } catch {
        /* private mode etc. — the backend copy still persists */
      }
    },
    cacheGet: () => {
      try {
        return localStorage.getItem(THEME_CACHE_KEY);
      } catch {
        return null;
      }
    },
    applyNative: (pref) => call("set_window_theme", { theme: normalizeTheme(pref) }),
  });
  return themeFlow;
}

async function setTheme(pref) {
  return ensureThemeFlow().setTheme(pref);
}

function wireTheme() {
  for (const btn of el.themeBtns) {
    if (!btn) continue;
    btn.addEventListener("click", () => setTheme(btn.dataset.theme));
  }
  if (el.settings.theme) {
    el.settings.theme.addEventListener("change", (e) => setTheme(e.target.value));
  }
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/**
 * Show only the settings that apply to this platform. Rows marked
 * `data-platform="android"` (PiP, developer log) are hidden elsewhere. If the
 * platform is unknown, they stay hidden: the safe choice.
 */
/**
 * `app_info().platform`, cached at boot and reused for anything platform
 * dependent (settings visibility, the clear-site-data wording). Empty when the
 * call failed: every consumer treats "" as "unknown" and answers conservatively.
 */
let currentPlatform = "";

async function loadPlatform() {
  try {
    const info = await call("app_info");
    currentPlatform = info && typeof info.platform === "string" ? info.platform : "";
  } catch {
    currentPlatform = "";
  }
  return currentPlatform;
}

async function applyPlatformVisibility() {
  const isAndroid = (await loadPlatform()) === "android";
  document.querySelectorAll("[data-platform]").forEach((node) => {
    node.style.display = node.dataset.platform === "android" && isAndroid ? "" : "none";
  });
}

async function openSettings() {
  try {
    await applyPlatformVisibility();
    const s = await call("get_settings");
    currentSettings = s;
    el.settings.blockPopups.checked = Boolean(s.blockPopups);
    el.settings.cosmetic.checked = Boolean(s.injectCosmeticRules);
    el.settings.blocklistEnabled.checked = Boolean(s.blocklist.enabled);
    el.settings.blocklist.value = s.blocklist.rules || "";
    el.settings.fullscreen.checked = Boolean(s.fullscreenSites);
    el.settings.pip.checked = Boolean(s.pipAutoEnter);
    el.settings.developer.checked = Boolean(s.developerOptions);
    setDeveloperPanel(Boolean(s.developerOptions));
    applyTheme(s.theme);
    el.settings.anilistId.value = s.anilist.clientId || "";
    el.settings.anilistSecret.value = "";
    el.settings.anilistRedirect.value = s.anilist.redirectUri || "";
    el.settings.keyHint.textContent =
      `Anahtar deposu: ${s.keyBackend}` +
      (s.keyBackendOsBacked
        ? " — anahtar işletim sistemi tarafından korunuyor."
        : " — işletim sistemi anahtar deposu bulunamadı; anahtar dosya izinleriyle korunuyor.");
    el.settings.anilistStatus.textContent = s.anilist.configured
      ? "AniList yapılandırılmış; giriş yapabilirsiniz."
      : "Giriş için önce Client ID'yi kaydedin (yoksa AniList WebView'da açılır).";
    el.settings.anilistStatus.dataset.state = s.anilist.configured ? "ok" : "";
    el.btnAnilistLogin.disabled = !s.anilist.configured;
    openModal(el.modalSettings);
    await refreshAniListStatus();
    if (s.developerOptions) await refreshDevLog();
  } catch (e) {
    showNotice(errMessage(e));
  }
}

function renderDevLog(snapshot) {
  currentDevLog = sanitizeDebugLogSnapshot(snapshot);
  const { events, droppedEvents, capacity } = currentDevLog;
  el.dev.summary.textContent =
    `${events.length} olay / ${capacity} kapasite · ${droppedEvents} olay düşürüldü`;
  el.dev.log.textContent = serializeDebugLogSnapshot(currentDevLog);
}

function setDeveloperPanel(open) {
  el.dev.panel.hidden = !open;
  if (!open) {
    currentDevLog = sanitizeDebugLogSnapshot(null);
    el.dev.summary.textContent = "";
    el.dev.status.textContent = "";
    el.dev.log.textContent = "";
  }
}

/** Read only when the saved developer_options gate is enabled. */
async function refreshDevLog() {
  if (!currentSettings?.developerOptions) {
    setDeveloperPanel(false);
    return false;
  }
  try {
    const snapshot = await call("debug_log");
    renderDevLog(snapshot);
    el.dev.status.textContent = "Tanılama belleği yenilendi.";
    return true;
  } catch {
    el.dev.status.textContent = "Kayıt okunamadı. Ayarı kapatıp yeniden açtıktan sonra tekrar deneyin.";
    return false;
  }
}

async function copyDebugLog() {
  if (!(await refreshDevLog())) return;
  const text = serializeDebugLogSnapshot(currentDevLog);
  try {
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(text);
    } else {
      throw new Error("clipboard_unavailable");
    }
  } catch {
    try {
      if (!copyTextFallback(text)) throw new Error("clipboard_unavailable");
    } catch {
      el.dev.status.textContent = "Panoya kopyalanamadı. JSON indirme düğmesini kullanabilirsiniz.";
      return;
    }
  }
  el.dev.status.textContent = "Güncel güvenli tanılama kaydı panoya kopyalandı.";
}

async function downloadDebugLog() {
  if (!(await refreshDevLog())) return;
  let objectUrl = null;
  let link = null;
  let downloadStarted = false;
  try {
    const blob = new Blob([serializeDebugLogSnapshot(currentDevLog)], {
      type: "application/json;charset=utf-8",
    });
    objectUrl = URL.createObjectURL(blob);
    link = document.createElement("a");
    const stamp = new Date().toISOString().replace(/[:.]/g, "-");
    link.href = objectUrl;
    link.download = `animehub-diagnostics-${stamp}.json`;
    document.body.append(link);
    link.click();
    downloadStarted = true;
    el.dev.status.textContent = "Güvenli JSON indirme başlatıldı.";
  } catch {
    el.dev.status.textContent = "JSON indirme başlatılamadı. Panoya kopyalamayı deneyin.";
  } finally {
    try {
      link?.remove();
    } catch {
      // The status message should not be masked by DOM cleanup.
    }
    if (objectUrl) {
      const revoke = () => {
        try {
          URL.revokeObjectURL(objectUrl);
        } catch {
          // Best effort: diagnostics are already serialized in memory.
        }
      };
      if (downloadStarted) {
        try {
          window.setTimeout(revoke, 0);
        } catch {
          revoke();
        }
      } else {
        revoke();
      }
    }
  }
}

async function resetDevLog() {
  if (!currentSettings?.developerOptions) return;
  try {
    await call("debug_log_reset");
    renderDevLog(await call("debug_log"));
    el.dev.status.textContent = "Tanılama belleği sıfırlandı; yeni olaylar yeniden kaydediliyor.";
  } catch {
    el.dev.status.textContent = "Tanılama belleği sıfırlanamadı. Daha sonra tekrar deneyin.";
  }
}

/** Draw the switches from the saved settings (used after a failed change). */
function syncSettingSwitches() {
  const s = switchStates(currentSettings);
  if (!s) return;
  el.settings.pip.checked = s.pip;
  el.settings.blockPopups.checked = s.blockPopups;
  el.settings.cosmetic.checked = s.cosmetic;
  el.settings.fullscreen.checked = s.fullscreen;
  el.settings.developer.checked = s.developer;
  el.settings.blocklistEnabled.checked = s.blocklistEnabled;
  setDeveloperPanel(s.developer);
}

// Save logic and the PiP switch rules live in logic/settings_flow.js.
const settingsFlow = createSettingsFlow({
  call,
  notify: (message, kind) => showNotice(message, kind),
  errorText: errMessage,
  refresh: refreshLauncher,
  drawSwitches: syncSettingSwitches,
  getCurrent: () => currentSettings,
  setCurrent: (value) => {
    currentSettings = value;
  },
  setDevPanel: (open) => setDeveloperPanel(open),
  refreshDevLog: () => refreshDevLog(),
});

function saveSettings(patch, okMessage) {
  return settingsFlow.save(patch, okMessage);
}

function wireSettings() {
  el.settings.blockPopups.addEventListener("change", (e) =>
    saveSettings({ blockPopups: e.target.checked }),
  );
  el.settings.cosmetic.addEventListener("change", (e) =>
    saveSettings({ injectCosmeticRules: e.target.checked }),
  );
  el.settings.fullscreen.addEventListener("change", (e) =>
    saveSettings({ fullscreenSites: e.target.checked }),
  );
  bindSwitch(el.settings.pip, (checked) => settingsFlow.changePipAutoEnter(checked));
  bindSwitch(el.settings.developer, (checked) =>
    settingsFlow.changeDeveloperOptions(checked),
  );
  el.dev.refresh.addEventListener("click", () => refreshDevLog());
  el.dev.copy.addEventListener("click", () => copyDebugLog());
  el.dev.download.addEventListener("click", () => downloadDebugLog());
  el.dev.reset.addEventListener("click", () => resetDevLog());
  bindSwitch(el.settings.blocklistEnabled, (checked) =>
    settingsFlow.changeBlocklistEnabled(checked),
  );

  el.btnSaveBlocklist.addEventListener("click", () =>
    saveSettings(
      {
        blocklist: {
          rules: el.settings.blocklist.value,
          enabled: el.settings.blocklistEnabled.checked,
        },
      },
      "Engelleme listesi kaydedildi.",
    ),
  );

  el.btnResetBlocklist.addEventListener("click", async () => {
    try {
      const count = await call("reset_blocklist");
      const s = await call("get_settings");
      currentSettings = s;
      el.settings.blocklist.value = s.blocklist.rules;
      el.settings.blocklistEnabled.checked = Boolean(s.blocklist.enabled);
      showNotice(`Varsayılan liste geri yüklendi (${count} kural).`, "info");
    } catch (e) {
      showNotice(errMessage(e));
    }
  });

  el.btnSaveAnilist.addEventListener("click", () => {
    const patch = {
      anilist: {
        clientId: el.settings.anilistId.value.trim(),
        clientSecret: el.settings.anilistSecret.value,
        redirectUri: el.settings.anilistRedirect.value.trim(),
      },
    };
    saveSettings(patch, "AniList ayarları kaydedildi.").then(refreshAniListStatus);
  });

  el.btnAnilistLogin.addEventListener("click", async () => {
    try {
      const start = await call("anilist_login_start");
      el.settings.anilistStatus.dataset.state = "";
      el.settings.anilistStatus.textContent =
        "Tarayıcıda AniList girişi bekleniyor… bitince bu ekran güncellenir.";
      await openExternal(start.url);
    } catch (e) {
      // Mirror into the modal's own status line: the global toast is easy to
      // dismiss, and a failure here (e.g. client_id not configured) is
      // exactly what made this button look dead.
      const msg = errMessage(e);
      el.settings.anilistStatus.dataset.state = "error";
      el.settings.anilistStatus.textContent = msg;
      showNotice(msg);
    }
  });

  el.btnAnilistLogout.addEventListener("click", async () => {
    try {
      await call("anilist_logout");
      el.settings.anilistStatus.textContent = "Oturum kapatıldı.";
      el.settings.anilistStatus.dataset.state = "";
      applyHero(heroModel(null));
    } catch (e) {
      showNotice(errMessage(e));
    }
  });

  el.linkAnilistDev.addEventListener("click", async (e) => {
    e.preventDefault();
    await openExternal("https://anilist.co/settings/developer");
  });
}

async function refreshAniListStatus() {
  try {
    const user = await call("anilist_status");
    if (user) {
      el.settings.anilistStatus.textContent = `Giriş yapıldı: ${user.name}`;
      el.settings.anilistStatus.dataset.state = "ok";
    } else if (currentSettings && currentSettings.anilist.configured) {
      el.settings.anilistStatus.textContent = "Yapılandırıldı, henüz giriş yapılmadı.";
      el.settings.anilistStatus.dataset.state = "";
    }
  } catch {
    /* status is informational; ignore */
  }
}

// ---------------------------------------------------------------------------
// About
// ---------------------------------------------------------------------------

async function openAbout() {
  el.aboutList.textContent = "";
  const rows = [["Uygulama", "Yükleniyor…"]];
  for (const [k, v] of rows) appendAboutRow(k, v);
  openModal(el.modalAbout);

  try {
    const info = await call("app_info");
    el.aboutList.textContent = "";
    appendAboutRow("Sürüm", info.version);
    appendAboutRow("Paket", "dev.animehub.app");
    appendAboutRow("Platform", info.platform);
    appendAboutRow("WebView", info.webview);
    appendAboutRow("Anahtar deposu", info.keyBackend, !info.keyBackendOsBacked);
    appendAboutRow(
      "AniList",
      info.anilistConfigured ? "Yerel entegrasyon etkin" : "Yapılandırılmamış (WebView)",
    );
    appendAboutRow("Lisans", "MIT");
  } catch (e) {
    el.aboutList.textContent = "";
    appendAboutRow("Hata", errMessage(e), true);
  }
}

function appendAboutRow(key, value, warn = false) {
  const dt = document.createElement("dt");
  dt.textContent = key;
  const dd = document.createElement("dd");
  dd.textContent = String(value);
  if (warn) dd.dataset.warn = "true";
  el.aboutList.append(dt, dd);
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function errMessage(e) {
  if (e instanceof ApiError) return e.message;
  if (e && typeof e.message === "string") return e.message;
  return String(e);
}

/**
 * Open a link in the system browser rather than inside the app.
 *
 * Falls back to `window.open` in browser-only dev mode.
 */
async function openExternal(url) {
  try {
    if (await isTauri()) {
      const { openUrl } = await import("@tauri-apps/plugin-opener");
      await openUrl(url);
      return;
    }
  } catch {
    /* fall through */
  }
  window.open(url, "_blank", "noopener,noreferrer");
}

// ---------------------------------------------------------------------------
// Boot
// ---------------------------------------------------------------------------

function wireGlobal() {
  el.btnSettings.addEventListener("click", openSettings);
  el.btnAbout.addEventListener("click", openAbout);
  [el.modalSite, el.modalSettings, el.modalAbout].forEach(wireModalClose);

  el.btnPickPhoto.addEventListener("click", () => el.sitePhoto.click());
  el.btnClearPhoto.addEventListener("click", () => {
    pendingImage = "";
    el.sitePhoto.value = "";
    setPhotoPreview(null);
  });
  el.sitePhoto.addEventListener("change", async () => {
    const file = el.sitePhoto.files && el.sitePhoto.files[0];
    if (!file) return;
    try {
      const dataUrl = await readPhotoFile(file);
      const safe = safeDataImage(dataUrl);
      if (!safe) {
        showFormError("Fotoğraf PNG, JPEG veya WebP olmalı.");
        return;
      }
      pendingImage = safe;
      setPhotoPreview(safe);
      showFormError(null);
    } catch (e) {
      showFormError(e && e.message ? e.message : "Fotoğraf okunamadı.");
    }
  });

  el.formSite.addEventListener("submit", async (event) => {
    event.preventDefault();
    const form = event.target;
    const result = validateDraft({
      name: form.elements.name.value,
      url: form.elements.url.value,
      letter: form.elements.letter.value,
      color: form.elements.color.value,
      category: form.elements.category.value,
      image: el.sitePhoto.disabled ? null : pendingImage,
    });
    if (!result.ok) {
      showFormError(result.message);
      form.elements[result.field]?.focus();
      return;
    }
    showFormError(null);
    try {
      if (editingId) {
        await call("update_site", { id: editingId, draft: result.draft });
      } else {
        await call("add_site", { draft: result.draft });
      }
      closeModal(el.modalSite);
      await refreshLauncher();
    } catch (e) {
      showFormError(errMessage(e));
    }
  });

  document.addEventListener("keydown", (event) => {
    trapFocus(event);
    if (event.key !== "Escape") return;
    // Escape during IME composition belongs to the input method, not to us.
    if (event.isComposing) return;
    if (overlayStack.length > 0) {
      closeModal(overlayStack[overlayStack.length - 1].el);
      return;
    }
    // No modal open: treat Escape as "back to launcher" (Android back maps
    // here through the WebView).
    void backToLauncher();
  });
}

async function wireBackendEvents() {
  await on("animehub://navigation-blocked", (message) => showNotice(String(message)));
  await on("animehub://error", (message) => showNotice(String(message)));
  await on("animehub://anilist-login", (name) => {
    showNotice(`AniList girişi tamamlandı: ${name}`, "info");
    void refreshAniListStatus();
    void refreshHero();
  });
  // The Rust side emits this when the `close_site` flow actually finished
  // (including closes started by the Android back hook). Clear the label
  // ONLY for the site this session owns — a stale event from an earlier
  // close must not orphan a newer one (A.5).
  await on("animehub://site-closed", (payload) => {
    const siteId = typeof payload === "string" ? payload : null;
    if (backSession.closedByEvent(siteId)) openSiteLabel = null;
  });
}

async function boot() {
  wireGlobal();
  wireSettings();
  wireTheme();
  renderLauncher(CATEGORIES.map((c) => ({ id: c.id, title: c.title, tiles: [] })));
  await loadPlatform();
  await refreshLauncher();
  await wireBackendEvents();
  // One-time, in-app honesty about the 0.3.4 storage migration: the legacy
  // localStorage/IndexedDB snapshots are deliberately not re-imported, and a
  // doc page nobody opens is not a notice. Shown once per launcher profile.
  const migrationNotice = takeMigrationNotice(window.localStorage);
  if (migrationNotice) showNotice(migrationNotice, "info");
  void refreshHero();

  try {
    const warning = await call("take_startup_warning");
    if (warning) showNotice(String(warning));
  } catch {
    /* non-fatal */
  }

  // Reconcile the theme with the encrypted settings: the localStorage mirror
  // above only covers the first paint.
  try {
    const s = await call("get_settings");
    currentSettings = s;
    if (normalizeTheme(s.theme) !== themePref) {
      applyTheme(s.theme);
      try {
        localStorage.setItem(THEME_CACHE_KEY, normalizeTheme(s.theme));
      } catch {
        /* ignore */
      }
    }
  } catch {
    /* theme stays at the cached value */
  }
}

void boot();
