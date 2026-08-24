use hardy_bpa::policy::{EgressPolicy as _, null_policy};

// NullPolicy.classify() should always return None (single-queue FIFO).
#[test]
fn test_flow_classification() {
    let policy = null_policy::EgressPolicy::new();

    // queue_count is 0 for null policy
    assert_eq!(policy.queue_count(), 0);

    // Any flow label maps to None (default queue)
    assert_eq!(policy.classify(None), None);
    assert_eq!(policy.classify(Some(0)), None);
    assert_eq!(policy.classify(Some(42)), None);
    assert_eq!(policy.classify(Some(u32::MAX)), None);
}

// Queue indices beyond queue_count should be treated as invalid.
// For NullPolicy with 0 queues, classify always returns None.
#[test]
fn test_queue_bounds() {
    let policy = null_policy::EgressPolicy::new();

    // With queue_count=0, there's only the default (None) queue
    let count = policy.queue_count();
    assert_eq!(count, 0);

    // Verify classify never returns a queue index >= queue_count
    for label in [None, Some(0), Some(1), Some(100), Some(u32::MAX)] {
        let queue = policy.classify(label);
        if let Some(idx) = queue {
            assert!(idx < count, "Queue index {idx} exceeds queue_count {count}");
        }
    }
}
