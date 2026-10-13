// The unreliable bundle transfer service (Section 4.4): BTP-U messages
// carried in QUIC DATAGRAM frames, each frame one BTP-U link-layer PDU.

use bytes::{Bytes, BytesMut};
use hardy_btpu::{
    receiver::{
        MaxRetainedBytes, MaxSegments, MaxTransferSize, Receiver, ReceiverConfig, ReceiverEvent,
    },
    sender::{
        self, BundleFraming, CarriedList, LinkFraming, PduSize, SendId, SendOptions,
        SendQueueHighWatermark, Sender, SenderConfig,
    },
    transfer::WindowSize,
};
use quinn::{Connection, ConnectionError, SendDatagramError};
use rand::rngs::{SysError, SysRng};
use thiserror::Error;

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error(
        "the unreliable service is unavailable: one of the peers did not advertise max_datagram_frame_size"
    )]
    Unavailable,

    #[error("the peer's largest datagram, {max} octets, cannot carry a BTP-U message")]
    DatagramTooSmall { max: usize },

    #[error("failed to choose an initial BTP-U transfer number: {0}")]
    Random(#[source] SysError),

    #[error(transparent)]
    Btpu(#[from] sender::Error),

    #[error("failed to send a datagram: {0}")]
    Send(#[source] SendDatagramError),

    #[error("failed to receive a datagram: {0}")]
    Receive(#[source] ConnectionError),
}

// The BTP-U parameters of the unreliable service. The transfer window must
// be the same at both peers (Section 4.4), and one value configures both
// directions here.
#[derive(Debug, Clone, Copy, Default)]
pub struct Config {
    pub window_size: WindowSize,
    pub send_queue_high_watermark: SendQueueHighWatermark,
    pub max_transfer_size: MaxTransferSize,
    pub max_retained_bytes: Option<MaxRetainedBytes>,
}

// Opens the unreliable service on an established session.
//
// The PDU size is the largest datagram the session can carry when it
// opens. With path MTU discovery off that stays fixed for the session's
// life; datagrams are not padded by BTP-U, since QUIC pads every packet.
//
// The initial transfer number comes from the operating system's RNG, so
// that a peer is unlikely to take a new session's transfers for an old
// one's.
pub fn open(connection: &Connection, config: Config) -> Result<(DatagramSender, DatagramReceiver)> {
    let max = connection.max_datagram_size().ok_or(Error::Unavailable)?;
    let pdu_size = PduSize::new(max).ok_or(Error::DatagramTooSmall { max })?;
    let sender = Sender::try_from_rng(
        SenderConfig {
            pdu_size,
            window_size: config.window_size,
            send_queue_high_watermark: config.send_queue_high_watermark,
            link_framing: LinkFraming::variable(BundleFraming::Message),
            ..SenderConfig::default()
        },
        &mut SysRng,
    )
    .map_err(Error::Random)?;
    let receiver = Receiver::new(ReceiverConfig {
        window_size: config.window_size,
        max_transfer_size: config.max_transfer_size,
        max_segments_per_transfer: Some(MaxSegments::for_link_pdu_size(
            max,
            config.max_transfer_size,
        )),
        max_retained_bytes: config.max_retained_bytes,
        ..ReceiverConfig::default()
    });
    Ok((
        DatagramSender {
            connection: connection.clone(),
            pdu_size,
            sender,
            carried: CarriedList::default(),
        },
        DatagramReceiver {
            connection: connection.clone(),
            receiver,
        },
    ))
}

#[derive(Debug)]
pub struct DatagramSender {
    connection: Connection,
    pdu_size: PduSize,
    sender: Sender,
    carried: CarriedList,
}

impl DatagramSender {
    // Queues a bundle; `flush` sends it.
    pub fn enqueue(&mut self, bundle: Bytes) -> Result<SendId> {
        Ok(self.sender.enqueue(bundle, SendOptions::default())?)
    }

    // Packs everything queued into datagrams and hands each to QUIC,
    // waiting for room in QUIC's datagram buffer rather than displacing
    // datagrams already in it. Returns the bundles whose last octet has
    // been handed over, which is all the unreliable service can report of
    // their outcome.
    pub async fn flush(&mut self) -> Result<Vec<SendId>> {
        let mut sent = Vec::new();
        loop {
            let mut buf = BytesMut::zeroed(self.pdu_size.get());
            let Some(len) = self.sender.next_pdu_write(&mut self.carried, &mut buf)? else {
                return Ok(sent);
            };
            buf.truncate(len);
            self.connection
                .send_datagram_wait(buf.freeze())
                .await
                .map_err(Error::Send)?;
            sent.extend(self.carried.completed());
        }
    }
}

#[derive(Debug)]
pub struct DatagramReceiver {
    connection: Connection,
    receiver: Receiver,
}

impl DatagramReceiver {
    // Waits for the next datagram and returns what it completes, cancels,
    // or drops.
    pub async fn receive(&mut self) -> Result<Vec<ReceiverEvent>> {
        let datagram = self
            .connection
            .read_datagram()
            .await
            .map_err(Error::Receive)?;
        Ok(self.receiver.receive_pdu(datagram))
    }
}

#[cfg(test)]
mod tests {
    use hardy_btpu::receiver::ReceiverEvent;

    use super::*;
    use crate::endpoint::{Unreliable, tests::sessions};

    // Receives datagrams until one completes a bundle, returning every
    // event seen on the way.
    async fn until_received(receiver: &mut DatagramReceiver) -> Vec<ReceiverEvent> {
        let mut events = Vec::new();
        while !events
            .iter()
            .any(|event| matches!(event, ReceiverEvent::Received { .. }))
        {
            events.extend(receiver.receive().await.unwrap());
        }
        events
    }

    // The bundle of the only event, which must be a received bundle. Any
    // other event would be a drop, a rejection, or a cancellation.
    fn received(events: &[ReceiverEvent]) -> &Bytes {
        match events {
            [ReceiverEvent::Received { data, .. }] => data,
            _ => panic!(
                "expected one bundle and nothing else, got {} events",
                events.len()
            ),
        }
    }

    fn bundle(len: usize) -> Bytes {
        (0..len).map(|i| i as u8).collect()
    }

    #[tokio::test]
    async fn a_bundle_that_fits_a_datagram_arrives_whole() {
        let (dialed, accepted) = sessions(Unreliable::Offered, Unreliable::Offered).await;
        let (mut sender, _) = dialed.open_datagrams(Config::default()).unwrap();
        let (_, mut receiver) = accepted.open_datagrams(Config::default()).unwrap();

        let id = sender.enqueue(bundle(100)).unwrap();
        assert_eq!(sender.flush().await.unwrap(), [id]);
        assert_eq!(received(&until_received(&mut receiver).await), &bundle(100));
    }

    // The session's datagrams hold under 1200 octets, so this bundle can
    // only arrive by BTP-U segmentation and reassembly.
    #[tokio::test]
    async fn a_bundle_larger_than_a_datagram_is_segmented_and_reassembled() {
        let (dialed, accepted) = sessions(Unreliable::Offered, Unreliable::Offered).await;
        let (mut sender, _) = dialed.open_datagrams(Config::default()).unwrap();
        let (_, mut receiver) = accepted.open_datagrams(Config::default()).unwrap();

        let id = sender.enqueue(bundle(5000)).unwrap();
        assert_eq!(sender.flush().await.unwrap(), [id]);
        assert_eq!(
            received(&until_received(&mut receiver).await),
            &bundle(5000)
        );
    }

    #[tokio::test]
    async fn bundles_travel_in_both_directions_at_once() {
        let (dialed, accepted) = sessions(Unreliable::Offered, Unreliable::Offered).await;
        let (mut dialed_sender, mut dialed_receiver) =
            dialed.open_datagrams(Config::default()).unwrap();
        let (mut accepted_sender, mut accepted_receiver) =
            accepted.open_datagrams(Config::default()).unwrap();

        dialed_sender.enqueue(bundle(3000)).unwrap();
        accepted_sender.enqueue(bundle(2000)).unwrap();
        let (to_listener, to_dialer, _, _) = tokio::join!(
            until_received(&mut accepted_receiver),
            until_received(&mut dialed_receiver),
            dialed_sender.flush(),
            accepted_sender.flush(),
        );
        assert_eq!(received(&to_listener), &bundle(3000));
        assert_eq!(received(&to_dialer), &bundle(2000));
    }

    // Section 4.4: the service is available only if both peers advertised
    // max_datagram_frame_size.
    #[tokio::test]
    async fn the_service_is_unavailable_unless_both_peers_offer_it() {
        let (dialed, accepted) = sessions(Unreliable::Offered, Unreliable::NotOffered).await;
        assert!(matches!(
            dialed.open_datagrams(Config::default()),
            Err(Error::Unavailable)
        ));
        assert!(matches!(
            accepted.open_datagrams(Config::default()),
            Err(Error::Unavailable)
        ));
    }
}
