#[derive(Debug, Clone)]
pub enum BundleStatus {
    IngressPending,
    DispatchPending,
    ReassemblyPending,
    CollectionPending,
    ForwardPending,
    ForwardAckPending(String, time::OffsetDateTime),
    Waiting(time::OffsetDateTime),
    Tombstone,
}
