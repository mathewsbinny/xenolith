# Xbox 360 to PC Porting Tools

A modular, pure-Rust toolkit for reading, decoding, and converting Xbox 360 containers, GPU/audio hardware formats, third-party middleware, and game-engine assets to PC formats.

Extracted from the console-to-PC conversion stack of `bfbc2-tool-suite` and organized as a layered modular monolith so additional middlewares (PhysX, Morpheme, Bink, ...) and engines (Unreal Engine 3, MT Framework, ...) can be added side by side while sharing the common Xbox 360 hardware and middleware crates.

## Documentation

* **[Architecture & Design Specification](docs/ARCHITECTURE.md)** — Layered design, data-flow diagrams, safety/determinism invariants, licensing isolation, and extension playbook.
* **[API Reference](docs/API_REFERENCE.md)** — Complete crate, module, type, and function reference.
* **[Quick Start Guide](docs/QUICKSTART.md)** — Prerequisites, build/test commands, and runnable Rust examples.
* **[Ecosystem & Tool Suite Roadmap](docs/ECOSYSTEM_ROADMAP.md)** — 3-repo architecture across `xenolith`, `battlefield-opfor`, and the planned game-agnostic `frostbite-tool-suite` (BFBC1, BF1943, BFBC2/OPFOR, and MoH 2010 MP).
* **[Engine & Middleware Research](docs/ENGINE_AND_MIDDLEWARE_RESEARCH.md)** — Survey of targetable Xbox 360 game engines, middleware formats, reuse analysis, and expansion roadmap.

## Workspace Layout

```text
platform/                   Engine-agnostic Xbox 360 hardware & OS layer
├── x360-containers/        STFS (LIVE / PIRS / CON) packages, XISO disc images & XEX2 executables
├── x360-xma/               Pure-Rust XMA2 audio decoder (LGPL-2.1-or-later)
├── x360-xenos-shader/      XDK shader container, microcode decoder, disasm, HLSL translator & CPU interpreter
└── x360-xenos-texture/     Engine-agnostic Xenos 32x32 block tiling, mip packing, endian swap & decode

middleware/                 Engine-agnostic third-party middleware converters
├── havok/                  Havok packfile reader, reflection, endian swapper, MOPP & 4.6.1 -> 5.5.0 porting
└── granny/                 RAD Granny 2 (.gr2) reader, BE -> LE conversion, curve & CRC editing

engines/                    Engine-specific adapters built on platform/ and middleware/
└── frostbite/              Frostbite 1 / 1.5 adapters (XenonTexture/ITexture + RES\x02/3D volumes, FbRB,
                            binary DBX, EA APT BIGF, MeshData/MeshSet, terrain/water/soot incl. 256x256,
                            EA-XAS/Wave/streaming_sounds SPS v1 banks, ShaderDatabase DXBC & Dx9/10/11 fxc bridge,
                            BFBC1 1.0 -> 1.5 upgraders (DBX + null-array pruning, Kynogon HPD v1 -> v2 navmeshes,
                            Bink unwrap), AnimTreeInfo v6, GUID-aware dedup, bundle_port, AimAnimation)

apps/                       Desktop GUI & headless CLI applications built on the workspace
└── xenolith/               Xenolith (`target/release/xenolith.exe`) — egui/egui_dock GUI + headless CLI subcommands

design/
└── tokens.json             Tool Suite Design Language color tokens (Dark & Light themes)
```

## Licensing

The workspace is licensed under the **MIT License** (`LICENSE`), with one exception:
* `platform/x360-xma` is a Rust port of FFmpeg's WMA Pro / XMA2 decoder and is licensed under **LGPL-2.1-or-later** (`platform/x360-xma/LICENSE`).

`x360-xma` is linked only through the `xma` Cargo feature of `x360-frostbite` and `xenolith`, which is on by default: a default `xenolith.exe` combines the MIT code with the LGPL-2.1-or-later decoder, whose complete source (with the rest of the workspace's) is this repository. `--no-default-features` (with `--features frostbite` for the app) builds MIT-only crates and binaries without XMA2 decoding; CI (`.github/workflows/ci.yml`, job `mit-only`) builds that configuration on every push and fails if `cargo tree` lists `x360-xma` in it.

## Quick Commands

```powershell
# Launch Xenolith GUI (debug) or build standalone target/release/xenolith.exe (release)
cargo run -p xenolith
cargo build --release -p xenolith

# Verify tests, lints, docs, and formatting
cargo test
cargo clippy --all-targets -- -D warnings
$env:RUSTDOCFLAGS="-D warnings"; cargo doc --workspace --no-deps
cargo fmt --check

# The MIT-only build (no x360-xma linked; CI's mit-only job)
cargo build -p xenolith --no-default-features --features frostbite
cargo tree -p xenolith --no-default-features --features frostbite -e normal --prefix none
```
