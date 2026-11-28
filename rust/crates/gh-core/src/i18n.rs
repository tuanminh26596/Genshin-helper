//! Bilingual VI/EN localization translator with embedded `locales/{vi,en}.json`.
//! Ported from `src/i18n.py`.

use crate::traits::LocaleProvider;
use std::collections::{BTreeMap, HashMap};
use std::sync::{OnceLock, RwLock};

const VI_JSON: &str = include_str!("../../../../assets/locales/vi.json");
const EN_JSON: &str = include_str!("../../../../assets/locales/en.json");

pub const SUPPORTED_LANGUAGES: [&str; 2] = ["vi", "en"];
pub const DEFAULT_LANGUAGE: &str = "en";

static GLOBAL_TRANSLATOR: OnceLock<Translator> = OnceLock::new();

/// Normalizes explicit tags (`"vi"`, `"VI-VN"`, `"vietnamese"`, `"en_US"`, `"english"`) -> `Some("vi"|"en")`.
/// Returns `None` for `None`, `""`, `"auto"`, `"system"`, `"default"`, or unknown tags.
pub fn normalize_lang_code(raw: Option<&str>) -> Option<&'static str> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }

    // Strip encoding/modifier suffixes like `.UTF-8` or `@euro`
    let base = raw
        .split(['.', '@'])
        .next()
        .unwrap_or(raw)
        .trim()
        .to_ascii_lowercase()
        .replace('_', "-");

    if matches!(base.as_str(), "" | "auto" | "system" | "default") {
        return None;
    }

    if base == "vi" || base.starts_with("vi-") || base.starts_with("vietnamese") {
        return Some("vi");
    }
    if base == "en" || base.starts_with("en-") || base.starts_with("english") {
        return Some("en");
    }
    None
}

/// Resolves language with priority:
/// explicit (`"vi"`/`"en"`) > `provider.system_language_tag()` > env (`LC_ALL`, `LC_MESSAGES`, `LANG`) > `"en"`.
pub fn resolve_language(
    setting: Option<&str>,
    provider: Option<&dyn LocaleProvider>,
) -> &'static str {
    if let Some(explicit) = normalize_lang_code(setting) {
        return explicit;
    }

    if let Some(prov) = provider {
        let tag = prov.system_language_tag();
        if let Some(detected) = normalize_lang_code(Some(&tag)) {
            return detected;
        }
    }

    for env_key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(val) = std::env::var(env_key)
            && let Some(detected) = normalize_lang_code(Some(&val))
        {
            return detected;
        }
    }

    DEFAULT_LANGUAGE
}

#[derive(Debug)]
struct TranslatorState {
    bundles: HashMap<String, HashMap<String, String>>,
    language: String,
}

#[derive(Debug)]
pub struct Translator {
    state: RwLock<TranslatorState>,
}

impl Translator {
    pub fn embedded(setting: Option<&str>, provider: Option<&dyn LocaleProvider>) -> Self {
        let mut bundles = HashMap::new();
        let vi_map: HashMap<String, String> =
            serde_json::from_str(VI_JSON).expect("embedded vi.json must be valid JSON");
        let en_map: HashMap<String, String> =
            serde_json::from_str(EN_JSON).expect("embedded en.json must be valid JSON");
        bundles.insert("vi".to_owned(), vi_map);
        bundles.insert("en".to_owned(), en_map);
        Self::from_bundles(bundles, setting, provider)
    }

    pub fn from_bundles(
        bundles: HashMap<String, HashMap<String, String>>,
        setting: Option<&str>,
        provider: Option<&dyn LocaleProvider>,
    ) -> Self {
        let lang = resolve_language(setting, provider).to_owned();
        Self {
            state: RwLock::new(TranslatorState {
                bundles,
                language: lang,
            }),
        }
    }

    pub fn get_language(&self) -> String {
        self.state
            .read()
            .expect("translator lock poisoned")
            .language
            .clone()
    }

    pub fn set_language(
        &self,
        setting: Option<&str>,
        provider: Option<&dyn LocaleProvider>,
    ) -> String {
        let resolved = resolve_language(setting, provider).to_owned();
        let mut guard = self.state.write().expect("translator lock poisoned");
        guard.language = resolved.clone();
        resolved
    }

    pub fn get_locale_dict(&self, lang: &str) -> BTreeMap<String, String> {
        let resolved = normalize_lang_code(Some(lang)).unwrap_or(DEFAULT_LANGUAGE);
        let guard = self.state.read().expect("translator lock poisoned");
        let mut out = BTreeMap::new();
        if let Some(bundle) = guard.bundles.get(resolved) {
            for (k, v) in bundle {
                out.insert(k.clone(), v.clone());
            }
        }
        out
    }

