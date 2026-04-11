use super::*;
use std::io::Read;

#[test]
fn test() {
    if let Ok(mut file) =
        std::fs::File::open("./artifacts/bpa/oom-da39a3ee5e6b4b0d3255bfef95601890afd80709")
    {
        let mut buffer = Vec::new();
        if file.read_to_end(&mut buffer).is_ok() {
            send_random(&buffer);
        }
    }
}

#[test]
fn test_all() {
    match std::fs::read_dir("./corpus/bpa") {
        Err(e) => {
            eprintln!(
                "Failed to open dir: {e}, curr dir: {}",
                std::env::current_dir().unwrap().display()
            );
        }
        Ok(dir) => {
            let mut count = 0u64;
            for path in dir.flatten() {
                let path = path.path();
                if path.is_file()
                    && let Ok(mut file) = std::fs::File::open(&path)
                {
                    let mut buffer = Vec::new();
                    if file.read_to_end(&mut buffer).is_ok() {
                        send_random(&buffer);

                        count = count.saturating_add(1);
                    }
                }
            }
            tracing::info!("Processed {count} bundles");
        }
    }
}
