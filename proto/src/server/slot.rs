// What a registration hands its surface, held until the session ends.
//
// Written once, when the BPA registers the component, and read by every
// data-plane door thereafter: the door either finds a registration or
// answers the `UNAVAILABLE` a call racing registration deserves. The
// CLA parks the sink together with the negotiated size cap, so a pair
// is published in one write and can never be read half-set.
//
// `T` is the already-shared form: an `Arc<dyn Sink>`, or a small
// `Clone` struct around one.

use hardy_async::sync::spin::Once;
use tonic::Status;

pub struct Slot<T>(Once<T>);

impl<T: Clone> Slot<T> {
    pub fn new() -> Self {
        Self(Once::new())
    }

    // Publishes what registration produced. The BPA registers exactly
    // once per component, so a second set is ignored.
    pub fn set(&self, registered: T) {
        self.0.call_once(|| registered);
    }

    // The registration for a data-plane door, or the unregistered status
    // a call arriving before (or racing) registration must answer.
    pub fn get(&self) -> Result<T, Status> {
        self.0
            .get()
            .cloned()
            .ok_or_else(|| Status::unavailable("Unregistered"))
    }

    // For the paths that tolerate its absence: tearing down a session
    // that never registered, and reading back what the BPA negotiated.
    pub fn peek(&self) -> Option<T> {
        self.0.get().cloned()
    }
}

impl<T: Clone> Default for Slot<T> {
    fn default() -> Self {
        Self::new()
    }
}
