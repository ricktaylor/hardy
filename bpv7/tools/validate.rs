use super::*;

/// Holds the arguments for the `show` subcommand.
#[derive(Parser, Debug)]
#[command(about, long_about = None)]
pub struct Command {
    // Use #[command(flatten)] to include the --key argument
    #[command(flatten)]
    key_args: keys::KeyLoaderArgs,

    /// The list of bundle files to validate, '-' to use stdin.
    files: Vec<io::Input>,
}

pub fn exec(args: Command) -> anyhow::Result<()> {
    if args.files.is_empty() {
        return Err(anyhow::anyhow!("No files to validate"));
    }

    let key_store: hardy_bpv7::bpsec::key::KeySet = args.key_args.try_into()?;

    let mut count_failed: usize = 0;
    for input in args.files {
        let bundle = input
            .read_all()
            .map_err(|e| anyhow::anyhow!("Failed to read input from {}: {e}", input.filepath()))?;

        match hardy_bpv7::bundle::ParsedBundle::parse(&bundle, &key_store) {
            Err(e) => {
                eprintln!("{}: Failed to parse bundle: {e}", input.filepath());
                count_failed = count_failed.saturating_add(1);
            }
            Ok(hardy_bpv7::bundle::ParsedBundle { non_canonical, .. }) => {
                if non_canonical {
                    eprintln!(
                        "{}: Non-canonical, but semantically valid bundle",
                        input.filepath()
                    );
                    count_failed = count_failed.saturating_add(1);
                }
            }
        }
    }

    (count_failed == 0)
        .then_some(())
        .ok_or(anyhow::anyhow!("{count_failed} files failed to validate"))
}
