use alloc::string::String;
use alloc::vec::Vec;

use hardy_bpv7::bpsec::key::{Key, KeySource, Operation};
use hardy_bpv7::eid::Eid;
use hardy_eid_patterns::EidPattern;

use crate::HashMap;

/// Binds an EID pattern to key identifiers for integrity and confidentiality.
struct KeyBinding {
    pattern: EidPattern,
    integrity_kid: Option<String>,
    confidentiality_kid: Option<String>,
}

/// A [`KeySource`] that selects keys by matching source EIDs against EID patterns.
///
/// Bindings are sorted by specificity at construction time (most specific first).
/// On lookup, the first matching binding wins.
pub struct PatternKeySource {
    keys: HashMap<String, Key>,
    bindings: Vec<KeyBinding>,
}

impl PatternKeySource {
    /// Creates a new `PatternKeySource` from pre-loaded keys and bindings.
    ///
    /// - `keys`: keys indexed by `kid`
    /// - `bindings`: `(pattern, integrity_kid, confidentiality_kid)` tuples,
    ///   will be sorted by specificity (highest first)
    pub fn new(
        keys: HashMap<String, Key>,
        mut bindings: Vec<(EidPattern, Option<String>, Option<String>)>,
    ) -> Self {
        bindings.sort_by(|a, b| {
            let score_a = a.0.specificity_score().unwrap_or(0);
            let score_b = b.0.specificity_score().unwrap_or(0);
            score_b.cmp(&score_a)
        });

        let bindings = bindings
            .into_iter()
            .map(|(pattern, integrity_kid, confidentiality_kid)| KeyBinding {
                pattern,
                integrity_kid,
                confidentiality_kid,
            })
            .collect();

        Self { keys, bindings }
    }
}

impl KeySource for PatternKeySource {
    fn key<'a>(&'a self, source: &Eid, operations: &[Operation]) -> Option<&'a Key> {
        // Find the most specific binding matching this source EID
        let binding = self.bindings.iter().find(|b| b.pattern.matches(source))?;

        // Return the appropriate key based on requested operations
        for op in operations {
            match op {
                Operation::Sign | Operation::Verify => {
                    if let Some(k) = binding
                        .integrity_kid
                        .as_ref()
                        .and_then(|kid| self.keys.get(kid))
                    {
                        return Some(k);
                    }
                }
                Operation::Encrypt
                | Operation::Decrypt
                | Operation::WrapKey
                | Operation::UnwrapKey => {
                    if let Some(k) = binding
                        .confidentiality_kid
                        .as_ref()
                        .and_then(|kid| self.keys.get(kid))
                    {
                        return Some(k);
                    }
                }
                _ => {}
            }
        }
        None
    }
}
