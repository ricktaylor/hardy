#[derive(Debug, Copy, Clone)]
pub enum BundleStatus {
    IngressPending,
    DispatchPending,
    ReassemblyPending,
    CollectionPending,
    ForwardPending,
    Waiting(time::OffsetDateTime),
    Tombstone,
}
