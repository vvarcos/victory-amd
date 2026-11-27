//! Exports the DirectX 12 Agility SDK selection (see `main.rs`).
//!
//! Microsoft's deployment mechanism: the system's `d3d12.dll` reads the
//! executable's export table when it loads, and `D3D12SDKVersion` and
//! `D3D12SDKPath` (the two statics in `main.rs`) name the app-local
//! `D3D12Core.dll` instead of the one in System32. Rust exports nothing by
//! default, so the linker is told to export them as data.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let windows = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");
    let agility = std::env::var_os("CARGO_FEATURE_AGILITY_SDK").is_some();
    if windows && agility {
        println!("cargo:rustc-link-arg=/EXPORT:D3D12SDKVersion,DATA");
        println!("cargo:rustc-link-arg=/EXPORT:D3D12SDKPath,DATA");
    }
}
