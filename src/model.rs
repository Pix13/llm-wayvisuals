//! Schema `llm-visuals.stats/1`, as produced by llm-visuals `--export-*`
//! (spec: ../../docs/export-schema.md in the llm-visuals repo).
//!
//! Contract shared by `source`, `history` and `render`. Every field is
//! optional and unknown fields are ignored: a producer may add fields
//! (additive changes stay in /1) and a missing value must render as `--`,
//! never as 0.

use serde::Deserialize;

pub const SCHEMA: &str = "llm-visuals.stats/1";

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Snapshot {
    pub schema: Option<String>,
    pub seq: Option<u64>,
    /// Unix seconds when the producer built this snapshot.
    pub ts: Option<f64>,
    pub host: Option<String>,
    /// "live" | "demo" | "replay"
    pub source: Option<String>,
    pub exporter_pid: Option<u32>,
    pub models: Vec<Model>,
    pub gpus: Vec<Gpu>,
    /// Host-wide figures (schema /1 addition, v2).
    pub system: Option<System>,
}

/// `system` (v2): host-wide CPU and RAM.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct System {
    /// Busy % of all logical CPUs (iowait excluded), 0..100.
    pub cpu_pct: Option<f64>,
    pub iowait_pct: Option<f64>,
    pub cpu_logical: Option<u32>,
    pub cpu_physical: Option<u32>,
    pub ram_used_mb: Option<u64>,
    pub ram_total_mb: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Phase {
    #[default]
    Idle,
    Prefill,
    Decode,
    /// A value this client does not know yet.
    Unknown,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Model {
    pub pid: Option<u32>,
    pub name: Option<String>,
    pub engine: Option<String>,
    pub quant: Option<String>,
    /// Raw string; use [`Model::phase`].
    #[serde(rename = "phase")]
    pub phase_raw: Option<String>,
    pub decode_tps: Option<f64>,
    pub prefill_tps: Option<f64>,
    pub peak_decode_tps: Option<f64>,
    pub peak_prefill_tps: Option<f64>,
    pub ctx_used: Option<u64>,
    pub ctx_max: Option<u64>,
    pub session_decoded: Option<u64>,
    pub session_prefilled: Option<u64>,
    pub requests_finished: Option<u64>,
    pub gpu_indices: Vec<u32>,
    pub current_request: Option<CurrentRequest>,
    pub last_request: Option<LastRequest>,
    /// Engine-provided display label, e.g. "DeepSeek V4.1 · EXL3 2.9 bpw" (v2).
    pub label: Option<String>,
    /// Inference process CPU use, % of `engine_threads` (can exceed 100) (v2).
    pub process_cpu_pct: Option<f64>,
    /// Same, in cores-equivalent (v2).
    pub process_cpu_cores: Option<f64>,
    /// Threads the engine computes with, and where that number came from:
    /// "engine" | "cmdline" | "physical_cores" | "logical_cpus" (v2).
    pub engine_threads: Option<u32>,
    pub engine_threads_source: Option<String>,
    /// Data path figures from llm-visuals' Bandwidth view (v2).
    pub bandwidth: Option<Bandwidth>,
}

/// `models[].bandwidth` (v2): the weight path SSD → RAM/CPU → PCIe → GPUs.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Bandwidth {
    /// System-wide block-device reads, MB/s.
    pub disk_mb_s: Option<f64>,
    /// The inference process's own storage reads, MB/s.
    pub proc_disk_mb_s: Option<f64>,
    pub majflt_per_s: Option<f64>,
    /// Weights streamed out of system RAM by the CPU, GB/s.
    pub ram_gb_s: Option<f64>,
    /// "estimated" (tensor table) | "maya" (engine expert tiers) | null.
    pub ram_source: Option<String>,
    pub pcie: Vec<PcieLink>,
    pub vram: Vec<VramStat>,
    /// Weight stream out of VRAM across the cards, GB/s, and its ceiling.
    pub vram_gb_s: Option<f64>,
    pub vram_peak_gb_s: Option<f64>,
    pub steps_per_s: Option<f64>,
    pub bottleneck: Option<Bottleneck>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct PcieLink {
    pub gpu: Option<u32>,
    /// Host → device, MB/s.
    pub rx_mb_s: Option<f64>,
    pub tx_mb_s: Option<f64>,
    /// Link capacity (gen × lanes), MB/s.
    pub cap_mb_s: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct VramStat {
    pub gpu: Option<u32>,
    /// Memory-controller busy %.
    pub busy_pct: Option<f64>,
    pub peak_gb_s: Option<f64>,
}

/// Which hop holds the model back (llm-visuals `bandwidth::assess`).
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Bottleneck {
    /// "disk" | "ram" | "pcie" | "vram" | "compute" | null.
    pub stage: Option<String>,
    /// GPU index for "pcie" / "vram" / "compute".
    pub gpu: Option<u32>,
    /// false when the verdict names no real bound (do not glow red).
    pub bound: Option<bool>,
    pub headline: Option<String>,
    pub detail: Option<String>,
}

impl Model {
    pub fn phase(&self) -> Phase {
        match self.phase_raw.as_deref() {
            Some("idle") | None => Phase::Idle,
            Some("prefill") => Phase::Prefill,
            Some("decode") => Phase::Decode,
            Some(_) => Phase::Unknown,
        }
    }

    /// Prefill rate to display: the live rate while it is meaningful,
    /// else the last finished request's average (Maya reports no prompt
    /// progress, so its live `prefill_tps` stays 0 during prefill).
    pub fn display_prefill_tps(&self) -> Option<f64> {
        match self.prefill_tps {
            Some(v) if v > 0.0 => Some(v),
            _ => self
                .last_request
                .as_ref()
                .and_then(|r| r.avg_prefill_tps)
                .filter(|v| *v > 0.0),
        }
    }

    /// Quant to display: `quant`, else the part of `label` after the last
    /// "·" (Maya: "DeepSeek V4.1 · EXL3 2.9 bpw" → "EXL3 2.9 bpw").
    pub fn display_quant(&self) -> Option<&str> {
        self.quant.as_deref().filter(|q| !q.is_empty()).or_else(|| {
            self.label
                .as_deref()
                .and_then(|l| l.rsplit_once('·'))
                .map(|(_, q)| q.trim())
                .filter(|q| !q.is_empty())
        })
    }

    /// The live bottleneck worth highlighting (bound == true).
    pub fn active_bottleneck(&self) -> Option<&Bottleneck> {
        self.bandwidth
            .as_ref()
            .and_then(|b| b.bottleneck.as_ref())
            .filter(|b| b.bound == Some(true) && b.stage.is_some())
    }

    /// Context fill in 0..=1, when both ends are known.
    pub fn ctx_fill(&self) -> Option<f64> {
        match (self.ctx_used, self.ctx_max) {
            (Some(u), Some(m)) if m > 0 => Some((u as f64 / m as f64).clamp(0.0, 1.0)),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct CurrentRequest {
    pub prompt_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub decoded: Option<u64>,
    pub ttft_s: Option<f64>,
    pub elapsed_s: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct LastRequest {
    pub prompt_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub decoded: Option<u64>,
    pub ttft_s: Option<f64>,
    pub duration_s: Option<f64>,
    pub avg_prefill_tps: Option<f64>,
    pub avg_decode_tps: Option<f64>,
    pub peak_decode_tps: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Gpu {
    pub index: Option<u32>,
    pub name: Option<String>,
    pub util_pct: Option<f64>,
    pub mem_used_mb: Option<u64>,
    pub mem_total_mb: Option<u64>,
    pub power_w: Option<f64>,
    pub power_max_w: Option<f64>,
    pub temp_c: Option<f64>,
    pub mem_temp_c: Option<f64>,
    pub temp_limit_c: Option<f64>,
    pub clock_sm_mhz: Option<u32>,
    pub clock_mem_mhz: Option<u32>,
    pub fan_pct: Option<f64>,
    pub fan_rpm: Option<u32>,
    pub unified: Option<bool>,
}

/// Which model of a snapshot a panel shows (config `model = ...`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ModelSelect {
    /// The first model; prefer one that is not idle.
    #[default]
    Auto,
    Pid(u32),
    /// Case-insensitive substring of the model name.
    Name(String),
}

impl Snapshot {
    pub fn parse(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    pub fn select(&self, sel: &ModelSelect) -> Option<&Model> {
        match sel {
            ModelSelect::Auto => self
                .models
                .iter()
                .find(|m| m.phase() != Phase::Idle)
                .or_else(|| self.models.first()),
            ModelSelect::Pid(p) => self.models.iter().find(|m| m.pid == Some(*p)),
            ModelSelect::Name(n) => {
                let n = n.to_lowercase();
                self.models.iter().find(|m| {
                    m.name
                        .as_deref()
                        .is_some_and(|x| x.to_lowercase().contains(&n))
                })
            }
        }
    }

    /// GPUs used by `model` (all GPUs when the model lists none).
    pub fn gpus_of<'a>(&'a self, model: Option<&Model>) -> Vec<&'a Gpu> {
        match model {
            Some(m) if !m.gpu_indices.is_empty() => self
                .gpus
                .iter()
                .filter(|g| g.index.is_some_and(|i| m.gpu_indices.contains(&i)))
                .collect(),
            _ => self.gpus.iter().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tolerant_parse_and_fallbacks() {
        let s = Snapshot::parse(
            r#"{"schema":"llm-visuals.stats/1","seq":3,"future":1,
                "models":[{"pid":7,"name":"DeepSeek","phase":"prefill","prefill_tps":0.0,
                           "last_request":{"avg_prefill_tps":50.8}}],
                "gpus":[{"index":0,"temp_c":null}]}"#,
        )
        .unwrap();
        let m = s.select(&ModelSelect::Auto).unwrap();
        assert_eq!(m.phase(), Phase::Prefill);
        assert_eq!(m.display_prefill_tps(), Some(50.8));
        assert_eq!(m.ctx_fill(), None);
        assert!(s.gpus[0].temp_c.is_none());
        assert!(s.select(&ModelSelect::Name("seek".into())).is_some());
        assert!(s.select(&ModelSelect::Pid(8)).is_none());
        assert!(s.system.is_none());
    }

    #[test]
    fn v2_fields() {
        let s = Snapshot::parse(
            r#"{"system":{"cpu_pct":72.5,"cpu_logical":28},
                "models":[{"label":"DeepSeek V4.1 · EXL3 2.9 bpw","process_cpu_pct":65.7,
                  "engine_threads":14,"engine_threads_source":"engine",
                  "bandwidth":{"ram_gb_s":5.4,"ram_source":"maya",
                    "pcie":[{"gpu":0,"rx_mb_s":3200,"cap_mb_s":31500}],
                    "bottleneck":{"stage":"ram","bound":true}}}]}"#,
        )
        .unwrap();
        let m = &s.models[0];
        assert_eq!(m.display_quant(), Some("EXL3 2.9 bpw"));
        assert_eq!(m.active_bottleneck().unwrap().stage.as_deref(), Some("ram"));
        assert_eq!(s.system.as_ref().unwrap().cpu_logical, Some(28));
    }
}