    pub fn t(&self, key: &str, args: &[(&str, &str)]) -> String {
        self.t_lang(key, None, args)
    }

    pub fn t_lang(&self, key: &str, lang: Option<&str>, args: &[(&str, &str)]) -> String {
        let guard = self.state.read().expect("translator lock poisoned");
        let target_lang = match lang {
            Some(l) => resolve_language(Some(l), None),
            None => guard.language.as_str(),
        };

        let template = lookup_non_empty(&guard.bundles, target_lang, key)
            .or_else(|| lookup_non_empty(&guard.bundles, DEFAULT_LANGUAGE, key))
            .unwrap_or(key);

        format_safe(template, args)
    }
}

fn lookup_non_empty<'a>(
    bundles: &'a HashMap<String, HashMap<String, String>>,
    lang: &str,
    key: &str,
) -> Option<&'a str> {
    let val = bundles.get(lang)?.get(key)?;
    if val.trim().is_empty() {
        None
    } else {
        Some(val.as_str())
    }
}

/// Safe Python `format_map(_SafeFormatDict(**params))` equivalent:
/// missing `{name}` stays `"{name}"`, `{{`/`}}` unescape, malformed braces return `template`.
fn format_safe(template: &str, args: &[(&str, &str)]) -> String {
    if args.is_empty() {
        return template.to_owned();
    }

    let mut out = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '{' => {
                if chars.peek() == Some(&'{') {
                    chars.next();
                    out.push('{');
                    continue;
                }
                let mut name = String::new();
                let mut closed = false;
                for inner in chars.by_ref() {
                    if inner == '}' {
                        closed = true;
                        break;
                    }
                    if inner == '{' {
                        return template.to_owned();
                    }
                    name.push(inner);
                }
                if !closed || name.is_empty() {
                    return template.to_owned();
                }
                if let Some((_, val)) = args.iter().find(|(k, _)| *k == name) {
                    out.push_str(val);
                } else {
                    out.push('{');
                    out.push_str(&name);
                    out.push('}');
                }
            }
            '}' => {
                if chars.peek() == Some(&'}') {
                    chars.next();
                    out.push('}');
                } else {
                    return template.to_owned();
                }
            }
            other => out.push(other),
        }
    }
    out
}

pub fn global() -> &'static Translator {
    GLOBAL_TRANSLATOR.get_or_init(|| Translator::embedded(None, None))
}

pub fn get_language() -> String {
    global().get_language()
}

pub fn set_language(setting: Option<&str>, provider: Option<&dyn LocaleProvider>) -> String {
    global().set_language(setting, provider)
}

pub fn get_locale_dict(lang: &str) -> BTreeMap<String, String> {
    global().get_locale_dict(lang)
}

pub fn t(key: &str, args: &[(&str, &str)]) -> String {
    global().t(key, args)
}

