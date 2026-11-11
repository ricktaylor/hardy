//! Fixtures shared by the bpa integration-test binaries. Lives under
//! `tests/common/` so cargo treats it as a module, not its own test binary.
//!
//! Different test binaries use different subsets, so unused helpers per
//! binary are expected.
#![allow(dead_code)]

use std::collections::HashMap;

use hardy_bpa::Bytes;
use hardy_bpv7::{
    bpsec::{self, Role, Roles, SecurityService, SecurityServices, checks::Checked, key::KeySet},
    parse,
};

// Every BIB operation in `data`, a complete bundle, checked with `keys` by
// bpv7's keyed verification over a fresh parse: an error for a failed check,
// and a panic should anything be deferred.
pub fn verify_all_bibs(data: &[u8], keys: &KeySet) -> Result<(), hardy_bpv7::Error> {
    let facts = parse::parse(Bytes::copy_from_slice(data))?.check(
        keys,
        &HashMap::new(),
        Roles::each(Role::Verifier, SecurityServices::INTEGRITY),
    )?;
    if facts
        .targets_of(SecurityService::Integrity, Checked::Failed)
        .next()
        .is_some()
    {
        return Err(bpsec::Error::IntegrityCheckFailed.into());
    }
    assert!(
        !facts.any(Checked::Deferred),
        "a complete buffer defers nothing"
    );
    Ok(())
}
