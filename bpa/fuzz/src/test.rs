use super::*;
use std::io::Read;

#[test]
fn test() {
    if let Ok(mut file) =
        std::fs::File::open("./artifacts/cla/crash-5943b7c21c186171effb01f9514dbe9302f2a606")
    {
        let mut buffer = Vec::new();
        if file.read_to_end(&mut buffer).is_ok() {
            send(&buffer);
        }
    }
}

#[test]
fn test_all() {
    match std::fs::read_dir("./corpus/cla") {
        Err(e) => {
            eprintln!(
                "Failed to open dir: {e}, curr dir: {}",
                std::env::current_dir().unwrap().to_string_lossy()
            );
        }
        Ok(dir) => {
            let mut count = 0u64;
            for entry in dir {
                if let Ok(path) = entry {
                    let path = path.path();
                    if path.is_file() {
                        if let Ok(mut file) = std::fs::File::open(&path) {
                            let mut buffer = Vec::new();
                            if file.read_to_end(&mut buffer).is_ok() {
                                send(&buffer);

                                count = count.saturating_add(1);
                            }
                        }
                    }
                }
            }
            tracing::info!("Processed {count} bundles");
        }
    }
}
