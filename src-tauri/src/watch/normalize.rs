//! Canonical episode identity.
//!
//! Every site spells the same episode differently:
//!
//! * `Mushoku Tensei III - Bölüm 5` (widget text)
//! * `mushokus3_b5.mp4` (file name)
//! * `mushoku_tensei-s3-b5.a256` (file name)
//!
//! The ledger key and the AniList match both need one canonical form, so the
//! grammar lives here — in the engine, never in a user-supplied descriptor.
//!
//! Tokens are split on every character that is not a letter or a digit (after
//! a Turkish-aware fold), then classified in this order:
//!
//! 1. the last dotted token is the file extension and is dropped,
//! 2. `sezon`/`season` + number (either order) sets the season,
//! 3. `bölüm`/`episode` + number (either order) sets the episode,
//! 4. a marker glued to a word (`mushokus3`, `bleachb5`) is split off,
//! 5. `s3` / `b5` / `e12` / `ep3` and `2x05` set season and episode,
//! 6. roman (`IV`) and ordinal (`2nd`) tokens set the season,
//! 7. quality / codec / language junk is dropped,
//! 8. a four digit year is dropped, and any other bare number becomes the
//!    episode when no marked episode was found,
//! 9. whatever is left is the title.
//!
//! Nothing here is configurable by a descriptor: a user-supplied adapter may
//! choose *where* a title and an episode number come from, never how they are
//! interpreted. That is what keeps two sites' lists comparable.

use std::collections::BTreeSet;

/// Longest sensible episode number; a marker like `b1234` is still accepted.
const EPISODE_MAX: u32 = 9999;
/// A bare number this large is far more likely to be noise than an episode.
const BARE_MAX: u32 = 999;

/// A four digit token in this range is a year, not an episode.
fn is_year(n: u32) -> bool {
    (1900..=2099).contains(&n)
}

/// Canonical identity of one episode.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Release {
    /// Folded, space-joined title tokens.
    pub title: String,
    /// Defaults to `1` when the source says nothing about seasons.
    pub season: u32,
    pub episode: u32,
}

/// What the parser could read out of one string, before defaults are applied.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Fields {
    pub title: String,
    pub season: Option<u32>,
    pub episode: Option<u32>,
}

/// Folded, space-joined title. Equivalent to [`parse_fields`]`.title`.
pub fn canonical_title(raw: &str) -> String {
    parse_fields(raw).title
}

/// Full canonical identity, or `None` when the string carries no episode
/// number at all (a show page, a banner, a "watch now" label).
pub fn parse_release(raw: &str) -> Option<Release> {
    let fields = parse_fields(raw);
    let episode = fields.episode?;
    Some(Release {
        title: fields.title,
        season: fields.season.unwrap_or(1),
        episode,
    })
}

/// Episode number carried by a widget field, e.g. `"Bölüm 5"`, `"5"`, `"B-5"`.
pub fn episode_from_text(raw: &str) -> Option<u32> {
    parse_fields(raw).episode
}

/// Season number carried by a widget field, e.g. `"3. Sezon"`, `"Season 3"`.
///
/// `None` means the source did not say — not that the season is one.
pub fn season_from_text(raw: &str) -> Option<u32> {
    parse_fields(raw).season
}

/// Sørensen–Dice overlap of two canonical titles, in whole percent.
///
/// Used to rank AniList candidates before the id is trusted; identical
/// strings score 100, unrelated ones score 0.
pub fn similarity(a: &str, b: &str) -> u32 {
    let left: BTreeSet<&str> = a.split(' ').filter(|t| !t.is_empty()).collect();
    let right: BTreeSet<&str> = b.split(' ').filter(|t| !t.is_empty()).collect();
    if left.is_empty() || right.is_empty() {
        return 0;
    }
    let common = left.intersection(&right).count();
    let total = left.len() + right.len();
    ((2 * common * 100) / total) as u32
}

/// One token plus whether a `.` immediately preceded it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Token {
    text: String,
    dotted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Marker {
    Season,
    Episode,
}

