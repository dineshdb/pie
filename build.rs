fn main() {
    // apple_ai's build.rs sets rpath on the library, but the final binary also needs it.
    // Prefer /usr/lib/swift (system dyld cache on macOS 12+) to avoid duplicate class warnings.
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
}
