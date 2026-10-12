//! Runtime localization and a per-user language preference, independent of the UI.
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum Language {
    #[default]
    English,
    Chinese,
}

impl Language {
    pub fn from_tag(tag: &str) -> Option<Self> {
        match tag.split(['-', '_', '.']).next()?.to_ascii_lowercase().as_str() {
            "en" => Some(Self::English),
            "zh" => Some(Self::Chinese),
            _ => None,
        }
    }

    pub const fn tag(self) -> &'static str {
        match self {
            Self::English => "en",
            Self::Chinese => "zh-CN",
        }
    }
}

static LANGUAGE: AtomicU8 = AtomicU8::new(Language::English as u8);
static EN: OnceLock<HashMap<String, String>> = OnceLock::new();
static ZH: OnceLock<HashMap<String, String>> = OnceLock::new();

fn catalog(language: Language) -> &'static HashMap<String, String> {
    let (cell, source) = match language {
        Language::English => (&EN, include_str!("../locales/en.json")),
        Language::Chinese => (&ZH, include_str!("../locales/zh-CN.json")),
    };
    cell.get_or_init(|| serde_json::from_str(source).expect("valid embedded language catalog"))
}

pub fn language() -> Language {
    if LANGUAGE.load(Ordering::Relaxed) == 1 {
        Language::Chinese
    } else {
        Language::English
    }
}

pub fn set_language(language: Language) {
    LANGUAGE.store(language as u8, Ordering::Relaxed);
}

pub fn t(key: &'static str) -> &'static str {
    translate(language(), key)
}

pub fn translate(language: Language, key: &'static str) -> &'static str {
    catalog(language)
        .get(key)
        .or_else(|| catalog(Language::English).get(key))
        .map(String::as_str)
        .unwrap_or(key)
}

/// Store templates until rendering so pending operations follow language changes.
#[derive(Clone, Debug)]
pub struct Message {
    key: &'static str,
    args: Vec<(&'static str, String)>,
    nested: Vec<(&'static str, Message)>,
}

impl Message {
    pub fn new(key: &'static str, args: Vec<(&'static str, String)>) -> Self {
        Self {
            key,
            args,
            nested: Vec::new(),
        }
    }

    pub fn with_message(mut self, name: &'static str, message: Message) -> Self {
        self.nested.push((name, message));
        self
    }

    pub fn render(&self, language: Language) -> String {
        let mut args = self.args.clone();
        args.extend(
            self.nested
                .iter()
                .map(|(name, message)| (*name, message.render(language))),
        );
        interpolate(translate(language, self.key), &args)
    }
}

impl From<&'static str> for Message {
    fn from(key: &'static str) -> Self {
        Self::new(key, Vec::new())
    }
}

impl fmt::Display for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render(language()))
    }
}

/// Replace placeholders once; braces inside file names must remain literal.
fn interpolate(template: &str, args: &[(&str, String)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        let Some(end) = rest.find('}') else { break };
        let key = &rest[1..end];
        if let Some((_, value)) = args.iter().find(|(name, _)| *name == key) {
            out.push_str(value);
        } else {
            out.push_str(&rest[..=end]);
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

#[macro_export]
macro_rules! msg {
    ($key:literal $(, $name:ident = $value:expr)* $(,)?) => {
        $crate::Message::new($key, vec![$((stringify!($name), ($value).to_string())),*])
    };
}

#[macro_export]
macro_rules! tr {
    ($($args:tt)*) => { $crate::msg!($($args)*).to_string() };
}

pub fn settings_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("ACRE_CONFIG_DIR").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(path).join("settings.json"));
    }
    #[cfg(windows)]
    let root = std::env::var_os("APPDATA").map(PathBuf::from);
    #[cfg(not(windows))]
    let root = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".config")));
    root.map(|p| p.join("Acre").join("settings.json"))
}

pub fn load_preference(path: &Path) -> Option<Language> {
    let bytes = std::fs::read(path).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    Language::from_tag(value.get("language")?.as_str()?)
}