/// Parse a release string into its raw parts.
pub fn parse_fields(raw: &str) -> Fields {
    let mut toks = tokenize(raw);

    // 1. File extension: only the final dotted token, and only when it cannot
    // be read as a marker itself (`Show.s3` keeps its season).
    if toks.len() > 1 {
        let last = &toks[toks.len() - 1];
        if last.dotted && is_extension(&last.text) {
            toks.pop();
        }
    }

    let mut season: Option<u32> = None;
    let mut episode: Option<u32> = None;
    // Bare numbers are resolved at the end: `3. Sezon` only reveals that the
    // `3` was a season once the next token has been seen.
    let mut bare: Vec<(usize, u32)> = Vec::new();
    let mut title: Vec<String> = Vec::new();

    let mut i = 0;
    while i < toks.len() {
        let text = toks[i].text.clone();

        if let Some(marker) = marker_word(&text) {
            // `Sezon 2` / `Bölüm 5`: the number follows the word. In
            // `3. Sezon 5. Bölüm` the number *precedes* the word, so a number
            // in front of it wins and this word carries nothing.
            let after_number = i > 0
                && number(&toks[i - 1].text).is_some_and(|n| !is_year(n) && n > 0);
            if !after_number {
                if let Some(n) = toks.get(i + 1).and_then(|t| number(&t.text)) {
                    match marker {
                        Marker::Season => season = Some(n),
                        Marker::Episode => episode = Some(n),
                    }
                    i += 2;
                    continue;
                }
            }
            // A bare marker word (`Sezon 5. Bölüm` keeps only the numbers).
            i += 1;
            continue;
        }

        if let Some(n) = word_glued(&text, &["sezon", "season"]) {
            season = Some(n);
            i += 1;
            continue;
        }
        if let Some(n) = word_glued(&text, &["bolum", "episode", "ep"]) {
            episode = Some(n);
            i += 1;
            continue;
        }

        if let Some((s, e)) = sx_form(&text) {
            season = Some(s);
            episode = Some(e);
            i += 1;
            continue;
        }

        if let Some((head, marker, n)) = glued_marker(&text) {
            match marker {
                Marker::Season => season = Some(n),
                Marker::Episode => episode = Some(n),
            }
            if !head.is_empty() {
                title.push(head);
            }
            i += 1;
            continue;
        }

        if is_junk(&text) {
            i += 1;
            continue;
        }

        if let Some(s) = roman_season(&text) {
            season = Some(s);
            i += 1;
            continue;
        }
        if let Some(s) = ordinal_season(&text) {
            season = Some(s);
            i += 1;
            continue;
        }

        if let Some(n) = number(&text) {
            if is_year(n) {
                // A year is neither an episode nor part of the title.
                i += 1;
                continue;
            }
            bare.push((i, n));
            i += 1;
            continue;
        }

        title.push(text);
        i += 1;
    }

    // 8. Bare numbers: one followed by `sezon`/`season` was a season; the last
    // of the rest is the episode (`Show 05`, `Show - 12`).
    for (idx, n) in bare {
        let season_word_next = toks
            .get(idx + 1)
            .is_some_and(|t| marker_word(&t.text) == Some(Marker::Season));
        if season_word_next {
            season = Some(n);
        } else if n > 0 && n <= BARE_MAX {
            episode = Some(n);
        }
    }

    Fields {
        title: title.join(" "),
        season,
        episode,
    }
}

/// Split a string into folded tokens, remembering a preceding `.`.
fn tokenize(raw: &str) -> Vec<Token> {
    let mut out: Vec<Token> = Vec::new();
    let mut cur = String::new();
    let mut dotted = false;
    let mut after_dot = false;
    for c in raw.chars() {
        match fold_char(c) {
            Some(folded) => {
                if cur.is_empty() {
                    dotted = after_dot;
                }
                cur.push(folded);
                after_dot = false;
            }
            None => {
                if !cur.is_empty() {
                    out.push(Token {
                        text: std::mem::take(&mut cur),
                        dotted,
                    });
                }
                after_dot = c == '.';
            }
        }
    }
    if !cur.is_empty() {
        out.push(Token {
            text: cur,
            dotted,
        });
    }
    out
}

/// Fold one character: Turkish letters to ASCII, the rest lowercased.
/// Non-ASCII letters (CJK, accented) are kept as they are, everything else is
/// a separator.
fn fold_char(c: char) -> Option<char> {
    match c {
        'ç' | 'Ç' => Some('c'),
        'ğ' | 'Ğ' => Some('g'),
        'ı' | 'İ' => Some('i'),
        'ö' | 'Ö' => Some('o'),
        'ş' | 'Ş' => Some('s'),
        'ü' | 'Ü' => Some('u'),
        'A'..='Z' => Some(c.to_ascii_lowercase()),
        _ if c.is_alphanumeric() => Some(c),
        _ => None,
    }
}

