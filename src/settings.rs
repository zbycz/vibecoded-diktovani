//! Tiny persisted user settings stored as `key=value` lines in
//! `~/.cache/diktovani/settings.conf`. Best-effort: any IO error falls back to
//! defaults and is otherwise ignored.

use std::fs;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Settings {
    /// Id of the chosen idle-icon color (see `ui::ICON_COLORS`); empty means the
    /// default monochrome menu-bar template.
    pub icon_color: String,
    /// Whisper transcription language code (e.g. "cs", "en").
    pub language: String,
    /// When true, the popup bubble under the menu-bar icon is never shown.
    pub hide_bubble: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            icon_color: String::new(),
            language: crate::core::LANGUAGE.to_string(),
            hide_bubble: false,
        }
    }
}

fn settings_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join(".cache")
            .join("diktovani")
            .join("settings.conf"),
    )
}

impl Settings {
    pub fn load() -> Self {
        let mut settings = Settings::default();
        let Some(path) = settings_path() else {
            return settings;
        };
        let Ok(content) = fs::read_to_string(&path) else {
            return settings;
        };
        for line in content.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key.trim() {
                "icon_color" => settings.icon_color = value.trim().to_string(),
                "language" => settings.language = value.trim().to_string(),
                "hide_bubble" => settings.hide_bubble = value.trim() == "true",
                _ => {}
            }
        }
        settings
    }

    pub fn save(&self) {
        let Some(path) = settings_path() else {
            return;
        };
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let content = format!(
            "icon_color={}\nlanguage={}\nhide_bubble={}\n",
            self.icon_color, self.language, self.hide_bubble
        );
        if let Err(err) = fs::write(&path, content) {
            eprintln!("[settings] failed to save {}: {err}", path.display());
        }
    }
}
