use base64::prelude::*;
use sha1::Digest;
use std::io::{Read, Write};

fn main() {
    built::write_built_file().expect("Failed to acquire build-time information");

    gen_migrations("schemas/").expect("Failed to build migration info");
}

fn gen_migrations(src_dir: &str) -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed={src_dir}");

    let out_dir = std::env::var("OUT_DIR")?;
    let mut out = std::io::BufWriter::new(std::fs::File::create(
        [&out_dir, "migrations.rs"]
            .iter()
            .collect::<std::path::PathBuf>(),
    )?);
    let regex = regex::Regex::new(r"(\d+)_+.+")?;

    let mut m = Vec::new();
    for entry in std::fs::read_dir(src_dir)?.flatten() {
        if let Ok(filetype) = entry.file_type() {
            if filetype.is_file() {
                if let Some(c) = regex.captures(&entry.file_name().to_string_lossy()) {
                    let seq: u64 = c.get(1).unwrap().as_str().parse().unwrap();
                    m.push((seq, entry.path()));
                }
            }
        }
    }

    m.sort_by(|(a, _), (b, _)| a.cmp(b));

    out.write_all(b"[")?;
    for (seq, file_path) in m {
        let mut in_buf = std::io::BufReader::new(std::fs::File::open(&file_path)?);
        let mut data = Vec::new();

        in_buf.read_to_end(&mut data)?;
        write!(
            out,
            "({seq}u64,r###\"{}\"###,\"{}\",",
            file_path.to_string_lossy(),
            BASE64_STANDARD_NO_PAD.encode(sha1::Sha1::digest(&data))
        )?;
        out.write_all("r###\"".as_bytes())?;
        out.write_all(&data)?;
        out.write_all("\"###),\n".as_bytes())?;
    }
    out.write_all(b"]")?;
    Ok(())
}
