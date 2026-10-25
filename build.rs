//! Embeds every po/<lang>.po catalog into the binary, so translations need no
//! installed files.

use std::env;
use std::fs;
use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=po");
    let po_dir = Path::new(&env::var("CARGO_MANIFEST_DIR").unwrap()).join("po");
    let mut entries = Vec::new();
    if let Ok(dir) = fs::read_dir(&po_dir) {
        for entry in dir.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("po") {
                continue;
            }
            let lang = path.file_stem().unwrap().to_string_lossy().into_owned();
            println!("cargo:rerun-if-changed={}", path.display());
            entries.push(format!("({lang:?}, include_str!({:?})),", path.display().to_string()));
        }
    }
    entries.sort();
    let out = Path::new(&env::var("OUT_DIR").unwrap()).join("catalogs.rs");
    fs::write(out, format!("const CATALOGS: &[(&str, &str)] = &[{}];\n", entries.join("\n"))).unwrap();
}
