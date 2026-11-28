//! Community guides for third-party map pins (AppSample).
//!
//! An AppSample pin popup shows a short summary and players' screenshots, which come from the
//! site's public comments cache (`cache-v2.lemonapi.com/comments/v2`), not from the marker data.
//! This module turns that thread into an ordered list of guide suggestions:
//!
//! 1. the official item image (`catalog.markerImageUrl`), when the extension sent one;
//! 2. player screenshots, highest votes first, the same order the site shows them.
//!
//! The player can step through the list with the "next guide suggestion" hotkey to skip memes
//! and unrelated photos. Only plain junk is dropped up front: comments without a screenshot,
//! negative votes, malformed image paths, and (after download) images that are too small or
//! not landscape, which are almost never game screenshots.
//!
//! Text follows the HoYoLAB guide rules (`pin_guide::fetch_sync_inner`): the player's language,
//! then English, and never CJK-only text in a Vietnamese or English HUD.

use serde_json::Value;

/// Comments cache endpoint the AppSample page itself calls when a pin popup opens.
pub const APPSAMPLE_COMMENTS_BASE: &str = "https://cache-v2.lemonapi.com/comments/v2";
/// Host that serves AppSample comment screenshots (`/gim/comments/<type>/<id>/<file>`).
pub const APPSAMPLE_MEDIA_BASE: &str = "https://game-cdn.appsample.com";
/// Upper bound on suggestions kept per pin.
pub const MAX_SUGGESTIONS: usize = 20;
/// Smallest screenshot accepted, in pixels.
pub const MIN_SCREENSHOT_W: u32 = 480;
pub const MIN_SCREENSHOT_H: u32 = 270;
/// Accepted width / height range: 4:3 game captures up to ultrawide phone screenshots.
pub const MIN_SCREENSHOT_ASPECT: f64 = 1.2;
pub const MAX_SCREENSHOT_ASPECT: f64 = 2.6;

/// Where a pin's community thread lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommunitySource {
    pub collection: String,
    pub doc_id: String,
}

fn is_safe_key(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

impl CommunitySource {
    /// Builds the source for AppSample marker `doc_id` of type `collection` (e.g. `o17`).
    /// Returns `None` for anything that is not a short `[A-Za-z0-9_-]` key.
    pub fn appsample(collection: &str, doc_id: &str) -> Option<Self> {
        let collection = collection.trim();
        let doc_id = doc_id.trim();
        (is_safe_key(collection) && is_safe_key(doc_id)).then(|| Self {
            collection: collection.to_owned(),
            doc_id: doc_id.to_owned(),
        })
    }

    /// The exact URL the AppSample page requests, so the request hits the same CDN cache entry
    /// (`ttl=1800`) instead of the site's backend.
    pub fn comments_url(&self) -> String {
        format!(
            "{APPSAMPLE_COMMENTS_BASE}?app=gim&ttl=1800&collection={}&docId={}&sort=&page=1&pageSize=100",
            self.collection, self.doc_id
        )
    }
}

/// One guide the player can see for a pin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuideSuggestion {
    /// Text shown under the image (summary and/or the comment's own words).
    pub text: String,
    /// Allowlisted HTTPS image URL, or empty for a text-only guide.
    pub image_url: String,
    /// The site's own item image: exempt from the screenshot shape check.
    pub official: bool,
}

/// `true` when more than 30% of the letters are Chinese, Japanese or Korean.
pub fn is_cjk_dominant(text: &str) -> bool {
    let mut letters = 0usize;
    let mut cjk = 0usize;
    for c in text.chars() {
        if !c.is_alphabetic() {
            continue;
        }
        letters += 1;
        let u = c as u32;
        if matches!(u,
            0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xAC00..=0xD7AF | 0xF900..=0xFAFF)
        {
            cjk += 1;
        }
    }
    letters > 0 && cjk * 10 > letters * 3
}

/// Drops text the HUD cannot show to a Vietnamese or English player (HoYoLAB rule: never put
/// CJK-only text into a VI/EN guide). `lang_tag` is a HoYoLAB tag such as `vi-vn`.
pub fn text_for_hud_lang(text: &str, lang_tag: &str) -> String {
    let short = lang_tag.split('-').next().unwrap_or("").to_ascii_lowercase();
    let cjk_reader = matches!(short.as_str(), "zh" | "ja" | "ko");
    if !cjk_reader && is_cjk_dominant(text) {
        String::new()
    } else {
        text.to_owned()
    }
}

