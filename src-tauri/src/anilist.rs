//! AniList native integration — clean-room implementation.
//!
//! This is an independent implementation written against AniList's public
//! OAuth 2.0 and GraphQL v2 documentation. No code was copied from
//! [Dantotsu](https://github.com/rebelonion/Dantotsu); that project informed
//! the *design* (sync an AniList library natively instead of shelling out to a
//! WebView) and is credited in `NOTICES.md`.
//!
//! AniList's GraphQL endpoint requires no API key, so this works out of the
//! box. OAuth needs a `client_id` you register at
//! <https://anilist.co/settings/developer>; until one is configured the
//! launcher still opens AniList as a normal site tile.

use crate::error::{AppError, AppResult};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};
use zeroize::{Zeroize, Zeroizing};

pub const ANILIST_GRAPHQL: &str = "https://graphql.anilist.co";
pub const ANILIST_AUTHORIZE: &str = "https://anilist.co/api/v2/oauth/authorize";
pub const ANILIST_TOKEN: &str = "https://anilist.co/api/v2/oauth/token";
pub const ANILIST_SITE: &str = "https://anilist.co/";

/// How much earlier than the real expiry we treat a token as stale, so a
/// request in flight never races the deadline.
const EXPIRY_SKEW_SECS: i64 = 60;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AniListConfig {
    /// From <https://anilist.co/settings/developer>. Not a secret, but kept
    /// out of the repo so forks don't share one.
    pub client_id: String,
    /// AniList issues a secret with the client id. Confidential clients need
    /// it for the token exchange; public clients leave it empty.
    #[serde(default)]
    pub client_secret: String,
    /// Must match the redirect URI registered for the client, character for
    /// character.
    pub redirect_uri: String,
}

impl Default for AniListConfig {
    fn default() -> Self {
        AniListConfig {
            client_id: std::env::var("ANIMEHUB_ANILIST_CLIENT_ID").unwrap_or_default(),
            client_secret: std::env::var("ANIMEHUB_ANILIST_CLIENT_SECRET").unwrap_or_default(),
            redirect_uri: std::env::var("ANIMEHUB_ANILIST_REDIRECT_URI")
                .unwrap_or_else(|_| "http://127.0.0.1:17395/callback".to_string()),
        }
    }
}

impl AniListConfig {
    pub fn is_configured(&self) -> bool {
        !self.client_id.trim().is_empty()
    }

    pub fn is_confidential(&self) -> bool {
        !self.client_secret.trim().is_empty()
    }
}

/// The only redirect an Android build may use for native OAuth.
///
/// A custom-scheme redirect (`animehub://anilist/callback`) can be claimed by
/// another installed app: the OS delivers the intent — including both `code`
/// and `state` — to that app, so single-use state validation cannot help. Until
/// AniList offers a verified App Link/PKCE flow, Android therefore uses the
/// same loopback listener as desktop. A custom-scheme redirect is refused
/// BEFORE a login is started, and the mobile deep-link registration is gone, so
/// a hostile app has no intent to intercept.
pub fn mobile_native_login_allowed(redirect_uri: &str) -> bool {
    loopback_port(redirect_uri).is_some()
}

/// User-facing refusal for an unsafe Android redirect. Never contains the
/// redirect value itself (it may carry a hostile host or credentials).
pub const ANDROID_CUSTOM_SCHEME_OAUTH_REFUSED: &str =
    "Android'de güvenli AniList girişi yalnızca yerel geri dönüş adresiyle \
     (http://127.0.0.1:PORT/callback) desteklenir. Özel şema (animehub://) \
     başka uygulamalar tarafından ele geçirilebildiği için kapatıldı.";

/// Build the authorization URL the user is sent to.
///
/// `state` must be a fresh, unpredictable value; the callback handler rejects
/// any response that does not echo it back, which is what stops a malicious
/// page from completing the flow with its own code.
pub fn authorize_url(cfg: &AniListConfig, state: &str) -> AppResult<String> {
    if !cfg.is_configured() {
        return Err(AppError::AniList(
            "AniList client_id yapılandırılmamış".into(),
        ));
    }
    if state.is_empty() {
        return Err(AppError::AniList("state boş olamaz".into()));
    }
    let mut url = url::Url::parse(ANILIST_AUTHORIZE)?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("client_id", cfg.client_id.trim());
        q.append_pair("redirect_uri", cfg.redirect_uri.trim());
        q.append_pair("response_type", "code");
        q.append_pair("state", state);
    }
    Ok(url.to_string())
}

/// The stored OAuth token.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Token {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    pub token_type: String,
    /// Unix seconds.
    pub expires_at: i64,
    #[serde(default)]
    pub scope: Option<String>,
}

impl Token {
    pub fn needs_refresh(&self, now_unix: i64) -> bool {
        self.expires_at - EXPIRY_SKEW_SECS <= now_unix
    }

    pub fn authorization_header(&self) -> String {
        format!("{} {}", self.token_type, self.access_token)
    }
}

impl Drop for Token {
    fn drop(&mut self) {
        // Wipe the secret material rather than leaving it in freed memory.
        self.access_token.zeroize();
        if let Some(r) = self.refresh_token.as_mut() {
            r.zeroize();
        }
    }
}

pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// AniList's token response.
#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default = "default_token_type")]
    pub token_type: String,
    #[serde(default = "default_expires_in")]
    pub expires_in: i64,
    #[serde(default)]
    pub scope: Option<String>,
}

fn default_token_type() -> String {
    "Bearer".into()
}

fn default_expires_in() -> i64 {
    3600
}

impl TokenResponse {
    pub fn into_token(self, now_unix: i64) -> Token {
        Token {
            access_token: self.access_token,
            refresh_token: self.refresh_token,
            token_type: if self.token_type.trim().is_empty() {
                "Bearer".into()
            } else {
                self.token_type
            },
            expires_at: now_unix.saturating_add(self.expires_in.max(0)),
            scope: self.scope,
        }
    }
}

/// OAuth error response from the token endpoint.
#[derive(Debug, Deserialize)]
pub struct OAuthError {
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub error_description: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

/// Map an OAuth error code to a message we are willing to show.
pub fn oauth_error_message(code: &str) -> &'static str {
    match code {
        "invalid_grant" => "Yetkilendirme kodu geçersiz veya süresi dolmuş. Tekrar deneyin.",
        "invalid_client" => "AniList client_id/client_secret reddedildi.",
        "invalid_request" => "AniList isteği hatalı biçimlendirilmiş.",
        "unauthorized_client" => "Bu istemci bu akış için yetkilendirilmemiş.",
        "access_denied" => "Yetkilendirme reddedildi.",
        _ => "AniList yetkilendirmesi başarısız oldu.",
    }
}

// ---------------------------------------------------------------------------
// Callback URL parsing
// ---------------------------------------------------------------------------

/// Split a callback URL into `(code, state)`, accepting ONLY the exact redirect
/// URI the pending login was started with — the loopback
/// (`http://127.0.0.1:PORT/callback?code=…`) and the custom-scheme
/// (`animehub://anilist/callback?code=…`) forms alike.
///
/// WHY "exact" AND NOT "is it loopback?": the old check accepted any
/// `http://localhost:ANY_PORT/ANY_PATH` and, for the custom scheme, any
/// `animehub://ANY_HOST/ANY_PATH`. A registered deep-link scheme can be claimed
/// by another installed app, and a loopback port/path that is not ours is not
/// our listener — so the callback has to match the configured redirect URI
/// (scheme, host, port and path) before its `code` is worth anything. Anything
/// else is rejected, and a repeated `code`/`state` parameter is rejected too:
/// one callback carries exactly one of each.
///
/// This does NOT and cannot fix custom-scheme hijacking on Android (another app
/// registering `animehub://anilist/callback` still receives the OS intent) —
/// see `docs/` and the PR description; that needs provider-side PKCE or an
/// app-link/asset-statement flow, and AniList's documented flow offers neither.
pub fn parse_callback(raw: &str, expected_redirect: &str) -> AppResult<(String, String)> {
    let url =
        url::Url::parse(raw).map_err(|_| AppError::AniList("geçersiz dönüş adresi".into()))?;
    let expected = url::Url::parse(expected_redirect)
        .map_err(|_| AppError::AniList("yapılandırılmış dönüş adresi geçersiz".into()))?;

    if url.scheme() != expected.scheme() {
        return Err(AppError::AniList(format!(
            "beklenmeyen dönüş şeması: {}",
            url.scheme()
        )));
    }

    // Uniform for EVERY accepted form, because a redirect URI carrying
    // credentials or a fragment is not the redirect URI that was registered —
    // even though its host and path still compare equal:
    // `http://user:pass@127.0.0.1:17395/callback` and
    // `animehub://anilist/callback#code=x` both did. The configured redirect is
    // checked with the same rule, so a nonsense `ANILIST_REDIRECT_URI` fails
    // closed instead of widening the comparison.
    for candidate in [&url, &expected] {
        if !candidate.username().is_empty() || candidate.password().is_some() {
            return Err(AppError::AniList(
                "dönüş adresinde kullanıcı bilgisi olamaz".into(),
            ));
        }
        if candidate.fragment().is_some() {
            return Err(AppError::AniList(
                "dönüş adresinde parça (fragment) olamaz".into(),
            ));
        }
    }

    let host = url.host_str().unwrap_or_default();
    let expected_host = expected.host_str().unwrap_or_default();
    match url.scheme() {
        "http" | "https" => {
            // Loopback only — and only OUR loopback endpoint.
            if !matches!(host, "127.0.0.1" | "localhost" | "[::1]") {
                return Err(AppError::AniList(
                    "OAuth dönüşü yalnızca yerel adresten kabul edilir".into(),
                ));
            }
            if host != expected_host {
                return Err(AppError::AniList(
                    "dönüş adresinin host'u beklenenden farklı".into(),
                ));
            }
            if url.port_or_known_default() != expected.port_or_known_default() {
                return Err(AppError::AniList(
                    "dönüş adresinin port'u beklenenden farklı".into(),
                ));
            }
        }
        // The custom scheme is registered with an exact host and path prefix
        // (`animehub://anilist/callback` in tauri.conf.json), so accept nothing
        // else: `animehub://evil/...` must not be able to carry a code here.
        "animehub" => {
            if host != expected_host {
                return Err(AppError::AniList(
                    "dönüş adresinin host'u beklenenden farklı".into(),
                ));
            }
            // The port is compared here too: a non-special scheme has no
            // default port, so the registered redirect has none and ANY
            // explicit port (`animehub://anilist:1234/callback`) makes it a
            // different URI. `port_or_known_default` would have hidden that by
            // answering `None` for both sides.
            if url.port() != expected.port() {
                return Err(AppError::AniList(
                    "dönüş adresinin port'u beklenenden farklı".into(),
                ));
            }
        }
        other => {
            return Err(AppError::AniList(format!(
                "beklenmeyen dönüş şeması: {other}"
            )))
        }
    }
    if url.path() != expected.path() {
        return Err(AppError::AniList(
            "dönüş adresinin yolu beklenenden farklı".into(),
        ));
    }

    // An error response (user denied consent) comes back as ?error=…
    let mut code = None;
    let mut state = None;
    let mut oauth_error = None;
    let mut duplicated: Option<&str> = None;
    for (k, v) in url.query_pairs() {
        match k.as_ref() {
            "code" => {
                if code.replace(v.to_string()).is_some() {
                    duplicated = Some("code");
                }
            }
            "state" => {
                if state.replace(v.to_string()).is_some() {
                    duplicated = Some("state");
                }
            }
            "error" => oauth_error = Some(v.to_string()),
            _ => {}
        }
    }
    if let Some(field) = duplicated {
        return Err(AppError::AniList(format!(
            "dönüş adresinde birden fazla {field} parametresi var"
        )));
    }

    if let Some(e) = oauth_error {
        return Err(AppError::AniList(oauth_error_message(&e).to_string()));
    }

    // Present-but-empty (`?code=&state=`) counts as absent: an empty state would
    // otherwise compare equal to an empty pending slot.
    let code = code
        .filter(|c| !c.is_empty())
        .ok_or_else(|| AppError::AniList("dönüş adresinde kod yok".into()))?;
    let state = state
        .filter(|c| !c.is_empty())
        .ok_or_else(|| AppError::AniList("dönüş adresinde state yok".into()))?;
    Ok((code, state))
}

