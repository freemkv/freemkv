const LOCALE_DE: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/locales/de.json"));
const LOCALE_EN: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/locales/en.json"));
const LOCALE_ES: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/locales/es.json"));
const LOCALE_FR: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/locales/fr.json"));
const LOCALE_IT: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/locales/it.json"));
const LOCALE_NL: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/locales/nl.json"));
const LOCALE_PT: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/locales/pt.json"));

fn bundled_locale(code: &str) -> Option<&'static str> {
    match code {
        "de" => Some(LOCALE_DE),
        "en" => Some(LOCALE_EN),
        "es" => Some(LOCALE_ES),
        "fr" => Some(LOCALE_FR),
        "it" => Some(LOCALE_IT),
        "nl" => Some(LOCALE_NL),
        "pt" => Some(LOCALE_PT),
        _ => None,
    }
}
