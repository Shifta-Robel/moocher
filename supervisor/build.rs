// The UI is embedded from ui-dist/ (rust-embed); rebuild when its files change
// (the hashed asset names change on every UI build).
fn main() {
    println!("cargo:rerun-if-changed=ui-dist");
    if let Ok(rd) = std::fs::read_dir("ui-dist/assets") {
        for e in rd.flatten() {
            println!("cargo:rerun-if-changed={}", e.path().display());
        }
    }
}
