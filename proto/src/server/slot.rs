// What a registration hands its surface, held until the session ends.
//
// Written once, when the BPA registers the component, and read by every
// data-plane door thereafter. The CLA parks the sink together with the
// negotiated size cap, so a pair is published in one write and can
// never be read half-set.
//
// What an empty slot means is the surface's, not this type's: a door
// answers its own `Disconnected`, while teardown of a session that
// never registered simply has nothing to do.
//
// `T` is the already-shared form: an `Arc<dyn Sink>`, or a small
// `Clone` struct around one.

use hardy_async::sync::spin::Once;

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

    // What registration published, or `None` for a call that arrived
    // before it, or racing it.
    pub fn get(&self) -> Option<T> {
        self.0.get().cloned()
    }
}

impl<T: Clone> Default for Slot<T> {
    fn default() -> Self {
        Self::new()
    }
}
