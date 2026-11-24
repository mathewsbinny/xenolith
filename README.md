# Xbox 360 to PC Porting Tools

A modular, pure-Rust toolkit for reading, decoding, and converting Xbox 360 containers, GPU/audio hardware formats, third-party middleware, and game-engine assets to PC formats.

Extracted from the console-to-PC conversion stack of `bfbc2-tool-suite` and organized as a layered modular monolith so additional middlewares (PhysX, Morpheme, Bink, ...) and engines (Unreal Engine 3, MT Framework, ...) can be added side by side while sharing the common Xbox 360 hardware and middleware crates.

## Documentation

* **[Architecture & Design Specification](docs/ARCHITECTURE.md)** — Layered design, data-flow diagrams, safety/determinism invariants, licensing isolation, and extension playbook.
* **[API Reference](docs/API_REFERENCE.md)** — Complete crate, module, type, and function reference.
* **[Quick Start Guide](docs/QUICKSTART.md)** — Prerequisites, build/test commands, and runnable Rust examples.

## Workspace Layout

```text
platform/                   Engine-agnostic Xbox 360 hardware & OS layer
├── x360-containers/        STFS (LIVE / PIRS / CON) packages & XISO disc images
├── x360-xma/               Pure-Rust XMA2 audio decoder (LGPL-2.1-or-later)
├── x360-xenos-shader/      XDK shader container, microcode decoder, disasm, HLSL translator & CPU interpreter
└── x360-xenos-texture/     Engine-agnostic Xenos 32x32 block tiling, mip packing, endian swap & decode

middleware/                 Engine-agnostic third-party middleware converters
├── havok/                  Havok packfile reader, reflection, endian swapper, MOPP & 4.6 -> 5.5 porting
└── granny/                 RAD Granny 2 (.gr2) reader, BE -> LE conversion, curve & CRC editing

engines/                    Engine-specific adapters built on platform/ and middleware/
└── frostbite/              Frostbite 1 / 1.5 adapters (92-byte XenonTexture/ITexture, MeshData/MeshSet,
                            terrain/water/soot, EA-XAS/Wave audio, Havok PE classes, AimAnimation)

apps/                       Desktop & CLI applications built on the workspace
└── xenolith/               Xenolith (`bin/xenolith.exe`) — egui/egui_dock GUI using `design/tokens.json`

design/
└── tokens.json             Tool Suite Design Language color tokens (Dark & Light themes)
```

## Licensing

The workspace is licensed under the **MIT License** (`LICENSE`), with one exception:
* `platform/x360-xma` is a Rust port of FFmpeg's WMA Pro / XMA2 decoder and is licensed under **LGPL-2.1-or-later** (`platform/x360-xma/LICENSE`).

## Quick Commands

```powershell
# Launch Xenolith GUI (debug) or build standalone bin/xenolith.exe (release)
cargo run -p xenolith
cargo build --release -p xenolith

# Verify tests, lints, docs, and formatting
cargo test
cargo clippy --all-targets -- -D warnings
$env:RUSTDOCFLAGS="-D warnings"; cargo doc --workspace --no-deps
cargo fmt --check
```
