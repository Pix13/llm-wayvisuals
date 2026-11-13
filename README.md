# llm-wayvisuals

Sci-fi 1920x480 "sensor panel" displays for LLM inference stats. Up to three
panels, one per screen, each fed by one
[llm-visuals](https://github.com/DingoOz/llm-visuals) instance (local file,
Unix socket, or remote HTTP). Linux only; works on Wayland (GNOME, KDE, sway,
Hyprland, ...) and on X11. CPU rendering, no GPU needed.

Shown: Gen and Prefill tok/s, model, engine, context, phase, session tokens,
GPU use, last request. A panel without fresh data shows NO SIGNAL, never old
values.

## Build

Rust (stable, edition 2021). No development packages are needed: winit loads
the display libraries at run time. At run time you need the libraries of your
session:

- Fedora: `libwayland-client libxkbcommon libxkbcommon-x11 libX11 libXcursor libXi libxcb`
- Debian/Ubuntu: `libwayland-client0 libxkbcommon0 libxkbcommon-x11-0 libx11-6 libxcursor1 libxi6 libx11-xcb1 libxcb1`

```
cargo build --release
./target/release/llm-wayvisuals --help
```

## Usage

```
# local file written by llm-visuals --export-json
llm-wayvisuals --server /run/user/1000/llmv-0.json,output=DP-2

# Unix socket (NDJSON)
llm-wayvisuals --server unix:///run/user/1000/llmv-0.sock,output=DP-3,theme=blue

# remote daemon over HTTP with a bearer token (keep the file at mode 0600)
llm-wayvisuals --server http://gpubox1.example.lan:9464,token-file=~/.config/llm-wayvisuals/gpubox1.token

# three panels
llm-wayvisuals --server /run/user/1000/llmv-0.json,output=DP-2 \
               --server unix:///run/user/1000/llmv-1.sock,output=DP-3 \
               --server http://host:9464,model=deepseek,title=Maya
```

## Stacking

One surface (a window, or a fullscreen monitor) shows 1 to 3 panels stacked
vertically. Panels are grouped by `output`: all panels without `output` share
one surface, panels with the same `output` share that monitor, and different
outputs give different surfaces.

```
# one window with 3 stacked panels (1920x1080 by default)
llm-wayvisuals --window --server A.json --server B.json --server C.json

# two screens, one panel each
llm-wayvisuals --server A.json,output=DP-2 --server B.json,output=DP-3

# three panels stacked on one screen
llm-wayvisuals --server A.json,output=DP-2 --server B.json,output=DP-2 --server C.json,output=DP-2
```

In a surface of W x H: one panel keeps its native 4:1 shape (W x W/4,
centred); two or three panels fill the surface, each W x H/n. Three real
1920x480 screens still work: give each panel its own `output`.

`--server` keys: `output`, `theme` (green, gold, purple, blue), `model`
(`auto`, a pid, or part of a name), `token-file` (http only), `name`, `title`.
https is not supported: use a VPN or `ssh -L` and point to `http://127.0.0.1:PORT`.

Without `--server`, the config file is read (`--config PATH`, else
`~/.config/llm-wayvisuals/config.toml`). See
[config.example.toml](config.example.toml). `--server` overrides the file.

- `--list-outputs` prints the monitors (index, name, size, position, refresh,
  scale) and exits.
- **Name outputs by connector (`DP-2`), not by index.** Indexes are not the
  same on Wayland and on X11. A name that is not found falls back to a normal
  window, with a message listing the available outputs.
- `--window` opens normal decorated windows, one per group (development, 1920x1080 by default, scaled down to fit the screen).
- Keys in a panel window: `Esc` or `q` quits, `F` toggles fullscreen for the whole surface.
- Frame rate: `--fps` (default 30) while a model is in prefill or decode,
  `--idle-fps` (default 15) when all live panels are idle, 2 fps when no panel
  has been live for 10 s.
- The window title, app id and WM_CLASS are `llm-wayvisuals`, for compositor
  rules.
- Logs go to stderr, prefixed `llm-wayvisuals:`. SIGINT/SIGTERM exit cleanly.

Other subcommands: `render-png --input FILE.json --out OUT.png` (draw a panel
without a display), `bench-render`.

## Exporter (llm-visuals)

This client lives in the llm-visuals repository (`clients/llm-wayvisuals/`)
and reads the exporter built from the repository root. It runs as a headless
daemon:

```
llm-visuals --export-json /run/user/1000/llmv-0.json          # file
llm-visuals --export-sock /run/user/1000/llmv-0.sock          # Unix socket
llm-visuals --export-listen 0.0.0.0:9464                      # HTTP, LAN
llm-visuals --export-listen 0.0.0.0:9464 --export-token-file ~/.t   # HTTP + token
llm-visuals --demo --export-json /tmp/demo.json               # no GPU needed
```

A systemd user unit for the daemon is in `contrib/systemd/` at the
repository root. The JSON schema (`llm-visuals.stats/1`) is described in
`docs/export-schema.md`.
