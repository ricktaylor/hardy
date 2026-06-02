use bytes::Bytes;
use hardy_bpv7::{bpsec::key, checks, parse, rewrite};
use serde_json::json;
use std::collections::{HashMap, HashSet};

/// Local parse pipeline for the fuzz harness, exercising the bpv7 parser:
/// classify A1/A2/A3 (with the §A delete-block list), `checks::verify`
/// (§B decrypt-and-validate BCB-covered BIBs, §C8 decrypt BCB-protected
/// extension blocks, §C7 verify all BIBs), §D decode the extension-block
/// fields it can (skipping anything that didn't decrypt), then
/// `apply_rewrites` (§E) for the rewrite chunks.
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
#[allow(clippy::result_large_err)]
fn parse_and_rewrite(
    data: &[u8],
    keys: &key::KeySet,
) -> Result<Option<Vec<hardy_bpv7::editor::Chunk>>, hardy_bpv7::Error> {
    let parse::Parsed {
        data,
        mut bundle,
        bcbs: bcb_ops,
        bibs: mut bib_ops,
    } = parse::parse(Bytes::copy_from_slice(data))?;

    // §A — classify (Unsupported errors propagate); collect deletables.
    let a1 = checks::classify_unrecognised_blocks(&bundle.blocks, &[])?;
    let _ = checks::classify_unsupported_bcbs(&bundle.blocks, &bcb_ops)?;
    let a3 = checks::classify_unsupported_bibs(&bundle.blocks, &bib_ops)?;
    let mut to_remove: HashSet<u64> = HashSet::new();
    to_remove.extend(a1.deletable);
    for n in &a3.deletable {
        to_remove.insert(*n);
        bib_ops.remove(n);
    }

    // §B + §C8 + §C7 — composed keyed verification, to exercise the
    // parser's decrypt/verify paths. The fuzzer is not a security acceptor,
    // so it enforces no NoKey/decrypt-failure policy: the returned
    // `VerifyFacts` (which blocks were NoKey / failed) are ignored, and
    // anything that did decrypt lands in `decrypted` for §D to decode
    // below. A genuine structural or BIB-verify error still propagates as a
    // parse failure.
    let mut decrypted = HashMap::new();
    let to_update: HashMap<u64, Vec<u8>> = HashMap::new();
    checks::verify(
        &data,
        keys,
        &mut bundle.blocks,
        &bcb_ops,
        &mut bib_ops,
        &mut decrypted,
        &to_update,
    )?;

    // §D — extension-field extraction + canonical re-emit queueing.
    let mut to_update = to_update;
    for (n, payload) in extract_canonical_rewrites(&data, &bundle.blocks, &decrypted)? {
        to_update.insert(n, payload);
    }

    if to_update.is_empty() && to_remove.is_empty() {
        return Ok(None);
    }

    // §E — apply rewrites; discard the post-rewrite Bundle (fuzz only
    // needs the chunks for its convergence replay).
    rewrite::apply_rewrites(&data, &bundle, keys, to_update, to_remove)
        .map(|opt| opt.map(|(_b, chunks)| chunks))
}

fn parse_exact<T>(data: &[u8], field: &'static str) -> Result<T, hardy_bpv7::Error>
where
    T: hardy_cbor::decode::FromCbor,
    T::Error: From<hardy_cbor::decode::Error> + Into<Box<dyn core::error::Error + Send + Sync>>,
{
    match hardy_cbor::decode::parse::<(T, usize)>(data) {
        Err(e) => Err(hardy_bpv7::Error::InvalidField {
            field,
            source: e.into(),
        }),
        Ok((_, len)) if len != data.len() => Err(hardy_bpv7::Error::InvalidField {
            field,
            source: hardy_bpv7::Error::AdditionalData.into(),
        }),
        Ok((t, _)) => Ok(t),
    }
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
    #[ignore] // Post-mortem debug test — run explicitly with `cargo test -- --ignored`
    fn test() {
        if let Ok(mut file) =
            std::fs::File::open("./artifacts/bundle/crash-effffdc7a8837e1dc7225d82466f3f068508a79a")
        {
            let mut buffer = Vec::new();
            if file.read_to_end(&mut buffer).is_ok() {
                test_bundle(&buffer);
            }
        }
    }

    #[test]
    #[ignore] // Post-mortem debug test — run explicitly with `cargo test -- --ignored`
    fn test_all() {
        match std::fs::read_dir("./corpus/bundle") {
            Err(e) => {
                eprintln!(
                    "Failed to open dir: {e}, curr dir: {}",
                    std::env::current_dir().unwrap().display()
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
                            test_bundle(&buffer);
                        }
                    }
                }
            }
        }
    }
}
