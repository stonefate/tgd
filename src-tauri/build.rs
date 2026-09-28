fn main() {
    #[cfg(feature = "desktop")]
    tauri_build::build();

    #[cfg(feature = "server")]
    ensure_web_embed_dir();
}

#[cfg(feature = "server")]
fn ensure_web_embed_dir() {
    let manifest = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let web = manifest.join("../build");
    if !web.join("index.html").is_file() {
        let _ = std::fs::create_dir_all(&web);
        let _ = std::fs::write(
            web.join("index.html"),
            "<!doctype html><meta charset=utf-8><title>纸飞机下载器</title><p>run pnpm build</p>\n",
        );
    }
    println!("cargo:rerun-if-changed={}", web.display());
}
