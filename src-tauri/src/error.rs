//! Error types shared across the AnimeHub backend.
//!
//! Every variant maps to a short, user-presentable message: the frontend
//! renders `to_string()` directly, so keep these free of paths, tokens and
//! other data that should never reach the UI.

use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("Geçersiz URL: {0}")]
    InvalidUrl(String),

    #[error("Yalnızca HTTPS adresler açılabilir.")]
    InsecureScheme,

    #[error("Bu adres engelleme listesinde: {0}")]
    Blocked(String),

    #[error("Site bulunamadı: {0}")]
    SiteNotFound(String),

    #[error("Aynı ada sahip bir site zaten var.")]
    DuplicateName,

    #[error("Bu adres zaten kayıtlı.")]
    DuplicateUrl,

    #[error("Depolama hatası: {0}")]
    Storage(String),

    #[error("Şifreleme hatası: {0}")]
    Crypto(String),

    #[error("Anahtarlık kullanılamıyor: {0}")]
    Keyring(String),

    #[error("AniList hatası: {0}")]
    AniList(String),

    #[error("AniList ile oturum açılmamış.")]
    Unauthorized,

    #[error("Ağ hatası: {0}")]
    Network(String),

    #[error("Beklenmeyen hata: {0}")]
    Other(String),

    /// A platform/feature the current environment cannot provide (e.g. a
    /// WebView provider without `MULTI_PROFILE`). The message is written to
    /// be shown to the user as-is.
    #[error("{0}")]
    Unsupported(String),

    /// A site profile the platform side refuses to open because its last
    /// in-place wipe timed out on a provider call that cannot be cancelled,
    /// so the wipe is UNVERIFIED and may still be mutating the profile (see
    /// `ProfileQuarantine` in the Kotlin flows). Transient by design — the
    /// message names the recovery step and is shown to the user as-is, which
    /// is why it is not folded into [`AppError::Other`]'s "unexpected error"
    /// wording.
    #[error("{0}")]
    Quarantined(String),

    /// A tile photo the user attached. The message is already safe to show.
    #[error("{0}")]
    Icon(String),

    /// The user's ad/tracker rules could not be handed to the Android site
    /// WebView. Deliberately its own variant: continuing would mean the site
    /// silently runs with stale or no rules (desktop-only enforcement), so
    /// `open_site` refuses and `update_settings` reports it.
    #[error("Engelleme listesi site görünümüne uygulanamadı; site açılmadı.")]
    BlocklistUnavailable,
}

/// Serializable payload for the `invoke` boundary.
///
/// `code` lets the frontend branch on the class of failure without parsing
/// human-readable text.
#[derive(Debug, Serialize)]
pub struct ErrorPayload {
    pub code: &'static str,
    pub message: String,
}

impl AppError {
    pub fn code(&self) -> &'static str {
        match self {
            AppError::InvalidUrl(_) => "invalid_url",
            AppError::InsecureScheme => "insecure_scheme",
            AppError::Blocked(_) => "blocked",
            AppError::SiteNotFound(_) => "site_not_found",
            AppError::DuplicateName => "duplicate_name",
            AppError::DuplicateUrl => "duplicate_url",
            AppError::Storage(_) => "storage",
            AppError::Crypto(_) => "crypto",
            AppError::Keyring(_) => "keyring",
            AppError::AniList(_) => "anilist",
            AppError::Unauthorized => "unauthorized",
            AppError::Network(_) => "network",
            AppError::Other(_) => "other",
            AppError::Unsupported(_) => "unsupported",
            AppError::Quarantined(_) => "quarantined",
            AppError::Icon(_) => "icon",
            AppError::BlocklistUnavailable => "blocklist_unavailable",
        }
    }
}

impl From<url::ParseError> for AppError {
    fn from(_e: url::ParseError) -> Self {
        // Parsing errors may quote user-controlled URL data. Keep only a safe
        // class; diagnostics and command errors do not need parser internals.
        AppError::InvalidUrl("adres biçimi geçersiz".into())
    }
}

impl From<std::io::Error> for AppError {
    fn from(_e: std::io::Error) -> Self {
        // `Display` for io errors can include a filesystem path. Keep both
        // the user-facing message and the system log to a static class.
        log::error!("dosya işlemi başarısız");
        AppError::Storage("dosya işlemi başarısız".into())
    }
}

impl From<serde_json::Error> for AppError {
    fn from(_e: serde_json::Error) -> Self {
        // Serde errors can quote a fragment of a site list or settings file.
        // Keep the log static; never emit parser input or exception text.
        log::error!("veri çözümlenemedi");
        AppError::Storage("veri çözümlenemedi".into())
    }
}

impl From<reqwest::Error> for AppError {
    fn from(e: reqwest::Error) -> Self {
        // reqwest errors can embed request URLs; keep only the class.
        if e.is_connect() {
            AppError::Network("AniList'e bağlanılamadı".into())
        } else if e.is_timeout() {
            AppError::Network("AniList isteği zaman aşımına uğradı".into())
        } else {
            AppError::Network("AniList isteği başarısız oldu".into())
        }
    }
}

impl From<aes_gcm::Error> for AppError {
    fn from(_e: aes_gcm::Error) -> Self {
        AppError::Crypto("şifreleme işlemi başarısız".into())
    }
}

pub type AppResult<T> = Result<T, AppError>;

/// Commands must return `Result<T, E>` where `E: Serialize`.
impl Serialize for AppError {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let payload = ErrorPayload {
            code: self.code(),
            message: self.to_string(),
        };
        payload.serialize(s)
    }
}
