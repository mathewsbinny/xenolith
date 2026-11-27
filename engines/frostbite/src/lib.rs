//! Frostbite 1 / 1.5 engine-specific Xbox 360 -> PC conversion adapters, built on the shared `platform/` and
//! `middleware/` crates.
#![deny(unsafe_code)]

pub mod aimanim;
pub mod apt;
pub mod audio;
pub mod audioframe;
pub mod bfbc1;
pub mod bundle_port;
pub mod dbx;
pub mod fbrb;
pub mod hkclasses;
pub mod menu_export;
pub mod menu_port;
pub mod mesh;
pub mod meshconv;
pub mod meshdata;
pub mod pe;
pub mod shader_db;
pub mod sns;
pub mod terrain;
pub mod texcheck;
pub mod texture;
pub mod wave;
pub mod wavport;

// Re-export shared platform & middleware crates used by Frostbite pipelines.
pub use x360_containers as containers;
pub use x360_granny as granny;
pub use x360_havok as havok;
pub use x360_xenos_shader as shader;
pub use x360_xenos_texture as xenos_texture;
#[cfg(feature = "xma")]
pub use x360_xma as xma;