pub fn save_preference(path: &Path, language: Language) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut settings = std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .filter(|v| v.is_object())
        .unwrap_or_else(|| serde_json::json!({}));
    settings["language"] = serde_json::Value::String(language.tag().to_string());
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&temporary, serde_json::to_vec_pretty(&settings)?)?;
    if let Err(error) = std::fs::rename(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
}

pub fn persist(language: Language) -> std::io::Result<()> {
    let path = settings_path()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "user settings directory is unavailable"))?;
    save_preference(&path, language)
}

pub fn initialize() -> Language {
    let saved = settings_path().and_then(|p| load_preference(&p));
    let selected = saved.or_else(system_language).unwrap_or_default();
    set_language(selected);
    selected
}

fn system_language() -> Option<Language> {
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetUserDefaultUILanguage() -> u16;
        }
        let primary = unsafe { GetUserDefaultUILanguage() } & 0x03ff;
        return match primary {
            0x04 => Some(Language::Chinese),
            _ => Some(Language::English),
        };
    }
    #[cfg(not(windows))]
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .into_iter()
        .find_map(|key| std::env::var(key).ok().and_then(|v| Language::from_tag(&v)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn placeholders(s: &str) -> BTreeSet<&str> {
        s.split('{')
            .skip(1)
            .filter_map(|s| s.split_once('}').map(|(k, _)| k))
            .collect()
    }

    #[test]
    fn languages_cover_the_same_messages_and_placeholders() {
        let en = catalog(Language::English);
        let zh = catalog(Language::Chinese);
        assert_eq!(en.keys().collect::<BTreeSet<_>>(), zh.keys().collect());
        for (key, value) in en {
            assert_eq!(placeholders(value), placeholders(&zh[key]), "{key}");
            assert!(!zh[key].is_empty(), "{key}");
        }
    }

    #[test]
    fn region_tags_and_unsupported_languages() {
        assert_eq!(Language::from_tag("zh_TW.UTF-8"), Some(Language::Chinese));
        assert_eq!(Language::from_tag("EN-us"), Some(Language::English));
        assert_eq!(Language::from_tag("de-DE"), None);
    }

    #[test]
    fn operation_messages_follow_the_selected_language_without_changing_paths() {
        let message = msg!(
            "Permanently deleted: {name} ({size})",
            name = "报告{size}.txt",
            size = "4 KB"
        );
        assert_eq!(
            message.render(Language::English),
            "Permanently deleted: 报告{size}.txt (4 KB)"
        );
        assert_eq!(message.render(Language::Chinese), "已直接删除：报告{size}.txt（4 KB）");
    }

    #[test]
    fn queued_errors_translate_nested_reasons_at_render_time() {
        let message = msg!("Could not delete {path}: {reason}", path = "report.txt")
            .with_message("reason", "Access denied".into());
        assert_eq!(
            message.render(Language::English),
            "Could not delete report.txt: Access denied"
        );
        assert_eq!(message.render(Language::Chinese), "无法删除 report.txt：没有权限");
    }

    #[test]
    fn missing_translation_falls_back_to_english() {
        assert_eq!(translate(Language::Chinese, "Unknown message"), "Unknown message");
    }

    #[test]
    fn preference_survives_restart_and_recovers_from_invalid_settings() {
        let root = std::env::temp_dir().join(format!("acre-i18n-test-{}", std::process::id()));
        let path = root.join("settings.json");
        assert_eq!(load_preference(&path), None);
        save_preference(&path, Language::Chinese).unwrap();
        assert_eq!(load_preference(&path), Some(Language::Chinese));
        save_preference(&path, Language::English).unwrap();
        assert_eq!(load_preference(&path), Some(Language::English));
        std::fs::write(&path, "{invalid").unwrap();
        assert_eq!(load_preference(&path), None);
        save_preference(&path, Language::Chinese).unwrap();
        assert_eq!(load_preference(&path), Some(Language::Chinese));
        std::fs::remove_dir_all(root).unwrap();
    }
}
