//! llm-wayvisuals: sci-fi 1920x480 panels fed by llm-visuals exports.
//!
//! Module ownership (see CLAUDE.md, phase 2):
//! - `model`   : schema `llm-visuals.stats/1` (contract, owned by the architect)
//! - `source`  : file / unix / http feeds, reconnection, staleness
//! - `history` : rolling series for the waveform
//! - `config`  : TOML config + panel definitions
//! - `theme`   : the 5 colour themes
//! - `render`  : offscreen drawing of one panel into a tiny-skia Pixmap
//! - `wayland` : surfaces per output (added by the frontend task)

pub mod config;
pub mod history;
pub mod model;
pub mod render;
pub mod source;
pub mod theme;