fn is_emoji_like(c: char) -> bool {
    let u = c as u32;
    matches!(u,
        0x1F000..=0x1FAFF | 0x2600..=0x27BF | 0x2B00..=0x2BFF | 0xFE00..=0xFE0F | 0x200D | 0xE0000..=0xE007F)
}

/// Removes emoji (the HUD font has no glyphs for them) and collapses whitespace.
fn tidy_text(raw: &str) -> String {
    let no_emoji: String = raw.chars().filter(|c| !is_emoji_like(*c)).collect();
    no_emoji.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_url_only(text: &str) -> bool {
    let t = text.trim();
    !t.is_empty() && !t.contains(' ') && (t.starts_with("http://") || t.starts_with("https://"))
}

fn mostly_ascii_letters(text: &str) -> bool {
    let letters: Vec<char> = text.chars().filter(|c| c.is_alphabetic()).collect();
    !letters.is_empty() && letters.iter().filter(|c| c.is_ascii()).count() * 10 >= letters.len() * 9
}

/// The comment's words in the player's language, else English, else nothing.
fn comment_text(comment: &Value, short_lang: &str) -> String {
    let content = comment.get("content").and_then(Value::as_str).unwrap_or("").trim();
    let lang = comment
        .get("lang")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let trans = |key: &str| {
        comment
            .get("trans")
            .and_then(|t| t.get(key))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let picked = if !content.is_empty() && lang == short_lang {
        content.to_owned()
    } else if let Some(t) = trans(short_lang) {
        t
    } else if !content.is_empty() && lang == "en" {
        content.to_owned()
    } else if let Some(t) = trans("en") {
        t
    } else if !content.is_empty() && mostly_ascii_letters(content) {
        // Untagged or mis-tagged short Latin text ("on the rock"): readable as English.
        content.to_owned()
    } else {
        String::new()
    };
    if is_url_only(&picked) {
        return String::new();
    }
    tidy_text(&picked)
}

/// `https://game-cdn.appsample.com/gim/comments/...` for a well-formed comment image path.
pub fn comment_image_url(path: &str) -> Option<String> {
    let p = path.trim();
    let lower = p.to_ascii_lowercase();
    let ok_ext = [".jpg", ".jpeg", ".png", ".webp"].iter().any(|e| lower.ends_with(e));
    let ok_chars = p
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'_' | b'-' | b'.'));
    (p.starts_with("/gim/comments/") && !p.contains("..") && !p.contains("//") && ok_ext && ok_chars)
        .then(|| format!("{APPSAMPLE_MEDIA_BASE}{p}"))
}

fn join_text(summary: &str, own: &str) -> String {
    match (summary.is_empty(), own.is_empty()) {
        (false, false) if !summary.eq_ignore_ascii_case(own) => format!("{summary}\n\u{201C}{own}\u{201D}"),
        (false, _) => summary.to_owned(),
        (true, false) => own.to_owned(),
        (true, true) => String::new(),
    }
}

/// Builds the ordered suggestion list from a comments-cache response.
///
/// `lang_tag` is the HUD's HoYoLAB tag (`vi-vn` / `en-us`). `official_text` / `official_image`
/// are what the extension already sent for the pin (the image is the site's item picture).
pub fn build_suggestions(
    body: &Value,
    lang_tag: &str,
    official_text: &str,
    official_image: &str,
) -> Vec<GuideSuggestion> {
    let short = lang_tag.split('-').next().unwrap_or("en").to_ascii_lowercase();
    let data = body.get("data").unwrap_or(&Value::Null);
    let summary_raw = data
        .get("summary")
        .and_then(|s| s.get("content"))
        .and_then(Value::as_str)
        .unwrap_or("");
    // The summary is written in English by the site; it is the English fallback for VI players.
    let summary = text_for_hud_lang(&tidy_text(summary_raw), lang_tag);
    let official_text = text_for_hud_lang(&tidy_text(official_text), lang_tag);
    let lead_text = if summary.is_empty() { official_text.clone() } else { summary.clone() };

    let mut out: Vec<GuideSuggestion> = Vec::new();
    let official_image = official_image.trim();
    if official_image.starts_with("https://") {
        out.push(GuideSuggestion {
            text: lead_text.clone(),
            image_url: official_image.to_owned(),
            official: true,
        });
    }

    let mut comments: Vec<(i64, usize, &Value)> = data
        .get("comments")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .enumerate()
                .map(|(i, c)| (c.get("vote").and_then(Value::as_i64).unwrap_or(0), i, c))
                .collect()
        })
        .unwrap_or_default();
    // Highest votes first, ties in the site's own order.
    comments.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));

    for (vote, _, c) in comments {
        if out.len() >= MAX_SUGGESTIONS {
            break;
        }
        if vote < 0 {
            continue;
        }
        let Some(url) = c.get("image").and_then(Value::as_str).and_then(comment_image_url) else {
            continue;
        };
        if out.iter().any(|s| s.image_url == url) {
            continue;
        }
        let own = text_for_hud_lang(&comment_text(c, &short), lang_tag);
        out.push(GuideSuggestion {
            text: join_text(&summary, &own),
            image_url: url,
            official: false,
        });
    }

    if out.is_empty() && !lead_text.is_empty() {
        out.push(GuideSuggestion {
            text: lead_text,
            image_url: String::new(),
            official: false,
        });
    }
    out
}

