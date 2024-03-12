use base64::prelude::*;
use sha1::Digest;
use std::{
    env,
    io::{Read, Write},
};

fn main() {
    built::write_built_file().expect("Failed to acquire build-time information");

    gen_migrations("schemas/");
}

fn gen_migrations(src_dir: &str) {
    println!("cargo:rerun-if-changed={src_dir}");

    let out_dir = env::var("OUT_DIR").unwrap();
    let mut out = std::io::BufWriter::new(
        std::fs::File::create(
            [&out_dir, "migrations.rs"]
                .iter()
                .collect::<std::path::PathBuf>(),
        )
        .unwrap(),
    );
    let regex = regex::Regex::new(r"(\d+)_+.+").unwrap();

    let mut m = Vec::new();
    for entry in std::fs::read_dir(src_dir).unwrap().flatten() {
        if let Ok(filetype) = entry.file_type() {
            if filetype.is_file() {
                if let Some(c) = regex.captures(entry.file_name().to_str().unwrap()) {
                    let seq: u64 = atoi::atoi(c[0].as_bytes()).unwrap();
                    m.push((seq, entry.path().to_str().unwrap().to_string()));
                }
            }
        }
    }

    m.sort_by(|(a, _), (b, _)| a.cmp(b));

    out.write_all(b"[").unwrap();
    for (seq, file_path) in m {
        let mut in_buf = std::io::BufReader::new(std::fs::File::open(&file_path).unwrap());
        let mut data = Vec::new();

        in_buf.read_to_end(&mut data).unwrap();
        let hash = sha1::Sha1::digest(&data);

        out.write_fmt(format_args!(
            "({seq}u64,r\"{}\",\"{}\",",
            file_path,
            BASE64_STANDARD.encode(hash)
        ))
        .unwrap();
        out.write_all(r#"r#""#.as_bytes()).unwrap();
        out.write_all(&data).unwrap();
        out.write_all(r##""#),"##.as_bytes()).unwrap();
        out.write_all("\n".as_bytes()).unwrap();
    }
    out.write_all(b"]").unwrap();
}
