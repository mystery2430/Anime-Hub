//! The site registry: built-in defaults plus whatever the user adds.
//!
//! The list lives in an encrypted JSON document on disk, so the built-ins can
//! be changed, hidden, or removed without a code change — these streaming
//! sites change domains often.

use crate::error::{AppError, AppResult};
use crate::sites::url_policy::{validate_site_url, Rejection};
use serde::{Deserialize, Serialize};

/// Launcher grouping shown in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    /// Tracking / database services (MyAnimeList, AniList, …).
    Tracking,
    /// Streaming sites.
    #[default]
    Watching,
}

/// Where a tile's icon comes from.
///
/// Remote icon URLs are deliberately *not* fetched: an attacker-controlled
/// icon host would otherwise become a beacon for "which users have this tile".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Icon {
    /// A single character / short emoji rendered on a coloured disc.
    Letter {
        text: String,
        /// `#rrggbb`.
        color: String,
    },
    /// A bundled asset path, relative to the frontend root (never a URL).
    Bundled { path: String },
}

impl Default for Icon {
    fn default() -> Self {
        Icon::Letter {
            text: "A".into(),
            color: "#6366f1".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Site {
    pub id: String,
    pub name: String,
    /// Always the normalised, HTTPS-validated form.
    pub url: String,
    pub category: Category,
    pub icon: Icon,
    /// `true` for entries shipped with the app. They can be hidden or
    /// re-pointed, but not deleted, so a wipe always leaves a usable launcher.
    pub builtin: bool,
    #[serde(default)]
    pub hidden: bool,
    /// Ordering hint inside a category; lower sorts first.
    #[serde(default)]
    pub sort: u32,
}

impl Site {
    /// Host of the stored URL; used for per-site session partitioning.
    pub fn host(&self) -> String {
        url::Url::parse(&self.url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_lowercase))
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Registry {
    pub version: u32,
    pub sites: Vec<Site>,
}

/// Display-name limits. Long names would blow out the tile grid and make the
/// launcher hard to scan.
const NAME_MAX: usize = 40;
const URL_MAX: usize = 2048;

/// Normalise a user-typed name into something safe to render.
///
/// The UI always renders names as *text nodes*, never as HTML, so this is
/// defence in depth rather than the primary XSS control. It still strips
/// control characters and tags so a pasted name cannot smuggle markup into a
/// `title`/`aria-label` attribute or a log line.
pub fn sanitize_name(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len().min(NAME_MAX));
    let mut prev_space = true; // also trims leading whitespace
    for ch in raw.chars() {
        if out.len() >= NAME_MAX {
            break;
        }
        match ch {
            // Drop control chars, including the bidi overrides used for
            // filename spoofing (U+202A..U+202E, U+2066..U+2069).
            c if c.is_control() => continue,
            c if ('\u{202a}'..='\u{202e}').contains(&c) => continue,
            c if ('\u{2066}'..='\u{2069}').contains(&c) => continue,
            '<' | '>' | '"' | '\'' | '`' | '\\' => continue,
            c if c.is_whitespace() => {
                if !prev_space {
                    out.push(' ');
                    prev_space = true;
                }
            }
            c => {
                out.push(c);
                prev_space = false;
            }
        }
    }
    out.trim().to_string()
}

/// Validate a `#rrggbb` colour, falling back rather than erroring: a bad
/// colour must not stop a user from saving a site.
pub fn sanitize_color(raw: &str, fallback: &str) -> String {
    let c = raw.trim().trim_start_matches('#');
    let ok = matches!(c.len(), 3 | 6)
        && c.chars()
            .all(|ch| ch.is_ascii_digit() || ('a'..='f').contains(&ch.to_ascii_lowercase()));
    if ok {
        format!("#{}", c.to_ascii_lowercase())
    } else {
        fallback.to_string()
    }
}

/// Shipped URL that a later release replaced. Only this exact string is
/// rewritten; anything the user saved stays.
fn retired_builtin_url(id: &str, url: &str) -> Option<&'static str> {
    if id == "builtin-animecix" && (url == "https://animecix.com/" || url == "https://animecix.com")
    {
        Some("https://animecix.tv/")
    } else {
        None
    }
}

/// The sites every install starts with.
pub fn default_sites() -> Vec<Site> {
    vec![
        Site {
            id: "builtin-openanime".into(),
            name: "OpenAnime".into(),
            url: "https://openani.me/".into(),
            category: Category::Watching,
            icon: Icon::Letter {
                text: "OA".into(),
                color: "#e11d48".into(),
            },
            builtin: true,
            hidden: false,
            sort: 0,
        },
        Site {
            id: "builtin-animecix".into(),
            name: "AnimeCix".into(),
            url: "https://animecix.tv/".into(),
            category: Category::Watching,
            icon: Icon::Letter {
                text: "AC".into(),
                color: "#7c3aed".into(),
            },
            builtin: true,
            hidden: false,
            sort: 1,
        },
    ]
}

/// Parameters for adding or editing a site. Everything is user input.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SiteDraft {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub category: Category,
    #[serde(default)]
    pub letter: Option<String>,
    #[serde(default)]
    pub color: Option<String>,
}

impl Registry {
    pub fn new_default() -> Self {
        Registry {
            version: 1,
            sites: default_sites(),
        }
    }