/// A token that is only a letter/digit run of extension length.
fn is_extension(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 5
        && text.bytes().all(|b| b.is_ascii_alphanumeric())
        && number(text).is_none()
        && glued_marker(text).is_none()
}

/// `sezon` / `season` / `bölüm` / `episode` as a standalone word.
fn marker_word(text: &str) -> Option<Marker> {
    match text {
        "sezon" | "sezonu" | "season" => Some(Marker::Season),
        "bolum" | "bolumu" | "episode" | "ep" | "episodes" => Some(Marker::Episode),
        _ => None,
    }
}

/// `season3` / `bolum5` written without a separator.
fn word_glued(text: &str, words: &[&str]) -> Option<u32> {
    for w in words {
        if let Some(tail) = text.strip_prefix(w) {
            if let Some(n) = number(tail) {
                return Some(n);
            }
        }
    }
    None
}

/// `mushokus3` → `("mushoku", Season, 3)`, `b5` → `("", Episode, 5)`.
///
/// The marker must either start the token or follow a letter, so `1080p` is
/// never read as an episode number.
fn glued_marker(text: &str) -> Option<(String, Marker, u32)> {
    // Longest tag first: `ep3` must not be read as `e` + `p3`.
    const TAGS: [(&str, Marker, usize); 4] = [
        ("ep", Marker::Episode, 4),
        ("s", Marker::Season, 2),
        ("b", Marker::Episode, 3),
        ("e", Marker::Episode, 3),
    ];
    for (tag, marker, max_digits) in TAGS {
        let Some(pos) = text.rfind(tag) else {
            continue;
        };
        let tail = &text[pos + tag.len()..];
        if tail.is_empty() || tail.len() > max_digits || !tail.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let head = &text[..pos];
        if !head.is_empty() && !head.ends_with(|c: char| c.is_alphabetic()) {
            continue;
        }
        let Ok(n) = tail.parse::<u32>() else {
            continue;
        };
        if n == 0 || n > EPISODE_MAX {
            continue;
        }
        return Some((head.to_string(), marker, n));
    }
    None
}

/// `2x05` → `(2, 5)`.
fn sx_form(text: &str) -> Option<(u32, u32)> {
    let (left, right) = text.split_once('x')?;
    let season = number(left)?;
    let episode = number(right)?;
    if !(1..=99).contains(&season) || episode == 0 || episode > EPISODE_MAX {
        return None;
    }
    Some((season, episode))
}

/// Roman season numerals. `i` is deliberately absent: in English titles it is
/// far more often the pronoun than a season number, and season one is the
/// default anyway.
fn roman_season(text: &str) -> Option<u32> {
    match text {
        "ii" => Some(2),
        "iii" => Some(3),
        "iv" => Some(4),
        "v" => Some(5),
        "vi" => Some(6),
        "vii" => Some(7),
        "viii" => Some(8),
        "ix" => Some(9),
        "x" => Some(10),
        _ => None,
    }
}

/// `2nd`, `3rd`, `4th`, `21st`.
fn ordinal_season(text: &str) -> Option<u32> {
    for suffix in ["st", "nd", "rd", "th"] {
        if let Some(head) = text.strip_suffix(suffix) {
            if let Some(n) = number(head) {
                if (1..=99).contains(&n) {
                    return Some(n);
                }
            }
        }
    }
    None
}

/// Quality, codec, language and boilerplate tokens that are not the title.
fn is_junk(text: &str) -> bool {
    const JUNK: [&str; 35] = [
        "1080p", "720p", "480p", "360p", "2160p", "4k", "x264", "x265", "h264", "h265", "hevc",
        "av1", "aac", "webdl", "webrip", "web", "dl", "bluray", "bd", "dvdrip", "hdrip", "hdtv",
        "sub", "subs", "subbed", "dub", "dubbed", "multi", "turkce", "altyazili", "izle", "hd",
        "full", "uncensored", "censored",
    ];
    if JUNK.contains(&text) {
        return true;
    }
    // `v2`, `v3` … release revisions.
    if let Some(tail) = text.strip_prefix('v') {
        if !tail.is_empty() && tail.len() <= 2 && tail.bytes().all(|b| b.is_ascii_digit()) {
            return true;
        }
    }
    false
}

