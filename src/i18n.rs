use fluent::{FluentArgs, FluentBundle, FluentResource};
use std::cell::RefCell;
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

/// Metadata for a language supported by the UI, used to build selectors dynamically.
#[derive(Clone, Copy)]
pub struct LanguageInfo {
    pub code: &'static str,
    pub language: Language,
    pub name: &'static str,
}

/// All UI languages. Add a new entry here plus a matching `locales/<code>/main.ftl`
/// to introduce a language; no settings-page code needs to change.
pub const LANGUAGES: &[LanguageInfo] = &[
    LanguageInfo {
        code: "en",
        language: Language::En,
        name: "English",
    },
    LanguageInfo {
        code: "zh-CN",
        language: Language::Zh,
        name: "中文",
    },
];

pub fn language_from_code(code: &str) -> Option<Language> {
    LANGUAGES
        .iter()
        .find(|l| l.code == code)
        .map(|l| l.language)
}

/// Resolves a stored language code to a `Language`. An empty code means "follow
/// the system", which falls back to the global language set by `detect()`.
pub fn resolve_language(code: &str) -> Language {
    if code.is_empty() {
        lang()
    } else {
        language_from_code(code).unwrap_or_else(lang)
    }
}

thread_local! {
    static BUNDLE: RefCell<Option<(Language, FluentBundle<FluentResource>)>> = const { RefCell::new(None) };
}

fn with_bundle<T>(lang: Language, f: impl FnOnce(&FluentBundle<FluentResource>) -> T) -> T {
    BUNDLE.with(|cell| {
        let mut guard = cell.borrow_mut();
        if guard.as_ref().map(|(l, _)| *l) != Some(lang) {
            let mut bundle = FluentBundle::new(vec![lang.id()]);
            let res = FluentResource::try_new(lang.ftl().to_owned())
                .expect("embedded .ftl is valid UTF-8");
            bundle.add_resource(res).expect("no duplicate message IDs");
            *guard = Some((lang, bundle));
        }
        f(&guard.as_ref().unwrap().1)
    })
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
        let mut fluent_args: FluentArgs = FluentArgs::with_capacity(args.len());
        for (k, v) in args {
            fluent_args.set::<std::string::String, fluent::FluentValue>(
                (*k).to_string(),
                (*v).to_string().into(),
            );
        }
        let mut errors = vec![];
        bundle
            .format_pattern(pattern, Some(&fluent_args), &mut errors)
            .into_owned()
    })
}

use std::sync::Mutex;

static LANG: Mutex<Option<Language>> = Mutex::new(None);

pub fn set_lang(lang: Language) {
    if let Ok(mut guard) = LANG.lock() {
        *guard = Some(lang);
    }
}

pub fn lang() -> Language {
    LANG.lock().ok().and_then(|g| *g).unwrap_or(Language::En)
}

pub fn detect() {
    let priority = ["LANGUAGE", "LC_ALL", "LC_MESSAGES", "LANG"];

    for key in priority {
        if let Ok(value) = std::env::var(key) {
            if value.is_empty() {
                continue;
            }
            let lower = value.to_ascii_lowercase();
            if lower.starts_with("zh") {
                set_lang(Language::Zh);
                crate::log_info!("detected language: zh-CN");
                return;
            }
            if lower != "c" && lower != "posix" {
                set_lang(Language::En);
                crate::log_info!("detected language: en");
                return;
            }
        }
    }

    set_lang(Language::En);
    crate::log_info!("detected language: en (default)");
}
