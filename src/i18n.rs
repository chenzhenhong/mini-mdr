use fluent::{FluentArgs, FluentBundle, FluentResource};
use std::sync::{Mutex, OnceLock};
use unic_langid::LanguageIdentifier;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Language {
    En,
    Zh,
}

impl Language {
    fn id(&self) -> LanguageIdentifier {
        match self {
            Self::En => "en".parse().expect("valid langid"),
            Self::Zh => "zh-CN".parse().expect("valid langid"),
        }
    }

    fn ftl(&self) -> &'static str {
        match self {
            Self::En => include_str!("../locales/en/main.ftl"),
            Self::Zh => include_str!("../locales/zh-CN/main.ftl"),
        }
    }
}

static BUNDLES: OnceLock<Mutex<Option<(Language, FluentBundle<FluentResource>)>>> = OnceLock::new();

fn with_bundle<T>(lang: Language, f: impl FnOnce(&FluentBundle<FluentResource>) -> T) -> T {
    let slot = BUNDLES.get_or_init(|| Mutex::new(None));
    let mut guard = slot.lock().expect("i18n lock poisoned");
    if guard.as_ref().map(|(l, _)| *l) != Some(lang) {
        let mut bundle = FluentBundle::new(vec![lang.id()]);
        let res =
            FluentResource::try_new(lang.ftl().to_owned()).expect("embedded .ftl is valid UTF-8");
        bundle.add_resource(res).expect("no duplicate message IDs");
        *guard = Some((lang, bundle));
    }
    f(&guard.as_ref().unwrap().1)
}

/// Translates a Fluent message key, returning an owned String.
pub fn t(lang: Language, key: &str) -> String {
    t_args(lang, key, &[])
}

/// Translates a Fluent message key with arguments.
pub fn t_args(lang: Language, key: &str, args: &[(&str, &str)]) -> String {
    with_bundle(lang, |bundle| {
        let msg = match bundle.get_message(key) {
            Some(msg) => msg,
            None => return key.to_owned(),
        };
        let pattern = match msg.value() {
            Some(p) => p,
            None => return key.to_owned(),
        };
        let fluent_args: FluentArgs = args.iter().map(|(k, v)| (*k, (*v).into())).collect();
        let mut errors = vec![];
        bundle
            .format_pattern(pattern, Some(&fluent_args), &mut errors)
            .into_owned()
    })
}

pub static mut LANG: Language = Language::En;

pub fn set_lang(lang: Language) {
    unsafe { LANG = lang };
}

pub fn lang() -> Language {
    unsafe { LANG }
}

pub fn detect() {
    let env: std::collections::HashMap<&str, String> =
        ["LANGUAGE", "LC_ALL", "LC_MESSAGES", "LANG"]
            .iter()
            .filter_map(|key| {
                std::env::var(key)
                    .ok()
                    .filter(|v| !v.is_empty())
                    .map(|v| (*key, v))
            })
            .collect();

    let priority = ["LANGUAGE", "LC_ALL", "LC_MESSAGES", "LANG"];

    for key in priority {
        if let Some(value) = env.get(key) {
            let lower = value.to_ascii_lowercase();
            if lower.starts_with("zh") {
                set_lang(Language::Zh);
                return;
            }
            if !lower.is_empty() && lower != "c" && lower != "posix" {
                set_lang(Language::En);
                return;
            }
        }
    }

    set_lang(Language::En);
}
