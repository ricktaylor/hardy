use super::*;

#[derive(Parser, Debug)]
#[command(
    about = "Check one or more bundles for validity",
    long_about = "Check one or more bundles for validity.\n\n\
        Validates that bundles are well-formed and conform to the BPv7 specification. \
        Reports any parsing errors or non-canonical encodings. Returns a non-zero exit \
        code if any bundles fail validation."
)]
pub struct Command {
    #[clap(flatten)]
    key_args: keys::KeySetLoaderArgs,

    /// The list of bundle files to validate, can include '-' to use stdin.
    files: Vec<io::Input>,
}

impl Command {
    pub fn exec(self) -> anyhow::Result<()> {
        if self.files.is_empty() {
            return Err(anyhow::anyhow!("No files to validate"));
        }

        let key_store: hardy_bpv7::bpsec::key::KeySet = self.key_args.try_into()?;

        let mut count_failed: usize = 0;
        for input in self.files {
            let bytes = input.read_all()?;

            // Structural parse + keyed BPSec validation, then check the
            // known extension blocks for canonical encoding (the only
            // other user-visible diagnostic this command reports).
            let result = parse_with_keys(bytes, &key_store)
                .and_then(|parsed| known_blocks_canonical(&parsed.data, &parsed.bundle.blocks));

            match result {
                Err(e) => {
                    eprintln!("{}: Failed to parse bundle: {e}", input.filepath());
                    count_failed += 1;
                }
                Ok(true) => {}
                Ok(false) => {
                    eprintln!(
                        "{}: Non-canonical, but semantically valid bundle",
                        input.filepath()
                    );
                    count_failed += 1;
                }
            }
        }

        if count_failed == 0 {
            Ok(())
        } else {
            Err(anyhow::anyhow!("{count_failed} files failed to validate"))
        }
    }
}

/// Check the known plaintext extension blocks (PreviousNode / BundleAge /
/// HopCount) for canonical encoding. Returns `Ok(true)` when all are
/// shortest-form, `Ok(false)` when any is non-canonical, and `Err` when a
/// body is malformed. Encrypted bodies (`b.bcb.is_some()`) are opaque and
/// skipped.
fn known_blocks_canonical(
    data: &[u8],
    blocks: &std::collections::HashMap<u64, hardy_bpv7::block::Block>,
) -> Result<bool, hardy_bpv7::Error> {
    for b in blocks.values() {
        if b.bcb.is_some() {
            continue;
        }
        // `data` is the full in-memory bundle from `parse_with_keys`.
        let Some(body) = b.payload(data) else {
            continue;
        };
        let shortest = match b.block_type {
            hardy_bpv7::block::Type::PreviousNode => {
                parse_exact::<(hardy_bpv7::eid::Eid, bool)>(body, "Previous Node Block")?.1
            }
            hardy_bpv7::block::Type::HopCount => {
                parse_exact::<(hardy_bpv7::hop_info::HopInfo, bool)>(body, "Hop Count Block")?.1
            }
            hardy_bpv7::block::Type::BundleAge => {
                // Always canonical; decoded only to reject a malformed body.
                parse_exact::<hardy_bpv7::bundle_age::BundleAge>(body, "Bundle Age Block")?;
                true
            }
            _ => true,
        };
        if !shortest {
            return Ok(false);
        }
    }
    Ok(true)
}
