//! Command line: clap definitions and `--server` parsing.
//!
//! `--server` values are turned into the same `Config` the TOML produces by
//! building an equivalent TOML document and going through
//! `Config::from_toml`, so there is a single validation path (themes, token
//! file rules, https rejection, `~` expansion).

use clap::{Args, Parser, Subcommand};
use llm_wayvisuals::config::{Config, MAX_PANELS};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "llm-wayvisuals",
    version,
    about = "Sci-fi 1920x480 sensor panels for llm-visuals stats (Wayland and X11)",
    args_conflicts_with_subcommands = true
)]
pub struct Cli {
    #[command(flatten)]
    pub run: RunArgs,
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Show the panels (default command)
    Run(RunArgs),
    /// Render one panel from a JSON snapshot into a PNG (no display needed)
    RenderPng(RenderPngArgs),
    /// Time the renderer (default: all signals)
    BenchRender(BenchArgs),
    /// Render every fixture with the CPU and the GPU renderer and compare them
    #[cfg(feature = "gpu")]
    CompareRenderers(CompareArgs),
}


#[derive(Args, Debug, Clone, Default)]
pub struct RunArgs {
    /// A panel: SOURCE[,output=NAME|INDEX][,theme=NAME][,model=auto|PID|NAME]
    /// [,token-file=PATH][,name=LABEL][,title=TEXT]. SOURCE is a path, file://,
    /// unix:// or http:// URL. Repeatable, 1 to 3. Overrides the config file.
    #[arg(long = "server", value_name = "SPEC")]
    pub servers: Vec<String>,
    /// Config file (default: ~/.config/llm-wayvisuals/config.toml)
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,
    /// Development mode: one normal decorated window per panel
    #[arg(long)]
    pub window: bool,
    /// List the monitors (index, name, size, position, refresh, scale) and exit
    #[arg(long)]
    pub list_outputs: bool,
    /// Frame rate while a live panel is in prefill/decode
    #[arg(long, default_value_t = 30, value_name = "N", value_parser = clap::value_parser!(u32).range(1..=240))]
    pub fps: u32,
    /// Frame rate while all live panels are idle (2 fps when none is live for 10 s)
    #[arg(long, default_value_t = 15, value_name = "N", value_parser = clap::value_parser!(u32).range(1..=240))]
    pub idle_fps: u32,
    /// Renderer: auto (GPU when a hardware GL context works, else CPU), gpu
    /// (error if unavailable) or cpu. Default: LLMV_RENDERER, else auto.
    #[arg(long, value_name = "auto|gpu|cpu")]
    pub renderer: Option<String>,
}

#[derive(Args, Debug)]
pub struct BenchArgs {
    /// live|stale|down|connecting (default: a table of all)
    #[arg(long)]
    pub signal: Option<String>,
    /// Render without any snapshot (with --signal)
    #[arg(long)]
    pub no_snapshot: bool,
    #[arg(long, default_value_t = 200)]
    pub frames: usize,
    /// WIDTHxHEIGHT of the pixmap (default 1920x480)
    #[arg(long, default_value = "1920x480")]
    pub size: String,
    /// cpu (default) or gpu (headless GL context, time includes glFinish)
    #[arg(long, value_name = "cpu|gpu", default_value = "cpu")]
    pub renderer: String,
}

#[cfg(feature = "gpu")]
#[derive(Args, Debug)]
pub struct CompareArgs {
    /// Directory holding the fixtures
    #[arg(long, default_value = "fixtures")]
    pub fixtures: String,
    /// Directory for the side-by-side PNGs of failing fixtures
    #[arg(long, default_value = ".")]
    pub out: String,
    /// WIDTHxHEIGHT list, comma separated
    #[arg(long, default_value = "1920x480,1920x348")]
    pub sizes: String,
    /// Also write the PNGs of passing fixtures
    #[arg(long)]
    pub all: bool,
}

#[derive(Args, Debug)]
pub struct RenderPngArgs {
    #[arg(long)]
    pub input: String,
    #[arg(long)]
    pub out: String,
    #[arg(long, default_value = "green")]
    pub theme: String,
    /// live|stale|down|connecting
    #[arg(long, default_value = "live")]
    pub signal: String,
    #[arg(long, default_value_t = 1.5)]
    pub t: f32,
    /// WIDTHxHEIGHT
    #[arg(long, default_value = "1920x480")]
    pub size: String,
    /// cpu (default) or gpu (headless GL context)
    #[arg(long, value_name = "cpu|gpu", default_value = "cpu")]
    pub renderer: String,
}

const KEYS: [&str; 6] = ["output", "theme", "model", "token-file", "name", "title"];

