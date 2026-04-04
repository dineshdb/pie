// build.rs — compiles the vendored Swift bridge and links it into the binary.
use std::path::PathBuf;
use std::process::Command;

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() != "macos" {
        println!("cargo:warning=Swift bridge is macOS-only; skipping compilation");
        return;
    }

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR not set"));
    let crate_root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let swift_src = crate_root.join("swift").join("apple-ai.swift");
    assert!(
        swift_src.exists(),
        "Swift source not found at {}",
        swift_src.display()
    );

    // 1. Compile Swift → static library
    let lib_path = out_dir.join("libapple_ai_bridge.a");
    let status = Command::new("swiftc")
        .args(["-emit-library", "-static"])
        .args(["-target", "arm64-apple-macosx26.0"])
        .arg("-whole-module-optimization")
        .arg("-parse-as-library")
        .arg("-suppress-warnings")
        .arg(swift_src.to_str().unwrap())
        .args(["-o", lib_path.to_str().unwrap()])
        .status()
        .expect("Failed to invoke swiftc. Is Xcode 26 installed?");
    assert!(
        status.success(),
        "swiftc failed with exit code {:?}",
        status.code()
    );

    // 2. Tell Cargo where to find the library
    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=apple_ai_bridge");

    // 3. Link system frameworks
    for framework in &["Foundation", "FoundationModels"] {
        println!("cargo:rustc-link-lib=framework={}", framework);
    }

    // 4. Swift runtime
    let swiftc_path = Command::new("xcrun")
        .args(["-f", "swiftc"])
        .output()
        .expect("xcrun not found");
    let swiftc_str = String::from_utf8(swiftc_path.stdout).unwrap();
    let swift_lib_dir = PathBuf::from(swiftc_str.trim())
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("lib/swift/macosx");

    println!("cargo:rustc-link-search=native={}", swift_lib_dir.display());
    println!("cargo:rustc-link-lib=dylib=swiftCore");
    println!("cargo:rustc-link-lib=dylib=swift_Concurrency");

    // rpath — Xcode toolchain
    println!("cargo:rustc-link-arg=-rpath");
    println!("cargo:rustc-link-arg={}", swift_lib_dir.display());

    // rpath — system dyld cache (macOS 12+)
    println!("cargo:rustc-link-arg=-rpath");
    println!("cargo:rustc-link-arg=/usr/lib/swift");

    // 5. Rebuild if Swift source changes
    println!("cargo:rerun-if-changed={}", swift_src.display());
    println!("cargo:rerun-if-changed=build.rs");
}