/// Whether downloaded bytes look like a game screenshot: decodable, at least
/// `MIN_SCREENSHOT_W` x `MIN_SCREENSHOT_H`, and landscape within the accepted aspect range.
/// Only the image header is read.
pub fn looks_like_screenshot(bytes: &[u8]) -> bool {
    let Ok(reader) = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format() else {
        return false;
    };
    let Ok((w, h)) = reader.into_dimensions() else {
        return false;
    };
    if w < MIN_SCREENSHOT_W || h < MIN_SCREENSHOT_H {
        return false;
    }
    let aspect = f64::from(w) / f64::from(h);
    (MIN_SCREENSHOT_ASPECT..=MAX_SCREENSHOT_ASPECT).contains(&aspect)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::new(w, h);
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    fn thread() -> Value {
        // Synthetic thread shaped like the live cache response: a top-voted off-topic post,
        // useful posts below it, junk the list must skip.
        json!({"data": {
            "summary": {"content": "Behind the crates at the bottom of the stairs."},
            "comments": [
                {"content": "greetings from my favourite character", "lang": "en", "vote": 300,
                 "image": "/gim/comments/o17/900/a1.jpg", "trans": {"ru": "привет"}},
                {"content": "behind the crates", "lang": "en", "vote": 120,
                 "image": "/gim/comments/o17/900/b2.jpeg", "trans": {}},
                {"content": "за ящиками внизу", "lang": "ru", "vote": 80,
                 "image": "/gim/comments/o17/900/c3.jpg", "trans": {"en": "behind the boxes below"}},
                {"content": "https://youtu.be/xyz", "lang": "en", "vote": 70, "image": ""},
                {"content": "no picture here", "lang": "en", "vote": 60},
                {"content": "spam", "lang": "en", "vote": -4, "image": "/gim/comments/o17/900/d4.jpg"},
                {"content": "evil", "lang": "en", "vote": 50, "image": "https://evil.example/x.jpg"},
                {"content": "dots", "lang": "en", "vote": 40, "image": "/gim/comments/../secret.jpg"},
                {"content": "宝箱在箱子后面", "lang": "zh", "vote": 30, "image": "/gim/comments/o17/900/e5.png"},
                {"content": "ok 👍🏻 right here", "lang": "", "vote": 10, "image": "/gim/comments/o17/900/f6.webp"}
            ]
        }})
    }

    #[test]
    fn source_builds_the_page_cache_url_and_rejects_unsafe_keys() {
        let s = CommunitySource::appsample("o17", "107156").unwrap();
        assert_eq!(
            s.comments_url(),
            "https://cache-v2.lemonapi.com/comments/v2?app=gim&ttl=1800&collection=o17&docId=107156&sort=&page=1&pageSize=100"
        );
        assert!(CommunitySource::appsample("o17&x=1", "1").is_none());
        assert!(CommunitySource::appsample("o17", "../1").is_none());
        assert!(CommunitySource::appsample("", "1").is_none());
        assert!(CommunitySource::appsample(&"o".repeat(40), "1").is_none());
    }

    #[test]
    fn suggestions_follow_votes_and_skip_junk() {
        let s = build_suggestions(&thread(), "en-us", "", "");
        let urls: Vec<&str> = s.iter().map(|x| x.image_url.as_str()).collect();
        assert_eq!(
            urls,
            vec![
                "https://game-cdn.appsample.com/gim/comments/o17/900/a1.jpg",
                "https://game-cdn.appsample.com/gim/comments/o17/900/b2.jpeg",
                "https://game-cdn.appsample.com/gim/comments/o17/900/c3.jpg",
                "https://game-cdn.appsample.com/gim/comments/o17/900/e5.png",
                "https://game-cdn.appsample.com/gim/comments/o17/900/f6.webp",
            ]
        );
        assert!(s.iter().all(|x| !x.official));
        // Summary first, the comment's own words quoted below it.
        assert_eq!(
            s[1].text,
            "Behind the crates at the bottom of the stairs.\n\u{201C}behind the crates\u{201D}"
        );
    }

    #[test]
    fn text_uses_player_language_then_english_and_never_cjk() {
        let s = build_suggestions(&thread(), "vi-vn", "", "");
        // Russian post: no Vietnamese translation, so its English translation.
        assert!(s[2].text.ends_with("\u{201C}behind the boxes below\u{201D}"), "{}", s[2].text);
        // Chinese-only post: the image stays, its words do not.
        assert_eq!(s[3].text, "Behind the crates at the bottom of the stairs.");
        // Emoji stripped from untagged Latin text.
        assert!(s[4].text.ends_with("\u{201C}ok right here\u{201D}"), "{}", s[4].text);

        let vi_post = json!({"data": {"comments": [
            {"content": "sau thùng gỗ", "lang": "vi", "vote": 5, "image": "/gim/comments/o1/1/a.jpg",
             "trans": {"en": "behind the crate"}}
        ]}});
        assert_eq!(build_suggestions(&vi_post, "vi-vn", "", "")[0].text, "sau thùng gỗ");
        assert_eq!(build_suggestions(&vi_post, "en-us", "", "")[0].text, "behind the crate");
    }

    #[test]
    fn official_image_comes_first_and_text_only_needs_a_summary() {
        let s = build_suggestions(&thread(), "en-us", "", "https://game-cdn.appsample.com/gim/item-imgs/1.jpg");
        assert!(s[0].official);
        assert_eq!(s[0].image_url, "https://game-cdn.appsample.com/gim/item-imgs/1.jpg");
        assert_eq!(s[0].text, "Behind the crates at the bottom of the stairs.");
        assert_eq!(s.len(), 6);

        let summary_only = json!({"data": {"summary": {"content": "On the roof."}, "comments": []}});
        assert_eq!(
            build_suggestions(&summary_only, "en-us", "", ""),
            vec![GuideSuggestion { text: "On the roof.".into(), image_url: String::new(), official: false }]
        );
        assert!(build_suggestions(&json!({"data": {"comments": []}}), "en-us", "", "").is_empty());
        assert!(build_suggestions(&json!({"oops": 1}), "en-us", "", "").is_empty());
    }

    #[test]
    fn suggestion_list_is_capped() {
        let comments: Vec<Value> = (0..50)
            .map(|i| json!({"content": "x", "lang": "en", "vote": i, "image": format!("/gim/comments/o1/1/{i}.jpg")}))
            .collect();
        let s = build_suggestions(&json!({"data": {"comments": comments}}), "en-us", "", "");
        assert_eq!(s.len(), MAX_SUGGESTIONS);
        assert!(s[0].image_url.ends_with("/49.jpg"));
    }

    #[test]
    fn screenshot_shape_check() {
        assert!(looks_like_screenshot(&png(1920, 1080)));
        assert!(looks_like_screenshot(&png(800, 600)));
        assert!(looks_like_screenshot(&png(2400, 1080)));
        assert!(!looks_like_screenshot(&png(250, 154)), "tiny reaction image");
        assert!(!looks_like_screenshot(&png(885, 1080)), "portrait");
        assert!(!looks_like_screenshot(&png(1000, 1000)), "square");
        assert!(!looks_like_screenshot(&png(3000, 1000)), "banner");
        assert!(!looks_like_screenshot(b"not an image"));
    }

    #[test]
    fn hud_language_filter_drops_cjk_text_for_vi_and_en_only() {
        assert_eq!(text_for_hud_lang("宝箱在箱子后面", "vi-vn"), "");
        assert_eq!(text_for_hud_lang("宝箱在箱子后面", "en-us"), "");
        assert_eq!(text_for_hud_lang("宝箱在箱子后面", "zh-cn"), "宝箱在箱子后面");
        assert_eq!(text_for_hud_lang("Chest behind 箱子", "en-us"), "Chest behind 箱子");
        assert_eq!(text_for_hud_lang("Rương sau thùng", "vi-vn"), "Rương sau thùng");
    }
}
