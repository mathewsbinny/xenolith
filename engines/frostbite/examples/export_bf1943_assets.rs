//! CLI runner to export BF1943's literal Xbox 360 menu PNGs and UI WAVs via `xenolith`.
#![allow(clippy::print_stdout, clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use x360_frostbite::menu_export::export_bf1943_menu_assets;

fn main() {
    let stfs = std::env::args().nth(1).map_or_else(
        || PathBuf::from(r"C:\Games\Battlefield 1943\945CD965D6B2B2584A9C06CE16F38A075A6D390558"),
        PathBuf::from,
    );
    let art_dir = std::env::args().nth(2).map_or_else(
        || PathBuf::from(r"C:\Programming\Projects\battlefield-opfor\modules\bf1943\art\x360"),
        PathBuf::from,
    );
    let sfx_dir = std::env::args().nth(3).map_or_else(
        || PathBuf::from(r"C:\Programming\Projects\battlefield-opfor\modules\bf1943\sfx"),
        PathBuf::from,
    );

    match export_bf1943_menu_assets(&stfs, &art_dir, &sfx_dir) {
        Ok(rep) => {
            println!("EXPORTED {} PNGs and {} WAVs!", rep.pngs_exported, rep.wavs_exported);
            for (name, w, h, fmt) in &rep.textures {
                println!("  {name:34} {w}x{h} (fmt={fmt})");
            }
        }
        Err(err) => {
            eprintln!("FAILED: {err}");
            std::process::exit(1);
        }
    }
}
