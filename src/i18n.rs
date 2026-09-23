//! 表示言語 (English / 日本語)。設定ファイルの `lang`、なければロケール (LANG など) から決める。
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    En,
    Ja,
}

impl Lang {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "en" | "english" => Some(Self::En),
            "ja" | "jp" | "japanese" | "日本語" => Some(Self::Ja),
            _ => None,
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Self::En => "en",
            Self::Ja => "ja",
        }
    }

    /// LC_ALL > LC_MESSAGES > LANG の順に見て、ja で始まれば日本語
    pub fn detect() -> Self {
        ["LC_ALL", "LC_MESSAGES", "LANG"]
            .iter()
            .filter_map(|v| std::env::var(v).ok().filter(|s| !s.is_empty()))
            .next()
            .filter(|s| s.starts_with("ja"))
            .map_or(Self::En, |_| Self::Ja)
    }
}

static JA: AtomicBool = AtomicBool::new(false);

pub fn set(l: Lang) {
    JA.store(l == Lang::Ja, Relaxed);
}

pub fn ja() -> bool {
    JA.load(Relaxed)
}

/// `tr!("English {x}", "日本語 {x}")` — 現在の言語で format! する
#[macro_export]
macro_rules! tr {
    ($en:literal, $ja:literal $(,)?) => {
        if $crate::i18n::ja() { format!($ja) } else { format!($en) }
    };
}
