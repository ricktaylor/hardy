fn main() {
    println!("cargo::rustc-check-cfg=cfg(fuzzing)");

    built::write_built_file().expect("Failed to acquire build-time information");
}