    pub fn get(&self, id: &str) -> Option<&Site> {
        self.sites.iter().find(|s| s.id == id)
    }

    /// Sites the launcher should show, grouped and sorted.
    pub fn visible(&self) -> Vec<Site> {
        let mut v: Vec<Site> = self.sites.iter().filter(|s| !s.hidden).cloned().collect();
        v.sort_by(|a, b| {
            let cat = |s: &Site| match s.category {
                Category::Tracking => 0u8,
                Category::Watching => 1u8,
            };
            cat(a)
                .cmp(&cat(b))
                .then(a.sort.cmp(&b.sort))
                .then(a.name.cmp(&b.name))
        });
        v
    }

    /// Validate a draft. Returns the reason as an [`AppError`] so the frontend
    /// can show it verbatim.
    pub fn check(&self, draft: &SiteDraft, ignore_id: Option<&str>) -> AppResult<(String, String)> {
        let name = sanitize_name(&draft.name);
        if name.is_empty() {
            return Err(AppError::InvalidUrl("bir isim gerekli".into()));
        }
        if draft.url.trim().len() > URL_MAX {
            return Err(AppError::InvalidUrl("adres çok uzun".into()));
        }

        let safe = validate_site_url(&draft.url).map_err(|r: Rejection| match r {
            Rejection::InsecureScheme => AppError::InsecureScheme,
            other => AppError::InvalidUrl(other.reason().to_string()),
        })?;

        let norm = safe.as_str().to_string();
        let taken_name = self.sites.iter().any(|s| {
            Some(s.id.as_str()) != ignore_id && s.name.eq_ignore_ascii_case(&name) && !s.hidden
        });
        if taken_name {
            return Err(AppError::DuplicateName);
        }
        let taken_url = self
            .sites
            .iter()
            .any(|s| Some(s.id.as_str()) != ignore_id && s.url == norm);
        if taken_url {
            return Err(AppError::DuplicateUrl);
        }

        Ok((name, norm))
    }

    /// Add a user site. Returns the stored entry.
    pub fn add(&mut self, draft: SiteDraft) -> AppResult<Site> {
        let (name, norm) = self.check(&draft, None)?;
        let id = format!("user-{}", uuid::Uuid::new_v4().simple());
        let sort = self
            .sites
            .iter()
            .filter(|s| s.category == draft.category)
            .map(|s| s.sort)
            .max()
            .map(|m| m + 1)
            .unwrap_or(0);

        let icon = Icon::Letter {
            text: draft
                .letter
                .as_deref()
                .map(sanitize_name)
                .filter(|s| !s.is_empty())
                .map(|s| s.chars().take(2).collect())
                .unwrap_or_else(|| {
                    name.chars()
                        .filter(|c| !c.is_whitespace())
                        .take(2)
                        .collect::<String>()
                })
                .to_uppercase(),
            color: draft
                .color
                .as_deref()
                .map(|c| sanitize_color(c, "#0ea5e9"))
                .unwrap_or_else(|| "#0ea5e9".into()),
        };

        let site = Site {
            id,
            name,
            url: norm,
            category: draft.category,
            icon,
            builtin: false,
            hidden: false,
            sort,
        };
        self.sites.push(site.clone());
        Ok(site)
    }

