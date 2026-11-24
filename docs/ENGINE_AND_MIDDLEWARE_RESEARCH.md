# Xbox 360 Game Engines & Middleware Research

**Scope:** Survey of 7th-generation (Xbox 360, 2005–2015) game engines and third-party middleware that **Xenolith** can support, ranked by technical feasibility, reuse of existing crates (`platform/` and `middleware/`), and PC-porting / preservation value.

---

## 1. Executive Summary: The Reuse Multiplier

Because Xbox 360 titles almost universally share the same underlying hardware constraints (PowerPC big-endian Xenon CPU, ATI/AMD Xenos GPU, hardware XMA2 audio, 512 MB unified RAM, and Microsoft's XDK compression), Xenolith's existing `platform/` crates already solve the hardest hardware-level problems across **most** Xbox 360 engines:

| Existing Crate | Cross-Engine Coverage |
|---|---|
| `x360-containers` (STFS, XISO) | ~100% of Xbox 360 discs, XBLA titles, and DLC packages |
| `x360-xenos-texture` (32×32 tiling, mip-tail pack, swap) | ~95% of Xbox 360 textures (every engine using D3D9/Xenos tiled surfaces) |
| `x360-xenos-shader` (XDK parse, disasm, HLSL, CPU exec) | All Xbox 360 FXC-compiled shaders (microcode + disassembly; HLSL bindings per engine) |
| `x360-xma` (XMA2 decoder) | ~80% of Xbox 360 audio (raw XMA2, XACT, FMOD FSB4, Wwise BNK, EA Wave) |
| `x360-havok` (Havok 4.6/5.5, MOPP) | Hundreds of Xbox 360 titles across Frostbite, Blam!, Gamebryo, Anvil, Hedgehog, etc. |
| `x360-granny` (Granny 2 `.gr2`) | Dozens of Xbox 360 titles (Frostbite, Civilization, BioWare/Obsidian/Ensemble engines) |

### Three High-Leverage `platform/` Additions That Unlock Most of the Catalog
Before adding many new engines, adding three small platform crates multiplies value across nearly every engine below:

1. **`platform/x360-xmem` (XMemCompress / LZX Decompressor):** Microsoft's XDK LZX stream format (`0x0FF512ED` / raw LZX blocks) is used by **Unreal Engine 3**, **Source**, **RAGE**, **XNA**, **Blam!**, and many XBLA titles.
2. **`platform/x360-pc-export` (Standard PC Asset Writers):** Clean pure-Rust writers for **DDS** (DX9/DX10 headers for untiled `x360-xenos-texture` output), **WAV** (already prototyped in `xenolith`), and **glTF 2.0 / OBJ** (for converted meshes & skeletons).
3. **`platform/x360-xex` (XEX2 Executable Reader):** Parses Xbox 360 `.xex` headers, LZX-compressed PE images, and import/export tables—enabling automated extraction of Havok `hkClass` tables, shader tables, and string tables directly from console executables.

---

## 2. Middleware Landscape (`middleware/`)

Third-party middleware was ubiquitous on Xbox 360. Supporting a middleware crate once unlocks it across *all* engines that licensed it.

### A. Audio Middleware *(Highest Immediate ROI — Reuses `x360-xma`)*

| Middleware | Xbox 360 Formats | Codec(s) | PC Target | Feasibility | Notable Xbox 360 Usage |
|---|---|---|---|---|---|
| **XACT** (Microsoft) | `.xwb` (Wave Bank), `.xsb` (Sound Bank), `.xgs` | XMA2, PCM, ADPCM | WAV / OGG | **Tier 1 (Easy)** | Early/mid 360 exclusives, XNA/XBLA games, *Blue Dragon*, *Crackdown* |
| **FMOD Designer** | `.fsb` (FSB3/FSB4), `.fev` | XMA2, MP3, IMA-ADPCM | WAV / FSB5 | **Tier 1 (Easy)** | *BioShock*, *Crysis*, *Dead Space* (select), *Forza*, *Just Cause 2*, *L.A. Noire* |
| **Audiokinetic Wwise** | `.bnk`, `.pck`, `.wem` | XMA2, Vorbis, PCM | Wwise PC / WAV | **Tier 1 (Medium)** | *Assassin's Creed*, *Borderlands 2*, *Dark Souls*, *Mass Effect 3*, *Shadowrun* |
| **EA Audio** *(promote from `x360-frostbite`)* | `.sps`, `.snr`, `.sns`, Frostbite `Wave` | EA-XMA, EA-XAS, EALayer3 | EA-XAS / WAV | **Tier 1 (Done in Frostbite)** | All 2007–2012 EA titles (*BFBC1/2*, *BF1943*, *Dead Space*, *Mirror's Edge*, *NFS*, *MoH*) |
| **CRI ADX / ADX2** | `.adx`, `.acb`, `.awb`, `.cpk` | ADX, HCA, XMA2 | WAV / HCA | **Tier 2 (Medium)** | Japanese 360 titles (*Bayonetta*, *Vanquish*, *Sonic*, *Tales of Vesperia*, *Persona 4 Arena*) |
| **Miles Sound System / Bink Audio** | `.mss`, `.bik` audio tracks | Bink Audio, MP3 | WAV | **Tier 2 (Medium)** | *Source* titles, *The Elder Scrolls*, *Dragon Age*, *CoD* |

### B. Physics Middleware

| Middleware | Xbox 360 Formats | PC Target | Feasibility | Notes |
|---|---|---|---|---|
| **Havok Physics** *(Implemented: 4.6/5.5)* | `hkPackfile` (`0x57E0E05710C0C010`), `.hkx`, MOPP | PC `hkPackfile` (LE) | **Tier 1 (Expandable)** | Extend `x360-havok` to Havok 6.x / 2010–2013 tagfiles (`hkTagfile` / `TAG0` format used in *Skyrim*, *Halo Reach*, *Dark Souls*, *BF3*). |
| **NVIDIA / Ageia PhysX** | Cooked `.nxu` / `NXS` triangle/convex meshes, UE3 `RbBodySetup` | PC PhysX cooked / raw mesh | **Tier 2 (Medium)** | Huge overlap with **Unreal Engine 3** (*Gears of War*, *Mirror's Edge*, *Batman*, *Borderlands*). |
| **Bullet Physics** | `.bullet` (`BULLETf` / `BULLETb`) | PC `.bullet` (LE) | **Tier 2 (Easy)** | Smaller 360 footprint (some Rockstar/indie/XBLA titles). |

### C. Animation, Rigging & Facial Middleware

| Middleware | Xbox 360 Formats | PC Target | Feasibility | Notes |
|---|---|---|---|---|
| **RAD Granny 2** *(Implemented)* | `.gr2` (BE) | `.gr2` (LE) / glTF | **Tier 1 (Done)** | Already supports parsing, BE→LE `to_pc`, curve editing, and CRC32. |
| **NaturalMotion Morpheme** | `.mcn` / `.nmb` / compiled animation networks | PC Morpheme / glTF clips | **Tier 3 (Hard)** | Used in *BioShock Infinite*, *Enslaved*, *Pure*, *Star Wars: TFU*. |
| **FaceFX** | `.fxa` / UE3 `FaceFXAsset` / `FaceFXAnimSet` | PC FaceFX (LE) | **Tier 2 (Medium)** | Shipped in almost every major **UE3** title (*Mass Effect*, *Gears*, *Borderlands*, *Batman*). |
| **Havok Animation / Behavior** | `.hkx` (`hkaSkeleton`, `hkaAnimationContainer`) | PC `.hkx` / glTF | **Tier 1–2 (Medium)** | Natural extension of `x360-havok` using the same packfile/tagfile reflection layer. |

### D. UI, Video & Foliage Middleware

| Middleware | Xbox 360 Formats | PC Target | Feasibility | Notes |
|---|---|---|---|---|
| **Scaleform GFx** | `.gfx` (`CFX`/`GFX` header + Flash SWF tags + Xenos DDS/TGA) | PC `.gfx` / `.swf` | **Tier 1 (Easy–Medium)** | Used across *Mass Effect*, *Batman*, *Borderlands*, *Crysis*, *Skyrim*, *Deus Ex*. Header is nearly standard SWF; textures reuse `x360-xenos-texture`. |
| **EA APT** *(exists in `bfbc2-tool-suite`)* | `.apt` / `.const` / `.dat` | PC `.apt` | **Tier 1 (Easy)** | Shared across EA's 2006–2012 catalog (*BFBC1/2*, *C&C 3/RA3*, *Lord of the Rings*, *MoH*). |
| **RAD Bink Video** | `.bik` (Bink 1 / Bink 2) | `.bik` (platform-agnostic!) | **Tier 1 (Trivial)** | `.bik` bitstreams are endian-independent; only container extraction is needed. |
| **CRI Sofdec / USM** | `.sfd` (MPEG-1/2 + ADX), `.usm` | MP4 / MPEG + WAV | **Tier 2 (Medium)** | Common in Japanese 360 titles (Sega, Bandai Namco, Capcom, PlatinumGames). |
| **SpeedTree** | `.spt` (v4/v5), `.srt` (v6+) | PC `.spt`/`.srt` or glTF | **Tier 2 (Medium)** | Used in *Oblivion*, *Gears*, *Mass Effect*, *Dragon Age*, *Batman*, *Fable*. |

---

## 3. Game Engine Landscape (`engines/`)

### Tier 1 — Highest ROI & Direct Reuse

#### 1. Frostbite 1 / 1.5 *(Implemented)* & Frostbite 2 / 3 *(Natural Expansion)*
* **Frostbite 1 / 1.5 (`engines/frostbite`):** Already supports BFBC1, BFBC2, BF1943, and Medal of Honor (2010 MP) textures, meshes, terrain/water/soot, audio, Havok classes, and AimAnimation.
* **Frostbite 2 / 3 (planned sibling or submodule):** Uses **Superbundles** (`.sb`/`.toc`), **CAS** (`cas.cat`), **EBX**, **RES**, and **Chunks**. Reuses `x360-xenos-texture`, `x360-xenos-shader`, `x360-xma`, `x360-havok` (tagfiles), and EA audio.
* **Notable 360 titles:** *Battlefield: Bad Company* (console-exclusive!), *Battlefield 1943* (console-exclusive!), *BF3*, *NFS: The Run*, *Army of Two: The Devil's Cartel*.

#### 2. Unreal Engine 3 (`engines/unreal3`) — *Top Next Engine Candidate*
* **Containers & Compression:** `.upk` / `.xxx` (big-endian Unreal Packages), `.tfc` (Texture File Cache), compressed with **LZO1X** or **XMemCompress (LZX)**.
* **Xenolith Reuse:**
  * `x360-xenos-texture` handles `UTexture2D` bulk mips (DXT1/3/5, G8, A8R8G8B8, V8U8, BC5).
  * `x360-xenos-shader` parses cooked `FShaderCache` Xenos shaders.
  * `x360-xma` decodes `USoundNodeWave` XMA2 audio.
  * Shares `Scaleform GFx`, `FaceFX`, `SpeedTree`, and `PhysX` middlewares.
* **Preservation / Porting Value:** **Enormous.** Covers console-exclusives (*Gears of War 2*, *Gears of War 3*, *Gears of War: Judgment*, *Lost Odyssey*, *Shadow Complex*, *Fable Anniversary* DLC) plus hundreds of multi-platform 360 titles.
* **Main Challenge:** UE3 package format has per-game `PackageFileVersion` / `LicenseeVersion` struct variations (similar to how `UModel` / `UEViewer` maintains a small game-version table).

#### 3. Source Engine (`engines/source`)
* **Containers & Compression:** `.xzp` (Xbox Zip), `.vpk`, `.bsp` (LZMA / XMemCompress compressed lumps).
* **Xenolith Reuse:**
  * `.vtf` (Xbox 360 VTF v7.3/7.4) uses Xenos tiled DXT1/3/5/RGBA8 → `x360-xenos-texture`.
  * `.wav` (Xbox 360 XMA2 RIFF) → `x360-xma`.
  * `.mdl` / `.vvd` / `.vtx` are well-documented big-endian structs → clean byte-swap to PC Source formats.
* **Notable 360 titles:** *The Orange Box* (*TF2 360*, *Portal*, *HL2*), *Left 4 Dead 1 & 2*, *Portal 2*, *CS:GO (360)*.

#### 4. MT Framework (`engines/mtframework`) — Capcom
* **Containers & Compression:** `.arc` (zlib-compressed archives with clean flat file tables).
* **Xenolith Reuse:**
  * `.tex` (Xenos tiled DXT1/3/5/BC5/ARGB8) → `x360-xenos-texture`.
  * `.sngw` / `.stq` (XMA2 streams) → `x360-xma`.
  * `.mod` (meshes) and `.lmt` (animations) have clean, well-documented community specs.
* **Notable 360 titles:** *Dead Rising*, *Lost Planet 1/2*, *Devil May Cry 4*, *Resident Evil 5/6*, *Dragon's Dogma*, *Marvel vs. Capcom 3*.

#### 5. Gamebryo / Creation Engine (`engines/gamebryo`)
* **Containers:** `.bsa` (Bethesda Softworks Archive, zlib-compressed).
* **Xenolith Reuse:**
  * Textures are Xenos-tiled DDS variants → `x360-xenos-texture`.
  * Audio is `.xma` → `x360-xma`.
  * Physics is Havok `.hkx` → `x360-havok`.
  * Meshes/animations are `.nif` / `.kf` (mature open specs from NifSkope).
* **Notable 360 titles:** *Oblivion*, *Fallout 3*, *Fallout: New Vegas*, *Skyrim*, *Catherine* (Gamebryo).

---

### Tier 2 — High Preservation Value, Moderate-High Effort

#### 6. RAGE (`engines/rage`) — Rockstar
* **Containers:** `.rpf` (RPF2/RPF3/RPF4/RPF6/RPF7, AES-encrypted with game keys + XMemCompress LZX / deflate).
* **Formats:** `.xtd` (texture dictionaries → `x360-xenos-texture`), `.xdr`/`.xft` (drawables/fragments), `.xbn` (bounds), `.awc` (audio wave containers with XMA2 → `x360-xma`).
* **Notable 360 titles:** *Red Dead Redemption*, *Midnight Club: Los Angeles* (console-exclusive!), *GTA IV*, *GTA V*, *Max Payne 3*, *Rockstar Table Tennis*.

#### 7. Blam! Engine (`engines/blam`) — Bungie / 343 Industries
* **Containers:** `.map` cache files with tag tables, `shared.map`, `mainmenu.map`.
* **Formats:** `bitm` tags (Xenos-tiled textures → `x360-xenos-texture`), `snd!` tags (XMA2 → `x360-xma`), `mode`/`coll`/`phmo` (meshes + Havok MOPP → `x360-havok`), `rmsh` (Xenos shaders → `x360-xenos-shader`).
* **Notable 360 titles:** *Halo 3*, *Halo 3: ODST*, *Halo: Reach*, *Halo 4*.

#### 8. IW Engine / Treyarch (`engines/iw`) — Call of Duty
* **Containers:** `.ff` (FastFile, zlib-compressed zone streams), `.iwd` (ZIP).
* **Formats:** `.iwi` (Xenos-tiled textures → `x360-xenos-texture`), `XModel` / `XAnim`, XMA2 sound aliases (`x360-xma`).
* **Notable 360 titles:** *Call of Duty 2*, *Call of Duty 3* (console-exclusive!), *CoD4: MW*, *World at War*, *MW2*, *Black Ops 1 & 2*.

#### 9. CryEngine 2 / 3 (`engines/cryengine`)
* **Containers:** `.pak` (ZIP / encrypted ZIP).
* **Formats:** Split `.dds.0`..`.dds.N` Xenos-tiled textures (`x360-xenos-texture`), `.cgf`/`.chr` meshes, FMOD FSB4 XMA2 audio (`x360-xma`).
* **Notable 360 titles:** *Crysis* (360 console-exclusive lighting/controls port), *Crysis 2*, *Crysis 3*, *Sniper: Ghost Warrior 2*.

#### 10. Anvil / Scimitar & Dunia (`engines/anvil`) — Ubisoft
* **Containers:** `.forge` (Anvil, LZO/XMemCompress), `.dat`/`.fat` (Dunia).
* **Formats:** Xenos textures (`x360-xenos-texture`), Havok physics (`x360-havok`), Wwise/XMA2 audio (`x360-xma`).
* **Notable 360 titles:** *Assassin's Creed 1–IV*, *Prince of Persia (2008)*, *Far Cry 2/3*.

#### 11. Refractor Engine — Console Branch (`engines/refractor`) — DICE / EA UK
* **Target Title:** *Battlefield 2: Modern Combat* (Xbox 360, 2006 — console-exclusive Battlefield entry with unique campaign, maps, vehicles, and hot-swap mechanics).
* **Containers & Architecture:** Console-adapted Refractor 2 lineage using big-endian disc-streamed archive containers, `.con`/`.tweak`-style level/object scripts, and big-endian geometry buffers.
* **Xenolith Reuse:**
  * Xenos-tiled DXT1–DXT5 / uncompressed textures → `x360-xenos-texture`.
  * Xbox 360 FXC shaders → `x360-xenos-shader`.
  * Console XMA audio streams → `x360-xma`.
  * Disc/DLC extraction → `x360-containers`.
* **Preservation / Porting Value:** High — *BF2: Modern Combat* never received a PC release, and its assets sit in the same DICE Battlefield lineage as Frostbite (`engines/frostbite`).

---

### Tier 3 — Specialized / Niche Engines

* **XNA / MonoGame (`engines/xna`):** `.xnb` (XMemCompress LZX + `Texture2D` Xenos-tiled + `SoundEffect` XMA2/PCM) + XACT `.xwb`/`.xsb`. Very clean binary format; unlocks dozens of XBLA indies (*Dust*, *Fez*, *Bastion*, *Castle Crashers*, *Charlie Murder*, *The Dishwasher*).
* **Hedgehog Engine (`engines/hedgehog`):** `.ar`/`.arl` archives, Xenos DDS textures, `.model`/`.terrain-model`, Havok `.hkx`, CRI ADX/CSB audio (*Sonic Unleashed*, *Sonic Generations*).
* **EGO Engine (`engines/ego`):** `.pssg` (self-describing XML-like binary containers with Xenos textures), `.nefs` (*DiRT 1–3*, *GRID*, *F1 2010–2013*).

---

## 4. Recommended Expansion Roadmap for Xenolith

### Phase 1 — Platform Force Multipliers (`platform/`)
1. **`platform/x360-xmem`** — Pure-Rust XMemCompress (LZX) decompressor.
2. **`platform/x360-pc-export`** — Standard PC **DDS** (DX9/DX10), **WAV**, and **glTF 2.0** writers so *every* engine adapter can export universal PC assets immediately.
3. **`platform/x360-xex`** — XEX2 header & embedded PE extractor.

### Phase 2 — High-Leverage Middleware (`middleware/`)
1. **`middleware/ea-audio`** — Promote EA-XAS / Wave / SPS / SNR / SNS out of `engines/frostbite` so all EA engines share it.
2. **`middleware/fmod`** (FSB4) & **`middleware/wwise`** (BNK/PCK) & **`middleware/xact`** (XWB/XSB) — All three wrap `x360-xma`, instantly unlocking audio extraction/conversion across hundreds of Xbox 360 titles.
3. **`middleware/scaleform`** (GFx) — High reuse across UE3, CryEngine, and Gamebryo.
4. **Expand `middleware/havok`** — Add Havok `hkTagfile` (`TAG0`) support for 2010–2013 titles.

### Phase 3 — Engine Expansion (`engines/`)
1. **`engines/unreal3`** — Biggest single library unlock on Xbox 360 (`UPK`/`TFC` + `x360-xmem` + `x360-xenos-texture` + `x360-xenos-shader` + `x360-xma`).
2. **`engines/xna`** — Very small effort once `x360-xmem` and `middleware/xact` exist; unlocks XBLA preservation.
3. **`engines/mtframework`** or **`engines/source`** — Clean, well-documented archive/asset formats with high reuse of `x360-xenos-texture` and `x360-xma`.
4. **Frostbite 2 / 3** — Natural follow-on to `engines/frostbite` (Superbundles / EBX / RES).
