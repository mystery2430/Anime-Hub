/**
 * Launcher UI.
 *
 * Rendering rule that the whole file follows: **no `innerHTML` anywhere**.
 * Site names, letters and hosts are user-controlled, so every dynamic value
 * goes through `textContent`, and colours are re-validated by `safeColor()`
 * before touching a style. See `logic/tiles.js` for the validation.
 */

import { call, on, ApiError, isTauri } from "./logic/bridge.js";
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
import {
  heroModel,
  displayHost,
  bannerStyle,
  bannerRgb,
} from "./logic/hero.js";

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
  settings: {
    blockPopups: document.getElementById("set-block-popups"),
    cosmetic: document.getElementById("set-cosmetic"),
    blocklistEnabled: document.getElementById("set-blocklist-enabled"),
    blocklist: document.getElementById("set-blocklist"),
    fullscreen: document.getElementById("set-fullscreen"),
    pip: document.getElementById("set-pip"),
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

function renderTile(tile) {
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

  const menu = document.createElement("button");
  menu.type = "button";
  menu.className = "tile-menu";
  menu.setAttribute("aria-label", `${tile.name} seçenekleri`);
  menu.textContent = "\u22ee";
  menu.addEventListener("click", (event) => {
    event.stopPropagation();
    siteMenu(tile);
  });
  button.appendChild(menu);

  return button;
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
    if (!opened.isolated) {
      showNotice(
        "Android'de oturum verileri (çerez + localStorage/IndexedDB) site geçişlerinde şifreli blob'lara aktarılır; IndexedDB en iyi çaba ilkesiyle taşınır.",
        "info",
      );
    }
  } catch (e) {
    showNotice(errMessage(e));
  }
}

/** Ask the backend to close the site WebView and return to the launcher. */
export async function backToLauncher() {
  if (!openSiteLabel) return;
  try {
    await call("close_site", { label: openSiteLabel });
  } catch (e) {
    showNotice(errMessage(e));
  } finally {
    openSiteLabel = null;
  }
}

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

  const card = document.createElement("div");
  card.className = "modal-card";
  card.setAttribute("role", "dialog");
  card.setAttribute("aria-modal", "true");

  const heading = document.createElement("h2");
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
      backdrop.remove();
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
  cancel.addEventListener("click", () => backdrop.remove());
  footer.appendChild(cancel);
  card.appendChild(footer);

  backdrop.appendChild(card);
  backdrop.addEventListener("click", (e) => {
    if (e.target === backdrop) backdrop.remove();
  });
  document.body.appendChild(backdrop);
  cancel.focus();
}

async function clearSiteData(id) {
  try {
    await call("clear_site_data", { id });
    showNotice("Site verileri temizlendi.", "info");
  } catch (e) {
    showNotice(errMessage(e));
  }
}

async function removeSite(tile) {
  const verb = tile.builtin ? "gizlemek" : "silmek";
  if (!window.confirm(`"${tile.name}" kaydını ${verb} istiyor musunuz?`)) return;
  try {
    await call("remove_site", { id: tile.id });
    await refreshLauncher();
  } catch (e) {
    showNotice(errMessage(e));
  }
}

// ---------------------------------------------------------------------------
// Modals
// ---------------------------------------------------------------------------

function openModal(modal) {
  modal.hidden = false;
  const focusable = modal.querySelector("input, textarea, button");
  if (focusable) focusable.focus();
}

function closeModal(modal) {
  modal.hidden = true;
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

async function setTheme(pref) {
  applyTheme(pref);
  try {
    localStorage.setItem(THEME_CACHE_KEY, themePref);
  } catch {
    /* private mode etc. — the backend copy still persists */
  }
  // Native window + WebViews (Rust applies it to every webview of the app,
  // so site pages that respect prefers-color-scheme follow too).
  try {
    await call("set_window_theme", { theme: themePref });
  } catch {
    /* cosmetic only — never block the UI over it */
  }
  // Persist into the encrypted settings.
  try {
    await call("update_settings", { patch: { theme: themePref } });
  } catch (e) {
    showNotice(errMessage(e));
  }
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

// Apply the cached choice immediately so the first paint matches; the
// backend reconciliation in boot() may correct it afterwards.
applyTheme(themePref);

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

let currentSettings = null;

async function openSettings() {
  try {
    const s = await call("get_settings");
    currentSettings = s;
    el.settings.blockPopups.checked = Boolean(s.blockPopups);
    el.settings.cosmetic.checked = Boolean(s.injectCosmeticRules);
    el.settings.blocklistEnabled.checked = Boolean(s.blocklist.enabled);
    el.settings.blocklist.value = s.blocklist.rules || "";
    el.settings.fullscreen.checked = Boolean(s.fullscreenSites);
    el.settings.pip.checked = Boolean(s.pipAutoEnter);
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
  } catch (e) {
    showNotice(errMessage(e));
  }
}

async function saveSettings(patch, okMessage) {
  try {
    currentSettings = await call("update_settings", { patch });
    if (okMessage) showNotice(okMessage, "info");
    await refreshLauncher();
  } catch (e) {
    showNotice(errMessage(e));
  }
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
  el.settings.pip.addEventListener("change", async (e) => {
    await saveSettings({ pipAutoEnter: e.target.checked });
    try {
      await call("set_pip_auto_enter", { enabled: e.target.checked });
    } catch {
      /* desktop: unsupported, nothing to do */
    }
  });
  el.settings.blocklistEnabled.addEventListener("change", (e) =>
    saveSettings({
      blocklist: { rules: el.settings.blocklist.value, enabled: e.target.checked },
    }),
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
    if (info.androidIsolationNote) {
      appendAboutRow(
        "Oturum izolasyonu",
        "Android: çerez + localStorage/IndexedDB şifreli aktarımı (IndexedDB en iyi çaba)",
        true,
      );
    }
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
    if (event.key !== "Escape") return;
    for (const m of [el.modalSite, el.modalSettings, el.modalAbout]) {
      if (!m.hidden) {
        closeModal(m);
        return;
      }
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
  // The Rust side can close a site on its own (the injected back button /
  // Esc navigates to animehub://close-site, handled natively). Clear the
  // label so the launcher's own "back" bookkeeping stays honest.
  await on("animehub://site-closed", () => {
    openSiteLabel = null;
  });
}

async function boot() {
  wireGlobal();
  wireSettings();
  wireTheme();
  renderLauncher(CATEGORIES.map((c) => ({ id: c.id, title: c.title, tiles: [] })));
  await refreshLauncher();
  await wireBackendEvents();
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