    /// Update an existing entry. Built-ins may be renamed/re-pointed/unhidden.
    pub fn update(&mut self, id: &str, draft: SiteDraft) -> AppResult<Site> {
        if !self.sites.iter().any(|s| s.id == id) {
            return Err(AppError::SiteNotFound(id.to_string()));
        }
        let (name, norm) = self.check(&draft, Some(id))?;
        let site = self
            .sites
            .iter_mut()
            .find(|s| s.id == id)
            .expect("checked above");
        site.name = name;
        site.url = norm;
        site.category = draft.category;
        if let (Some(letter), Some(color)) = (draft.letter.as_deref(), draft.color.as_deref()) {
            let letter = sanitize_name(letter);
            site.icon = Icon::Letter {
                text: if letter.is_empty() {
                    site.name.chars().take(2).collect()
                } else {
                    letter.chars().take(2).collect::<String>().to_uppercase()
                },
                color: sanitize_color(color, "#0ea5e9"),
            };
        }
        Ok(site.clone())
    }

    /// Delete a user site. Built-ins are hidden instead of deleted.
    pub fn remove(&mut self, id: &str) -> AppResult<()> {
        let Some(site) = self.sites.iter().find(|s| s.id == id) else {
            return Err(AppError::SiteNotFound(id.to_string()));
        };
        if site.builtin {
            return self.set_hidden(id, true);
        }
        self.sites.retain(|s| s.id != id);
        Ok(())
    }

    pub fn set_hidden(&mut self, id: &str, hidden: bool) -> AppResult<()> {
        let Some(site) = self.sites.iter_mut().find(|s| s.id == id) else {
            return Err(AppError::SiteNotFound(id.to_string()));
        };
        site.hidden = hidden;
        Ok(())
    }