/// What the single-use OAuth state slot decided about one callback.
#[derive(Debug, PartialEq, Eq)]
pub enum PendingState {
    /// No login was pending at all.
    None,
    /// The state matched and the attempt is now CONSUMED (the slot is empty).
    Consumed(String),
    /// A login is pending but this state does not match it. The genuine attempt
    /// is deliberately left in place, so a bogus or replayed callback cannot
    /// destroy a real login.
    Mismatched,
}

/// Consume the pending OAuth attempt **if and only if** the returned state
/// matches, atomically under the slot's lock.
///
/// WHY CONSUME-BEFORE-EXCHANGE: the old handler cloned the pending state,
/// compared it, exchanged the code and only then cleared the slot — so two
/// callbacks carrying the same state (a retry, a replayed deep link, or the
/// loopback listener and the OS intent arriving together) both passed the check
/// and raced two token exchanges and two token writes. Taking the value while
/// still holding the lock makes the attempt single-use: the loser finds no
/// pending session and is refused, and a *mismatched* state never consumes
/// anything, so a stray callback cannot burn a legitimate login.
pub fn take_pending_state(
    slot: &std::sync::Mutex<Option<String>>,
    returned_state: &str,
) -> AppResult<PendingState> {
    let mut guard = slot
        .lock()
        .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
    Ok(match guard.as_deref() {
        // No pending session — including a slot that somehow holds an empty
        // state, which must never compare equal to anything.
        None | Some("") => PendingState::None,
        Some(expected) => {
            if state_matches(expected, returned_state) {
                match guard.take() {
                    Some(taken) => PendingState::Consumed(taken),
                    None => PendingState::None,
                }
            } else {
                PendingState::Mismatched
            }
        }
    })
}

/// Drop the pending state of an attempt that can no longer complete.
///
/// `anilist_login_start` writes the state BEFORE the loopback listener is bound
/// and then serves the callback from a background task. When binding fails
/// (the port is taken) or no callback arrives within the listener's window,
/// that task reports the error and is done — but the state value stayed in the
/// slot, i.e. an armed login nothing would ever complete. Clearing it is not a
/// security fix by itself (`take_pending_state` only ever consumes the CURRENT
/// value, so the leftover cannot be replayed with a fresh attempt's code), but
/// it keeps the slot honest: "pending" then always means "a listener is
/// actually waiting for it".
///
/// Conditional on purpose: a newer attempt may have overwritten the slot
/// between the failure and this call, and clearing THAT attempt's state would
/// break the login the user is actually running.
pub fn abandon_pending_state(slot: &std::sync::Mutex<Option<String>>, expected: &str) -> bool {
    let mut guard = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if guard.as_deref() == Some(expected) && !expected.is_empty() {
        *guard = None;
        return true;
    }
    false
}

/// Constant-time comparison of the returned `state`.
pub fn state_matches(expected: &str, got: &str) -> bool {
    use subtle::ConstantTimeEq;
    expected.as_bytes().ct_eq(got.as_bytes()).into()
}

// ---------------------------------------------------------------------------
// GraphQL types
// ---------------------------------------------------------------------------

/// The shape we ask AniList for the signed-in user.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Viewer {
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub avatar_large: Option<String>,
}

/// Aggregate counters the launcher hero bar shows (all optional on the wire
/// — a fresh account legitimately reports zero).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ViewerStats {
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub avatar_large: Option<String>,
    #[serde(default)]
    pub episodes_watched: u64,
    /// Currently watching + rewatching entries.
    #[serde(default)]
    pub current: u64,
    #[serde(default)]
    pub completed: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MediaListStatus {
    Current,
    Planning,
    Completed,
    Dropped,
    Paused,
    Repeating,
}

impl MediaListStatus {
    pub fn label(self) -> &'static str {
        match self {
            MediaListStatus::Current => "İzleniyor",
            MediaListStatus::Planning => "Planlandı",
            MediaListStatus::Completed => "Tamamlandı",
            MediaListStatus::Dropped => "Bırakıldı",
            MediaListStatus::Paused => "Duraklatıldı",
            MediaListStatus::Repeating => "Yeniden izleniyor",
        }
    }

    pub fn all() -> [MediaListStatus; 6] {
        [
            MediaListStatus::Current,
            MediaListStatus::Planning,
            MediaListStatus::Completed,
            MediaListStatus::Dropped,
            MediaListStatus::Paused,
            MediaListStatus::Repeating,
        ]
    }
}

/// One row of the user's library.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryEntry {
    pub entry_id: i64,
    pub media_id: i64,
    pub status: MediaListStatus,
    pub progress: u32,
    pub title: String,
    #[serde(default)]
    pub cover: Option<String>,
    #[serde(default)]
    pub total_episodes: Option<u32>,
    #[serde(default)]
    pub average_score: Option<u32>,
}

/// A search hit, used by the "add to library" flow.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    pub media_id: i64,
    pub title: String,
    #[serde(default)]
    pub cover: Option<String>,
    #[serde(default)]
    pub episodes: Option<u32>,
    #[serde(default)]
    pub format: Option<String>,
}

/// Raw GraphQL envelope: `data` **or** `errors`, never both required.
///
/// `Deserialize` is hand-written rather than derived so that `T` needs no
/// extra bound: both fields are `Option`, so a missing `data` is simply
/// `None` and `unwrap_gql` turns that into an error.
#[derive(Debug)]
pub struct GqlResponse<T> {
    pub data: Option<T>,
    pub errors: Option<Vec<GqlError>>,
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for GqlResponse<T> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(bound(deserialize = "T: Deserialize<'de>"))]
        struct Helper<T> {
            #[serde(default)]
            data: Option<T>,
            #[serde(default)]
            errors: Option<Vec<GqlError>>,
        }
        let h = Helper::<T>::deserialize(d)?;
        Ok(GqlResponse {
            data: h.data,
            errors: h.errors,
        })
    }
}

#[derive(Debug, Deserialize)]
pub struct GqlError {
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub status: Option<u16>,
}

/// Flatten a GraphQL envelope into a result.
pub fn unwrap_gql<T>(resp: GqlResponse<T>) -> AppResult<T> {
    if let Some(d) = resp.data {
        return Ok(d);
    }
    let msg = resp
        .errors
        .and_then(|e| e.into_iter().next())
        .map(|e| {
            e.message
                .unwrap_or_else(|| "bilinmeyen GraphQL hatası".into())
        })
        .unwrap_or_else(|| "AniList boş yanıt döndürdü".into());

    // AniList reports an expired token as a GraphQL error, not an HTTP 401,
    // so match on the message as well as the status.
    let lower = msg.to_lowercase();
    let expired = lower.contains("invalid token")
        || lower.contains("unauthor")
        || lower.contains("token has expired");
    if expired {
        Err(AppError::Unauthorized)
    } else {
        Err(AppError::AniList(msg))
    }
}

// ---------------------------------------------------------------------------
// Queries (kept as constants so they are trivially auditable)
// ---------------------------------------------------------------------------

pub const Q_VIEWER: &str = r#"
query Viewer {
  Viewer {
    id
    name
    avatar { large }
  }
}"#;

/// A single GraphQL round-trip hero stats card kaynağı: toplu istatistikler.
pub const Q_VIEWER_STATS: &str = r#"
query ViewerStats {
  Viewer {
    id
    name
    avatar { large }
    statistics {
      anime {
        episodesWatched
        statuses { status count }
      }
    }
  }
}"#;

