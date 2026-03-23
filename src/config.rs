// src/config.rs — TUI persistent configuration and session cache
//
// Two files:
//   ~/.qmt/tui.toml              — user config (theme, server defaults)
//   ~/.cache/qmt/tui-cache.toml  — per-session mode→model+effort cache

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use crate::app::CachedModeState;

// ── path overrides for tests ─────────────────────────────────────────────────

static CONFIG_PATH_OVERRIDE: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();
static CACHE_PATH_OVERRIDE: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();

fn config_path_override() -> &'static Mutex<Option<PathBuf>> {
    CONFIG_PATH_OVERRIDE.get_or_init(|| Mutex::new(None))
}

fn cache_path_override() -> &'static Mutex<Option<PathBuf>> {
    CACHE_PATH_OVERRIDE.get_or_init(|| Mutex::new(None))
}

/// Override the config path used by `TuiConfig::load()` / `save()`.
/// Intended for tests only; production code should not call this.
pub fn test_set_config_path_override(path: Option<PathBuf>) {
    *config_path_override().lock().unwrap() = path;
}

/// Override the cache path used by `TuiCache::load()` / `save()`.
/// Intended for tests only; production code should not call this.
pub fn test_set_cache_path_override(path: Option<PathBuf>) {
    *cache_path_override().lock().unwrap() = path;
}

// ── TuiConfig — ~/.qmt/tui.toml ──────────────────────────────────────────────

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub addr: Option<String>,
    pub tls: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TuiConfig {
    pub theme: Option<String>,
    pub server: ServerConfig,
}

impl TuiConfig {
    pub fn config_path() -> PathBuf {
        if let Some(path) = config_path_override().lock().unwrap().clone() {
            return path;
        }
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".qmt")
            .join("tui.toml")
    }

    /// Load from the default path (`~/.qmt/tui.toml`).
    pub fn load() -> Self {
        Self::load_from_path(&Self::config_path())
    }

    /// Load from an explicit path. Returns `Default` on any I/O or parse error.
    pub fn load_from_path(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default()
    }

    /// Save to the default path (`~/.qmt/tui.toml`).
    pub fn save(&self) {
        self.save_to_path(&Self::config_path());
    }

    /// Save to an explicit path. Creates parent directories if needed.
    /// Errors are intentionally ignored (best-effort persistence).
    pub fn save_to_path(&self, path: &Path) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(text) = toml::to_string_pretty(self) {
            let _ = std::fs::write(path, text);
        }
    }

    pub fn from_app(_app: &crate::app::App) -> Self {
        TuiConfig {
            theme: Some(crate::theme::Theme::current_id().to_string()),
            server: ServerConfig::default(),
        }
    }
}

// ── TuiCache — ~/.cache/qmt/tui-cache.toml ───────────────────────────────────

/// Serializable per-mode state: which model was used and at what effort.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModeState {
    /// `"provider/model"` e.g. `"anthropic/claude-sonnet-4-20250514"`
    pub model: String,
    /// `"auto" | "low" | "medium" | "high" | "max"`
    pub effort: String,
}

/// Per-session cache entry.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionCache {
    /// Per-mode state. Key: `"build"` / `"plan"` etc.
    pub modes: HashMap<String, ModeState>,
}

/// Top-level cache file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TuiCache {
    pub sessions: HashMap<String, SessionCache>,
}

impl TuiCache {
    pub fn cache_path() -> PathBuf {
        if let Some(path) = cache_path_override().lock().unwrap().clone() {
            return path;
        }
        dirs::cache_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("qmt")
            .join("tui-cache.toml")
    }

    /// Load from the default path (`~/.cache/qmt/tui-cache.toml`).
    pub fn load() -> Self {
        Self::load_from_path(&Self::cache_path())
    }

    /// Load from an explicit path. Returns `Default` on any I/O or parse error.
    pub fn load_from_path(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default()
    }

    /// Save to the default path (`~/.cache/qmt/tui-cache.toml`).
    pub fn save(&self) {
        self.save_to_path(&Self::cache_path());
    }

