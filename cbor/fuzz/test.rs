#![cfg(test)]

use std::io::Read;

#[test]
fn test_all() {
    match std::fs::read_dir("./corpus/decode") {
        Err(e) => {
            eprintln!(
                "Failed to open dir: {e}, curr dir: {}",
                std::env::current_dir().unwrap().to_string_lossy()
            );
        }
        Ok(dir) => {
            for entry in dir.flatten() {
                let path = entry.path();
                if path.is_file()
                    && let Ok(mut file) = std::fs::File::open(&path)
                {
                    let mut buffer = Vec::new();
                    if file.read_to_end(&mut buffer).is_ok() {
                        _ = hardy_cbor::decode::try_parse_value(&buffer, |value, _, _| {
                            _ = format!("{value:?}");
                            Ok::<_, hardy_cbor::decode::Error>(())
                        });
                    }
                }
            }
        }
    }
}