pub const Q_LIBRARY: &str = r#"
query Library($status: MediaListStatus, $page: Int) {
  Page(page: $page, perPage: 50) {
    pageInfo { hasNextPage }
    mediaList(status: $status, sort: UPDATED_TIME_DESC) {
      id
      status
      progress
      media {
        id
        averageScore
        episodes
        coverImage { large }
        title { romaji english native }
      }
    }
  }
}"#;

pub const Q_SEARCH: &str = r#"
query Search($q: String) {
  Page(page: 1, perPage: 20) {
    media(search: $q, type: ANIME, sort: SEARCH_MATCH) {
      id
      episodes
      format
      coverImage { large }
      title { romaji english native }
    }
  }
}"#;

pub const M_SAVE: &str = r#"
mutation Save($mediaId: Int, $status: MediaListStatus, $progress: Int) {
  SaveMediaListEntry(mediaId: $mediaId, status: $status, progress: $progress) {
    id
    status
    progress
  }
}"#;

pub const M_DELETE: &str = r#"
mutation Delete($id: Int) {
  DeleteMediaListEntry(id: $id) { deleted }
}"#;

/// Pick the best available title from AniList's three fields.
pub fn best_title(romaji: &str, english: &str, native: &str) -> String {
    [english, romaji, native]
        .into_iter()
        .map(str::trim)
        .find(|s| !s.is_empty())
        .unwrap_or("(başlıksız)")
        .to_string()
}

// ---------------------------------------------------------------------------
// Raw wire shapes -> domain types
// ---------------------------------------------------------------------------

mod wire {
    use serde::Deserialize;

    #[derive(Deserialize)]
    pub struct ViewerData {
        #[serde(rename = "Viewer")]
        pub viewer: ViewerInner,
    }
    #[derive(Deserialize)]
    pub struct ViewerStatsData {
        #[serde(rename = "Viewer")]
        pub viewer: ViewerStatsInner,
    }
    #[derive(Deserialize)]
    pub struct ViewerStatsInner {
        pub id: i64,
        pub name: String,
        #[serde(default)]
        pub avatar: Option<Avatar>,
        #[serde(default)]
        pub statistics: Option<UserStatistics>,
    }
    #[derive(Deserialize)]
    pub struct UserStatistics {
        #[serde(default)]
        pub anime: Option<AnimeStatistics>,
    }
    #[derive(Deserialize)]
    pub struct AnimeStatistics {
        #[serde(rename = "episodesWatched", default)]
        pub episodes_watched: u64,
        #[serde(default)]
        pub statuses: Vec<StatusCount>,
    }
    #[derive(Deserialize)]
    pub struct StatusCount {
        pub status: crate::anilist::MediaListStatus,
        #[serde(default)]
        pub count: u64,
    }
    #[derive(Deserialize)]
    pub struct ViewerInner {
        pub id: i64,
        pub name: String,
        #[serde(default)]
        pub avatar: Option<Avatar>,
    }
    #[derive(Deserialize)]
    pub struct Avatar {
        #[serde(default)]
        pub large: Option<String>,
    }

    // Two concrete page shapes rather than one generic `Page<T>`: `Vec<T>`
    // is `Default`, but `#[serde(default)]` on the field still makes serde's
    // derive demand `T: Default`, which drags the bound into every caller.
    // Keeping them concrete avoids the phantom type parameter entirely.
    #[derive(Deserialize)]
    pub struct LibraryPage {
        #[serde(rename = "Page")]
        pub page: LibraryPageInner,
    }
    #[derive(Deserialize)]
    pub struct LibraryPageInner {
        // Same trap as `hasNextPage` below: serde does not map snake_case
        // fields to camelCase keys, so this needs an explicit rename.
        #[serde(default, rename = "pageInfo")]
        pub page_info: Option<PageInfo>,
        // `default` because AniList omits the key entirely for an empty list
        // rather than returning `[]`. `Vec<T>` is `Default`, so unlike a
        // generic parameter this adds no bound to callers.
        #[serde(default, rename = "mediaList")]
        pub media_list: Vec<MediaListEntry>,
    }
    #[derive(Deserialize)]
    pub struct SearchPage {
        #[serde(rename = "Page")]
        pub page: SearchPageInner,
    }
    #[derive(Deserialize)]
    pub struct SearchPageInner {
        pub media: Vec<Media>,
    }
    #[derive(Deserialize)]
    pub struct PageInfo {
        // Named for the wire key explicitly: serde has no automatic
        // snake->camel mapping, so without this the field silently fell back
        // to its `default` and every page looked like the last one.
        #[serde(default, rename = "hasNextPage")]
        pub has_next_page: bool,
    }
    #[derive(Deserialize)]
    pub struct MediaListEntry {
        pub id: i64,
        pub status: crate::anilist::MediaListStatus,
        #[serde(default)]
        pub progress: u32,
        pub media: Media,
    }
    #[derive(Deserialize)]
    pub struct Media {
        pub id: i64,
        #[serde(default, rename = "averageScore")]
        pub average_score: Option<u32>,
        #[serde(default)]
        pub episodes: Option<u32>,
        #[serde(default, rename = "coverImage")]
        pub cover_image: Option<Avatar>,
        #[serde(default)]
        pub format: Option<String>,
        pub title: Title,
    }
    #[derive(Deserialize)]
    pub struct Title {
        #[serde(default)]
        pub romaji: Option<String>,
        #[serde(default)]
        pub english: Option<String>,
        #[serde(default)]
        pub native: Option<String>,
    }

    #[derive(Deserialize)]
    pub struct SaveData {
        #[serde(rename = "SaveMediaListEntry")]
        pub entry: SavedEntry,
    }
    #[derive(Deserialize)]
    pub struct SavedEntry {
        pub id: i64,
        pub status: crate::anilist::MediaListStatus,
        #[serde(default)]
        pub progress: u32,
    }
    #[derive(Deserialize)]
    pub struct DeleteData {
        #[serde(rename = "DeleteMediaListEntry")]
        pub entry: DeletedEntry,
    }
    #[derive(Deserialize)]
    pub struct DeletedEntry {
        #[serde(default)]
        pub deleted: bool,
    }
}

/// Convert the viewer payload.
pub fn map_viewer(d: wire::ViewerData) -> Viewer {
    Viewer {
        id: d.viewer.id,
        name: d.viewer.name,
        avatar_large: d.viewer.avatar.and_then(|a| a.large),
    }
}

/// Fold AniList's per-status counters into the three hero numbers. `current`
/// crowds both `Current` and `Repeating` — a rewatch is still "devam eden".
pub fn map_viewer_stats(d: wire::ViewerStatsData) -> ViewerStats {
    let v = d.viewer;
    let anime = v.statistics.and_then(|s| s.anime);
    let episodes_watched = anime.as_ref().map(|a| a.episodes_watched).unwrap_or(0);
    let mut current = 0u64;
    let mut completed = 0u64;
    if let Some(anime) = &anime {
        for sc in &anime.statuses {
            match sc.status {
                MediaListStatus::Current | MediaListStatus::Repeating => {
                    current = current.saturating_add(sc.count)
                }
                MediaListStatus::Completed => completed = completed.saturating_add(sc.count),
                _ => {}
            }
        }
    }
    ViewerStats {
        id: v.id,
        name: v.name,
        avatar_large: v.avatar.and_then(|a| a.large),
        episodes_watched,
        current,
        completed,
    }
}

/// Convert a library page.
pub fn map_library(d: wire::LibraryPage) -> (Vec<LibraryEntry>, bool) {
    let has_next = d.page.page_info.map(|p| p.has_next_page).unwrap_or(false);
    let entries = d
        .page
        .media_list
        .into_iter()
        .map(|e| LibraryEntry {
            entry_id: e.id,
            media_id: e.media.id,
            status: e.status,
            progress: e.progress,
            title: best_title(
                e.media.title.romaji.as_deref().unwrap_or_default(),
                e.media.title.english.as_deref().unwrap_or_default(),
                e.media.title.native.as_deref().unwrap_or_default(),
            ),
            cover: e.media.cover_image.and_then(|c| c.large),
            total_episodes: e.media.episodes,
            average_score: e.media.average_score,
        })
        .collect();
    (entries, has_next)
}

