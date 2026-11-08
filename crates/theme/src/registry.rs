//! The process theme state from packages/coding-agent theme.ts: registered (in-memory) themes, the theme list,
//! `initTheme` / `setTheme` with the silent fallback to `dark`. The file watcher, `setThemeInstance` and the change
//! callback are not part of this port.

use std::path::PathBuf;

use super::{
    ColorMode, Theme, assert_theme_name_is_valid, create_theme, fallback_theme, load_theme_from_path, load_theme_json,
};

/// `ThemeInfo`: a theme name and the file it came from (the embedded built-ins and registered themes may have none).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThemeInfo {
    pub name: String,
    pub path: Option<String>,
}

/// TS keeps these in module globals; here the caller owns one value.
pub struct ThemeRegistry {
    /// `<agent dir>/themes` (TS `getCustomThemesDir()`).
    pub custom_dir: PathBuf,
    pub mode: ColorMode,
    registered: Vec<Theme>,
    current: Option<Theme>,
}

impl ThemeRegistry {
    pub fn new(custom_dir: PathBuf, mode: ColorMode) -> Self {
        ThemeRegistry {
            custom_dir,
            mode,
            registered: Vec::new(),
            current: None,
        }
    }

    /// `setRegisteredThemes`: replace the in-memory themes; a name containing `/` fails (themes before it stay).
    pub fn set_registered_themes(&mut self, themes: Vec<Theme>) -> Result<(), String> {
        self.registered.clear();
        for theme in themes {
            let Some(name) = theme.name.clone() else { continue };
            assert_theme_name_is_valid(&name)?;
            self.registered.retain(|t| t.name.as_deref() != Some(&name));
            self.registered.push(theme);
        }
        Ok(())
    }

    fn registered(&self, name: &str) -> Option<&Theme> {
        self.registered.iter().find(|t| t.name.as_deref() == Some(name))
    }

    /// `getAvailableThemesWithPaths`: built-ins, valid custom theme files, then registered themes, first name wins,
    /// sorted by name.
    pub fn available_themes_with_paths(&self) -> Vec<ThemeInfo> {
        let mut result: Vec<ThemeInfo> = Vec::new();
        let mut add = |info: ThemeInfo| {
            if !result.iter().any(|r| r.name == info.name) {
                result.push(info);
            }
        };
        for name in ["dark", "light"] {
            add(ThemeInfo {
                name: name.into(),
                path: None,
            });
        }
        let mut files: Vec<String> = std::fs::read_dir(&self.custom_dir)
            .map(|dir| dir.flatten().filter_map(|e| e.file_name().into_string().ok()).collect())
            .unwrap_or_default();
        files.sort();
        for file in files.iter().filter(|f| f.ends_with(".json")) {
            let path = self.custom_dir.join(file);
            if let Ok(Theme { name: Some(name), .. }) = load_theme_from_path(&path, self.mode) {
                add(ThemeInfo {
                    name,
                    path: Some(path.display().to_string()),
                });
            }
        }
        for theme in &self.registered {
            if let Some(name) = &theme.name {
                add(ThemeInfo {
                    name: name.clone(),
                    path: theme.source_path.clone(),
                });
            }
        }
        result.sort_by(|a, b| jsrt::collate::locale_compare(&a.name, &b.name));
        result
    }

    /// `getAvailableThemes`.
    pub fn available_themes(&self) -> Vec<String> {
        self.available_themes_with_paths().into_iter().map(|t| t.name).collect()
    }

    /// `loadTheme`: a registered theme, else a built-in or `<custom_dir>/<name>.json`.
    pub fn load_theme(&self, name: &str) -> Result<Theme, String> {
        if let Some(theme) = self.registered(name) {
            return Ok(theme.clone());
        }
        create_theme(&load_theme_json(name, &self.custom_dir)?, self.mode, None)
    }

    fn fall_back_to_dark(&mut self) {
        self.current = Some(fallback_theme(self.mode));
    }

    /// `initTheme(name)`: `default_name` stands in for TS `getDefaultTheme()` (terminal background detection); an
    /// invalid theme silently falls back to `dark`.
    pub fn init_theme(&mut self, name: Option<&str>, default_name: &str) {
        let name = name.unwrap_or(default_name);
        match self.load_theme(name) {
            Ok(theme) => self.current = Some(theme),
            Err(_) => self.fall_back_to_dark(),
        }
    }

    /// `setTheme(name)`: switch; on error fall back to `dark` and return the message.
    pub fn set_theme(&mut self, name: &str) -> Result<(), String> {
        match self.load_theme(name) {
            Ok(theme) => {
                self.current = Some(theme);
                Ok(())
            }
            Err(e) => {
                self.fall_back_to_dark();
                Err(e)
            }
        }
    }

    /// The global `theme` (`None` before `initTheme`, where TS throws "Theme not initialized").
    pub fn current(&self) -> Option<&Theme> {
        self.current.as_ref()
    }
}
