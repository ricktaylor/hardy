//! The chunk size a session runs at, the rule for splitting one data-plane
//! transfer into chunks of it, and the two ends of a chunked transfer.

#[cfg(any(feature = "client", feature = "server"))]
mod receiver;
#[cfg(any(feature = "client", feature = "server"))]
mod sender;

#[cfg(any(feature = "client", feature = "server"))]
use core::iter::from_fn;

#[cfg(any(feature = "client", feature = "server"))]
use hardy_bpa::stream::Segment;
#[cfg(feature = "server")]
use tonic::Status;

use crate::MAX_MESSAGE_SIZE;

#[cfg(any(feature = "client", feature = "server"))]
pub(crate) use self::receiver::ChunkReceiver;
#[cfg(feature = "client")]
pub(crate) use self::sender::ChunkSender;
#[cfg(feature = "server")]
pub(crate) use self::{receiver::BoundedChunkReceiver, sender::BoundedChunkSender};

/// How a bounded transfer broke.
///
/// A stall carries no status, because the bound it outlasted names the
/// server's internals: the stage is for the session's log, and what the client
/// is told is the same whichever bound ran out, so both are the caller's to
/// make.
#[cfg(feature = "server")]
#[derive(Debug, thiserror::Error)]
pub(crate) enum TransferError {
    #[error("the client fell behind the bound on the transfer")]
    Stalled,
    #[error(transparent)]
    Failed(#[from] Status),
}

/// The chunk size a session runs at unless its client asked for less.
///
/// It is large enough to keep per-message overhead small and small enough for
/// transfers to interleave fairly on a shared HTTP/2 connection.
pub const DEFAULT_CHUNK_SIZE: usize = 1024 * 1024;

/// The largest chunk size a session may run at.
///
/// It is the largest whole multiple of [`DEFAULT_CHUNK_SIZE`] that leaves room
/// in one message for the rest of its fields, so that splitting a transfer at
/// it never leaves a ragged chunk against the default.
pub const MAX_CHUNK_SIZE: usize = (MAX_MESSAGE_SIZE / DEFAULT_CHUNK_SIZE - 1) * DEFAULT_CHUNK_SIZE;

const _: () = assert!(
    DEFAULT_CHUNK_SIZE <= MAX_CHUNK_SIZE && MAX_CHUNK_SIZE < MAX_MESSAGE_SIZE,
    "a chunk and the rest of its message must fit in one message"
);

/// The smallest chunk size a session may run at.
///
/// A chunk is what the per-message cost is charged against, and the rate rule
/// counts bytes rather than messages, so a client asking for a tiny chunk size
/// would turn one transfer into arbitrarily many messages for free.
#[cfg(any(feature = "client", feature = "server"))]
const MIN_CHUNK_SIZE: usize = 1024;

/// The chunk size one session runs at: the largest run of payload bytes either
/// end puts in one `chunk` or `last_chunk` message.
///
/// It lies between [`MIN_CHUNK_SIZE`] and [`MAX_CHUNK_SIZE`] by construction,
/// so nothing downstream re-checks it.
#[cfg(any(feature = "client", feature = "server"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChunkSize(usize);

#[cfg(any(feature = "client", feature = "server"))]
impl ChunkSize {
    /// Returns a chunk size of `size` bytes, or `None` if that is below
    /// [`MIN_CHUNK_SIZE`] or above [`MAX_CHUNK_SIZE`].
    ///
    /// This is where the range is enforced; everything holding a `ChunkSize`
    /// takes it as given.
    pub fn new(size: u64) -> Option<Self> {
        if !(MIN_CHUNK_SIZE as u64..=MAX_CHUNK_SIZE as u64).contains(&size) {
            return None;
        }
        // Narrowed only once in range, so a 64-bit size cannot wrap a 32-bit
        // `usize`.
        usize::try_from(size).ok().map(Self)
    }

