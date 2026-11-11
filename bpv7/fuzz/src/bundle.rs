use std::collections::{HashMap, HashSet};

use bytes::Bytes;
use hardy_bpv7::{
    bpsec::{Role, Roles, SecurityServices, edit::BPSecEditor, key},
    editor::Editor,
    parse, rewrite,
};
use serde_json::json;

/// Local parse pipeline for the fuzz harness, exercising the bpv7 parser:
/// classify A1/A2/A3 (with the §A delete-block list), the receiver's keyed
/// pass (`Parsed::check`: §B decrypt-and-validate BCB-covered BIBs, §C8
/// decrypt BCB-protected extension blocks, §C7 verify all BIBs), §D decode
/// the extension-block fields it can (skipping anything
/// that didn't decrypt), then `apply_rewrites` (§E) for the rewrite chunks.
///
/// This is **not** a security acceptor: it enforces no NoKey /
/// decrypt-failure policy. The goal is to drive the parse/decode paths and
/// the rewrite-convergence property, not to make accept/reject decisions.
///
/// Returns:
/// * `Ok(Some(chunks))` when blocks were removed or canonical re-emits
///   were queued — `chunks` is the rewritten wire stream the caller
///   flattens for the convergence replay.
/// * `Ok(None)` when the bundle parsed cleanly with no rewrites needed.
/// * `Err(err)` for any parse / validation failure.
fn parse_and_rewrite(
    data: &[u8],
    keys: &key::KeySet,
) -> Result<Option<Vec<hardy_bpv7::editor::Chunk>>, Box<dyn std::error::Error>> {
    let mut parsed = parse::parse(Bytes::copy_from_slice(data))?;

    // §A — the parser's classification (a deletion verdict propagates);
    // collect deletables.
    if let Some(error) = parsed.classification.delete_bundle.take() {
        return Err(error.into());
    }
    let mut to_remove: HashSet<u64> = HashSet::new();
    to_remove.extend(parsed.classification.unrecognised_deletable.iter().copied());
    to_remove.extend(parsed.classification.bib_deletable.iter().copied());

    // The composed keyed verification, to exercise the parser's
    // decrypt/verify paths. The fuzzer is not a security acceptor, so it
    // enforces no NoKey/failure policy: the returned `Facts` are ignored,
    // and anything that did decrypt lands in `decrypted` for §D to decode
    // below. A genuine structural error still propagates as a parse
    // failure.
    let to_update: HashMap<u64, Vec<u8>> = HashMap::new();
    let per_hop: HashMap<u64, Roles> = parsed
        .bundle
        .blocks
        .iter()
        .filter(|(_, b)| {
            matches!(
                b.block_type,
                hardy_bpv7::block::Type::PreviousNode
                    | hardy_bpv7::block::Type::BundleAge
                    | hardy_bpv7::block::Type::HopCount
            )
        })
        .map(|(&n, _)| (n, Roles::each(Role::Acceptor, SecurityServices::BOTH)))
        .collect();
    parsed.check(
        keys,
        &per_hop,
        Roles::each(Role::Verifier, SecurityServices::INTEGRITY),
    )?;

    // The destination's acceptance of every BCB, on a scratch copy, to
    // exercise the decrypt path, the coupling and the checks a decrypted BIB
    // gets; its failures are the bundle's, so they are ignored.
    let mut editor = Editor::new(&parsed.bundle, &parsed.data).with_key_source(keys);
    for (&n, block) in &parsed.bundle.blocks {
        if block.bcb.is_some() {
            editor = match editor.remove_encryption(n) {
                Ok(editor) | Err((editor, _)) => editor,
            };
        }
    }
    let _ = editor.rebuild();

    // §D — extension-field extraction + canonical re-emit queueing.
    let mut to_update = to_update;
    for (n, payload) in extract_canonical_rewrites(
        &parsed.data,
        &parsed.bundle.blocks,
        &parsed.decrypted.plaintext,
    )? {
        to_update.insert(n, payload);
    }

    if to_update.is_empty() && to_remove.is_empty() {
        return Ok(None);
    }

    // §E — apply rewrites; discard the post-rewrite Bundle (fuzz only
    // needs the chunks for its convergence replay).
    Ok(rewrite::apply_rewrites(
        Editor::new(&parsed.bundle, &parsed.data).with_key_source(keys),
        to_update,
        to_remove,
    )?
    .map(|(_, chunks)| chunks))
}