/// A plain decimal number of at most four digits.
fn number(text: &str) -> Option<u32> {
    if text.is_empty() || text.len() > 4 {
        return None;
    }
    if !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse::<u32>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rel(title: &str, season: u32, episode: u32) -> Release {
        Release {
            title: title.into(),
            season,
            episode,
        }
    }

    #[test]
    fn both_spellings_of_one_release_agree() {
        // The handoff note's example: a glued file name and a separated one
        // must produce the same season/episode pair.
        let a = parse_release("mushokus3_b5.mp4").expect("glued");
        let b = parse_release("mushoku_tensei-s3-b5.a256").expect("separated");
        assert_eq!((a.season, a.episode), (3, 5));
        assert_eq!((b.season, b.episode), (3, 5));
        assert_eq!(a.title, "mushoku");
        assert_eq!(b.title, "mushoku tensei");
    }

    #[test]
    fn reads_turkish_widget_text() {
        assert_eq!(
            parse_release("Mushoku Tensei 3. Sezon 5. Bölüm İzle"),
            Some(rel("mushoku tensei", 3, 5))
        );
        assert_eq!(
            parse_release("Bleach - 5. Bölüm"),
            Some(rel("bleach", 1, 5))
        );
        assert_eq!(
            parse_release("One Piece Bölüm 1102 izle"),
            Some(rel("one piece", 1, 1102))
        );
    }

    #[test]
    fn reads_english_and_roman_seasons() {
        assert_eq!(
            parse_release("Overlord IV Bölüm 9"),
            Some(rel("overlord", 4, 9))
        );
        assert_eq!(
            parse_release("Mushoku Tensei 2nd Season Episode 3"),
            Some(rel("mushoku tensei", 2, 3))
        );
        assert_eq!(
            parse_release("Kimetsu no Yaiba Season 3 Episode 11"),
            Some(rel("kimetsu no yaiba", 3, 11))
        );
    }

    #[test]
    fn reads_episode_first_spelling() {
        assert_eq!(
            parse_release("Jujutsu Kaisen Episode 24"),
            Some(rel("jujutsu kaisen", 1, 24))
        );
        assert_eq!(
            parse_release("5x03 - Some Show"),
            Some(rel("some show", 5, 3))
        );
    }

    #[test]
    fn drops_quality_tokens_and_year() {
        assert_eq!(
            parse_release("Mushoku Tensei 2021 05 1080p x265"),
            Some(rel("mushoku tensei", 1, 5))
        );
        assert_eq!(
            parse_release("mushoku_tensei_s2_b10_1080p.WEB-DL.x264.mkv"),
            Some(rel("mushoku tensei", 2, 10))
        );
    }

    #[test]
    fn a_title_without_an_episode_is_not_a_release() {
        // `Steins;Gate 0` is the show's name, and `0` is not an episode.
        assert_eq!(parse_release("Steins;Gate 0"), None);
        assert_eq!(parse_release("Son Eklenen Bölümler"), None);
        assert_eq!(parse_release(""), None);
    }

    #[test]
    fn title_tokens_survive_without_the_junk() {
        assert_eq!(canonical_title("Bleach - Bölüm 5 İzle 1080p"), "bleach");
        assert_eq!(
            canonical_title("Kaguya-sama wa Kokurasetai: Ultra Romantic"),
            "kaguya sama wa kokurasetai ultra romantic"
        );
    }

    #[test]
    fn season_helpers_report_absence() {
        assert_eq!(season_from_text("2. Sezon"), Some(2));
        assert_eq!(season_from_text("Bölüm 5"), None);
        assert_eq!(episode_from_text("Bölüm 12"), Some(12));
        assert_eq!(episode_from_text("Son Eklenenler"), None);
    }

    #[test]
    fn similarity_ranks_candidates() {
        assert_eq!(similarity("mushoku tensei", "mushoku tensei"), 100);
        assert_eq!(similarity("bleach", "naruto"), 0);
        // Extra words dilute but do not break the match.
        let partial = similarity("mushoku tensei", "mushoku tensei jobless reincarnation");
        assert!(partial > 60 && partial < 100, "got {partial}");
    }

    #[test]
    fn glued_markers_need_a_word_boundary() {
        // `1080p` must not become episode 80 of season … anything.
        assert_eq!(parse_release("Show 1080p Bölüm 3"), Some(rel("show", 1, 3)));
        assert_eq!(glued_marker("1080p"), None);
        assert_eq!(
            glued_marker("ep13"),
            Some((String::new(), Marker::Episode, 13))
        );
    }
}