    /// Save to an explicit path. Creates parent directories if needed.
    /// Errors are intentionally ignored (best-effort persistence).
    pub fn save_to_path(&self, path: &Path) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(text) = toml::to_string_pretty(self) {
            let _ = std::fs::write(path, text);
        }
    }

    /// Build from live app state.
    pub fn from_app(app: &crate::app::App) -> Self {
        let sessions = app
            .session_cache
            .iter()
            .map(|(sid, modes)| {
                let modes = modes
                    .iter()
                    .map(|(mode, cms)| {
                        let ms = ModeState {
                            model: cms.model.clone(),
                            effort: cms.effort.as_deref().unwrap_or("auto").to_string(),
                        };
                        (mode.clone(), ms)
                    })
                    .collect();
                (sid.clone(), SessionCache { modes })
            })
            .collect();
        TuiCache { sessions }
    }

    /// Hydrate `app.session_cache` from the loaded cache file.
    pub fn hydrate_app(&self, app: &mut crate::app::App) {
        for (sid, sc) in &self.sessions {
            let modes = sc
                .modes
                .iter()
                .map(|(mode, ms)| {
                    let cms = CachedModeState {
                        model: ms.model.clone(),
                        effort: match ms.effort.as_str() {
                            "auto" => None,
                            s => Some(s.to_string()),
                        },
                    };
                    (mode.clone(), cms)
                })
                .collect();
            app.session_cache.insert(sid.clone(), modes);
        }
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, CachedModeState};

    fn unique_temp_dir(label: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("qmt-tui-tests-{label}-{pid}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    struct TestPathGuard;

    impl TestPathGuard {
        fn new(label: &str) -> Self {
            let dir = unique_temp_dir(label);
            test_set_config_path_override(Some(dir.join("tui.toml")));
            test_set_cache_path_override(Some(dir.join("tui-cache.toml")));
            Self
        }
    }

    impl Drop for TestPathGuard {
        fn drop(&mut self) {
            test_set_config_path_override(None);
            test_set_cache_path_override(None);
        }
    }

    // ── TuiConfig ─────────────────────────────────────────────────────────────

    #[test]
    fn config_round_trip() {
        let cfg = TuiConfig {
            theme: Some("base16-ocean".into()),
            server: ServerConfig {
                addr: Some("127.0.0.1:3030".into()),
                tls: Some(false),
            },
        };
        let text = toml::to_string_pretty(&cfg).unwrap();
        assert_eq!(toml::from_str::<TuiConfig>(&text).unwrap(), cfg);
    }

    #[test]
    fn config_empty_deserializes_to_default() {
        assert_eq!(
            toml::from_str::<TuiConfig>("").unwrap(),
            TuiConfig::default()
        );
    }

    // ── TuiCache TOML ─────────────────────────────────────────────────────────

    #[test]
    fn cache_round_trip() {
        let mut sc = SessionCache::default();
        sc.modes.insert(
            "build".into(),
            ModeState {
                model: "anthropic/claude-sonnet".into(),
                effort: "high".into(),
            },
        );
        sc.modes.insert(
            "plan".into(),
            ModeState {
                model: "openai/gpt-4o".into(),
                effort: "auto".into(),
            },
        );
        let cache = TuiCache {
            sessions: [("sid-1".into(), sc)].into_iter().collect(),
        };
        let text = toml::to_string_pretty(&cache).unwrap();
        assert_eq!(toml::from_str::<TuiCache>(&text).unwrap(), cache);
    }

    #[test]
    fn cache_empty_deserializes_to_default() {
        assert_eq!(toml::from_str::<TuiCache>("").unwrap(), TuiCache::default());
    }

    #[test]
    fn cache_bad_toml_returns_default() {
        assert!(toml::from_str::<TuiCache>("not toml!!!").is_err());
    }

    #[test]
    fn cache_load_from_path_missing_returns_default() {
        let dir = unique_temp_dir("cache-missing");
        let path = dir.join("missing.toml");
        let loaded = TuiCache::load_from_path(&path);
        assert_eq!(loaded, TuiCache::default());
    }

    #[test]
    fn cache_load_from_path_malformed_returns_default() {
        let dir = unique_temp_dir("cache-bad");
        let path = dir.join("bad.toml");
        std::fs::write(&path, "not toml ???").unwrap();
        let loaded = TuiCache::load_from_path(&path);
        assert_eq!(loaded, TuiCache::default());
    }

    #[test]
    fn cache_save_to_path_and_load_round_trip() {
        let mut sc = SessionCache::default();
        sc.modes.insert(
            "build".into(),
            ModeState {
                model: "anthropic/claude-sonnet".into(),
                effort: "high".into(),
            },
        );
        let cache = TuiCache {
            sessions: [("sid-1".into(), sc)].into_iter().collect(),
        };

        let dir = unique_temp_dir("cache-save");
        let path = dir.join("nested").join("tui-cache.toml");
        cache.save_to_path(&path);

        let loaded = TuiCache::load_from_path(&path);
        assert_eq!(loaded, cache);
    }

    #[test]
    fn cache_default_load_save_respects_override_path() {
        let _guard = TestPathGuard::new("cache-override");
        let mut sc = SessionCache::default();
        sc.modes.insert(
            "build".into(),
            ModeState {
                model: "anthropic/claude-sonnet".into(),
                effort: "high".into(),
            },
        );
        let cache = TuiCache {
            sessions: [("sid-1".into(), sc)].into_iter().collect(),
        };
        cache.save();
        let loaded = TuiCache::load();
        assert_eq!(loaded, cache);
    }

    #[test]
    fn config_load_from_path_missing_returns_default() {
        let dir = unique_temp_dir("cfg-missing");
        let path = dir.join("missing.toml");
        let loaded = TuiConfig::load_from_path(&path);
        assert_eq!(loaded, TuiConfig::default());
    }

    #[test]
    fn config_load_from_path_malformed_returns_default() {
        let dir = unique_temp_dir("cfg-bad");
        let path = dir.join("bad.toml");
        std::fs::write(&path, "not toml ???").unwrap();
        let loaded = TuiConfig::load_from_path(&path);
        assert_eq!(loaded, TuiConfig::default());
    }

    #[test]
    fn config_save_to_path_and_load_round_trip() {
        let dir = unique_temp_dir("cfg-save");
        let path = dir.join("nested").join("tui.toml");
        let cfg = TuiConfig {
            theme: Some("base16-ocean".into()),
            server: ServerConfig {
                addr: Some("127.0.0.1:3030".into()),
                tls: Some(false),
            },
        };
        cfg.save_to_path(&path);
        let loaded = TuiConfig::load_from_path(&path);
        assert_eq!(loaded, cfg);
    }

    #[test]
    fn config_default_load_save_respects_override_path() {
        let _guard = TestPathGuard::new("cfg-override");
        let cfg = TuiConfig {
            theme: Some("base16-ocean".into()),
            server: ServerConfig::default(),
        };
        cfg.save();
        let loaded = TuiConfig::load();
        assert_eq!(loaded, cfg);
    }

    // ── from_app ──────────────────────────────────────────────────────────────

    #[test]
    fn from_app_captures_session_cache() {
        let mut app = App::new();
        let mut modes = HashMap::new();
        modes.insert(
            "build".into(),
            CachedModeState {
                model: "anthropic/claude-sonnet".into(),
                effort: Some("high".into()),
            },
        );
        modes.insert(
            "plan".into(),
            CachedModeState {
                model: "openai/gpt-4o".into(),
                effort: None, // auto
            },
        );
        app.session_cache.insert("sid-1".into(), modes);

        let cache = TuiCache::from_app(&app);
        let sc = cache.sessions.get("sid-1").unwrap();
        assert_eq!(sc.modes["build"].model, "anthropic/claude-sonnet");
        assert_eq!(sc.modes["build"].effort, "high");
        assert_eq!(sc.modes["plan"].model, "openai/gpt-4o");
        assert_eq!(sc.modes["plan"].effort, "auto");
    }

    // ── hydrate_app ───────────────────────────────────────────────────────────

    #[test]
    fn hydrate_app_restores_session_cache() {
        let mut sc = SessionCache::default();
        sc.modes.insert(
            "build".into(),
            ModeState {
                model: "anthropic/claude-sonnet".into(),
                effort: "high".into(),
            },
        );
        sc.modes.insert(
            "plan".into(),
            ModeState {
                model: "openai/gpt-4o".into(),
                effort: "auto".into(),
            },
        );
        let cache = TuiCache {
            sessions: [("sid-1".into(), sc)].into_iter().collect(),
        };

        let mut app = App::new();
        cache.hydrate_app(&mut app);

        let modes = app.session_cache.get("sid-1").unwrap();
        assert_eq!(modes["build"].model, "anthropic/claude-sonnet");
        assert_eq!(modes["build"].effort, Some("high".into()));
        assert_eq!(modes["plan"].model, "openai/gpt-4o");
        assert_eq!(modes["plan"].effort, None);
    }

    #[test]
    fn hydrate_empty_cache_leaves_app_unchanged() {
        let mut app = App::new();
        TuiCache::default().hydrate_app(&mut app);
        assert!(app.session_cache.is_empty());
    }

    // ── round-trip ────────────────────────────────────────────────────────────

    #[test]
    fn from_app_hydrate_round_trip() {
        let mut app = App::new();
        let mut modes = HashMap::new();
        modes.insert(
            "build".into(),
            CachedModeState {
                model: "anthropic/claude-sonnet".into(),
                effort: Some("max".into()),
            },
        );
        app.session_cache.insert("sid-1".into(), modes);

        let cache = TuiCache::from_app(&app);
        let mut app2 = App::new();
        cache.hydrate_app(&mut app2);

        assert_eq!(
            app2.session_cache["sid-1"]["build"],
            CachedModeState {
                model: "anthropic/claude-sonnet".into(),
                effort: Some("max".into()),
            }
        );
    }
}
