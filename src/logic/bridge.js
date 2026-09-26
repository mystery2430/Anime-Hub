/**
 * Thin wrapper over the Tauri `invoke` bridge.
 *
 * Exported as plain functions so the UI layer stays testable: the logic
 * modules never import this, they take an injected API object instead.
 *
 * When running outside Tauri (plain browser, or `npm test`) this module falls
 * back to an in-memory stub so the UI still renders for development.
 */

let invoke = null;
let listen = null;

async function loadTauri() {
  if (invoke) return { invoke, listen };
  try {
    const core = await import("@tauri-apps/api/core");
    const event = await import("@tauri-apps/api/event");
    invoke = core.invoke;
    listen = event.listen;
    return { invoke, listen };
  } catch {
    return { invoke: null, listen: null };
  }
}

/** True when the real Rust backend is reachable. */
export async function isTauri() {
  const { invoke: fn } = await loadTauri();
  return Boolean(fn);
}

/**
 * Call a Rust command.
 *
 * Errors arrive as `{ code, message }` from `AppError`; we rethrow as an
 * `ApiError` so callers can branch on `code` and show `message` verbatim.
 */
export async function call(cmd, args = {}) {
  const { invoke: fn } = await loadTauri();
  if (!fn) return stub(cmd, args);
  try {
    return await fn(cmd, args);
  } catch (e) {
    throw toApiError(e);
  }
}

/** Subscribe to a backend event. Returns an unsubscribe function. */
export async function on(event, handler) {
  const { listen: fn } = await loadTauri();
  if (!fn) return () => {};
  return fn(event, (e) => handler(e.payload));
}

/** Shown when the backend returned something we cannot describe. */
export const FALLBACK_MESSAGE = "Beklenmeyen bir hata oluştu.";

export class ApiError extends Error {
  constructor(code, message) {
    super(message);
    this.name = "ApiError";
    this.code = code;
  }
}

export function toApiError(e) {
  // Tauri serialises Err(AppError) as the JSON payload itself.
  if (e && typeof e === "object" && typeof e.message === "string") {
    return new ApiError(e.code || "other", e.message);
  }
  if (typeof e === "string") {
    // A JSON string is the other shape Tauri uses for structured errors.
    try {
      const parsed = JSON.parse(e);
      if (parsed && typeof parsed.message === "string") {
        return new ApiError(parsed.code || "other", parsed.message);
      }
    } catch {
      /* not JSON — fall through */
    }
    return new ApiError("other", e);
  }
  // Anything else: show a real sentence rather than `[object Object]`, which
  // would otherwise be rendered straight into the notice bar.
  return new ApiError("other", FALLBACK_MESSAGE);
}

// ---------------------------------------------------------------------------
// Browser-only stub, so `npm run dev` outside Tauri still shows a launcher.
// ---------------------------------------------------------------------------

const stubState = {
  sites: [
    {
      id: "builtin-openanime",
      name: "OpenAnime",
      url: "https://openani.me/",
      host: "openani.me",
      category: "watching",
      icon: { kind: "letter", text: "OA", color: "#e11d48" },
      builtin: true,
      hidden: false,
      native: false,
    },
    {
      id: "builtin-animecix",
      name: "AnimeCix",
      url: "https://animecix.com/",
      host: "animecix.com",
      category: "watching",
      icon: { kind: "letter", text: "AC", color: "#7c3aed" },
      builtin: true,
      hidden: false,
      native: false,
    },
  ],
  settings: {
    blocklist: { rules: "# tarayıcı modu: liste yüklenmedi\n", enabled: true },
    blocklistCount: 0,
    blockPopups: true,
    injectCosmeticRules: true,
    fullscreenSites: true,
    pipAutoEnter: false,
    anilist: {
      configured: false,
      clientId: "",
      redirectUri: "http://127.0.0.1:17395/callback",
      confidential: false,
    },
    keyBackend: "Tarayıcı modu (şifreleme yok)",
    keyBackendOsBacked: false,
    platform: "browser",
  },
};

export async function stub(cmd, args) {
  switch (cmd) {
    case "list_sites":
      return stubState.sites.filter((s) => !s.hidden);
    case "add_site":
      return stubState.sites.push(makeStubSite(args.draft)) && stubState.sites.at(-1);
    case "remove_site":
      stubState.sites = stubState.sites.filter((s) => s.id !== args.id);
      return null;
    case "get_settings":
      return stubState.settings;
    case "update_settings":
      Object.assign(stubState.settings, args.patch || {});
      return stubState.settings;
    case "app_info":
      return {
        version: "0.1.0-dev",
        name: "AnimeHub",
        platform: "browser",
        keyBackend: "Tarayıcı modu (şifreleme yok)",
        keyBackendOsBacked: false,
        webview: "Tarayıcı",
        anilistConfigured: false,
        androidIsolationNote: false,
      };
    case "take_startup_warning":
      return "Tauri olmadan çalışıyor: gerçek WebView, şifreleme ve oturum izolasyonu devre dışı.";
    default:
      throw new ApiError("other", `Tarayıcı modunda '${cmd}' desteklenmiyor.`);
  }
}

function makeStubSite(draft) {
  return {
    id: `stub-${Math.random().toString(36).slice(2, 10)}`,
    name: draft.name,
    url: draft.url,
    host: new URL(draft.url).hostname,
    category: draft.category || "watching",
    icon: {
      kind: "letter",
      text: (draft.letter || draft.name).slice(0, 2).toUpperCase(),
      color: draft.color || "#0ea5e9",
    },
    builtin: false,
    hidden: false,
    native: false,
  };
}
