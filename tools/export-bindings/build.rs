//! Windows（MSVC）上给生成程序嵌入应用清单。放在 src-tauri 里时由 tauri-build 自动嵌入，挪出来后要自己加。

fn main() {
    let windows = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");
    let msvc = std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    if windows && msvc {
        let manifest = std::path::Path::new(
            &std::env::var("CARGO_MANIFEST_DIR").expect("缺少 CARGO_MANIFEST_DIR"),
        )
        .join("windows-app-manifest.xml");
        println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}",
            manifest.display()
        );
    }
    println!("cargo:rerun-if-changed=windows-app-manifest.xml");
}
