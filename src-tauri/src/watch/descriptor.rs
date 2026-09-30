//! The site-adapter descriptor: schema v1.
//!
//! A descriptor is *data*, never code. It says where a site publishes its
//! "latest episodes" list and which fields to read out of it; the engine here
//! decides everything else — the request method, the host, the pace, and how
//! a title is normalised. A descriptor is therefore only ever allowed to make
//! the app *slower* or *narrower* than the engine's own limits.
//!
//! ```json
//! {
//!   "schema": 1,
//!   "name": "Example Site",
//!   "host": "example-anime.test",
//!   "endpoint": {
//!     "path": "/api/latest",
//!     "headers": { "x-requested-with": "XMLHttpRequest" },
//!     "query": { "page": "1" }
//!   },
//!   "extract": {
//!     "kind": "json",
//!     "rows": "/data/items",
//!     "title": { "pointer": "/title" },
//!     "episode": { "pointer": "/episode" }
//!   },
//!   "limits": { "min_interval_secs": 1800, "max_requests_per_day": 24 }
//! }
//! ```
//!
//! Pointers are RFC 6901 JSON pointers (`~0` for `~`, `~1` for `/`). A field
//! spec is exactly one of `pointer`, `constant`, or `filename`; the last one
//! runs the release-name grammar over a file name, which is how an RSS feed
//! or a download list is read.

use crate::error::{AppError, AppResult};
use crate::sites::registry::sanitize_name;
use crate::sites::url_policy::validate_site_url;
use crate::watch::normalize;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// The only schema this engine understands. A file that declares another
/// version is refused rather than guessed at.
pub const SCHEMA_VERSION: u32 = 1;
/// Rows read out of one response. A hostile or broken endpoint cannot make
/// the app allocate without bound.
pub const MAX_ROWS: usize = 200;
/// Descriptors in one import file.
pub const MAX_DESCRIPTORS: usize = 64;
const PATH_MAX: usize = 512;
const VALUE_MAX: usize = 256;
const NAME_MAX: usize = 80;

/// Headers a descriptor may set.
///
/// Cookies, `Authorization`, `Host`, `Origin` and `Referer` are *refused*
/// rather than ignored: an adapter must never be able to borrow the user's
/// session or speak for another origin.
pub const ALLOWED_HEADERS: [&str; 3] = ["accept", "accept-language", "x-requested-with"];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Descriptor {
    pub schema: u32,
    pub name: String,
    /// Pinned host, lowercase, no scheme and no path.
    pub host: String,
    pub endpoint: Endpoint,
    pub extract: Extract,
    #[serde(default)]
    pub limits: Option<DescriptorLimits>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    /// Absolute path on the pinned host, e.g. `/api/latest`.
    pub path: String,
    /// Reserved: the engine only ever sends GET.
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Fixed query parameters, appended in sorted order.
    #[serde(default)]
    pub query: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Extract {
    pub kind: ExtractKind,
    /// Pointer to the array (or object of objects) holding the latest rows.
    pub rows: String,
    pub title: FieldSpec,
    #[serde(default)]
    pub season: Option<FieldSpec>,
    pub episode: FieldSpec,
    #[serde(default)]
    pub url: Option<FieldSpec>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtractKind {
    /// A JSON (or JSON-ish) endpoint. The only kind v1 implements.
    Json,
    /// Reserved for a later release; refused today so an adapter written
    /// against it fails loudly instead of silently matching nothing.
    Html,
}

/// Where one value comes from inside a row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldSpec {
    #[serde(default)]
    pub pointer: Option<String>,
    #[serde(default)]
    pub constant: Option<Value>,
    /// Pointer to a release *file name*; the title/season/episode grammar
    /// runs over it.
    #[serde(default)]
    pub filename: Option<String>,
}

impl FieldSpec {
    pub fn pointer(path: &str) -> Self {
        FieldSpec {
            pointer: Some(path.into()),
            constant: None,
            filename: None,
        }
    }

    pub fn constant(value: Value) -> Self {
        FieldSpec {
            pointer: None,
            constant: Some(value),
            filename: None,
        }
    }

    pub fn filename(path: &str) -> Self {
        FieldSpec {
            pointer: None,
            constant: None,
            filename: Some(path.into()),
        }
    }

    /// The one variant that is set, as a name for error messages.
    fn kind(&self) -> &'static str {
        match (&self.pointer, &self.constant, &self.filename) {
            (Some(_), None, None) => "pointer",
            (None, Some(_), None) => "constant",
            (None, None, Some(_)) => "filename",
            _ => "belirsiz",
        }
    }
}

