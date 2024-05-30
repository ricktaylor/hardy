#[derive(Debug, Clone)]
pub enum BundleStatus {
    IngressPending,
    DispatchPending,
    ReassemblyPending,
    CollectionPending,
    ForwardPending,
    ForwardAckPending(u32, time::OffsetDateTime),
    Waiting(time::OffsetDateTime),
    Tombstone,
}
