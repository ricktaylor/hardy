// Initialise all TCPCLv4 metric descriptions.
//
// Call once during CLA startup. Descriptions are registered with the global
// `metrics` recorder so that OTEL instruments are created with correct units
// and descriptions on first use.
pub fn init() {
    // -- Sessions --
    metrics::describe_counter!(
        "tcpclv4.session.established",
        metrics::Unit::Count,
        "TCPCLv4 sessions established"
    );
    metrics::describe_counter!(
        "tcpclv4.session.terminated",
        metrics::Unit::Count,
        "TCPCLv4 sessions terminated (by reason)"
    );

    // -- Throughput --
    metrics::describe_counter!(
        "tcpclv4.session.bytes.sent",
        metrics::Unit::Bytes,
        "Total bytes written to TCP connections"
    );
    metrics::describe_counter!(
        "tcpclv4.session.bytes.received",
        metrics::Unit::Bytes,
        "Total bytes read from TCP connections"
    );

    // -- Transfers --
    metrics::describe_counter!(
        "tcpclv4.transfers.sent",
        metrics::Unit::Count,
        "Complete bundles forwarded to peers"
    );
    metrics::describe_counter!(
        "tcpclv4.transfers.received",
        metrics::Unit::Count,
        "Complete bundles received from peers"
    );
    metrics::describe_counter!(
        "tcpclv4.transfers.refused",
        metrics::Unit::Count,
        "Transfers refused by peer (by reason)"
    );

    // -- Segments --
    metrics::describe_counter!(
        "tcpclv4.segments.sent",
        metrics::Unit::Count,
        "XFER_SEGMENT messages sent"
    );
    metrics::describe_counter!(
        "tcpclv4.segments.received",
        metrics::Unit::Count,
        "XFER_SEGMENT messages received"
    );

    // -- Connection Pool --
    metrics::describe_gauge!(
        "tcpclv4.pool.idle",
        metrics::Unit::Count,
        "Idle connections available for reuse"
    );
    metrics::describe_counter!(
        "tcpclv4.pool.reused",
        metrics::Unit::Count,
        "Connections reused from the idle pool"
    );
}
