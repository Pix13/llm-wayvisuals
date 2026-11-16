//! Renderer selection (`--renderer auto|gpu|cpu`, `LLMV_RENDERER`): pure
//! logic, no GL here. The probing of the GL context is passed in as a closure.

use std::str::FromStr;

/// What the user asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RendererPref {
    /// GPU when compiled in and a hardware context works, else CPU.
    #[default]
    Auto,
    /// GPU or an error.
    Gpu,
    /// The tiny-skia path.
    Cpu,
}

impl FromStr for RendererPref {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "gpu" => Ok(Self::Gpu),
            "cpu" => Ok(Self::Cpu),
            other => Err(format!("unknown renderer {other:?} (auto|gpu|cpu)")),
        }
    }
}

/// Command line wins over the environment, which wins over `auto`. An empty
/// environment value counts as unset.
pub fn resolve_pref(cli: Option<&str>, env: Option<&str>) -> Result<RendererPref, String> {
    match (cli, env.filter(|e| !e.trim().is_empty())) {
        (Some(c), _) => c.parse(),
        (None, Some(e)) => e.parse().map_err(|m| format!("LLMV_RENDERER: {m}")),
        (None, None) => Ok(RendererPref::Auto),
    }
}

/// The backend that will be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    /// Description of the GL device, e.g. `AMD Radeon Graphics (radeonsi, renoir), GL 4.6`.
    Gpu(String),
    /// Why the CPU path is used.
    Cpu(String),
}

impl Choice {
    /// The one stderr line announcing the choice.
    pub fn log_line(&self) -> String {
        match self {
            Choice::Gpu(d) => format!("renderer: gpu ({d})"),
            Choice::Cpu(r) => format!("renderer: cpu ({r})"),
        }
    }
    pub fn is_gpu(&self) -> bool {
        matches!(self, Choice::Gpu(_))
    }
}

/// Applies the selection rules. `probe` creates (and tests) a GL context and
/// returns its description or the reason it is unusable; it is only called
/// when the GPU path is wanted and compiled in.
pub fn decide(pref: RendererPref, gpu_compiled: bool, probe: impl FnOnce() -> Result<String, String>) -> Result<Choice, String> {
    match pref {
        RendererPref::Cpu => Ok(Choice::Cpu("requested".into())),
        _ if !gpu_compiled => match pref {
            RendererPref::Gpu => Err("--renderer gpu: this build has no GPU backend (rebuild with --features gpu)".into()),
            _ => Ok(Choice::Cpu("built without the gpu feature".into())),
        },
        RendererPref::Gpu => probe().map(Choice::Gpu).map_err(|e| format!("--renderer gpu: {e}")),
        RendererPref::Auto => Ok(match probe() {
            Ok(d) => Choice::Gpu(d),
            Err(e) => Choice::Cpu(e),
        }),
    }
}

/// True for the software GL implementations that must not count as a GPU.
pub fn is_software_renderer(renderer: &str, vendor: &str) -> bool {
    let r = renderer.to_ascii_lowercase();
    let v = vendor.to_ascii_lowercase();
    ["llvmpipe", "softpipe", "swrast", "software rasterizer", "lavapipe", "microsoft basic render", "basic render driver"]
        .iter()
        .any(|k| r.contains(k) || v.contains(k))
        || v.contains("mesa/x.org") && r.contains("software")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse() {
        assert_eq!("AUTO".parse(), Ok(RendererPref::Auto));
        assert_eq!(" gpu ".parse(), Ok(RendererPref::Gpu));
        assert_eq!("cpu".parse(), Ok(RendererPref::Cpu));
        assert!("opengl".parse::<RendererPref>().is_err());
    }

    #[test]
    fn precedence() {
        assert_eq!(resolve_pref(None, None), Ok(RendererPref::Auto));
        assert_eq!(resolve_pref(None, Some("cpu")), Ok(RendererPref::Cpu));
        assert_eq!(resolve_pref(None, Some("")), Ok(RendererPref::Auto));
        assert_eq!(resolve_pref(Some("gpu"), Some("cpu")), Ok(RendererPref::Gpu));
        assert!(resolve_pref(None, Some("zzz")).unwrap_err().contains("LLMV_RENDERER"));
        assert!(resolve_pref(Some("zzz"), None).is_err());
    }

    #[test]
    fn decisions() {
        let ok = || Ok::<_, String>("AMD (radeonsi), GL 4.6".to_string());
        let ko = || Err::<String, _>("only llvmpipe available".to_string());
        // cpu never probes
        assert_eq!(decide(RendererPref::Cpu, true, || panic!("probed")).unwrap(), Choice::Cpu("requested".into()));
        // no feature
        assert!(decide(RendererPref::Gpu, false, ok).is_err());
        assert!(!decide(RendererPref::Auto, false, || panic!("probed")).unwrap().is_gpu());
        // feature
        assert!(decide(RendererPref::Gpu, true, ok).unwrap().is_gpu());
        assert!(decide(RendererPref::Gpu, true, ko).unwrap_err().contains("llvmpipe"));
        assert!(decide(RendererPref::Auto, true, ok).unwrap().is_gpu());
        assert_eq!(decide(RendererPref::Auto, true, ko).unwrap(), Choice::Cpu("only llvmpipe available".into()));
    }

    #[test]
    fn log_lines() {
        assert_eq!(Choice::Gpu("X, GL 4.6".into()).log_line(), "renderer: gpu (X, GL 4.6)");
        assert_eq!(Choice::Cpu("requested".into()).log_line(), "renderer: cpu (requested)");
    }

    #[test]
    fn software_detection() {
        assert!(is_software_renderer("llvmpipe (LLVM 20.1.2, 256 bits)", "Mesa"));
        assert!(is_software_renderer("softpipe", "Mesa"));
        assert!(is_software_renderer("Software Rasterizer", "Mesa/X.org"));
        assert!(is_software_renderer("zink Vulkan 1.3 (llvmpipe (LLVM 20))", "Mesa"));
        assert!(is_software_renderer("Microsoft Basic Render Driver", "Microsoft"));
        // only the listed names count as software
        assert!(!is_software_renderer("SWR", "Intel"));
        assert!(!is_software_renderer("AMD Radeon Graphics (radeonsi, renoir, LLVM 20)", "AMD"));
        assert!(!is_software_renderer("NVIDIA GeForce RTX 3090/PCIe/SSE2", "NVIDIA Corporation"));
        assert!(is_software_renderer("lavapipe", "Mesa"));
    }
}