pub fn t_lang(key: &str, lang: Option<&str>, args: &[(&str, &str)]) -> String {
    global().t_lang(key, lang, args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    struct MockLocale(&'static str);
    impl LocaleProvider for MockLocale {
        fn system_language_tag(&self) -> String {
            self.0.to_owned()
        }
    }

    fn extract_placeholders(s: &str) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        let mut chars = s.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '{' {
                if chars.peek() == Some(&'{') {
                    chars.next();
                    continue;
                }
                let mut name = String::new();
                for inner in chars.by_ref() {
                    if inner == '}' {
                        break;
                    }
                    name.push(inner);
                }
                if !name.is_empty() {
                    out.insert(name);
                }
            }
        }
        out
    }

    #[test]
    fn embedded_vi_and_en_bundles_have_exact_key_and_placeholder_parity() {
        let tr = Translator::embedded(Some("en"), None);
        let vi = tr.get_locale_dict("vi");
        let en = tr.get_locale_dict("en");

        assert!(vi.len() >= 99);
        assert_eq!(vi.len(), en.len());

        let vi_keys: BTreeSet<_> = vi.keys().collect();
        let en_keys: BTreeSet<_> = en.keys().collect();
        assert_eq!(vi_keys, en_keys);

        for (k, v) in &vi {
            assert!(!v.trim().is_empty(), "empty vi value for {k}");
            let en_v = &en[k];
            assert!(!en_v.trim().is_empty(), "empty en value for {k}");
            assert_eq!(
                extract_placeholders(v),
                extract_placeholders(en_v),
                "placeholder mismatch for {k}"
            );
        }

        for required in [
            "tray.tooltip.connected",
            "hud.state.arrived",
            "hud.state.floor_mismatch",
            "hud.state.lost",
            "toast.synced.title",
            "toast.route.loaded_detail",
            "toast.map_update.updated_detail",
            "uipi.warning.title",
            "borderless.already_running",
            "hdr.warning.title",
        ] {
            assert!(
                vi.keys().any(|k| k == required || k.starts_with(required)),
                "missing domain key/prefix: {required}"
            );
        }
    }

    #[test]
    fn resolve_language_priority_with_locale_provider() {
        let vi_prov = MockLocale("vi-VN");
        let en_prov = MockLocale("en_US.UTF-8");
        let fr_prov = MockLocale("fr-FR");

        assert_eq!(resolve_language(Some("vi"), Some(&en_prov)), "vi");
        assert_eq!(resolve_language(Some("ENGLISH"), Some(&vi_prov)), "en");
        assert_eq!(resolve_language(Some("auto"), Some(&vi_prov)), "vi");
        assert_eq!(resolve_language(Some("system"), Some(&en_prov)), "en");
        assert_eq!(resolve_language(None, Some(&vi_prov)), "vi");
        assert_eq!(resolve_language(Some("auto"), Some(&fr_prov)), "en");
    }

    #[test]
    fn translator_fallback_and_safe_format_dict() {
        let mut bundles = HashMap::new();
        let mut vi = HashMap::new();
        vi.insert("empty_val".to_owned(), "".to_owned());
        vi.insert("ws_val".to_owned(), "   \t\n  ".to_owned());
        vi.insert("both_empty".to_owned(), "   ".to_owned());
        vi.insert("greet".to_owned(), "Xin chào {name}, map {map}!".to_owned());

        let mut en = HashMap::new();
        en.insert("empty_val".to_owned(), "Fallback EN".to_owned());
        en.insert("ws_val".to_owned(), "Whitespace EN".to_owned());
        en.insert("both_empty".to_owned(), "".to_owned());
        en.insert("en_only".to_owned(), "Only English".to_owned());

        bundles.insert("vi".to_owned(), vi);
        bundles.insert("en".to_owned(), en);

        let tr = Translator::from_bundles(bundles, Some("vi"), None);
        assert_eq!(tr.t("empty_val", &[]), "Fallback EN");
        assert_eq!(tr.t("ws_val", &[]), "Whitespace EN");
        assert_eq!(tr.t("both_empty", &[]), "both_empty");
        assert_eq!(tr.t("en_only", &[]), "Only English");
        assert_eq!(tr.t("missing.key", &[]), "missing.key");
        assert_eq!(
            tr.t("greet", &[("name", "Traveler")]),
            "Xin chào Traveler, map {map}!"
        );
    }

    #[test]
    fn real_bundle_interpolation_matches_python_suite() {
        let tr = Translator::embedded(Some("en"), None);
        let floor_msg = tr.t_lang(
            "hud.state.floor_mismatch",
            Some("vi"),
            &[("player_floor", "G"), ("target_floor", "B2")],
        );
        assert!(floor_msg.contains("G") && floor_msg.contains("B2"));

        let route_msg = tr.t_lang(
            "toast.route.loaded_detail",
            Some("en"),
            &[("sections", "7")],
        );
        assert!(route_msg.contains("(7 sections)"));

        let map_msg = tr.t_lang(
            "toast.map_update.updated_detail",
            Some("vi"),
            &[("count", "12")],
        );
        assert!(map_msg.contains("12") && !map_msg.contains("{count}"));
    }

    #[test]
    fn get_locale_dict_falls_back_to_default_en_for_auto_and_unknown_without_env_leakage() {
        let tr = Translator::embedded(Some("vi"), None);
        let en = tr.get_locale_dict("en");
        assert_eq!(normalize_lang_code(Some("auto")), None);
        assert_eq!(normalize_lang_code(Some("unknown")), None);
        assert_eq!(tr.get_locale_dict("auto"), en);
        assert_eq!(tr.get_locale_dict("system"), en);
        assert_eq!(tr.get_locale_dict("unknown"), en);
        assert_eq!(tr.get_locale_dict(""), en);
        assert_eq!(tr.get_locale_dict("VI-VN"), tr.get_locale_dict("vi"));
    }
}
