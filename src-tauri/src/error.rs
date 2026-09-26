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
        }
    }
}

impl From<url::ParseError> for AppError {
    fn from(e: url::ParseError) -> Self {
        AppError::InvalidUrl(e.to_string())
    }
}

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        AppError::Storage(e.to_string())
    }
}

impl From<serde_json::Error> for AppError {
    fn from(e: serde_json::Error) -> Self {
        AppError::Storage(format!("veri çözümlenemedi ({e})"))
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
    fn from(e: aes_gcm::Error) -> Self {
        AppError::Crypto(e.to_string())
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
