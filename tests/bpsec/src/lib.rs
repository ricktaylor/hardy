use hardy_bpv7::block;
use hardy_bpv7::bundle;

#[derive(Debug, PartialEq, Eq)]
pub enum PolicyAction {
    Pass,
    Reject,
    RemoveBlock(u64),
}

pub fn check_required_bcb(bundle: &bundle::Bundle, target: u64) -> PolicyAction {
    match bundle.blocks.get(&target) {
        Some(blk) if blk.bcb.is_some() => PolicyAction::Pass,
        Some(_) => PolicyAction::Reject,
        None => PolicyAction::Reject,
    }
}

pub fn check_required_bib(bundle: &bundle::Bundle, target: u64) -> PolicyAction {
    match bundle.blocks.get(&target) {
        Some(blk) if blk.bib != block::BibCoverage::None => PolicyAction::Pass,
        Some(blk) if matches!(blk.block_type, block::Type::Payload | block::Type::Primary) => {
            PolicyAction::Reject
        }
        Some(_) => PolicyAction::RemoveBlock(target),
        None => PolicyAction::Reject,
    }
}