fn toml_str(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

/// Builds a `Config` from repeated `--server` values.
pub fn config_from_servers(servers: &[String]) -> Result<Config, String> {
    if servers.is_empty() || servers.len() > MAX_PANELS {
        return Err(format!(
            "--server must be given 1 to {MAX_PANELS} times, found {}",
            servers.len()
        ));
    }
    let mut doc = String::new();
    for (i, spec) in servers.iter().enumerate() {
        let n = i + 1;
        let mut parts = spec.split(',');
        let source = parts.next().unwrap_or("").trim();
        if source.is_empty() {
            return Err(format!("--server {n}: missing source in {spec:?}"));
        }
        doc.push_str("[[panel]]\n");
        doc.push_str(&format!("source = {}\n", toml_str(source)));
        let mut seen: Vec<&str> = Vec::new();
        for kv in parts {
            let Some((k, v)) = kv.split_once('=') else {
                return Err(format!(
                    "--server {n}: expected key=value, got {kv:?} (keys: {})",
                    KEYS.join(", ")
                ));
            };
            let k = k.trim();
            if !KEYS.contains(&k) {
                return Err(format!(
                    "--server {n}: unknown key {k:?} (keys: {})",
                    KEYS.join(", ")
                ));
            }
            if seen.contains(&k) {
                return Err(format!("--server {n}: duplicate key {k:?}"));
            }
            seen.push(k);
            doc.push_str(&format!("{} = {}\n", k.replace('-', "_"), toml_str(v.trim())));
        }
    }
    Config::from_toml(&doc).map_err(|e| format!("--server: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_wayvisuals::model::ModelSelect;
    use llm_wayvisuals::source::SourceSpec;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn all_keys() {
        let dir = std::env::temp_dir().join(format!("llmvw-cli-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tok = dir.join("t.token");
        std::fs::write(&tok, "abc\n").unwrap();
        std::fs::set_permissions(&tok, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
        let spec = format!(
            "http://h:9464,output=DP-2,theme=gold,model=deepseek,token-file={},name=remote,title=Maya V4",
            tok.display()
        );
        let c = config_from_servers(&[spec]).unwrap();
        let p = &c.panels[0];
        assert_eq!(p.output.as_deref(), Some("DP-2"));
        assert_eq!(p.theme, "gold");
        assert_eq!(p.model, ModelSelect::Name("deepseek".into()));
        assert_eq!(p.name, "remote");
        assert_eq!(p.title.as_deref(), Some("Maya V4"));
        assert_eq!(
            p.source,
            SourceSpec::Http { url: "http://h:9464/stream".into(), token: Some("abc".into()) }
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plain_sources_and_defaults() {
        let c = config_from_servers(&s(&[
            "/a.json",
            "unix:///run/x.sock,model=42",
            "file:///b.json,output=1",
        ]))
        .unwrap();
        assert_eq!(c.panels.len(), 3);
        assert_eq!(c.panels[0].theme, "green");
        assert_eq!(c.panels[1].theme, "blue");
        assert_eq!(c.panels[1].model, ModelSelect::Pid(42));
        assert_eq!(c.panels[2].output.as_deref(), Some("1"));
        assert_eq!(c.panels[0].name, "panel-1");
    }

    #[test]
    fn errors() {
        let e = config_from_servers(&s(&["/a.json,bogus=1"])).unwrap_err();
        assert!(e.contains("bogus"), "{e}");
        let e = config_from_servers(&s(&["/a.json,theme"])).unwrap_err();
        assert!(e.contains("key=value"), "{e}");
        let e = config_from_servers(&s(&["/a.json,theme=a,theme=b"])).unwrap_err();
        assert!(e.contains("duplicate"), "{e}");
        let e = config_from_servers(&s(&["/a", "/b", "/c", "/d"])).unwrap_err();
        assert!(e.contains("1 to 3"), "{e}");
        assert!(config_from_servers(&[]).is_err());
        let e = config_from_servers(&s(&["/a.json,theme=pink"])).unwrap_err();
        assert!(e.contains("pink"), "{e}");
        let e = config_from_servers(&s(&["https://h:1"])).unwrap_err();
        assert!(e.contains("ssh -L"), "{e}");
        let e = config_from_servers(&s(&["/a.json,token-file=/x"])).unwrap_err();
        assert!(e.contains("http://"), "{e}");
        let e = config_from_servers(&s([",theme=green"].as_slice())).unwrap_err();
        assert!(e.contains("missing source"), "{e}");
    }

    #[test]
    fn quotes_in_values_are_safe() {
        let c = config_from_servers(&s(&["/a.json,title=say \"hi\" \\ x"])).unwrap();
        assert_eq!(c.panels[0].title.as_deref(), Some("say \"hi\" \\ x"));
    }
}
