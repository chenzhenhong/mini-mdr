use std::{
    collections::HashMap,
    sync::atomic::{AtomicU8, Ordering},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Language {
    En,
    Zh,
}

static LANG: AtomicU8 = AtomicU8::new(Language::En as u8);

pub fn set_lang(lang: Language) {
    LANG.store(lang as u8, Ordering::Relaxed);
}

pub fn lang() -> Language {
    match LANG.load(Ordering::Relaxed) {
        1 => Language::Zh,
        _ => Language::En,
    }
}

pub fn detect() {
    let env: HashMap<&str, String> = ["LANGUAGE", "LC_ALL", "LC_MESSAGES", "LANG", "LANGS"]
        .iter()
        .filter_map(|key| {
            std::env::var(key)
                .ok()
                .filter(|v| !v.is_empty())
                .map(|v| (*key, v))
        })
        .collect();

    let priority = ["LANGUAGE", "LC_ALL", "LC_MESSAGES", "LANG", "LANGS"];

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

macro_rules! s {
    ($lang:expr, $zh:expr, $en:expr) => {
        match $lang {
            $crate::i18n::Language::En => $en,
            $crate::i18n::Language::Zh => $zh,
        }
    };
}

pub(crate) use s;
