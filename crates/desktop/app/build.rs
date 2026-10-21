//! Standard Tauri build script: embeds `tauri.conf.json`, the capability
//! files, and the bundle icons into the generated context the
//! `generate_context!` macro in `main.rs` reads.

fn main() {
    tauri_build::build()
}
