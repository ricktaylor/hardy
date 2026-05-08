use super::*;
use hardy_bpv7::*;
use hardy_bpv7_tools::compare::compare_bundles;

#[derive(Parser, Debug)]
#[command(
    about = "Compare two bundles for semantic equivalence",
    long_about = "Compare two bundles for semantic equivalence.\n\n\
        Byte-compares whole blocks where the encoding is deterministic \
        (primary, payload, regular extension blocks). Falls back to semantic \
        comparison for security blocks where the RFC allows non-deterministic \
        encodings (target array ordering per RFC 9172 Section 3.6, random IV \
        per RFC 9173 Section 4.3.1).\n\n\
        If keys are provided, encrypted payloads are decrypted before comparison."
)]
pub struct Command {
    #[clap(flatten)]
    key_args: keys::KeySetLoaderArgs,

    /// First bundle file, '-' for stdin
    bundle_a: io::Input,

    /// Second bundle file
    bundle_b: io::Input,
}

impl Command {
    pub fn exec(self) -> anyhow::Result<()> {
        let key_store: bpsec::key::KeySet = self.key_args.try_into()?;
        let data_a = self.bundle_a.read_all()?;
        let data_b = self.bundle_b.read_all()?;

        let parsed_a = bundle::ParsedBundle::parse_with_keys(&data_a, &key_store)
            .map_err(|e| anyhow::anyhow!("Failed to parse bundle A: {e}"))?;
        let parsed_b = bundle::ParsedBundle::parse_with_keys(&data_b, &key_store)
            .map_err(|e| anyhow::anyhow!("Failed to parse bundle B: {e}"))?;

        let diffs = compare_bundles(
            &parsed_a.bundle,
            &data_a,
            &parsed_b.bundle,
            &data_b,
            &key_store,
        );

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
