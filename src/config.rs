//! TOML configuration: 1..=3 panels, each bound to one source.

use crate::model::ModelSelect;
use crate::source::{expand_home, SourceSpec};
use serde::Deserialize;
use std::path::{Path, PathBuf};

pub const THEMES: [&str; 4] = ["green", "gold", "purple", "blue"];
const DEFAULT_THEMES: [&str; 3] = ["green", "blue", "purple"];
pub const MAX_PANELS: usize = 3;

#[derive(Debug, Clone)]
pub struct Config {
    pub panels: Vec<PanelConfig>,
    /// Non-fatal problems found while loading (e.g. a token file readable by
    /// others). `load` also prints them on stderr.
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct PanelConfig {
    pub name: String,
    pub output: Option<String>,
    pub source: SourceSpec,
    pub model: ModelSelect,
    pub theme: String,
    pub title: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    panel: Vec<RawPanel>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawModel {
    Pid(u32),
    Text(String),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPanel {
    name: Option<String>,
    output: Option<String>,
    source: String,
    token_file: Option<String>,
    model: Option<RawModel>,
    theme: Option<String>,
    title: Option<String>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Config, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let cfg = Self::from_toml(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        for w in &cfg.warnings {
            eprintln!("llm-wayvisuals: warning: {w}");
        }
        Ok(cfg)
    }

    pub fn from_toml(text: &str) -> Result<Config, String> {
        let raw: RawConfig = toml::from_str(text).map_err(|e| e.to_string())?;
        if raw.panel.is_empty() || raw.panel.len() > MAX_PANELS {
            return Err(format!(
                "config needs 1 to {MAX_PANELS} [[panel]] entries, found {}",
                raw.panel.len()
            ));
        }
        let mut warnings = Vec::new();
        let mut panels = Vec::new();
        for (i, p) in raw.panel.into_iter().enumerate() {
            let n = i + 1;
            let err = |m: String| format!("panel {n}: {m}");
            if p.token_file.is_some() && !p.source.trim_start().starts_with("http://") {
                return Err(err("token_file only applies to http:// sources".into()));
            }
            let token = match &p.token_file {
                Some(f) => Some(read_token(f, &mut warnings).map_err(&err)?),
                None => None,
            };
            let source = SourceSpec::parse(&p.source, token).map_err(&err)?;
            let theme = p
                .theme
                .unwrap_or_else(|| DEFAULT_THEMES[i % DEFAULT_THEMES.len()].to_string());
            if !THEMES.contains(&theme.as_str()) {
                return Err(err(format!(
                    "unknown theme {theme:?} (expected one of {})",
                    THEMES.join(", ")
                )));
            }
            let model = match p.model {
                None => ModelSelect::Auto,
                Some(RawModel::Pid(pid)) => ModelSelect::Pid(pid),
                Some(RawModel::Text(t)) => {
                    let t = t.trim();
                    if t.is_empty() {
                        return Err(err("model must not be empty".into()));
                    } else if t.eq_ignore_ascii_case("auto") {
                        ModelSelect::Auto
                    } else if let Ok(pid) = t.parse::<u32>() {
                        ModelSelect::Pid(pid)
                    } else {
                        ModelSelect::Name(t.to_string())
                    }
                }
            };
            panels.push(PanelConfig {
                name: p.name.unwrap_or_else(|| format!("panel-{n}")),
                output: p.output,
                source,
                model,
                theme,
                title: p.title,
            });
        }
        Ok(Config { panels, warnings })
    }

    /// `$XDG_CONFIG_HOME` or `~/.config`, plus `llm-wayvisuals/config.toml`.
    pub fn default_path() -> PathBuf {
        let base = match std::env::var_os("XDG_CONFIG_HOME") {
            Some(v) if !v.is_empty() => PathBuf::from(v),
            _ => std::env::var_os("HOME")
                .map(|h| PathBuf::from(h).join(".config"))
                .unwrap_or_else(|| PathBuf::from(".config")),
        };
        base.join("llm-wayvisuals").join("config.toml")
    }
}

fn read_token(file: &str, warnings: &mut Vec<String>) -> Result<String, String> {
    use std::os::unix::fs::PermissionsExt;
    let path = expand_home(file);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("token_file {}: {e}", path.display()))?;
    if let Ok(md) = std::fs::metadata(&path) {
        let mode = md.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            warnings.push(format!(
                "token_file {} has mode {mode:03o}; it should be 0600",
                path.display()
            ));
        }
    }
    let token = text.trim().to_string();
    if token.is_empty() {
        return Err(format!("token_file {} is empty", path.display()));
    }
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_config_and_defaults() {
        let c = Config::from_toml(
            r#"
            [[panel]]
            output = "DP-2"
            source = "/run/user/1000/llmv-0.json"
            [[panel]]
            source = "unix:///run/user/1000/llmv-1.sock"
            model = 4242
            title = "Maya"
            [[panel]]
            source = "http://gpubox1.example.lan:9464"
            model = "deepseek"
            theme = "gold"
            name = "remote"
            "#,
        )
        .unwrap();
        assert_eq!(c.panels.len(), 3);
        assert_eq!(c.panels[0].name, "panel-1");
        assert_eq!(c.panels[0].output.as_deref(), Some("DP-2"));
        assert_eq!(c.panels[0].theme, "green");
        assert_eq!(c.panels[0].model, ModelSelect::Auto);
        assert_eq!(c.panels[1].theme, "blue");
        assert_eq!(c.panels[1].model, ModelSelect::Pid(4242));
        assert_eq!(c.panels[1].title.as_deref(), Some("Maya"));
        assert_eq!(c.panels[2].theme, "gold");
        assert_eq!(c.panels[2].name, "remote");
        assert_eq!(c.panels[2].model, ModelSelect::Name("deepseek".into()));
        assert!(matches!(c.panels[2].source, SourceSpec::Http { .. }));
    }

    #[test]
    fn model_string_forms() {
        let c = Config::from_toml("[[panel]]\nsource='/a.json'\nmodel='auto'\n[[panel]]\nsource='/b.json'\nmodel='77'").unwrap();
        assert_eq!(c.panels[0].model, ModelSelect::Auto);
        assert_eq!(c.panels[1].model, ModelSelect::Pid(77));
    }

    #[test]
    fn panel_count_errors() {
        assert!(Config::from_toml("").is_err());
        let one = "[[panel]]\nsource='/a.json'\n";
        assert!(Config::from_toml(&one.repeat(3)).is_ok());
        let e = Config::from_toml(&one.repeat(4)).unwrap_err();
        assert!(e.contains("1 to 3"), "{e}");
    }

    #[test]
    fn unknown_key_and_bad_theme_and_https() {
        let e = Config::from_toml("[[panel]]\nsource='/a.json'\nbogus=1").unwrap_err();
        assert!(e.contains("bogus"), "{e}");
        let e = Config::from_toml("[top]\nx=1\n[[panel]]\nsource='/a.json'").unwrap_err();
        assert!(e.contains("top"), "{e}");
        let e = Config::from_toml("[[panel]]\nsource='/a.json'\ntheme='pink'").unwrap_err();
        assert!(e.contains("pink") && e.contains("panel 1"), "{e}");
        let e = Config::from_toml("[[panel]]\nsource='https://h:9464'").unwrap_err();
        assert!(e.contains("ssh -L"), "{e}");
        let e = Config::from_toml("[[panel]]\nsource='/a.json'\ntoken_file='/x'").unwrap_err();
        assert!(e.contains("http://"), "{e}");
        assert!(Config::from_toml("[[panel]]\nsource = ").is_err());
    }

    #[test]
    fn token_file_read_and_warning() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("llmvw-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let tok = dir.join("t.token");
        std::fs::write(&tok, "  s3cret\n").unwrap();
        std::fs::set_permissions(&tok, std::fs::Permissions::from_mode(0o600)).unwrap();
        let toml = format!(
            "[[panel]]\nsource='http://127.0.0.1:9464'\ntoken_file='{}'\n",
            tok.display()
        );
        let c = Config::from_toml(&toml).unwrap();
        assert!(c.warnings.is_empty());
        assert_eq!(
            c.panels[0].source,
            SourceSpec::Http {
                url: "http://127.0.0.1:9464/stream".into(),
                token: Some("s3cret".into())
            }
        );
        std::fs::set_permissions(&tok, std::fs::Permissions::from_mode(0o644)).unwrap();
        let c = Config::from_toml(&toml).unwrap();
        assert_eq!(c.warnings.len(), 1);
        assert!(c.warnings[0].contains("0600"));
        // missing token file -> error; load() from a file works
        let missing = toml.replace("t.token", "nope.token");
        assert!(Config::from_toml(&missing).unwrap_err().contains("nope.token"));
        let cfg = dir.join("config.toml");
        std::fs::write(&cfg, toml).unwrap();
        assert_eq!(Config::load(&cfg).unwrap().panels.len(), 1);
        assert!(Config::load(&dir.join("absent.toml")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_path_shape() {
        let p = Config::default_path();
        assert!(p.ends_with("llm-wayvisuals/config.toml"));
    }
}
