// The reliable bundle transfer service (Section 4.3): one bundle per
// unidirectional stream, the stream's content framed as in Section 4.3.2.
//
// A transfer succeeds at the sender once the peer's QUIC endpoint has
// acknowledged every octet and the FIN (Section 4.3.1). That is receipt by
// the peer's transport, not acceptance by its BPA.

use bytes::Bytes;
use quinn::{
    Connection, ConnectionError, ReadError, RecvStream, SendStream, StoppedError, VarInt,
    WriteError,
};
use thiserror::Error;
use tracing::debug;

use crate::{
    error_code::ErrorCode,
    stream::{self, Pushed, StreamReader, wrapped_head},
};

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("failed to open a stream: {0}")]
    Open(#[source] ConnectionError),

    #[error("failed to accept a stream: {0}")]
    Accept(#[source] ConnectionError),

    // STOP_SENDING at the sender, RESET_STREAM at the receiver.
    #[error("the peer cancelled the transfer with {0}")]
    Cancelled(ErrorCode),

    #[error("the connection ended before the transfer completed: {0}")]
    ConnectionLost(#[source] ConnectionError),

    #[error("the bundle continued past the {declared} octets declared")]
    TooLong { declared: u64 },

    #[error("the bundle ended after {written} of the {declared} octets declared")]
    TooShort { declared: u64, written: u64 },

    // The receiver has closed the connection with QBCL_PROTOCOL_ERROR.
    #[error("the peer violated the stream framing: {0}")]
    Framing(#[from] stream::Error),

    #[error("the stream was already closed locally")]
    Closed,

    // Neither role sends or accepts 0-RTT data (Section 6.4).
    #[error("the peer rejected 0-RTT data")]
    ZeroRttRejected,
}

impl From<WriteError> for Error {
    fn from(e: WriteError) -> Self {
        match e {
            WriteError::Stopped(code) => Self::Cancelled(ErrorCode::from_stream_reset(code.into())),
            WriteError::ConnectionLost(e) => Self::ConnectionLost(e),
            WriteError::ClosedStream => Self::Closed,
            WriteError::ZeroRttRejected => Self::ZeroRttRejected,
        }
    }
}

impl From<StoppedError> for Error {
    fn from(e: StoppedError) -> Self {
        match e {
            StoppedError::ConnectionLost(e) => Self::ConnectionLost(e),
            StoppedError::ZeroRttRejected => Self::ZeroRttRejected,
        }
    }
}

impl From<ReadError> for Error {
    fn from(e: ReadError) -> Self {
        match e {
            ReadError::Reset(code) => Self::Cancelled(ErrorCode::from_stream_reset(code.into())),
            ReadError::ConnectionLost(e) => Self::ConnectionLost(e),
            // Only an unordered read makes a later ordered read illegal, and
            // this crate reads in order only.
            ReadError::ClosedStream | ReadError::IllegalOrderedRead => Self::Closed,
            ReadError::ZeroRttRejected => Self::ZeroRttRejected,
        }
    }
}

pub fn var_int(code: ErrorCode) -> VarInt {
    VarInt::from_u64(code.value()).expect("every Table 2 code is far below 2^62")
}

// Opens a stream for a bundle of `len` octets, in the wrapped form: the
// sender always knows the length, and the wrapped form lets the receiver
// apply its size policy before the bulk arrives (Section 4.3.2). Waits for
// stream credit from the peer.
pub async fn open(connection: &Connection, len: u64) -> Result<BundleSender> {
    let stream = connection.open_uni().await.map_err(Error::Open)?;
    // Built before the head is written, so that a failed write resets the
    // stream when the sender drops.
    let mut sender = BundleSender {
        stream,
        declared: len,
        written: 0,
        finished: false,
    };
    sender.stream.write_chunk(wrapped_head(len)).await?;
    Ok(sender)
}

// One bundle on its way to the peer. A sender dropped before `finish`
// resets the stream with QBCL_TRANSFER_CANCELLED: quinn would otherwise
// finish it, and a stream that ends early must read as cancelled, not as
// complete.
#[derive(Debug)]
pub struct BundleSender {
    stream: SendStream,
    declared: u64,
    written: u64,
    finished: bool,
}

impl BundleSender {
    // Writes the next part of the bundle, waiting for flow control credit.
    // A part that would carry the bundle past its declared length cancels
    // the transfer.
    pub async fn write(&mut self, part: Bytes) -> Result<()> {
        let len = u64::try_from(part.len()).expect("a buffer length fits in u64");
        let written = self.written.saturating_add(len);
        if written > self.declared {
            self.cancel(ErrorCode::TransferCancelled);
            return Err(Error::TooLong {
                declared: self.declared,
            });
        }
        self.stream.write_chunk(part).await?;
        self.written = written;
        Ok(())
    }

    // Sends the FIN and waits for the transfer's outcome: `Ok` once the peer
    // has acknowledged every octet and the FIN. A bundle shorter than
    // declared cancels the transfer.
    pub async fn finish(mut self) -> Result<()> {
        if self.written < self.declared {
            self.cancel(ErrorCode::TransferCancelled);
            return Err(Error::TooShort {
                declared: self.declared,
                written: self.written,
            });
        }
        self.stream.finish().map_err(|_| Error::Closed)?;
        self.finished = true;
        match self.stream.stopped().await? {
            None => Ok(()),
            Some(code) => Err(Error::Cancelled(ErrorCode::from_stream_reset(code.into()))),
        }
    }

    // Abandons the transfer with `code` (Section 4.6).
    pub fn cancel(&mut self, code: ErrorCode) {
        if !self.finished {
            // A stream already reset needs no second reset.
            let _ = self.stream.reset(var_int(code));
            self.finished = true;
        }
    }
}

impl Drop for BundleSender {
    fn drop(&mut self) {
        self.cancel(ErrorCode::TransferCancelled);
    }
}

// Accepts the next stream the peer opens. Streams whose data had fully
// arrived before the connection closed are still returned after it closes.
pub async fn accept(connection: &Connection) -> Result<BundleReceiver> {
    let stream = connection.accept_uni().await.map_err(Error::Accept)?;
    Ok(BundleReceiver {
        connection: connection.clone(),
        stream,
        reader: Some(StreamReader::new()),
    })
}

// One bundle arriving from the peer. A receiver dropped before the end of
// its stream stops the stream with QBCL_TRANSFER_CANCELLED: quinn would
// otherwise send code 0, QBCL_NO_ERROR, which Section 5 forbids there.
#[derive(Debug)]
pub struct BundleReceiver {
    connection: Connection,
    stream: RecvStream,
    // `None` once the stream has ended, been stopped, or failed.
    reader: Option<StreamReader>,
}

impl BundleReceiver {
    // The next part of the bundle, with the stream's head on the first part
    // that completes it, or `None` once the stream has ended and its framing
    // checked out.
    //
    // A framing fault closes the connection with QBCL_PROTOCOL_ERROR before
    // it is returned, as Section 4.3.2 requires.
    pub async fn next(&mut self) -> Result<Option<Pushed>> {
        loop {
            let Some(reader) = self.reader.as_mut() else {
                return Ok(None);
            };
            let read = self.stream.read_chunk(usize::MAX, true).await;
            let framed = match read {
                Ok(Some(chunk)) => reader.push(chunk.bytes).map(Some),
                Ok(None) => {
                    let reader = self.reader.take().expect("checked above");
                    reader.finish().map(|()| None)
                }
                Err(e) => {
                    self.reader = None;
                    return Err(e.into());
                }
            };
            match framed {
                Ok(Some(pushed)) if pushed.head.is_none() && pushed.data.is_empty() => continue,
                Ok(pushed) => return Ok(pushed),
                Err(e) => {
                    self.reader = None;
                    debug!("closing the connection: {e}");
                    self.connection
                        .close(var_int(ErrorCode::ProtocolError), e.to_string().as_bytes());
                    return Err(e.into());
                }
            }
        }
    }

    // Cancels the transfer with `code` (Section 4.6). Data arriving later is
    // discarded.
    pub fn stop(&mut self, code: ErrorCode) {
        if self.reader.take().is_some() {
            // A stream already finished or reset needs no STOP_SENDING.
            let _ = self.stream.stop(var_int(code));
        }
    }
}

impl Drop for BundleReceiver {
    fn drop(&mut self) {
        self.stop(ErrorCode::TransferCancelled);
    }
}

#[cfg(test)]
mod tests {
    use quinn::{ApplicationClose, ConnectionError};

    use super::*;
    use crate::{
        endpoint::{Session, Unreliable, tests::sessions},
        stream::{BundleVersion, Form, StreamHead},
    };

    async fn pair() -> (Session, Session) {
        sessions(Unreliable::NotOffered, Unreliable::NotOffered).await
    }

    // A BPv7-looking bundle of `len` octets: the framing checks only the
    // first octet.
    fn bundle(len: usize) -> Bytes {
        (0..len)
            .map(|i| if i == 0 { 0x9f } else { i as u8 })
            .collect()
    }

    // Larger than quinn's default stream receive window, so that a receiver
    // that reads only the head holds the sender up mid-bundle.
    const LARGER_THAN_THE_STREAM_WINDOW: usize = 4 * 1024 * 1024;

    // Reads a stream to its end, returning every head reported and the
    // bundle octets.
    async fn read_to_end(receiver: &mut BundleReceiver) -> Result<(Vec<StreamHead>, Vec<u8>)> {
        let mut heads = Vec::new();
        let mut data = Vec::new();
        while let Some(pushed) = receiver.next().await? {
            heads.extend(pushed.head);
            data.extend_from_slice(&pushed.data);
        }
        Ok((heads, data))
    }

    // Reads until the stream fails, returning the error.
    async fn read_to_error(receiver: &mut BundleReceiver) -> Error {
        loop {
            match receiver.next().await {
                Ok(Some(_)) => {}
                Ok(None) => panic!("the stream ended without an error"),
                Err(e) => return e,
            }
        }
    }

    // Reads until the stream's head arrives.
    async fn read_head(receiver: &mut BundleReceiver) -> StreamHead {
        loop {
            if let Some(head) = receiver.next().await.unwrap().unwrap().head {
                return head;
            }
        }
    }

    fn application_close_code(e: &ConnectionError) -> u64 {
        match e {
            ConnectionError::ApplicationClosed(ApplicationClose { error_code, .. }) => {
                (*error_code).into()
            }
            other => panic!("expected an application close, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_bundle_arrives_whole_in_the_wrapped_form_and_is_acknowledged() {
        let (dialed, accepted) = pair().await;
        let content = bundle(3000);

        let mut sender = dialed.open_bundle(3000).await.unwrap();
        for part in [
            content.slice(..1),
            content.slice(1..1500),
            content.slice(1500..),
        ] {
            sender.write(part).await.unwrap();
        }
        let (outcome, received) = tokio::join!(sender.finish(), async {
            read_to_end(&mut accepted.accept_bundle().await.unwrap()).await
        });

        outcome.unwrap();
        let (heads, data) = received.unwrap();
        assert_eq!(
            heads,
            [StreamHead {
                form: Form::Wrapped { len: 3000 },
                version: BundleVersion::V7,
            }]
        );
        assert_eq!(data, content);
    }

    // Section 4.6: the receiver's STOP_SENDING code reaches the sender. The
    // bundle is larger than the stream window, so the sender cannot have had
    // every octet acknowledged before the receiver stops it.
    #[tokio::test]
    async fn a_receiver_cancels_a_transfer_with_its_code() {
        let (dialed, accepted) = pair().await;
        let content = bundle(LARGER_THAN_THE_STREAM_WINDOW);

        let send = async {
            let mut sender = dialed.open_bundle(content.len() as u64).await.unwrap();
            sender.write(content.clone()).await
        };
        let receive = async {
            let mut receiver = accepted.accept_bundle().await.unwrap();
            read_head(&mut receiver).await;
            receiver.stop(ErrorCode::BundleTooLarge);
        };
        let (sent, ()) = tokio::join!(send, receive);

        assert!(matches!(
            sent,
            Err(Error::Cancelled(ErrorCode::BundleTooLarge))
        ));
    }

    // quinn sends code 0 when a receive stream is dropped unread, which is
    // QBCL_NO_ERROR, forbidden on STOP_SENDING (Section 5).
    #[tokio::test]
    async fn a_dropped_receiver_stops_the_stream_with_transfer_cancelled() {
        let (dialed, accepted) = pair().await;

        let mut raw = dialed.connection().open_uni().await.unwrap();
        raw.write_chunk(wrapped_head(100)).await.unwrap();
        raw.write_chunk(bundle(50)).await.unwrap();
        let mut receiver = accepted.accept_bundle().await.unwrap();
        read_head(&mut receiver).await;
        drop(receiver);

        assert_eq!(
            raw.stopped().await.unwrap(),
            Some(var_int(ErrorCode::TransferCancelled))
        );
    }

    // quinn finishes a send stream that is dropped, which would end the
    // stream as if the bundle were complete.
    #[tokio::test]
    async fn a_dropped_sender_resets_the_stream_with_transfer_cancelled() {
        let (dialed, accepted) = pair().await;

        let mut sender = dialed.open_bundle(100).await.unwrap();
        sender.write(bundle(50)).await.unwrap();
        let mut receiver = accepted.accept_bundle().await.unwrap();
        read_head(&mut receiver).await;
        drop(sender);

        assert!(matches!(
            read_to_error(&mut receiver).await,
            Error::Cancelled(ErrorCode::TransferCancelled)
        ));
    }

    #[tokio::test]
    async fn writing_past_the_declared_length_cancels_the_transfer() {
        let (dialed, accepted) = pair().await;

        let mut sender = dialed.open_bundle(10).await.unwrap();
        assert!(matches!(
            sender.write(bundle(11)).await,
            Err(Error::TooLong { declared: 10 })
        ));
        let mut receiver = accepted.accept_bundle().await.unwrap();
        assert!(matches!(
            read_to_error(&mut receiver).await,
            Error::Cancelled(ErrorCode::TransferCancelled)
        ));
    }

    #[tokio::test]
    async fn finishing_short_of_the_declared_length_cancels_the_transfer() {
        let (dialed, accepted) = pair().await;

        let mut sender = dialed.open_bundle(10).await.unwrap();
        sender.write(bundle(5)).await.unwrap();
        assert!(matches!(
            sender.finish().await,
            Err(Error::TooShort {
                declared: 10,
                written: 5
            })
        ));
        let mut receiver = accepted.accept_bundle().await.unwrap();
        assert!(matches!(
            read_to_error(&mut receiver).await,
            Error::Cancelled(ErrorCode::TransferCancelled)
        ));
    }

    // Section 4.3.2: a framing fault closes the connection with
    // QBCL_PROTOCOL_ERROR.
    #[tokio::test]
    async fn a_framing_fault_closes_the_connection_with_protocol_error() {
        let (dialed, accepted) = pair().await;

        let mut raw = dialed.connection().open_uni().await.unwrap();
        raw.write_chunk(Bytes::from_static(&[0x00])).await.unwrap();
        raw.finish().unwrap();
        let mut receiver = accepted.accept_bundle().await.unwrap();

        assert!(matches!(
            read_to_error(&mut receiver).await,
            Error::Framing(stream::Error::UnknownFirstOctet(0x00))
        ));
        assert_eq!(
            application_close_code(&dialed.closed().await),
            ErrorCode::ProtocolError.value()
        );
    }

    // A sender that saw its transfer acknowledged and then closed the
    // session has reported success; the bundle must still reach the
    // receiver, though the receiver accepts its stream only after the close
    // has arrived.
    #[tokio::test]
    async fn a_bundle_acknowledged_before_the_sender_closes_is_still_received() {
        let (dialed, accepted) = pair().await;
        let content = bundle(3000);

        let mut sender = dialed.open_bundle(3000).await.unwrap();
        sender.write(content.clone()).await.unwrap();
        sender.finish().await.unwrap();
        dialed.close(ErrorCode::NoError);
        assert_eq!(
            application_close_code(&accepted.closed().await),
            ErrorCode::NoError.value()
        );

        let mut receiver = accepted.accept_bundle().await.unwrap();
        let (_, data) = read_to_end(&mut receiver).await.unwrap();
        assert_eq!(data, content);
    }

    // Section 4.5: a transfer in progress fails when the connection closes.
    #[tokio::test]
    async fn a_transfer_in_progress_fails_when_the_peer_closes_the_session() {
        let (dialed, accepted) = pair().await;
        let content = bundle(LARGER_THAN_THE_STREAM_WINDOW);

        let send = async {
            let mut sender = dialed.open_bundle(content.len() as u64).await.unwrap();
            sender.write(content.clone()).await
        };
        let receive = async {
            let mut receiver = accepted.accept_bundle().await.unwrap();
            read_head(&mut receiver).await;
            accepted.close(ErrorCode::ShuttingDown);
        };
        let (sent, ()) = tokio::join!(send, receive);

        match sent {
            Err(Error::ConnectionLost(e)) => {
                assert_eq!(application_close_code(&e), ErrorCode::ShuttingDown.value());
            }
            other => panic!("expected a lost connection, got {other:?}"),
        }
    }
}
