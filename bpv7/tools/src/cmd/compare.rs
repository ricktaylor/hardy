use super::*;

#[derive(Parser, Debug)]
#[command(
    about = "Compare two bundles for semantic equivalence",
    long_about = "Compare two bundles for semantic equivalence.\n\n\
        Compares parsed content, handling CBOR encoding freedoms \
        (definite vs indefinite length, block ordering, block number assignment, \
        ASB target/parameter/result ordering) transparently."
)]
pub struct Command {
    /// First bundle file, '-' for stdin
    bundle_a: io::Input,

    /// Second bundle file
    bundle_b: io::Input,
}

impl Command {
    pub fn exec(self) -> anyhow::Result<()> {
        let data_a = self.bundle_a.read_all()?;
        let data_b = self.bundle_b.read_all()?;

        let diffs = crate::compare::compare_bundles(&data_a, &data_b)
            .map_err(|e| anyhow::anyhow!("Failed to parse bundle: {e}"))?;

        if diffs.is_empty() {
            println!("Bundles are semantically equivalent.");
            Ok(())
        } else {
            for diff in &diffs {
                eprintln!("  - {diff}");
            }
            Err(anyhow::anyhow!("{} difference(s) found", diffs.len()))
        }
    }
}
