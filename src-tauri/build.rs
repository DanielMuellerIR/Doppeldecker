// Tauri-Build-Skript: liest tauri.conf.json und bindet das Frontend + die
// Command-Metadaten zur Compile-Zeit ein.
#[path = "../native/build-admin.rs"]
mod admin;
fn main() {
    admin::build();
    tauri_build::build()
}