/// What a descriptor may ask for. Both values can only be *stricter* than the
/// engine ceiling; [`crate::watch::rate::Speed::resolve`] clamps them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DescriptorLimits {
    #[serde(default)]
    pub min_interval_secs: Option<u32>,
    #[serde(default)]
    pub max_requests_per_day: Option<u32>,
}

/// One episode row read out of a response, still in the site's spelling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub title: String,
    pub season: Option<u32>,
    pub episode: u32,
}

impl Row {
    /// Canonical identity for the ledger.
    pub fn release(&self) -> normalize::Release {
        normalize::Release {
            title: self.title.clone(),
            season: self.season.unwrap_or(1),
            episode: self.episode,
        }
    }
}

/// Parse and validate one descriptor.
pub fn parse(raw: &str) -> AppResult<Descriptor> {
    let mut desc: Descriptor = decode(raw)?;
    validate(&mut desc)?;
    Ok(desc)
}

/// Parse a file that holds one descriptor or an array of them.
pub fn load_many(raw: &str) -> AppResult<Vec<Descriptor>> {
    let value: Value = serde_json::from_str(raw)
        .map_err(|e| AppError::Watch(format!("JSON çözümlenemedi: {}", excerpt(&e.to_string()))))?;
    let items = match value {
        Value::Array(items) => items,
        single => vec![single],
    };
    if items.len() > MAX_DESCRIPTORS {
        return Err(AppError::Watch(format!(
            "tek dosyada en fazla {MAX_DESCRIPTORS} adaptör olabilir"
        )));
    }

    let mut out: Vec<Descriptor> = Vec::new();
    for item in items {
        let mut desc: Descriptor = serde_json::from_value(item).map_err(|e| {
            AppError::Watch(format!("adaptör okunamadı: {}", excerpt(&e.to_string())))
        })?;
        validate(&mut desc)?;
        let duplicate = out
            .iter()
            .any(|d| d.host == desc.host && d.endpoint.path == desc.endpoint.path);
        if duplicate {
            return Err(AppError::Watch(format!(
                "aynı host ve uç iki kez tanımlanmış: {}",
                desc.host
            )));
        }
        out.push(desc);
    }
    Ok(out)
}

fn decode(raw: &str) -> AppResult<Descriptor> {
    serde_json::from_str(raw)
        .map_err(|e| AppError::Watch(format!("adaptör okunamadı: {}", excerpt(&e.to_string()))))
}