/// Convert a search page.
pub fn map_search(d: wire::SearchPage) -> Vec<SearchResult> {
    d.page
        .media
        .into_iter()
        .map(|m| SearchResult {
            media_id: m.id,
            title: best_title(
                m.title.romaji.as_deref().unwrap_or_default(),
                m.title.english.as_deref().unwrap_or_default(),
                m.title.native.as_deref().unwrap_or_default(),
            ),
            cover: m.cover_image.and_then(|c| c.large),
            episodes: m.episodes,
            format: m.format,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

pub struct AniListClient {
    http: reqwest::Client,
    cfg: AniListConfig,
}

impl AniListClient {
    pub fn new(cfg: AniListConfig) -> Self {
        AniListClient {
            http: reqwest::Client::builder()
                .user_agent(concat!("AnimeHub/", env!("CARGO_PKG_VERSION")))
                .https_only(true) // defence in depth: no plaintext fallback
                .timeout(std::time::Duration::from_secs(20))
                .build()
                .expect("valid client config"),
            cfg,
        }
    }

    /// Exchange an authorization code for a token.
    pub async fn exchange_code(&self, code: &str) -> AppResult<Token> {
        let mut form = vec![
            ("grant_type", "authorization_code"),
            ("client_id", self.cfg.client_id.trim()),
            ("redirect_uri", self.cfg.redirect_uri.trim()),
            ("code", code),
        ];
        if self.cfg.is_confidential() {
            form.push(("client_secret", self.cfg.client_secret.trim()));
        }

        let resp = self
            .http
            .post(ANILIST_TOKEN)
            .header("Accept", "application/json")
            .form(&form)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let body: OAuthError = resp.json().await.unwrap_or(OAuthError {
                error: None,
                error_description: None,
                message: None,
            });
            let code_str = body.error.unwrap_or_else(|| "unknown".into());
            return Err(AppError::AniList(
                oauth_error_message(&code_str).to_string(),
            ));
        }

        let body: TokenResponse = resp.json().await?;
        Ok(body.into_token(now_unix()))
    }

    /// Refresh an access token.
    pub async fn refresh(&self, refresh_token: &str) -> AppResult<Token> {
        let mut form = vec![
            ("grant_type", "refresh_token"),
            ("client_id", self.cfg.client_id.trim()),
            ("refresh_token", refresh_token),
        ];
        if self.cfg.is_confidential() {
            form.push(("client_secret", self.cfg.client_secret.trim()));
        }

        let resp = self
            .http
            .post(ANILIST_TOKEN)
            .header("Accept", "application/json")
            .form(&form)
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(AppError::Unauthorized);
        }
        let body: TokenResponse = resp.json().await?;
        Ok(body.into_token(now_unix()))
    }

    async fn gql<T: serde::de::DeserializeOwned>(
        &self,
        token: &Token,
        query: &str,
        vars: serde_json::Value,
    ) -> AppResult<T> {
        let resp = self
            .http
            .post(ANILIST_GRAPHQL)
            .bearer_auth(token.access_token.trim())
            .json(&serde_json::json!({ "query": query, "variables": vars }))
            .send()
            .await?;

        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(AppError::Unauthorized);
        }
        let env: GqlResponse<T> = resp.json().await?;
        unwrap_gql(env)
    }

    pub async fn viewer(&self, token: &Token) -> AppResult<Viewer> {
        let d: wire::ViewerData = self.gql(token, Q_VIEWER, serde_json::json!({})).await?;
        Ok(map_viewer(d))
    }

    pub async fn viewer_stats(&self, token: &Token) -> AppResult<ViewerStats> {
        let d: wire::ViewerStatsData = self
            .gql(token, Q_VIEWER_STATS, serde_json::json!({}))
            .await?;
        Ok(map_viewer_stats(d))
    }

    pub async fn library(
        &self,
        token: &Token,
        status: Option<MediaListStatus>,
        page: u32,
    ) -> AppResult<(Vec<LibraryEntry>, bool)> {
        let vars = serde_json::json!({ "status": status, "page": page.max(1) });
        let d: wire::LibraryPage = self.gql(token, Q_LIBRARY, vars).await?;
        Ok(map_library(d))
    }

    pub async fn search(&self, token: &Token, query: &str) -> AppResult<Vec<SearchResult>> {
        let vars = serde_json::json!({ "q": query });
        let d: wire::SearchPage = self.gql(token, Q_SEARCH, vars).await?;
        Ok(map_search(d))
    }

    pub async fn save_entry(
        &self,
        token: &Token,
        media_id: i64,
        status: MediaListStatus,
        progress: u32,
    ) -> AppResult<(i64, MediaListStatus, u32)> {
        let vars = serde_json::json!({
            "mediaId": media_id,
            "status": status,
            "progress": progress,
        });
        let d: wire::SaveData = self.gql(token, M_SAVE, vars).await?;
        Ok((d.entry.id, d.entry.status, d.entry.progress))
    }

    pub async fn delete_entry(&self, token: &Token, entry_id: i64) -> AppResult<bool> {
        let d: wire::DeleteData = self
            .gql(token, M_DELETE, serde_json::json!({ "id": entry_id }))
            .await?;
        Ok(d.entry.deleted)
    }

    pub fn config(&self) -> &AniListConfig {
        &self.cfg
    }
}

/// A one-shot listener for the OAuth redirect on desktop.
///
/// Binds `127.0.0.1` only, accepts exactly one request, answers with a static
/// HTML page (no user data reflected), then shuts down. Reflecting the
/// callback query back into the response is the classic open-redirect/XSS
/// mistake here, so the page is a constant string.
pub struct LoopbackListener {
    listener: tokio::net::TcpListener,
    redirect_uri: String,
}

/// The only path [`LoopbackListener`] answers.
pub const LOOPBACK_CALLBACK_PATH: &str = "/callback";

/// Port of a redirect URI the local listener can actually serve.
///
/// This used to accept ANY `http` URI on `127.0.0.1`/`localhost` and answer
/// `port_or_known_default()`, so `http://127.0.0.1/cb`, `…:17395/anything`,
/// `http://localhost:17395/callback` and even a URI with a query, a fragment
/// or userinfo all "looked like" a loopback redirect. The listener binds
/// `127.0.0.1:<port>` and answers only `/callback`, so every one of those
/// forms produced a login that was started (pending state written, browser
/// opened) and could never be completed: the redirect landed on a path or
/// host the listener refused, or on port 80 where nothing listens.
///
/// The rule here is therefore EXACTLY "what `bind_port` serves":
/// `http://127.0.0.1:<explicit port>/callback`, nothing else.
pub fn loopback_port(redirect_uri: &str) -> Option<u16> {
    let url = url::Url::parse(redirect_uri).ok()?;
    if url.scheme() != "http" {
        return None;
    }
    // `bind_port` listens on 127.0.0.1 only: `localhost` may resolve to ::1,
    // and `[::1]` is not the bound address either.
    if url.host_str() != Some("127.0.0.1") {
        return None;
    }
    // An omitted port means 80. We never bind 80 (privileged) and the
    // registered redirect always carries a non-default port, so require one.
    let port = url.port()?;
    if port == 0 {
        return None;
    }
    if url.path() != LOOPBACK_CALLBACK_PATH {
        return None;
    }
    if url.query().is_some() || url.fragment().is_some() {
        return None;
    }
    if !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    Some(port)
}

/// How an authorised login will be completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginRedirect {
    /// A local listener on this port receives the redirect.
    Loopback(u16),
    /// The OS delivers `animehub://anilist/callback` to the deep-link handler.
    CustomScheme,
}

/// Validate the configured redirect URI BEFORE a pending OAuth state exists.
///
/// Both this and the callback parser ([`parse_callback`]) must agree exactly:
/// accepting a redirect here that the callback parser (or the listener) then
/// rejects produces a login that burns a state value and can never finish.
/// * loopback form → [`LoginRedirect::Loopback`], served by
///   [`LoopbackListener::bind_port`];
/// * `animehub://anilist/callback` → [`LoginRedirect::CustomScheme`], allowed
///   on desktop only (another app could register the scheme on Android, which
///   is why `allow_custom_scheme` is false there);
/// * anything else (`https://`, a remote host, a missing port, another path,
///   a query, a fragment, userinfo) → error, and the caller must not write
///   `pending state` or build an authorization URL.
pub fn validate_redirect_uri(
    redirect_uri: &str,
    allow_custom_scheme: bool,
) -> AppResult<LoginRedirect> {
    if let Some(port) = loopback_port(redirect_uri) {
        return Ok(LoginRedirect::Loopback(port));
    }
    let url = url::Url::parse(redirect_uri)?;
    let is_expected_custom_scheme = url.scheme() == "animehub"
        && url.host_str() == Some("anilist")
        && url.path() == "/callback"
        && url.port().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && url.username().is_empty()
        && url.password().is_none();
    if is_expected_custom_scheme && allow_custom_scheme {
        return Ok(LoginRedirect::CustomScheme);
    }
    if is_expected_custom_scheme {
        return Err(AppError::AniList(
            ANDROID_CUSTOM_SCHEME_OAUTH_REFUSED.into(),
        ));
    }
    Err(AppError::AniList(
        "dönüş adresi desteklenmiyor: yalnızca http://127.0.0.1:<port>/callback \
         veya animehub://anilist/callback kullanılabilir"
            .into(),
    ))
}

impl LoopbackListener {
    /// Random-port variant, used by unit tests.
    pub async fn bind() -> AppResult<Self> {
        Self::bind_port(0).await
    }

    /// Bind the loopback listener on a concrete port. The port must match the
    /// redirect URI registered at AniList, otherwise the browser will never
    /// reach us after the user grants access.
    pub async fn bind_port(port: u16) -> AppResult<Self> {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
        let port = listener.local_addr()?.port();
        Ok(LoopbackListener {
            listener,
            redirect_uri: format!("http://127.0.0.1:{port}/callback"),
        })
    }