    /// Merge in any built-ins that are missing (e.g. after a partial wipe or a
    /// downgrade of the stored document). A built-in still pointing at a
    /// retired shipped URL is moved forward. A URL the user set with
    /// [`Self::update`] is left alone.
    pub fn ensure_builtins(&mut self) {
        for b in default_sites() {
            if let Some(existing) = self.sites.iter_mut().find(|s| s.id == b.id) {
                if let Some(next) = retired_builtin_url(&existing.id, &existing.url) {
                    existing.url = next.to_string();
                }
            } else {
                self.sites.push(b);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft(name: &str, url: &str) -> SiteDraft {
        SiteDraft {
            name: name.into(),
            url: url.into(),
            category: Category::Watching,
            letter: None,
            color: None,
        }
    }

    #[test]
    fn defaults_ship_openanime_and_animecix_over_https() {
        let r = Registry::new_default();
        let names: Vec<_> = r.sites.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["OpenAnime", "AnimeCix"]);
        assert!(r.sites.iter().all(|s| s.url.starts_with("https://")));
        assert!(r.sites.iter().all(|s| s.builtin));
    }

    #[test]
    fn sanitize_name_strips_markup_and_controls() {
        assert_eq!(
            sanitize_name("<img src=x onerror=alert(1)>"),
            "img src=x onerror=alert(1)"
        );
        assert_eq!(sanitize_name("  multi   space  "), "multi space");
        assert_eq!(sanitize_name("a\u{202e}b"), "ab");
        assert_eq!(sanitize_name(""), "");
    }

    #[test]
    fn sanitize_name_caps_length() {
        let long = "x".repeat(200);
        assert_eq!(sanitize_name(&long).len(), 40);
    }

    #[test]
    fn sanitize_color_normalises_and_falls_back() {
        assert_eq!(sanitize_color("ABC", "#000000"), "#abc");
        assert_eq!(sanitize_color("#ff8800", "#000000"), "#ff8800");
        assert_eq!(sanitize_color("javascript:alert(1)", "#000000"), "#000000");
        assert_eq!(sanitize_color("", "#000000"), "#000000");
    }

    #[test]
    fn add_rejects_http_url() {
        let mut r = Registry::new_default();
        let err = r.add(draft("Bad", "http://example.com")).unwrap_err();
        assert_eq!(err.code(), "insecure_scheme");
    }

    #[test]
    fn add_rejects_duplicate_name_case_insensitively() {
        let mut r = Registry::new_default();
        let err = r
            .add(draft("openANIME", "https://other.example/"))
            .unwrap_err();
        assert_eq!(err.code(), "duplicate_name");
    }

    #[test]
    fn add_rejects_duplicate_url_after_normalisation() {
        let mut r = Registry::new_default();
        // Same site, different spelling: trailing slash + fragment.
        let err = r
            .add(draft("Clone", "https://openani.me/#top"))
            .unwrap_err();
        assert_eq!(err.code(), "duplicate_url");
    }

    #[test]
    fn add_assigns_uuid_id_and_derives_letter() {
        let mut r = Registry::new_default();
        let s = r
            .add(draft("My Site", "https://sub.example.org/watch"))
            .unwrap();
        assert!(s.id.starts_with("user-"));
        assert!(!s.builtin);
        assert_eq!(s.url, "https://sub.example.org/watch");
        match s.icon {
            Icon::Letter { text, color } => {
                assert_eq!(text, "MY");
                assert_eq!(color, "#0ea5e9");
            }
            other => panic!("unexpected icon {other:?}"),
        }
    }

    #[test]
    fn remove_deletes_user_site_but_only_hides_builtin() {
        let mut r = Registry::new_default();
        let s = r.add(draft("Temp", "https://temp.example/")).unwrap();
        r.remove(&s.id).unwrap();
        assert!(r.get(&s.id).is_none());

        r.remove("builtin-openanime").unwrap();
        let openanime = r.get("builtin-openanime").expect("still present");
        assert!(openanime.hidden);
        assert!(r.visible().iter().all(|s| s.id != "builtin-openanime"));
    }

    #[test]
    fn remove_unknown_id_errors() {
        let mut r = Registry::new_default();
        assert_eq!(r.remove("nope").unwrap_err().code(), "site_not_found");
    }

    #[test]
    fn visible_groups_tracking_before_watching() {
        let mut r = Registry::new_default();
        r.add(SiteDraft {
            name: "AniList".into(),
            url: "https://anilist.co/".into(),
            category: Category::Tracking,
            letter: None,
            color: None,
        })
        .unwrap();
        let v = r.visible();
        assert_eq!(v[0].name, "AniList");
        assert_eq!(v[0].category, Category::Tracking);
    }

    #[test]
    fn update_can_repoint_a_builtin_without_duplicate_error() {
        let mut r = Registry::new_default();
        let s = r
            .update(
                "builtin-animecix",
                draft("AnimeCix", "https://animecix.net/"),
            )
            .unwrap();
        assert_eq!(s.url, "https://animecix.net/");
        assert!(s.builtin);
    }

    #[test]
    fn ensure_builtins_restores_wiped_entries() {
        let mut r = Registry::new_default();
        r.sites.clear();
        r.ensure_builtins();
        assert_eq!(r.sites.len(), 2);
    }

    #[test]
    fn ensure_builtins_repoints_retired_animecix_but_not_a_user_edit() {
        let mut r = Registry::new_default();
        r.sites
            .iter_mut()
            .find(|s| s.id == "builtin-animecix")
            .unwrap()
            .url = "https://animecix.com/".into();
        r.ensure_builtins();
        assert_eq!(
            r.get("builtin-animecix").unwrap().url,
            "https://animecix.tv/"
        );

        r.update(
            "builtin-animecix",
            draft("AnimeCix", "https://animecix.net/"),
        )
        .unwrap();
        r.ensure_builtins();
        assert_eq!(
            r.get("builtin-animecix").unwrap().url,
            "https://animecix.net/"
        );
    }

    #[test]
    fn site_host_extracts_partition_key() {
        let mut r = Registry::new_default();
        let s = r
            .add(draft("X", "https://watch.example.org/a/b?c=1"))
            .unwrap();
        assert_eq!(s.host(), "watch.example.org");
    }

    #[test]
    fn registry_survives_json_roundtrip() {
        let mut r = Registry::new_default();
        r.add(draft("Extra", "https://extra.example/")).unwrap();
        let json = serde_json::to_string(&r).unwrap();
        let back: Registry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.sites.len(), r.sites.len());
        assert_eq!(back.sites[2].name, "Extra");
        assert_eq!(back.version, 1);
    }
}