fn parse_exact<T>(data: &[u8], field: &'static str) -> Result<T, hardy_bpv7::Error>
where
    T: hardy_cbor::decode::FromCbor,
    T::Error: From<hardy_cbor::decode::Error>,
    hardy_bpv7::Error: From<T::Error>,
{
    hardy_cbor::decode::parse_exact::<T>(data).map_err(|e| hardy_bpv7::Error::InvalidField {
        field,
        source: Box::new(e.into()),
    })
}

/// §D decode step: decode every known extension block
/// (PreviousNode / BundleAge / HopCount) from its decrypted-or-wire bytes
/// — exercising the decoders and surfacing malformed-body errors — and
/// return the `(block_number, canonical_payload)` re-emits for any
/// non-shortest *plaintext* PreviousNode/HopCount (the only ones that can
/// be canonically re-emitted). Blocks that didn't decrypt are skipped.
fn extract_canonical_rewrites<V: AsRef<[u8]>>(
    data: &[u8],
    blocks: &HashMap<u64, hardy_bpv7::block::Block>,
    decrypted: &HashMap<u64, V>,
) -> Result<Vec<(u64, Vec<u8>)>, hardy_bpv7::Error> {
    use hardy_bpv7::block::Type;
    let mut rewrites = Vec::new();

    let candidates: Vec<(u64, Type)> = blocks
        .iter()
        .filter_map(|(&n, b)| {
            matches!(
                b.block_type,
                Type::PreviousNode | Type::BundleAge | Type::HopCount
            )
            .then_some((n, b.block_type))
        })
        .collect();

    for (n, block_type) in candidates {
        let b = blocks.get(&n).expect("filtered above");
        let is_encrypted = b.bcb.is_some();
        let payload: Option<&[u8]> = if let Some(p) = decrypted.get(&n) {
            Some(p.as_ref())
        } else if is_encrypted {
            None
        } else {
            // `data` is the complete in-memory bundle from `parse::parse`.
            b.payload(data)
        };
        let Some(payload) = payload else { continue };

        match block_type {
            Type::PreviousNode => {
                let (v, shortest) =
                    parse_exact::<(hardy_bpv7::eid::Eid, bool)>(payload, "Previous Node Block")?;
                if !shortest && !is_encrypted {
                    rewrites.push((n, hardy_cbor::encode::emit(&v).0));
                }
            }
            Type::BundleAge => {
                let _ =
                    parse_exact::<hardy_bpv7::bundle_age::BundleAge>(payload, "Bundle Age Block")?;
            }
            Type::HopCount => {
                let (v, shortest) = parse_exact::<(hardy_bpv7::hop_info::HopInfo, bool)>(
                    payload,
                    "Hop Count Block",
                )?;
                if !shortest && !is_encrypted {
                    rewrites.push((n, hardy_cbor::encode::emit(&v).0));
                }
            }
            _ => unreachable!("filtered above"),
        }
    }
    Ok(rewrites)
}

pub fn test_bundle(orig_data: &[u8]) {
    static KEYS: std::sync::OnceLock<key::KeySet> = std::sync::OnceLock::new();

    let keys = KEYS.get_or_init(|| {
        serde_json::from_value(json!({
            "keys": [
                {
                    "kid": "ipn:2.1",
                    "kty": "oct",
                    "alg": "HS512",
                    "key_ops": ["verify"],
                    "k": "GisaKxorGisaKxorGisaKw"
                },
                {
                    "kid": "ipn:2.1",
                    "kty": "oct",
                    "alg": "A128KW",
                    "key_ops": ["unwrapKey", "decrypt"],
                    "k": "YWJjZGVmZ2hpamtsbW5vcA"
                },
                {
                    "kid": "ipn:3.0",
                    "kty": "oct",
                    "alg": "HS256",
                    "key_ops": ["verify"],
                    "k": "GisaKxorGisaKxorGisaKw"
                },
                {
                    "kid": "ipn:2.1",
                    "kty": "oct",
                    "alg": "dir",
                    "enc": "A128GCM",
                    "key_ops": ["decrypt"],
                    "k": "cXdlcnR5dWlvcGFzZGZnaA"
                },
                {
                    "kid": "ipn:2.1",
                    "kty": "oct",
                    "alg": "HS384",
                    "key_ops": ["verify"],
                    "k": "GisaKxorGisaKxorGisaKw"
                },
                {
                    "kid": "ipn:2.1",
                    "kty": "oct",
                    "enc": "A256GCM",
                    "key_ops": ["decrypt"],
                    "k": "cXdlcnR5dWlvcGFzZGZnaHF3ZXJ0eXVpb3Bhc2RmZ2g"
                }
            ]
        }))
        .unwrap()
    });

    // First parse: if the input gets rewritten, replay the rewrite back
    // back through the pipeline and assert it converges (i.e. the rewrite
    // output is itself Valid — no further rewriting, no error).
    if let Ok(Some(chunks)) = parse_and_rewrite(orig_data, keys) {
        let new_data = hardy_bpv7::editor::Chunk::flatten(chunks, orig_data);
        match parse_and_rewrite(&new_data, keys) {
            Ok(None) => {}
            Ok(Some(_)) => {
                eprintln!("Original: {orig_data:02x?}");
                eprintln!("Rewrite: {new_data:02x?}");
                panic!("Rewrite produced non-canonical results")
            }
            Err(error) => {
                eprintln!("Original: {orig_data:02x?}");
                eprintln!("Rewrite: {new_data:02x?}");
                panic!("Rewrite produced invalid results: {error}")
            }
        };
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::io::Read;

    #[test]
    #[ignore] // Post-mortem debug test: run explicitly with `cargo test -- --ignored`
    fn test() {
        const ARTIFACT: &str = "./artifacts/bundle/crash-effffdc7a8837e1dc7225d82466f3f068508a79a";

        // A missing artifact is a skip (it is not committed), but a
        // present-yet-unreadable one is a failure.
        let mut file = match std::fs::File::open(ARTIFACT) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("Skipping: crash artifact {ARTIFACT} not present");
                return;
            }
            Err(e) => panic!("Failed to open crash artifact {ARTIFACT}: {e}"),
        };
        let mut buffer = Vec::new();
        file.read_to_end(&mut buffer)
            .unwrap_or_else(|e| panic!("Failed to read crash artifact {ARTIFACT}: {e}"));
        test_bundle(&buffer);
    }

    #[test]
    #[ignore] // Post-mortem debug test: run explicitly with `cargo test -- --ignored`
    fn test_all() {
        const CORPUS_DIR: &str = "./corpus/bundle";

        // A missing corpus directory is a skip (it is not committed), but a
        // present-yet-unreadable directory or file is a failure.
        let dir = match std::fs::read_dir(CORPUS_DIR) {
            Ok(dir) => dir,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("Skipping: corpus directory {CORPUS_DIR} not present");
                return;
            }
            Err(e) => panic!("Failed to open corpus directory {CORPUS_DIR}: {e}"),
        };

        let mut tested = 0usize;
        for entry in dir {
            let path = entry.expect("Failed to read corpus directory entry").path();
            if path.is_file() {
                let mut buffer = Vec::new();
                std::fs::File::open(&path)
                    .and_then(|mut file| file.read_to_end(&mut buffer))
                    .unwrap_or_else(|e| panic!("Failed to read {}: {e}", path.display()));
                test_bundle(&buffer);
                tested += 1;
            }
        }
        if tested == 0 {
            eprintln!("Skipping: corpus directory {CORPUS_DIR} contains no files");
        }
    }
}