/// Check a descriptor and normalise the parts that have one spelling
/// (the host, the header names).
pub fn validate(desc: &mut Descriptor) -> AppResult<()> {
    if desc.schema != SCHEMA_VERSION {
        return Err(AppError::Watch(format!(
            "şema sürümü {} desteklenmiyor; bu sürüm yalnızca v{SCHEMA_VERSION} okur",
            desc.schema
        )));
    }

    if desc.name.trim().chars().count() > NAME_MAX {
        return Err(AppError::Watch(format!(
            "adaptör adı en fazla {NAME_MAX} karakter olabilir"
        )));
    }
    desc.name = sanitize_name(&desc.name);
    if desc.name.is_empty() {
        return Err(AppError::Watch("adaptörün bir adı olmalı".into()));
    }

    desc.host = desc.host.trim().to_lowercase();
    let bad_host_char = |c: char| matches!(c, '/' | ':' | '@' | '?' | '#' | ' ' | '\\');
    if desc.host.is_empty() || desc.host.len() > 253 || desc.host.contains(bad_host_char) {
        return Err(AppError::Watch(
            "host alanına yalnızca alan adı yazılır (şema, yol veya port olmadan)".into(),
        ));
    }
    // The same policy the launcher applies to a site: HTTPS-only, public
    // host, no embedded credentials.
    let safe = validate_site_url(&format!("https://{}/", desc.host))
        .map_err(|r| AppError::Watch(format!("host reddedildi: {}", r.reason())))?;
    if safe.host() != desc.host {
        return Err(AppError::Watch(format!(
            "host normalleştirilemedi: {}",
            sanitize_name(&desc.host)
        )));
    }

    validate_path(&desc.endpoint.path, "endpoint.path", PATH_MAX)?;
    if let Some(method) = &desc.endpoint.method {
        if !method.eq_ignore_ascii_case("get") {
            return Err(AppError::Watch(
                "adaptör ucu yalnızca GET olabilir".into(),
            ));
        }
    }

    let headers: BTreeMap<String, String> = desc
        .endpoint
        .headers
        .iter()
        .map(|(k, v)| (k.trim().to_lowercase(), v.trim().to_string()))
        .collect();
    for (name, value) in &headers {
        if !ALLOWED_HEADERS.contains(&name.as_str()) {
            return Err(AppError::Watch(format!(
                "izin verilmeyen başlık: {} (izinliler: {})",
                sanitize_name(name),
                ALLOWED_HEADERS.join(", ")
            )));
        }
        validate_value(value, "endpoint.headers")?;
    }
    desc.endpoint.headers = headers;

    for (key, value) in &desc.endpoint.query {
        if key.is_empty()
            || key.len() > 64
            || !key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
        {
            return Err(AppError::Watch(format!(
                "sorgu parametresi adı geçersiz: {}",
                sanitize_name(key)
            )));
        }
        validate_value(value, "endpoint.query")?;
    }

    if desc.extract.kind != ExtractKind::Json {
        return Err(AppError::Watch(
            "bu sürümde yalnızca \"kind\": \"json\" destekleniyor".into(),
        ));
    }
    check_pointer(&desc.extract.rows, "extract.rows")?;
    check_field(&desc.extract.title, "extract.title", FieldKind::Text)?;
    check_field(&desc.extract.episode, "extract.episode", FieldKind::Number)?;
    if let Some(season) = &desc.extract.season {
        check_field(season, "extract.season", FieldKind::Number)?;
    }
    if let Some(url) = &desc.extract.url {
        check_field(url, "extract.url", FieldKind::Text)?;
        if url.filename.is_some() {
            return Err(AppError::Watch(
                "extract.url dosya adından türetilemez".into(),
            ));
        }
    }

    if let Some(limits) = desc.limits {
        if let Some(per_day) = limits.max_requests_per_day {
            if per_day == 0 {
                return Err(AppError::Watch(
                    "limits.max_requests_per_day en az 1 olmalı".into(),
                ));
            }
        }
    }

    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FieldKind {
    Text,
    Number,
}

fn check_field(spec: &FieldSpec, field: &str, kind: FieldKind) -> AppResult<()> {
    match (&spec.pointer, &spec.constant, &spec.filename) {
        (Some(path), None, None) | (None, None, Some(path)) => {
            check_pointer(path, field)?;
        }
        (None, Some(value), None) => {
            if kind == FieldKind::Number && coerce_number(value).is_none() {
                return Err(AppError::Watch(format!(
                    "{field} sabit değeri bir sayı olmalı"
                )));
            }
        }
        _ => {
            return Err(AppError::Watch(format!(
                "{field} için pointer/constant/filename alanlarından tam olarak biri dolu olmalı ({} bulundu)",
                spec.kind()
            )));
        }
    }
    Ok(())
}

fn check_pointer(pointer: &str, field: &str) -> AppResult<()> {
    if !pointer.starts_with('/') {
        return Err(AppError::Watch(format!(
            "{field} bir JSON pointer olmalı ve / ile başlamalı"
        )));
    }
    if pointer.len() > PATH_MAX {
        return Err(AppError::Watch(format!("{field} çok uzun")));
    }
    Ok(())
}

fn validate_path(path: &str, field: &str, max: usize) -> AppResult<()> {
    if !path.starts_with('/') {
        return Err(AppError::Watch(format!("{field} / ile başlamalı")));
    }
    if path.len() > max {
        return Err(AppError::Watch(format!("{field} çok uzun")));
    }
    if path.chars().any(|c| c.is_control() || c == ' ') {
        return Err(AppError::Watch(format!("{field} kontrol karakteri içeriyor")));
    }
    if path.split('/').any(|part| part == "..") {
        return Err(AppError::Watch(format!("{field} üst dizine çıkamaz")));
    }
    Ok(())
}

fn validate_value(value: &str, field: &str) -> AppResult<()> {
    if value.is_empty() {
        return Err(AppError::Watch(format!("{field} boş değer içeriyor")));
    }
    if value.len() > VALUE_MAX {
        return Err(AppError::Watch(format!("{field} değeri çok uzun")));
    }
    if value.chars().any(char::is_control) {
        return Err(AppError::Watch(format!(
            "{field} değeri kontrol karakteri içeriyor"
        )));
    }
    Ok(())
}

/// First line of an error message, clipped, with control characters removed.
fn excerpt(message: &str) -> String {
    message
        .chars()
        .filter(|c| !c.is_control())
        .take(200)
        .collect()
}

/// Resolve an RFC 6901 JSON pointer. `""` is the document itself.
pub fn resolve_pointer<'a>(doc: &'a Value, pointer: &str) -> Option<&'a Value> {
    if pointer.is_empty() {
        return Some(doc);
    }
    let rest = pointer.strip_prefix('/')?;
    let mut current = doc;
    for token in rest.split('/') {
        let key = token.replace("~1", "/").replace("~0", "~");
        current = match current {
            Value::Object(map) => map.get(&key)?,
            Value::Array(items) => {
                let index: usize = key.parse().ok()?;
                items.get(index)?
            }
            _ => return None,
        };
    }
    Some(current)
}

