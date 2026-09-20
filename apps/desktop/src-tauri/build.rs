fn main() {
    tauri_build::build();
    // tauri-build links its resource object (icons + the Windows application
    // manifest) into the BIN only. The lib's unit-test binaries import
    // comctl32 v6 symbols through tao/tauri (`TaskDialogIndirect`), which the
    // loader resolves only under that manifest: without it every unit-test
    // binary dies at load with STATUS_ENTRYPOINT_NOT_FOUND. Cargo has no
    // link-arg form for a lib's unit tests (`-tests` means a tests/ target),
    // and the all-targets form would link the resource into the bin twice
    // (LNK1123), so the tests opt in through the environment:
    //   RILLIO_LINK_TEST_MANIFEST=1 cargo test --lib
    println!("cargo:rerun-if-env-changed=RILLIO_LINK_TEST_MANIFEST");
    #[cfg(windows)]
    if std::env::var_os("RILLIO_LINK_TEST_MANIFEST").is_some_and(|v| !v.is_empty()) {
        if let Ok(out_dir) = std::env::var("OUT_DIR") {
            let resource = std::path::Path::new(&out_dir).join("resource.lib");
            if resource.is_file() {
                println!("cargo:rustc-link-arg={}", resource.display());
            }
        }
    }
}