    /// Returns the chunk size a session runs at, given the `max_chunk_size` its
    /// client asked for.
    ///
    /// A client that asks for nothing gets [`DEFAULT_CHUNK_SIZE`], and one that
    /// asks for more than that gets [`DEFAULT_CHUNK_SIZE`] too: the ask is a
    /// ceiling, never a floor.
    ///
    /// # Errors
    ///
    /// Returns `INVALID_ARGUMENT` if the ask is below [`MIN_CHUNK_SIZE`]. The
    /// server refuses the session rather than answering with a size above what
    /// was asked for.
    #[cfg(feature = "server")]
    pub fn negotiate(max_chunk_size: Option<u64>) -> Result<Self, Status> {
        let Some(asked) = max_chunk_size else {
            return Ok(Self::default());
        };
        Self::new(asked.min(DEFAULT_CHUNK_SIZE as u64)).ok_or_else(|| {
            Status::invalid_argument(format!(
                "Register.max_chunk_size is {asked} bytes, below the {MIN_CHUNK_SIZE} byte minimum"
            ))
        })
    }

    /// Returns the size in bytes.
    pub fn get(self) -> usize {
        self.0
    }
}

/// [`DEFAULT_CHUNK_SIZE`], which is what a session that asked for nothing runs
/// at.
#[cfg(any(feature = "client", feature = "server"))]
impl Default for ChunkSize {
    fn default() -> Self {
        Self(DEFAULT_CHUNK_SIZE)
    }
}

/// Returns the number of bytes a segment carries.
#[cfg(feature = "server")]
pub(crate) fn segment_len(segment: &Segment) -> u64 {
    let (Segment::Next(bytes) | Segment::Final(bytes)) = segment;
    bytes.len() as u64
}

/// Returns an iterator over the chunks of one segment, each of at most
/// `chunk_size` bytes.
///
/// Every chunk but the last is exactly `chunk_size` bytes, and only the last
/// chunk of a [`Segment::Final`] is itself `Final`. An empty intermediate
/// segment yields nothing, since an empty `chunk` would carry no information;
/// an empty final segment yields one empty `Final`, since that is the
/// end-of-transfer signal.
#[cfg(any(feature = "client", feature = "server"))]
pub(crate) fn chunks(segment: Segment, chunk_size: ChunkSize) -> impl Iterator<Item = Segment> {
    let (mut bytes, last) = match segment {
        Segment::Next(bytes) => (bytes, false),
        Segment::Final(bytes) => (bytes, true),
    };
    let mut done = bytes.is_empty() && !last;
    from_fn(move || {
        if done {
            return None;
        }
        let slice = bytes.split_to(bytes.len().min(chunk_size.get()));
        done = bytes.is_empty();
        Some(if last && done {
            Segment::Final(slice)
        } else {
            Segment::Next(slice)
        })
    })
}

#[cfg(all(test, any(feature = "client", feature = "server")))]
mod tests {
    use hardy_bpa::Bytes;
    #[cfg(feature = "server")]
    use tonic::Code;

    use super::*;

    #[test]
    #[cfg(feature = "server")]
    fn a_client_that_asks_for_nothing_runs_at_the_servers_chunk_size() {
        assert_eq!(
            ChunkSize::negotiate(None).unwrap().get(),
            DEFAULT_CHUNK_SIZE
        );
    }

    #[test]
    #[cfg(feature = "server")]
    fn an_ask_is_a_ceiling_and_never_a_floor() {
        let negotiate = |asked| ChunkSize::negotiate(Some(asked)).unwrap().get();

        assert_eq!(negotiate(4096), 4096);
        assert_eq!(negotiate(DEFAULT_CHUNK_SIZE as u64), DEFAULT_CHUNK_SIZE);
        assert_eq!(negotiate(u64::MAX), DEFAULT_CHUNK_SIZE);
    }