    /// The redirect URI to register with AniList for this run.
    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// Wait for the redirect request. Returns the raw request target.
    ///
    /// The listener survives anything else that shows up on the port: a
    /// speculative browser preconnect, a favicon probe, a fragmented request
    /// line, a silent or oversized client. Each of those is answered and closed,
    /// and the wait continues until the real callback arrives or `timeout`
    /// expires. It used to accept exactly ONE connection and read it with a
    /// single `read`, so the first unrelated socket — or a request line that
    /// arrived in two segments — consumed the whole authorisation.
    pub async fn wait_for_callback(&self, timeout: std::time::Duration) -> AppResult<String> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(AppError::AniList(
                    "yetkilendirme zaman aşımına uğradı".into(),
                ));
            }
            match tokio::time::timeout(remaining, self.accept_one()).await {
                Ok(Ok(Accepted::Callback(target))) => return Ok(target),
                Ok(Ok(Accepted::Ignored)) => continue,
                Ok(Err(e)) => {
                    return Err(AppError::AniList(format!("geri çağırma dinlenemedi: {e}")));
                }
                Err(_) => {
                    return Err(AppError::AniList(
                        "yetkilendirme zaman aşımına uğradı".into(),
                    ));
                }
            }
        }
    }

    /// The path the registered redirect URI points at (`/callback`).
    fn callback_path(&self) -> String {
        url::Url::parse(&self.redirect_uri)
            .map(|url| url.path().to_string())
            .unwrap_or_else(|_| "/callback".to_string())
    }

    /// Handle one connection: read the header block within a bounded budget,
    /// answer it, and say whether it was the callback.
    async fn accept_one(&self) -> AppResult<Accepted> {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let (mut sock, _peer) = self.listener.accept().await?;

        // Incremental, bounded, time-bounded. One `read` is not guaranteed to
        // carry the whole request line, and a truncated target used to fail an
        // otherwise valid authorisation; an unbounded read would let any local
        // process make us buffer forever.
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 1024];
        let mut complete = false;
        let read_deadline = tokio::time::Instant::now() + CONNECTION_READ_BUDGET;
        while buf.len() < MAX_REQUEST_BYTES {
            let remaining = read_deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, sock.read(&mut chunk)).await {
                Ok(Ok(0)) => break, // peer closed without finishing
                Ok(Ok(n)) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.windows(4).any(|window| window == b"\r\n\r\n") {
                        complete = true;
                        break;
                    }
                }
                // A slow or broken client loses its connection, not the flow.
                Ok(Err(_)) | Err(_) => break,
            }
        }
        let oversized = !complete && buf.len() >= MAX_REQUEST_BYTES;

        let req = String::from_utf8_lossy(&buf);
        let mut parts = req.split_whitespace();
        let method = parts.next().unwrap_or("");
        let target = parts.next().unwrap_or("");
        // Only the registered redirect path counts, so a probe cannot consume
        // the authorisation and the completion page is still only ever served
        // for the real callback.
        let is_callback = complete
            && !oversized
            && method == "GET"
            && target.split('?').next().unwrap_or("") == self.callback_path();

        let (status, body) = if is_callback {
            ("200 OK", CALLBACK_HTML)
        } else if oversized {
            ("413 Payload Too Large", "")
        } else {
            ("404 Not Found", "")
        };
        let resp = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = sock.write_all(resp.as_bytes()).await;
        let _ = sock.flush().await;
        if !complete {
            // The peer is still sending (a flood, or a client that never
            // finished its headers). Drain briefly and discard so the close is
            // a FIN rather than a RST — a reset would throw our own answer away
            // on the client side. Bounded by the budget, never by the peer.
            let drain_deadline = tokio::time::Instant::now() + RESPONSE_DRAIN_BUDGET;
            loop {
                let remaining =
                    drain_deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    break;
                }
                match tokio::time::timeout(remaining, sock.read(&mut chunk)).await {
                    Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
                    Ok(Ok(_)) => continue,
                }
            }
        }
        let _ = sock.shutdown().await;

        Ok(if is_callback {
            Accepted::Callback(target.to_string())
        } else {
            Accepted::Ignored
        })
    }
}

/// Hard cap on one request's header block. A browser OAuth callback is a few
/// hundred bytes; anything larger is not our request and is never buffered
/// further.
const MAX_REQUEST_BYTES: usize = 8 * 1024;

/// Per-connection budget for the header block, so one silent client cannot
/// consume the whole authorisation window.
const CONNECTION_READ_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);

/// Budget for discarding what a refused peer is still sending, so our answer
/// reaches it instead of being lost to a reset.
const RESPONSE_DRAIN_BUDGET: std::time::Duration = std::time::Duration::from_millis(300);

/// What one accepted connection turned out to be.
enum Accepted {
    /// The OAuth redirect, as the raw request target (`/callback?code=…`).
    Callback(String),
    /// Anything else: answered, closed, and the wait continues.
    Ignored,
}

/// Static completion page. Contains no reflected input.
pub const CALLBACK_HTML: &str = "<!doctype html><html lang=\"tr\"><meta charset=\"utf-8\">\
<title>AnimeHub</title><body style=\"font-family:system-ui;text-align:center;padding:3rem\">\
<h1>AnimeHub</h1><p>AniList yetkilendirmesi tamamlandı. Bu sekmeyi kapatabilirsiniz.</p>\
</body></html>";

/// Reassemble an absolute callback URL from a request target.
pub fn absolute_callback(redirect_uri: &str, target: &str) -> AppResult<String> {
    let base = url::Url::parse(redirect_uri)?;
    let joined = base.join(target)?;
    Ok(joined.to_string())
}

