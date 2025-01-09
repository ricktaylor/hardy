use std::sync::atomic::{AtomicBool, Ordering};

pub struct ConnectedFlag(AtomicBool);

impl std::fmt::Debug for ConnectedFlag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.is_connected())
    }
}

impl std::default::Default for ConnectedFlag {
    fn default() -> Self {
        Self::new(false)
    }
}

impl ConnectedFlag {
    pub fn new(connected: bool) -> Self {
        Self(AtomicBool::new(connected))
    }

    pub fn is_connected(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    pub fn connect(&self) -> bool {
        match self
            .0
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::Acquire)
        {
            Ok(b) => b,
            Err(b) => b,
        }
    }

    pub fn disconnect(&self) -> bool {
        match self
            .0
            .compare_exchange(true, false, Ordering::SeqCst, Ordering::Acquire)
        {
            Ok(b) => b,
            Err(b) => b,
        }
    }
}