    #[test]
    #[cfg(feature = "server")]
    fn an_ask_below_the_minimum_is_refused() {
        let status = ChunkSize::negotiate(Some(MIN_CHUNK_SIZE as u64 - 1)).unwrap_err();

        assert_eq!(status.code(), Code::InvalidArgument);
        assert!(
            status.message().contains("Register.max_chunk_size"),
            "{}",
            status.message()
        );
    }

    #[test]
    fn only_a_size_in_range_is_a_chunk_size() {
        let new = |size| ChunkSize::new(size).map(ChunkSize::get);

        assert_eq!(new(0), None);
        assert_eq!(new(MIN_CHUNK_SIZE as u64 - 1), None);
        assert_eq!(new(MIN_CHUNK_SIZE as u64), Some(MIN_CHUNK_SIZE));
        assert_eq!(new(DEFAULT_CHUNK_SIZE as u64), Some(DEFAULT_CHUNK_SIZE));
        assert_eq!(new(MAX_CHUNK_SIZE as u64), Some(MAX_CHUNK_SIZE));
        assert_eq!(new(MAX_CHUNK_SIZE as u64 + 1), None);
        assert_eq!(new(u64::MAX), None);
    }

    fn frame(segment: Segment) -> Vec<(Bytes, bool)> {
        chunks(segment, ChunkSize::default())
            .map(|chunk| match chunk {
                Segment::Next(bytes) => (bytes, false),
                Segment::Final(bytes) => (bytes, true),
            })
            .collect()
    }

    fn payload(len: usize) -> Bytes {
        Bytes::from_iter((0..len).map(|i| (i % 251) as u8))
    }

    #[test]
    fn an_empty_intermediate_segment_yields_nothing() {
        assert_eq!(frame(Segment::Next(Bytes::new())), []);
    }

    #[test]
    fn an_empty_final_segment_still_ends_the_transfer() {
        assert_eq!(frame(Segment::Final(Bytes::new())), [(Bytes::new(), true)]);
    }

    #[test]
    fn a_segment_within_the_bound_is_one_chunk() {
        let bytes = payload(DEFAULT_CHUNK_SIZE - 1);
        assert_eq!(
            frame(Segment::Next(bytes.clone())),
            [(bytes.clone(), false)]
        );
        assert_eq!(frame(Segment::Final(bytes.clone())), [(bytes, true)]);
    }

    #[test]
    fn a_segment_of_exactly_the_bound_is_one_chunk() {
        let bytes = payload(DEFAULT_CHUNK_SIZE);
        assert_eq!(
            frame(Segment::Next(bytes.clone())),
            [(bytes.clone(), false)]
        );
        assert_eq!(frame(Segment::Final(bytes.clone())), [(bytes, true)]);
    }

    #[test]
    fn only_the_last_chunk_of_a_final_segment_is_final() {
        let bytes = payload(2 * DEFAULT_CHUNK_SIZE + 7);
        let frames = frame(Segment::Final(bytes.clone()));
        assert_eq!(
            frames
                .iter()
                .map(|(chunk, last)| (chunk.len(), *last))
                .collect::<Vec<_>>(),
            [
                (DEFAULT_CHUNK_SIZE, false),
                (DEFAULT_CHUNK_SIZE, false),
                (7, true)
            ]
        );
        assert_eq!(
            frames
                .into_iter()
                .flat_map(|(chunk, _)| chunk)
                .collect::<Bytes>(),
            bytes
        );
    }

    #[test]
    fn no_chunk_of_an_intermediate_segment_ends_the_transfer() {
        let bytes = payload(2 * DEFAULT_CHUNK_SIZE);
        let frames = frame(Segment::Next(bytes.clone()));
        assert_eq!(
            frames
                .iter()
                .map(|(chunk, last)| (chunk.len(), *last))
                .collect::<Vec<_>>(),
            [(DEFAULT_CHUNK_SIZE, false), (DEFAULT_CHUNK_SIZE, false)]
        );
        assert_eq!(
            frames
                .into_iter()
                .flat_map(|(chunk, _)| chunk)
                .collect::<Bytes>(),
            bytes
        );
    }
}
