//! POSIX locale detection for Linux, implementing `gh_core::traits::LocaleProvider`.

use gh_core::traits::LocaleProvider;

/// Linux locale provider using environment variables.
#[derive(Debug, Default, Clone, Copy)]
pub struct LinuxLocaleProvider;

/// Alias for cross-platform `gh_platform::locale::Win32LocaleProvider` parity.
pub type Win32LocaleProvider = LinuxLocaleProvider;

impl LinuxLocaleProvider {
    pub fn new() -> Self {
        Self
    }
}

impl LocaleProvider for LinuxLocaleProvider {
    fn system_language_tag(&self) -> String {
        detect_linux_locale().unwrap_or_default()
    }
}

/// Detects the system locale from environment variables in POSIX priority order.
pub fn detect_linux_locale() -> Option<String> {
    // POSIX priority: LC_ALL > LC_MESSAGES > LANG
    for var in &["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(val) = std::env::var(var) {
            let trimmed = val.trim();
            if !trimmed.is_empty() && trimmed != "C" && trimmed != "POSIX" {
                // Convert "vi_VN.UTF-8" → "vi-VN"
                let normalized = normalize_posix_locale(trimmed);
                if !normalized.is_empty() {
                    return Some(normalized);
                }
            }
        }
    }
    None
}

fn normalize_posix_locale(locale: &str) -> String {
    // Strip encoding suffix: "vi_VN.UTF-8" → "vi_VN"
    let without_encoding = locale.split('.').next().unwrap_or(locale);
    // Strip modifier: "vi_VN@euro" → "vi_VN"
    let without_modifier = without_encoding.split('@').next().unwrap_or(without_encoding);
    // Replace underscore with dash: "vi_VN" → "vi-VN"
    without_modifier.replace('_', "-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_converts_posix_to_bcp47() {
        assert_eq!(normalize_posix_locale("vi_VN.UTF-8"), "vi-VN");
        assert_eq!(normalize_posix_locale("en_US.UTF-8"), "en-US");
        assert_eq!(normalize_posix_locale("fr_FR"), "fr-FR");
        assert_eq!(normalize_posix_locale("C"), "C");
    }
}
