fn main() {
    let version = rustc_version::version().expect("Failed to get rustc version");
    println!("cargo:rustc-env=RUSTC_VERSION={version}");
}