/// Number from a JSON scalar, or from text like `"Bölüm 5"` / `"05"`.
pub fn coerce_number(value: &Value) -> Option<u32> {
    match value {
        Value::Number(n) => n.as_u64().and_then(|v| u32::try_from(v).ok()),
        Value::String(s) => {
            let trimmed = s.trim();
            if let Ok(n) = trimmed.parse::<u32>() {
                return Some(n);
            }
            normalize::episode_from_text(trimmed).or_else(|| normalize::season_from_text(trimmed))
        }
        _ => None,
    }
}

/// Episode number from a JSON scalar, trying the release grammar on text.
pub fn coerce_episode(value: &Value) -> Option<u32> {
    match value {
        Value::String(s) => {
            let trimmed = s.trim();
            if let Ok(n) = trimmed.parse::<u32>() {
                return Some(n);
            }
            normalize::episode_from_text(trimmed)
        }
        other => coerce_number(other),
    }
}

/// Short text from a JSON scalar.
pub fn coerce_text(value: &Value) -> Option<String> {
    let text = match value {
        Value::String(s) => s.trim().to_string(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    if text.is_empty() {
        return None;
    }
    Some(text.chars().take(VALUE_MAX).collect())
}

fn value_of<'a>(spec: &FieldSpec, row: &'a Value) -> Option<&'a Value> {
    match (&spec.pointer, &spec.constant, &spec.filename) {
        (Some(path), _, _) | (_, _, Some(path)) => resolve_pointer(row, path),
        (_, Some(value), _) => Some(value),
        _ => None,
    }
}

/// Read the rows out of a decoded response body.
pub fn extract_rows(desc: &Descriptor, body: &Value) -> Vec<Row> {
    let Some(rows) = resolve_pointer(body, &desc.extract.rows) else {
        return Vec::new();
    };
    let items: Vec<&Value> = match rows {
        Value::Array(items) => items.iter().take(MAX_ROWS).collect(),
        Value::Object(map) => map.values().take(MAX_ROWS).collect(),
        _ => return Vec::new(),
    };

    let mut out = Vec::new();
    for item in items {
        if let Some(row) = read_row(desc, item) {
            out.push(row);
        }
    }
    out
}

fn read_row(desc: &Descriptor, item: &Value) -> Option<Row> {
    let mut title;
    let mut season = None;
    let mut episode = None;

    if let Some(path) = &desc.extract.title.filename {
        // A file name carries the whole identity; explicit fields below win.
        let name = coerce_text(resolve_pointer(item, path)?)?;
        let fields = normalize::parse_fields(&name);
        title = fields.title;
        season = fields.season;
        episode = fields.episode;
    } else {
        title = normalize::canonical_title(&coerce_text(value_of(&desc.extract.title, item)?)?);
    }

    if let Some(spec) = &desc.extract.season {
        if let Some(value) = value_of(spec, item) {
            season = coerce_number(value);
        }
    }
    if let Some(value) = value_of(&desc.extract.episode, item) {
        episode = coerce_episode(value);
    }

    if title.is_empty() {
        return None;
    }
    Some(Row {
        title,
        season,
        episode: episode?,
    })
}