/// Zeroing wrapper for the config's secret.
pub fn zeroize_secret(s: &mut String) {
    let _ = Zeroizing::new(std::mem::take(s));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// The desktop loopback redirect the deep-link/loopback flow starts with
    /// (`DEFAULT_REDIRECT_URI`).
    const LOOPBACK: &str = "http://127.0.0.1:17395/callback";
    /// The registered Android custom-scheme redirect (tauri.conf.json
    /// `plugins.deep-link.mobile`).
    const SCHEME: &str = "animehub://anilist/callback";

    #[test]
    fn viewer_stats_folds_statuses_into_hero_numbers() {
        let d: wire::ViewerStatsData = serde_json::from_value(serde_json::json!({
            "Viewer": {
                "id": 7,
                "name": "kage",
                "avatar": { "large": "https://x/a.png" },
                "statistics": { "anime": {
                    "episodesWatched": 142,
                    "statuses": [
                        { "status": "CURRENT", "count": 4 },
                        { "status": "REPEATING", "count": 2 },
                        { "status": "COMPLETED", "count": 23 },
                        { "status": "PLANNING", "count": 9 },
                        { "status": "DROPPED", "count": 1 }
                    ]
                } }
            }
        }))
        .unwrap();
        let s = map_viewer_stats(d);
        assert_eq!(s.name, "kage");
        assert_eq!(s.episodes_watched, 142);
        assert_eq!(s.current, 6, "current + repeating");
        assert_eq!(s.completed, 23);
        assert_eq!(s.avatar_large.as_deref(), Some("https://x/a.png"));
    }

    #[test]
    fn viewer_stats_defaults_to_zero_for_fresh_accounts() {
        let d: wire::ViewerStatsData = serde_json::from_value(serde_json::json!({
            "Viewer": { "id": 1, "name": "yeni" }
        }))
        .unwrap();
        let s = map_viewer_stats(d);
        assert_eq!(s.episodes_watched, 0);
        assert_eq!(s.current, 0);
        assert_eq!(s.completed, 0);
        assert!(s.avatar_large.is_none());
    }

    fn cfg() -> AniListConfig {
        AniListConfig {
            client_id: "1234".into(),
            client_secret: String::new(),
            redirect_uri: "http://127.0.0.1:17395/callback".into(),
        }
    }

    #[test]
    fn default_config_is_unconfigured_without_env() {
        let c = AniListConfig::default();
        // In CI/test there is no ANIMEHUB_ANILIST_CLIENT_ID, so this must be
        // false — the app must not silently talk to AniList with an empty id.
        if std::env::var("ANIMEHUB_ANILIST_CLIENT_ID").is_err() {
            assert!(!c.is_configured());
            assert!(!c.is_confidential());
        }
    }

    #[test]
    fn authorize_url_carries_required_params() {
        let u = authorize_url(&cfg(), "st4te").unwrap();
        let parsed = url::Url::parse(&u).unwrap();
        assert_eq!(parsed.host_str(), Some("anilist.co"));
        assert_eq!(parsed.path(), "/api/v2/oauth/authorize");
        let q: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(q["client_id"], "1234");
        assert_eq!(q["redirect_uri"], "http://127.0.0.1:17395/callback");
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["state"], "st4te");
    }

    #[test]
    fn authorize_url_needs_client_id() {
        let c = AniListConfig {
            client_id: "  ".into(),
            ..cfg()
        };
        assert_eq!(authorize_url(&c, "s").unwrap_err().code(), "anilist");
    }

    #[test]
    fn authorize_url_needs_state() {
        assert!(authorize_url(&cfg(), "").is_err());
    }

    #[test]
    fn parse_callback_extracts_code_and_state() {
        let (code, state) = parse_callback(
            "http://127.0.0.1:17395/callback?code=abc&state=s1",
            LOOPBACK,
        )
        .unwrap();
        assert_eq!(code, "abc");
        assert_eq!(state, "s1");
    }

    #[test]
    fn parse_callback_accepts_custom_scheme() {
        let (code, _) =
            parse_callback("animehub://anilist/callback?code=abc&state=s1", SCHEME).unwrap();
        assert_eq!(code, "abc");
    }

    #[test]
    fn parse_callback_rejects_remote_http_hosts() {
        let err =
            parse_callback("http://evil.example/callback?code=abc&state=s1", LOOPBACK).unwrap_err();
        assert_eq!(err.code(), "anilist");
        assert!(err.to_string().contains("yerel adresten"), "{err}");
    }

    #[test]
    fn parse_callback_rejects_unknown_scheme() {
        assert!(parse_callback("https://anilist.co/cb?code=a&state=b", LOOPBACK).is_err());
        assert!(parse_callback("ftp://127.0.0.1/callback?code=a&state=b", LOOPBACK).is_err());
        // The configured redirect is the desktop loopback one: the same path on
        // a remote https origin is not it, either way round.
        assert!(parse_callback("http://127.0.0.1:17395/callback?code=a&state=b", SCHEME).is_err());
    }

    #[test]
    fn parse_callback_surfaces_oauth_error() {
        let err = parse_callback(
            "http://127.0.0.1:17395/callback?error=access_denied&state=s",
            LOOPBACK,
        )
        .unwrap_err();
        assert!(err.to_string().contains("reddedildi"), "{err}");
        // ... and on the registered custom scheme too.
        let err = parse_callback(
            "animehub://anilist/callback?error=access_denied&state=s",
            SCHEME,
        )
        .unwrap_err();
        assert!(err.to_string().contains("reddedildi"), "{err}");
    }

    #[test]
    fn parse_callback_requires_both_fields() {
        assert!(parse_callback("http://127.0.0.1:17395/callback?code=a", LOOPBACK).is_err());
        assert!(parse_callback("http://127.0.0.1:17395/callback?state=b", LOOPBACK).is_err());
        assert!(parse_callback(SCHEME, SCHEME).is_err());
    }

    // ---- the exact-redirect-URI requirement (AH-07 / AH-G1 / AH-G4 / AH-G5 / AH-G11) ----

    #[test]
    fn parse_callback_rejects_a_different_loopback_port_or_path() {
        // Same scheme, same loopback host — but not OUR endpoint. The old
        // parser accepted any loopback port and any path, which is a bigger
        // target than the listener that started the flow.
        let wrong_port = "http://127.0.0.1:17396/callback?code=abc&state=s1";
        assert!(parse_callback(wrong_port, LOOPBACK).is_err());
        let wrong_path = "http://127.0.0.1:17395/other?code=abc&state=s1";
        let err = parse_callback(wrong_path, LOOPBACK).unwrap_err();
        assert!(err.to_string().contains("yolu beklenenden"), "{err}");
        // The default port is not our 17395 either.
        assert!(parse_callback("http://127.0.0.1/callback?code=a&state=b", LOOPBACK).is_err());
    }

    #[test]
    fn parse_callback_rejects_a_different_loopback_host_form() {
        // localhost and [::1] are loopback, but they are not the configured
        // host, and the OS resolves each of them independently — an attacker
        // controlling one must not satisfy the other's login.
        assert!(
            parse_callback("http://localhost:17395/callback?code=a&state=b", LOOPBACK).is_err()
        );
        assert!(parse_callback("http://[::1]:17395/callback?code=a&state=b", LOOPBACK).is_err());
        // A non-loopback host that merely contains the loopback name is out too.
        assert!(parse_callback(
            "http://127.0.0.1.evil.example:17395/callback?code=a&state=b",
            LOOPBACK
        )
        .is_err());
    }

    #[test]
    fn parse_callback_rejects_a_hijacked_custom_scheme() {
        // Another app can register `animehub://`; only OUR host+path pair is a
        // callback for the flow this build started.
        let err = parse_callback("animehub://evil/callback?code=abc&state=s1", SCHEME).unwrap_err();
        assert!(err.to_string().contains("host'u beklenenden"), "{err}");
        assert!(parse_callback("animehub://anilist/other?code=abc&state=s1", SCHEME).is_err());
        assert!(parse_callback("ANIMEHUB://anilist/callback?code=abc&state=s1", SCHEME).is_ok());
    }

    #[test]
    fn parse_callback_rejects_an_explicit_port_on_the_custom_scheme() {
        // The loopback branch always compared ports; the custom-scheme branch
        // did not, so `animehub://anilist:1234/callback` was "our" redirect.
        let err =
            parse_callback("animehub://anilist:1234/callback?code=a&state=s", SCHEME).unwrap_err();
        assert!(err.to_string().contains("port'u beklenenden"), "{err}");
        assert!(parse_callback("animehub://anilist:0/callback?code=a&state=s", SCHEME).is_err());
        // The registered form still works, and the loopback port rule is
        // unchanged in both directions.
        assert!(parse_callback("animehub://anilist/callback?code=a&state=s", SCHEME).is_ok());
        assert!(
            parse_callback("http://127.0.0.1:17396/callback?code=a&state=s", LOOPBACK).is_err()
        );
        assert!(parse_callback("http://127.0.0.1:17395/callback?code=a&state=s", LOOPBACK).is_ok());
    }

    #[test]
    fn parse_callback_rejects_userinfo_and_fragments() {
        // Both forms carry the right host and path, which is exactly why the
        // comparison has to look at the whole URI.
        for raw in [
            "http://user:pass@127.0.0.1:17395/callback?code=a&state=s",
            "http://user@127.0.0.1:17395/callback?code=a&state=s",
            "animehub://user:pass@anilist/callback?code=a&state=s",
            "animehub://attacker@anilist/callback?code=a&state=s",
        ] {
            let expected = if raw.starts_with("animehub") {
                SCHEME
            } else {
                LOOPBACK
            };
            let err = parse_callback(raw, expected).unwrap_err();
            assert!(
                err.to_string().contains("kullanıcı bilgisi"),
                "{raw} → {err}"
            );
        }
        for raw in [
            "http://127.0.0.1:17395/callback?code=a&state=s#extra",
            "http://127.0.0.1:17395/callback#code=a&state=s",
            "animehub://anilist/callback?code=a&state=s#extra",
            "animehub://anilist/callback#anything",
        ] {
            let expected = if raw.starts_with("animehub") {
                SCHEME
            } else {
                LOOPBACK
            };
            let err = parse_callback(raw, expected).unwrap_err();
            assert!(err.to_string().contains("fragment"), "{raw} → {err}");
        }
        // An empty fragment (`#`) is a fragment too: `Url::fragment()` answers
        // Some("") and the registered redirect has none.
        assert!(
            parse_callback("http://127.0.0.1:17395/callback?code=a&state=s#", LOOPBACK).is_err()
        );

        // Empty userinfo is deliberately NOT in the rejected list above:
        // WHATWG parsing drops a `:@` whose username and password are both
        // empty, so the URL that reaches the comparison is byte-for-byte the
        // registered redirect. Asserting a rejection would pin a difference
        // that does not exist; asserting acceptance documents what the parser
        // actually hands us.
        assert!(
            parse_callback("http://:@127.0.0.1:17395/callback?code=a&state=s", LOOPBACK).is_ok()
        );
    }

    #[test]
    fn parse_callback_refuses_a_configured_redirect_with_userinfo_or_fragment() {
        // The same rule applies to the configured redirect: a hostile or merely
        // broken ANILIST_REDIRECT_URI must fail closed, not widen the check.
        let callback = "http://127.0.0.1:17395/callback?code=a&state=s";
        assert!(parse_callback(callback, "http://u:p@127.0.0.1:17395/callback").is_err());
        assert!(parse_callback(callback, "http://127.0.0.1:17395/callback#f").is_err());
        assert!(parse_callback(callback, LOOPBACK).is_ok());
    }

    #[test]
    fn parse_callback_rejects_present_but_empty_fields() {
        // A partial or hand-built request (`?code=&state=`) must fail rather
        // than hand out an empty code/state pair.
        assert!(parse_callback("http://127.0.0.1:17395/callback?code=&state=", LOOPBACK).is_err());
        assert!(parse_callback("http://127.0.0.1:17395/callback?code=a&state=", LOOPBACK).is_err());
        assert!(parse_callback("animehub://anilist/callback?code=&state=s", SCHEME).is_err());
    }

    #[test]
    fn parse_callback_rejects_a_truncated_request_target() {
        // What a half-read or mangled callback looks like in practice.
        assert!(parse_callback("", LOOPBACK).is_err());
        assert!(parse_callback("http://127.0.0.1:17395", LOOPBACK).is_err());
        assert!(parse_callback("/callback?code=a&state=b", LOOPBACK).is_err());
        assert!(parse_callback("http://127.0.0.1:17395/callback?", LOOPBACK).is_err());
        assert!(
            parse_callback("http://127.0.0.1:17395/callback#code=a&state=b", LOOPBACK).is_err()
        );
    }

    #[test]
    fn parse_callback_rejects_duplicated_code_or_state() {
        // One callback carries exactly one code and one state. A repeated
        // parameter is how a relayed or hand-built callback smuggles in a
        // value the first check did not see.
        let dup_code = "http://127.0.0.1:17395/callback?code=good&code=bad&state=s1";
        let err = parse_callback(dup_code, LOOPBACK).unwrap_err();
        assert!(err.to_string().contains("birden fazla code"), "{err}");
        let dup_state = "http://127.0.0.1:17395/callback?code=a&state=s1&state=s2";
        let err = parse_callback(dup_state, LOOPBACK).unwrap_err();
        assert!(err.to_string().contains("birden fazla state"), "{err}");
        // Unknown parameters stay ignored, as before.
        let extra = "http://127.0.0.1:17395/callback?code=a&state=s1&scope=user";
        assert!(parse_callback(extra, LOOPBACK).is_ok());
    }

    #[test]
    fn parse_callback_refuses_a_broken_configured_redirect() {
        // A nonsensical ANILIST_REDIRECT_URI must fail closed rather than
        // silently widening the check.
        assert!(parse_callback(LOOPBACK, "not a uri").is_err());
    }

    #[test]
    fn mobile_native_login_requires_a_loopback_redirect() {
        // A registered custom scheme is claimable by another app; the OS hands
        // it the code and the state, so this must never be a login redirect.
        assert!(!mobile_native_login_allowed("animehub://anilist/callback"));
        assert!(!mobile_native_login_allowed("animehub://attacker/callback"));
        assert!(!mobile_native_login_allowed(
            "https://127.0.0.1:17395/callback"
        ));
        assert!(!mobile_native_login_allowed("http://example.com/callback"));
        assert!(!mobile_native_login_allowed("not a url"));
        assert!(mobile_native_login_allowed(
            "http://127.0.0.1:17395/callback"
        ));
        // `localhost` is NOT enough: `bind_port` listens on 127.0.0.1 only and
        // localhost may resolve to ::1 first, so a login armed for it could
        // never be completed. Refusing is the same fixed error as above.
        assert!(!mobile_native_login_allowed(
            "http://localhost:17395/callback"
        ));
        // The refusal is fixed text: it must never echo the hostile redirect.
        assert!(
            ANDROID_CUSTOM_SCHEME_OAUTH_REFUSED.contains("loopback")
                || ANDROID_CUSTOM_SCHEME_OAUTH_REFUSED.contains("127.0.0.1")
        );
    }

    // -----------------------------------------------------------------------
    // Redirect URI validation (must match what the listener serves)
    // -----------------------------------------------------------------------

    #[test]
    fn loopback_port_accepts_only_the_listener_endpoint() {
        assert_eq!(
            loopback_port("http://127.0.0.1:17395/callback"),
            Some(17395)
        );
        // Every form below used to pass the old prefix-only check and then
        // fail at the listener: the login burned its state and never finished.
        for bad in [
            "http://127.0.0.1:17395/cb", // listener answers /callback
            "http://127.0.0.1:17395/callback/x",
            "http://127.0.0.1/callback", // port 80, nothing bound
            "http://127.0.0.1:17395/callback?a=1", // query on the registered URI
            "http://127.0.0.1:17395/callback#f", // fragment
            "http://user@127.0.0.1:17395/callback", // userinfo
            "http://user:pw@127.0.0.1:17395/callback",
            "http://localhost:17395/callback", // listener binds 127.0.0.1
            "http://[::1]:17395/callback",
            "http://127.0.0.1:0/callback",
            "https://127.0.0.1:17395/callback", // listener is plain http
            "animehub://anilist/callback",
            "not a url",
        ] {
            assert_eq!(
                loopback_port(bad),
                None,
                "{bad} must not be a loopback redirect"
            );
        }
    }

    #[test]
    fn validate_redirect_uri_rejects_before_state_is_written() {
        // Accepted forms.
        assert_eq!(
            validate_redirect_uri("http://127.0.0.1:17395/callback", false).unwrap(),
            LoginRedirect::Loopback(17395)
        );
        assert_eq!(
            validate_redirect_uri("animehub://anilist/callback", true).unwrap(),
            LoginRedirect::CustomScheme
        );
        // Custom scheme on Android (allow=false) is refused, not downgraded.
        let refused = validate_redirect_uri("animehub://anilist/callback", false).unwrap_err();
        assert_eq!(refused.code(), "anilist");
        // The refusal names the only accepted form and never echoes the
        // configured redirect (it may carry a hostile host or credentials).
        assert!(refused.to_string().contains("127.0.0.1"));
        assert!(!refused.to_string().contains("animehub://anilist/callback"));
        // Unsupported/insecure forms are refused for both platforms.
        for bad in [
            "https://anilist.co/callback",
            "http://evil.example:17395/callback",
            "http://127.0.0.1:17395/cb",
            "http://127.0.0.1/callback",
            "animehub://evil/callback",
            "animehub://anilist:1234/callback",
            "ftp://127.0.0.1:17395/callback",
        ] {
            assert!(
                validate_redirect_uri(bad, true).is_err(),
                "{bad} must be refused before a pending state is written"
            );
        }
    }

    #[test]
    fn abandoning_a_pending_state_clears_only_its_own_attempt() {
        let slot: Mutex<Option<String>> = Mutex::new(Some("ours".to_string()));
        assert!(abandon_pending_state(&slot, "ours"));
        assert!(
            slot.lock().unwrap().is_none(),
            "a failed attempt must not leave an armed state behind"
        );
        // Nothing pending: a second abandon is a no-op, not a panic.
        assert!(!abandon_pending_state(&slot, "ours"));

        // A newer attempt owns the slot: never clear somebody else's login.
        let slot: Mutex<Option<String>> = Mutex::new(Some("newer".to_string()));
        assert!(!abandon_pending_state(&slot, "ours"));
        assert_eq!(slot.lock().unwrap().as_deref(), Some("newer"));

        // An empty expected value is never a match.
        let slot: Mutex<Option<String>> = Mutex::new(Some("".to_string()));
        assert!(!abandon_pending_state(&slot, ""));
    }

    #[test]
    fn take_pending_state_is_single_use_and_only_consumes_on_a_match() {
        let slot: Mutex<Option<String>> = Mutex::new(Some("st4te".to_string()));

        // A bogus callback does NOT burn the genuine attempt...
        assert_eq!(
            take_pending_state(&slot, "nope").unwrap(),
            PendingState::Mismatched
        );
        assert_eq!(slot.lock().unwrap().as_deref(), Some("st4te"));
        // ... and neither does a malformed one.
        assert_eq!(
            take_pending_state(&slot, "").unwrap(),
            PendingState::Mismatched
        );
        assert_eq!(slot.lock().unwrap().as_deref(), Some("st4te"));

        // The matching callback consumes it exactly once.
        assert_eq!(
            take_pending_state(&slot, "st4te").unwrap(),
            PendingState::Consumed("st4te".to_string())
        );
        assert!(slot.lock().unwrap().is_none());
        // A replay of the same callback now finds no pending session instead of
        // racing a second token exchange.
        assert_eq!(
            take_pending_state(&slot, "st4te").unwrap(),
            PendingState::None
        );
    }

    #[test]
    fn take_pending_state_never_matches_an_empty_slot() {
        let slot: Mutex<Option<String>> = Mutex::new(Some(String::new()));
        assert_eq!(take_pending_state(&slot, "").unwrap(), PendingState::None);
        assert_eq!(
            take_pending_state(&slot, "st4te").unwrap(),
            PendingState::None
        );
    }

    #[test]
    fn take_pending_state_reports_no_pending_session() {
        let slot: Mutex<Option<String>> = Mutex::new(None);
        assert_eq!(
            take_pending_state(&slot, "anything").unwrap(),
            PendingState::None
        );
    }

    #[test]
    fn loopback_port_accepts_only_the_exact_listener_endpoint() {
        assert_eq!(
            loopback_port("http://127.0.0.1:17395/callback"),
            Some(17395)
        );
        // Everything the old rule waved through is refused here: `localhost`
        // (may resolve to ::1, never bound), another path (`/cb`), and an
        // omitted port (which would mean 80, never bound — and the registered
        // redirect always carries a port).
        assert_eq!(loopback_port("http://localhost:8080/callback"), None);
        assert_eq!(loopback_port("http://127.0.0.1:8080/cb"), None);
        assert_eq!(loopback_port("http://127.0.0.1/callback"), None);
    }

    #[test]
    fn loopback_port_rejects_everything_else() {
        assert_eq!(loopback_port("animehub://anilist/callback"), None);
        assert_eq!(loopback_port("http://example.com:8080/cb"), None);
        assert_eq!(loopback_port("https://127.0.0.1:443/cb"), None);
        assert_eq!(loopback_port("hiç-url-değil"), None);
    }

    #[test]
    fn state_comparison_is_exact() {
        assert!(state_matches("abc", "abc"));
        assert!(!state_matches("abc", "abd"));
        assert!(!state_matches("abc", "abcd"));
        assert!(!state_matches("", "x"));
    }

    #[test]
    fn token_expiry_uses_skew() {
        let t = Token {
            access_token: "a".into(),
            refresh_token: None,
            token_type: "Bearer".into(),
            expires_at: 1000,
            scope: None,
        };
        assert!(!t.needs_refresh(900));
        assert!(!t.needs_refresh(939));
        assert!(t.needs_refresh(940), "60s skew must apply");
        assert!(t.needs_refresh(2000));
        assert_eq!(t.authorization_header(), "Bearer a");
    }

    #[test]
    fn token_response_maps_to_token() {
        let r = TokenResponse {
            access_token: "tok".into(),
            refresh_token: Some("ref".into()),
            token_type: "Bearer".into(),
            expires_in: 3600,
            scope: None,
        };
        let t = r.into_token(1000);
        assert_eq!(t.expires_at, 4600);
        assert_eq!(t.refresh_token.as_deref(), Some("ref"));
    }

    #[test]
    fn token_response_defaults_are_sane() {
        let json = r#"{"access_token":"t"}"#;
        let r: TokenResponse = serde_json::from_str(json).unwrap();
        let t = r.into_token(0);
        assert_eq!(t.token_type, "Bearer");
        assert_eq!(t.expires_at, 3600);
    }

    #[test]
    fn negative_expires_in_does_not_underflow() {
        let r = TokenResponse {
            access_token: "t".into(),
            refresh_token: None,
            token_type: "Bearer".into(),
            expires_in: -5,
            scope: None,
        };
        assert_eq!(r.into_token(100).expires_at, 100);
    }

    #[test]
    fn oauth_error_messages_are_localised() {
        assert!(oauth_error_message("invalid_grant").contains("geçersiz"));
        assert!(oauth_error_message("nope").contains("başarısız"));
    }

    #[test]
    fn unwrap_gql_returns_data() {
        let r: GqlResponse<u32> = serde_json::from_str(r#"{"data":7}"#).unwrap();
        assert_eq!(unwrap_gql(r).unwrap(), 7);
    }

    #[test]
    fn unwrap_gql_maps_401_to_unauthorized() {
        let r: GqlResponse<u32> =
            serde_json::from_str(r#"{"errors":[{"message":"Invalid token","status":400}]}"#)
                .unwrap();
        assert_eq!(unwrap_gql(r).unwrap_err().code(), "unauthorized");
    }

    #[test]
    fn unwrap_gql_surfaces_message() {
        let r: GqlResponse<u32> =
            serde_json::from_str(r#"{"errors":[{"message":"Field x not found"}]}"#).unwrap();
        let e = unwrap_gql(r).unwrap_err();
        assert!(e.to_string().contains("Field x not found"), "{e}");
    }

    #[test]
    fn unwrap_gql_handles_empty_response() {
        let r: GqlResponse<u32> = serde_json::from_str(r#"{}"#).unwrap();
        assert!(unwrap_gql(r).is_err());
    }

    #[test]
    fn title_prefers_english_then_romaji_then_native() {
        assert_eq!(best_title("Romaji", "English", "Native"), "English");
        assert_eq!(best_title("Romaji", "", "Native"), "Romaji");
        assert_eq!(best_title("", "", "Native"), "Native");
        assert_eq!(best_title("", "  ", ""), "(başlıksız)");
    }

    #[test]
    fn status_labels_and_serde_agree() {
        let json = serde_json::to_string(&MediaListStatus::Current).unwrap();
        assert_eq!(json, "\"CURRENT\"");
        let back: MediaListStatus = serde_json::from_str("\"PAUSED\"").unwrap();
        assert_eq!(back, MediaListStatus::Paused);
        assert_eq!(MediaListStatus::all().len(), 6);
        assert_eq!(MediaListStatus::Completed.label(), "Tamamlandı");
    }

    #[test]
    fn maps_viewer_payload() {
        let json = r#"{"Viewer":{"id":42,"name":"user","avatar":{"large":"http://x/a.png"}}}"#;
        let d: wire::ViewerData = serde_json::from_str(json).unwrap();
        let v = map_viewer(d);
        assert_eq!(v.id, 42);
        assert_eq!(v.name, "user");
        assert_eq!(v.avatar_large.as_deref(), Some("http://x/a.png"));
    }

    #[test]
    fn maps_library_payload() {
        let json = r#"{"Page":{"pageInfo":{"hasNextPage":true},"mediaList":[
            {"id":1,"status":"CURRENT","progress":3,
             "media":{"id":99,"averageScore":80,"episodes":12,
                      "coverImage":{"large":"c.png"},
                      "title":{"romaji":"R","english":"E","native":"N"}}}
        ]}}"#;
        let d: wire::LibraryPage = serde_json::from_str(json).unwrap();
        let (entries, next) = map_library(d);
        assert!(next);
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.entry_id, 1);
        assert_eq!(e.media_id, 99);
        assert_eq!(e.status, MediaListStatus::Current);
        assert_eq!(e.progress, 3);
        assert_eq!(e.title, "E");
        assert_eq!(e.total_episodes, Some(12));
        assert_eq!(e.average_score, Some(80));
    }

    #[test]
    fn maps_empty_library() {
        let json = r#"{"Page":{}}"#;
        let d: wire::LibraryPage = serde_json::from_str(json).unwrap();
        let (entries, next) = map_library(d);
        assert!(entries.is_empty());
        assert!(!next);
    }

    #[test]
    fn maps_search_payload() {
        let json = r#"{"Page":{"media":[
            {"id":5,"episodes":24,"format":"TV",
             "coverImage":{"large":"c.png"},
             "title":{"romaji":"R","english":null,"native":null}}
        ]}}"#;
        let d: wire::SearchPage = serde_json::from_str(json).unwrap();
        let r = map_search(d);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].title, "R");
        assert_eq!(r[0].episodes, Some(24));
        assert_eq!(r[0].format.as_deref(), Some("TV"));
    }

    #[test]
    fn maps_save_and_delete_payloads() {
        let s: wire::SaveData = serde_json::from_str(
            r#"{"SaveMediaListEntry":{"id":7,"status":"PLANNING","progress":0}}"#,
        )
        .unwrap();
        assert_eq!(s.entry.id, 7);
        assert_eq!(s.entry.status, MediaListStatus::Planning);

        let d: wire::DeleteData =
            serde_json::from_str(r#"{"DeleteMediaListEntry":{"deleted":true}}"#).unwrap();
        assert!(d.entry.deleted);
    }

    #[test]
    fn absolute_callback_joins_target() {
        let u = absolute_callback(
            "http://127.0.0.1:17395/callback",
            "/callback?code=a&state=b",
        )
        .unwrap();
        assert_eq!(u, "http://127.0.0.1:17395/callback?code=a&state=b");
    }

    #[test]
    fn callback_html_is_static_and_safe() {
        assert!(CALLBACK_HTML.starts_with("<!doctype html>"));
        assert!(!CALLBACK_HTML.contains("<script"));
    }

    #[test]
    fn client_builder_is_https_only() {
        let c = AniListClient::new(cfg());
        assert_eq!(c.config().client_id, "1234");
    }

    /// Exercises the real socket path: bind, accept one request, answer, and
    /// hand back the request target.
    #[tokio::test]
    async fn loopback_listener_captures_the_redirect() {
        let l = LoopbackListener::bind().await.expect("bind");
        let uri = l.redirect_uri().to_string();
        let port: u16 = uri
            .rsplit(':')
            .next()
            .unwrap()
            .split('/')
            .next()
            .unwrap()
            .parse()
            .unwrap();

        let handle =
            tokio::spawn(
                async move { l.wait_for_callback(std::time::Duration::from_secs(5)).await },
            );

        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let mut sock = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        sock.write_all(
            b"GET /callback?code=abc123&state=st4te HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
        )
        .await
        .unwrap();
        let mut resp = Vec::new();
        sock.read_to_end(&mut resp).await.unwrap();
        let text = String::from_utf8_lossy(&resp);
        assert!(text.starts_with("HTTP/1.1 200 OK"), "got: {text}");
        // The completion page must not echo the code back into the HTML.
        assert!(!text.contains("abc123"), "callback reflected user input");

        let target = handle.await.unwrap().expect("callback");
        assert_eq!(target, "/callback?code=abc123&state=st4te");

        let (code, state) =
            parse_callback(&absolute_callback(&uri, &target).unwrap(), &uri).unwrap();
        assert_eq!(code, "abc123");
        assert_eq!(state, "st4te");
    }

    /// Send `request` to the listener and return the response it answers with.
    async fn exchange(port: u16, request: &[u8]) -> String {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let mut sock = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        // A refused/oversized request may close the socket mid-write; the
        // answer, not the write, is what the test asserts.
        let _ = sock.write_all(request).await;
        let mut resp = Vec::new();
        let _ = sock.read_to_end(&mut resp).await;
        String::from_utf8_lossy(&resp).to_string()
    }

    fn listener_port(uri: &str) -> u16 {
        uri.rsplit(':')
            .next()
            .unwrap()
            .split('/')
            .next()
            .unwrap()
            .parse()
            .unwrap()
    }

    #[tokio::test]
    async fn loopback_listener_ignores_unrelated_requests_and_keeps_waiting() {
        // A speculative preconnect, a favicon probe or any local process used to
        // consume the ONE accept the listener performed, so the real browser
        // callback then had nobody listening and the login failed.
        let l = LoopbackListener::bind().await.expect("bind");
        let uri = l.redirect_uri().to_string();
        let port = listener_port(&uri);
        let handle =
            tokio::spawn(
                async move { l.wait_for_callback(std::time::Duration::from_secs(5)).await },
            );

        let probe = exchange(
            port,
            b"GET /favicon.ico HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
        )
        .await;
        assert!(probe.starts_with("HTTP/1.1 404"), "probe answer: {probe}");
        let root = exchange(port, b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").await;
        assert!(root.starts_with("HTTP/1.1 404"), "root answer: {root}");
        // A POST to the callback path is not the redirect either.
        let post = exchange(
            port,
            b"POST /callback?code=x&state=y HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 0\r\n\r\n",
        )
        .await;
        assert!(post.starts_with("HTTP/1.1 404"), "post answer: {post}");

        // The real callback still lands.
        let real = exchange(
            port,
            b"GET /callback?code=abc123&state=st4te HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
        )
        .await;
        assert!(
            real.starts_with("HTTP/1.1 200 OK"),
            "callback answer: {real}"
        );
        assert!(!real.contains("abc123"), "callback reflected user input");
        let target = handle.await.unwrap().expect("callback");
        assert_eq!(target, "/callback?code=abc123&state=st4te");
    }

    #[tokio::test]
    async fn loopback_listener_reassembles_a_fragmented_request_line() {
        // One `read` is not guaranteed to deliver the whole request line; a
        // truncated target used to fail an otherwise valid authorisation.
        use tokio::io::AsyncWriteExt as _;

        let l = LoopbackListener::bind().await.expect("bind");
        let uri = l.redirect_uri().to_string();
        let port = listener_port(&uri);
        let handle =
            tokio::spawn(
                async move { l.wait_for_callback(std::time::Duration::from_secs(5)).await },
            );

        let mut sock = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        sock.write_all(b"GET /call").await.unwrap();
        sock.flush().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        sock.write_all(b"back?code=frag&state=s1 HTTP/1.1\r\nHost: 127.0.0.1\r\n")
            .await
            .unwrap();
        sock.flush().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        sock.write_all(b"\r\n").await.unwrap();
        sock.flush().await.unwrap();

        let target = handle.await.unwrap().expect("callback");
        assert_eq!(target, "/callback?code=frag&state=s1");
        let (code, state) =
            parse_callback(&absolute_callback(&uri, &target).unwrap(), &uri).unwrap();
        assert_eq!(code, "frag");
        assert_eq!(state, "s1");
    }

    #[tokio::test]
    async fn loopback_listener_refuses_an_oversized_header_block() {
        // Bounded: a local process must not be able to make the listener buffer
        // without limit, and the refusal must not end the authorisation.
        let l = LoopbackListener::bind().await.expect("bind");
        let uri = l.redirect_uri().to_string();
        let port = listener_port(&uri);
        let handle =
            tokio::spawn(
                async move { l.wait_for_callback(std::time::Duration::from_secs(8)).await },
            );

        let mut flood = b"GET /callback HTTP/1.1\r\nX-Pad: ".to_vec();
        flood.extend(std::iter::repeat(b'a').take(MAX_REQUEST_BYTES + 4096));
        let answer = exchange(port, &flood).await;
        assert!(
            answer.starts_with("HTTP/1.1 413"),
            "oversized answer: {}",
            answer.chars().take(40).collect::<String>()
        );

        let real = exchange(
            port,
            b"GET /callback?code=after&state=flood HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
        )
        .await;
        assert!(
            real.starts_with("HTTP/1.1 200 OK"),
            "callback answer: {real}"
        );
        assert_eq!(
            handle.await.unwrap().expect("callback"),
            "/callback?code=after&state=flood"
        );
    }

    #[tokio::test]
    async fn loopback_listener_drops_a_silent_client_and_still_finishes() {
        // A connection that sends nothing is abandoned after its own budget
        // instead of holding the whole authorisation window.
        let l = LoopbackListener::bind().await.expect("bind");
        let uri = l.redirect_uri().to_string();
        let port = listener_port(&uri);
        let handle =
            tokio::spawn(
                async move { l.wait_for_callback(std::time::Duration::from_secs(8)).await },
            );

        let silent = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        // Held open, never written to: it must be dropped once its budget runs
        // out, and the real callback must still be accepted afterwards.
        tokio::time::sleep(CONNECTION_READ_BUDGET + std::time::Duration::from_millis(200)).await;
        drop(silent);

        let real = exchange(
            port,
            b"GET /callback?code=late&state=quiet HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
        )
        .await;
        assert!(
            real.starts_with("HTTP/1.1 200 OK"),
            "callback answer: {real}"
        );
        assert_eq!(
            handle.await.unwrap().expect("callback"),
            "/callback?code=late&state=quiet"
        );
    }

    #[tokio::test]
    async fn loopback_listener_times_out() {
        let l = LoopbackListener::bind().await.expect("bind");
        let err = l
            .wait_for_callback(std::time::Duration::from_millis(50))
            .await
            .unwrap_err();
        assert_eq!(err.code(), "anilist");
    }
}
