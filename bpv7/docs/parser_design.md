# bpv7 Parse & Validation Pipeline Design

How `hardy-bpv7` turns received wire bytes into a validated, optionally-rewritten bundle, and why the work is split the way it is. This is also the canonical reference for the `§A`–`§E` section labels the code carries (`A1`/`A2`/`A3`, `B`/`B6`, `C7`, `C8`, `D`, `E`).

## Design Goals

The pipeline turns untrusted wire bytes into a validated bundle while keeping three concerns strictly separated, so the BPA ingress path, the CLI tools, the fuzz harness, and the integration tests all share one implementation rather than each re-deriving it.

- **Wire truth vs operational meaning.** bpv7 decides only what the bytes *are* — structure and BPSec validity. What they *mean* operationally — hop-limit enforcement, which failures should reject a bundle, whether to accept or merely verify a security operation — is the consumer's business. Keeping that line sharp is what lets bpv7 be reused unchanged by very different callers.
- **Mechanism, not policy.** The validation helpers are fact-producers: they report outcomes (decrypted / NoKey / decrypt-failed, block coverage, unsupported-block classification) and never themselves decide accept-or-reject. The consumer layers policy on top of the facts. Because the helpers are also non-mutating, the (read-only) ones can be run in parallel by a filter framework.
- **Composability.** Each stage is a small helper a caller can use on its own, with one composed entry point — `verify` — for the common keyed pass, so callers get the load-bearing ordering for free rather than re-stitching it (and drifting).

## Architecture Overview

A bundle moves through three layers, lowest to highest. The lower two are pure bpv7 mechanism; policy sits above them in the consumer, primarily `hardy_bpa::bp7_parse`.

1. **Structural parse** (`parse::parse`) — keyless. Decodes the CBOR into a `Bundle` (primary block plus a map of canonical blocks) and the BPSec OperationSets. Having no keys, it cannot read the target list of a BCB-encrypted BIB, so it conservatively marks every block such a BIB *might* cover as "maybe covered" (see *Coverage stamping*).
2. **Keyed validation** (`checks`) — the §A–§C steps: keyless classification (§A) plus the keyed decrypt/verify steps (§B, §C8, §C7), composed by `checks::verify`.
3. **Rewrite application** (`rewrite::apply_rewrites`) — §E: applies block removals and canonical re-emits through the BPSec-aware editor cascade, returning the new wire bytes.

The §-sections, in execution order:

| § | Stage | Keys? | What it establishes |
|---|---|---|---|
| A1 / A2 / A3 | classify unrecognised blocks / unsupported BCBs / unsupported BIBs | no | which blocks this node cannot process (delete/report facts; a hard error on `delete_bundle_on_failure`) |
| B | decrypt & validate BCB-encrypted BIBs | yes | the real targets of encrypted BIBs, replacing "maybe" coverage |
| B6 | resolve residual "maybe" coverage | no | finalised coverage, once every encrypted BIB is accounted for |
| C8 | decrypt BCB-protected `PreviousNode` / `BundleAge` / `HopCount` | yes | plaintext extension-block bodies (or NoKey / decrypt-failed) |
| C7 | verify every BIB | yes | the integrity of each covered block |
| D | decode extension-block fields | reads §C8 output | typed field values + canonical re-emit candidates (in the consumer) |
| E | apply rewrites | yes | the rewritten wire bytes |

The letters are stage *names*, not a strict numeric sequence — the numbering is historical (from the original step list), which is why `C8` runs before `C7`.

## Key Design Decisions

### Policy-free fact-producers

The `checks` helpers return facts rather than verdicts because the same security outcome means different things to different callers: at BPA ingress a wrong key may mean "drop the corrupt block", at delivery it means "drop the bundle", and in the fuzz harness it means nothing at all. Encoding any one of those choices into bpv7 would force the others to fight it. Returning the raw outcome (and leaving `accept`/`reject`/`ignore` to the caller) is also what enables the BPA's Verifier/Acceptor role model, and — because a fact-producer need not mutate the bundle — lets read-only checks run concurrently.

### Eager "maybe" coverage, resolved after decryption

The structural parser cannot read a BCB-encrypted BIB's target list, so at parse time it genuinely does not know which blocks that BIB covers. Rather than fail, guess, or demand keys at the structural layer, it marks every candidate `BibCoverage::Maybe`; §B then decrypts the BIB and rewrites those marks to the real `BibCoverage::Some(bib)`, and §B6 collapses any leftover `Maybe` to `None` once all encrypted BIBs are resolved. The practical consequence is a safety rule: a `Maybe` block must not be removed before resolution, because a still-hidden BIB might depend on it.

### Execution order §B → §C8 → §C7

Verification (§C7) reads plaintext recovered earlier: encrypted BIB bodies from §B and encrypted extension blocks from §C8. Because a BIB may cover an extension block, decryption has to precede verification — so `verify` always runs the steps in this order and threads one shared decrypted-plaintext map through them. This ordering is the main reason the three keyed steps are composed into a single entry point rather than left to each call site.

### Extension-field decode (§D) lives in the BPA, not bpv7

`PreviousNode` / `BundleAge` / `HopCount` values are operational meaning, not wire truth, so they are decoded where they are consumed (`bpa/src/bp7_parse.rs`) rather than cached permanently on the bpv7 `Bundle`. Keeping them out of bpv7 avoids entrenching a typed field cache that the streaming refactor intends to remove, and matches its decode-on-demand seam (byte availability differs by phase: an in-memory header buffer early, a storage stream later).

### The keyless / keyed seam

Classification (§A) and the per-OperationSet structural checks (`check_bib` / `check_bcb`) need no keys and are shared with the structural parser; decrypt/verify (§B/§C7/§C8) and the edit cascade need a `KeySource`. Splitting helpers at that key boundary means a caller can run the keyless checks with no key material at all, and the structural parser and the keyed pass can share one source of truth for the structural rules.

## Integration

bpv7 supplies the mechanism; `hardy_bpa::bp7_parse` composes it with BPA policy — the NoKey disposition, which failures reject, the §D field cache, and the status-report reason mapping. The bpv7 CLI tools (`bpv7/tools`), the fuzz harness (`bpv7/fuzz`), and the integration tests (`bpv7/tests`) compose the same sections for their own purposes. The BPSec edit cascade that §E drives lives in `bpsec::edit`, layered over the keyless `editor`.

## Standards Compliance

- **RFC 9171** — bundle and block structure, and the canonical-form rules (deterministic CBOR with indefinite-length items permitted) the structural parse enforces.
- **RFC 9172** — BPSec processing. §3.8 (a BCB targeting a BIB must share a target with it) and §3.9 (a BIB whose target is BCB-encrypted must itself be BCB-encrypted) are enforced per-OperationSet in §B and the structural checks; the §5.1.1 failure handling is a *policy* applied by the consumer to the §B/§C8 facts, not by bpv7.
- **RFC 9173** — the BCB-AES-GCM and BIB-HMAC-SHA2 security contexts whose decrypt/verify operations §B, §C7, and §C8 drive.

## Testing

- `bpv7/tests/parse.rs` — the structural parser's public-API acceptance and rejection decisions on wire bytes (truncation, trailing data, canonical-form rules, CRC types).
- `bpv7/tests/checks.rs` — §A–§E pipeline composition and the BPSec removal cascade.
- `bpv7/tests/rfc9173.rs` — RFC 9173 security-context test vectors (sign / encrypt / verify / decrypt).
- `bpv7/fuzz` — structural and validation fuzzing with a rewrite-convergence assertion.
