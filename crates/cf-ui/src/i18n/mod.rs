use cf_core::Locale;

pub fn set_locale(locale: Locale) {
    rust_i18n::set_locale(locale.code());
}

/// Looks up a literal translation key. Missing keys fall back to English.
pub fn tr(key: &'static str) -> String {
    rust_i18n::t!(key).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_locales_have_core_strings() {
        for locale in Locale::ALL {
            set_locale(locale);
            for key in ["app.name", "settings.title", "about.title"] {
                assert!(!tr(key).is_empty(), "missing {key} for {}", locale.code());
                assert_ne!(tr(key), key, "untranslated {key} for {}", locale.code());
            }
        }
    }
}