/// The request URL for a descriptor: always HTTPS, always the pinned host,
/// query parameters in sorted order so a descriptor has one canonical form.
pub fn request_url(desc: &Descriptor) -> AppResult<url::Url> {
    let raw = format!("https://{}{}", desc.host, desc.endpoint.path);
    let mut url = url::Url::parse(&raw)
        .map_err(|_| AppError::Watch("istek adresi kurulamadı".into()))?;
    // The host is never taken from response data, so this can only fail if
    // validation was bypassed.
    if url.host_str() != Some(desc.host.as_str()) {
        return Err(AppError::Watch("istek adresi host'a bağlanamadı".into()));
    }
    {
        let mut pairs = url.query_pairs_mut();
        for (key, value) in &desc.endpoint.query {
            pairs.append_pair(key, value);
        }
    }
    Ok(url)
}

/// Whether a URL found in a response still points at the pinned host.
pub fn same_host(candidate: &str, desc: &Descriptor) -> bool {
    url::Url::parse(candidate)
        .ok()
        .and_then(|u| u.host_str().map(str::to_lowercase))
        .is_some_and(|host| host == desc.host)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SAMPLE: &str = r#"{
        "schema": 1,
        "name": "Example Site",
        "host": "example-anime.test",
        "endpoint": {
            "path": "/api/latest",
            "headers": { "x-requested-with": "XMLHttpRequest" },
            "query": { "page": "1", "type": "subbed" }
        },
        "extract": {
            "kind": "json",
            "rows": "/data/items",
            "title": { "pointer": "/title" },
            "season": { "constant": 2 },
            "episode": { "pointer": "/episode" }
        },
        "limits": { "min_interval_secs": 3600, "max_requests_per_day": 12 }
    }"#;

    fn sample() -> Descriptor {
        parse(SAMPLE).expect("örnek adaptör geçerli olmalı")
    }

    #[test]
    fn accepts_the_documented_example() {
        let desc = sample();
        assert_eq!(desc.host, "example-anime.test");
        assert_eq!(desc.extract.kind, ExtractKind::Json);
        assert_eq!(desc.limits.and_then(|l| l.max_requests_per_day), Some(12));
    }

    #[test]
    fn rejects_the_wrong_schema_version() {
        let raw = SAMPLE.replace("\"schema\": 1", "\"schema\": 2");
        let err = parse(&raw).expect_err("v2 reddedilmeli");
        assert!(err.to_string().contains("şema"));
    }

    #[test]
    fn rejects_unknown_and_dangerous_fields() {
        // A typo must fail loudly instead of silently matching nothing.
        let typo = SAMPLE.replace("\"host\"", "\"hosts\"");
        assert!(parse(&typo).is_err());

        // The engine always sends GET; a descriptor cannot ask for more.
        let post = SAMPLE.replace("\"path\": \"/api/latest\"", "\"path\": \"/api/latest\", \"method\": \"POST\"");
        let err = parse(&post).expect_err("POST reddedilmeli");
        assert!(err.to_string().contains("GET"));
    }

    #[test]
    fn refuses_private_hosts_and_hosts_with_paths() {
        for host in ["localhost", "127.0.0.1", "192.168.1.10", "10.0.0.5", "example.test/api"] {
            let raw = SAMPLE.replace("example-anime.test", host);
            assert!(parse(&raw).is_err(), "{host} reddedilmeliydi");
        }
    }

    #[test]
    fn refuses_headers_that_would_borrow_a_session() {
        for header in ["cookie", "authorization", "origin", "host"] {
            let raw = SAMPLE.replace(
                "\"x-requested-with\": \"XMLHttpRequest\"",
                &format!("\"{header}\": \"x\""),
            );
            assert!(parse(&raw).is_err(), "{header} reddedilmeliydi");
        }
    }

    #[test]
    fn refuses_html_extraction_for_now() {
        let raw = SAMPLE.replace("\"kind\": \"json\"", "\"kind\": \"html\"");
        let err = parse(&raw).expect_err("html henüz yok");
        assert!(err.to_string().contains("json"));
    }

    #[test]
    fn refuses_an_ambiguous_field_spec() {
        let both = SAMPLE.replace(
            "{ \"pointer\": \"/title\" }",
            "{ \"pointer\": \"/title\", \"constant\": \"x\" }",
        );
        assert!(parse(&both).is_err());
    }

    #[test]
    fn resolves_pointers() {
        let doc = json!({
            "data": { "items": [ { "title": "A" }, { "title": "B" } ] },
            "a/b": 1,
            "m~n": 2
        });
        assert_eq!(resolve_pointer(&doc, "/data/items/1/title"), Some(&json!("B")));
        assert_eq!(resolve_pointer(&doc, "/a~1b"), Some(&json!(1)));
        assert_eq!(resolve_pointer(&doc, "/m~0n"), Some(&json!(2)));
        assert_eq!(resolve_pointer(&doc, "/data/items/9"), None);
        assert_eq!(resolve_pointer(&doc, "/missing"), None);
        assert_eq!(resolve_pointer(&doc, "/data/items/0/title/deeper"), None);
        // An index into an object is not a pointer.
        assert_eq!(resolve_pointer(&doc, "/data/0"), None);
    }

    #[test]
    fn reads_rows_out_of_a_json_body() {
        let desc = sample();
        let body = json!({
            "data": { "items": [
                { "title": "Show A - Bölüm 5", "episode": 5, "url": "https://example-anime.test/a/5" },
                { "title": "Show B", "episode": "Bölüm 12", "url": "https://evil.test/b/12" },
                { "title": "Show C" }
            ]},
            "ok": true
        });
        let rows = extract_rows(&desc, &body);
        assert_eq!(rows.len(), 2, "bölüm numarası olmayan satır atlanır");
        assert_eq!(rows[0].title, "show a");
        assert_eq!(rows[0].episode, 5);
        assert_eq!(rows[0].season, Some(2), "sabit alan pointer'ı geçersiz kılar");
        assert_eq!(rows[1].title, "show b");
        assert_eq!(rows[1].episode, 12);
        // Rows keep only the pinned host's own links.
        assert!(same_host("https://example-anime.test/a/5", &desc));
        assert!(!same_host("https://evil.test/b/12", &desc));
    }

    #[test]
    fn reads_a_file_name_row() {
        let raw = SAMPLE.replace(
            "{ \"pointer\": \"/title\" }",
            "{ \"filename\": \"/name\" }",
        );
        let desc = parse(&raw).expect("geçerli");
        let body = json!({
            "data": { "items": [ { "name": "mushokus3_b5.a256" } ] }
        });
        let rows = extract_rows(&desc, &body);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "mushoku");
        assert_eq!(rows[0].episode, 5);
        assert_eq!(rows[0].season, Some(2));
    }

    #[test]
    fn builds_one_canonical_request_url() {
        let desc = sample();
        let url = request_url(&desc).expect("url");
        assert_eq!(
            url.as_str(),
            "https://example-anime.test/api/latest?page=1&type=subbed"
        );
        assert_eq!(url.host_str(), Some("example-anime.test"));
    }

    #[test]
    fn loads_one_or_many_and_refuses_duplicates() {
        let one = load_many(SAMPLE).expect("tek adaptör");
        assert_eq!(one.len(), 1);

        let array = format!("[{SAMPLE}, {SAMPLE}]");
        let err = load_many(&array).expect_err("aynı host + uç iki kez olamaz");
        assert!(err.to_string().contains("iki kez"));

        let mut other = SAMPLE.replace("example-anime.test", "second-anime.test");
        other = other.replace("/api/latest", "/api/new");
        let both = format!("[{SAMPLE}, {other}]");
        assert_eq!(load_many(&both).expect("iki farklı adaptör").len(), 2);
    }

    #[test]
    fn coerces_widget_values() {
        assert_eq!(coerce_episode(&json!(5)), Some(5));
        assert_eq!(coerce_episode(&json!("5")), Some(5));
        assert_eq!(coerce_episode(&json!("Bölüm 5")), Some(5));
        assert_eq!(coerce_episode(&json!("12. Bölüm")), Some(12));
        assert_eq!(coerce_episode(&json!("Son Eklenenler")), None);
        assert_eq!(coerce_number(&json!("3. Sezon")), Some(3));
        assert_eq!(coerce_text(&json!("  hi  ")), Some("hi".into()));
        assert_eq!(coerce_text(&json!(12)), Some("12".into()));
        assert_eq!(coerce_text(&json!(null)), None);
    }
}
