use alloc::string::String;
use alloc::vec::Vec;

use hardy_bpv7::bpsec::key::{Key, KeySource, Operation};
use hardy_bpv7::eid::Eid;
use hardy_eid_patterns::EidPattern;

use crate::HashMap;

/// The BPA's role with respect to a security block (RFC 9172 Section 2.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SecurityRole {
    /// Verify BIBs but keep them. Ignore BCBs. Default for intermediate nodes.
    #[default]
    Verifier,
    /// Verify + remove BIBs. Decrypt + remove BCBs.
    Acceptor,
    /// Add BIB/BCB at egress.
    Source,
}

/// Binds an EID pattern to a set of key identifiers and a security role.
///
/// Keys are not split by integrity/confidentiality: the BPSec backend
/// selects the right key by matching the required operation against each
/// key's `key_ops` field.
#[derive(Debug, Clone)]
struct KeyBinding {
    pattern: EidPattern,
    role: SecurityRole,
    kids: Vec<String>,
}

/// A [`KeySource`] that selects keys by matching EIDs against EID patterns.
///
/// Bindings are sorted by specificity (most specific first).
/// On lookup, the first matching binding wins, then keys are filtered
/// by the requested operation against each key's `key_ops`.
#[derive(Debug, Clone)]
pub struct PatternKeySource {
    keys: HashMap<String, Key>,
    bindings: Vec<KeyBinding>,
}

impl PatternKeySource {
    /// Creates a new `PatternKeySource` from pre-loaded keys and bindings.
    ///
    /// - `keys`: keys indexed by `kid`
    /// - `bindings`: `(pattern, role, key_ids)` tuples,
    ///   sorted by specificity (highest first)
    pub fn new(
        keys: HashMap<String, Key>,
        mut bindings: Vec<(EidPattern, SecurityRole, Vec<String>)>,
    ) -> Self {
        Self::sort_bindings(&mut bindings);

        let bindings = bindings
            .into_iter()
            .map(|(pattern, role, kids)| KeyBinding {
                pattern,
                role,
                kids,
            })
            .collect();

        Self { keys, bindings }
    }

    /// Creates an empty source with no keys or bindings.
    pub fn empty() -> Self {
        Self {
            keys: HashMap::new(),
            bindings: Vec::new(),
        }
    }

    /// Returns the security role for the given EID, if any binding matches.
    pub fn role(&self, eid: &Eid) -> Option<SecurityRole> {
        self.bindings
            .iter()
            .find(|b| b.pattern.matches(eid))
            .map(|b| b.role)
    }

    /// Add a key. Replaces any existing key with the same `kid`.
    pub fn add_key(&mut self, kid: String, key: Key) {
        self.keys.insert(kid, key);
    }

    /// Remove a key by `kid`. Returns true if the key existed.
    pub fn remove_key(&mut self, kid: &str) -> bool {
        self.keys.remove(kid).is_some()
    }

    /// Add a binding. Inserts in specificity order.
    pub fn add_binding(&mut self, pattern: EidPattern, role: SecurityRole, kids: Vec<String>) {
        let score = pattern.specificity_score().unwrap_or(0);

        let pos = self
            .bindings
            .iter()
            .position(|b| b.pattern.specificity_score().unwrap_or(0) < score)
            .unwrap_or(self.bindings.len());

        self.bindings.insert(
            pos,
            KeyBinding {
                pattern,
                role,
                kids,
            },
        );
    }

    /// Remove all bindings matching the given pattern. Returns the number removed.
    pub fn remove_binding(&mut self, pattern: &EidPattern) -> usize {
        let before = self.bindings.len();
        self.bindings.retain(|b| &b.pattern != pattern);
        before - self.bindings.len()
    }

    fn sort_bindings(bindings: &mut [(EidPattern, SecurityRole, Vec<String>)]) {
        bindings.sort_by(|a, b| {
            let score_a = a.0.specificity_score().unwrap_or(0);
            let score_b = b.0.specificity_score().unwrap_or(0);
            score_b.cmp(&score_a)
        });
    }
}

impl KeySource for PatternKeySource {
    fn key<'a>(&'a self, source: &Eid, operations: &[Operation]) -> Option<&'a Key> {
        let binding = self.bindings.iter().find(|b| b.pattern.matches(source))?;

        // Find the first bound key whose key_ops supports any requested operation
        for kid in &binding.kids {
            if let Some(key) = self.keys.get(kid) {
                if let Some(key_ops) = &key.operations {
                    if operations.iter().any(|op| key_ops.contains(op)) {
                        return Some(key);
                    }
                }
            }
        }
        None
    }
}
