# Engine Adapters

Each subdirectory here is a self-contained crate for a specific game engine's Xbox 360 -> PC conversion pipeline, built on top of the shared `platform/` and `middleware/` crates.

## Current Engines
- `frostbite/` (`x360-frostbite`) — Frostbite 1 / 1.5 (BFBC1, BFBC2, BF1943, Medal of Honor 2010 MP)

## Adding a New Engine (e.g. `unreal3/`)
1. Create `engines/<engine>/Cargo.toml` and add `"engines/<engine>"` to `[workspace.members]` in the root `Cargo.toml`.
2. Depend on the shared crates you need (e.g. `x360-containers`, `x360-xenos-texture`, `x360-xenos-shader`, `x360-xma`, `x360-havok`, `x360-granny`).
3. Keep engine-specific headers, package containers (e.g. UPK/XXX), and binding tables inside `engines/<engine>/`, and promote any reusable hardware or middleware logic up into `platform/` or `middleware/`.
