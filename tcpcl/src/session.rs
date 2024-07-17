use super::*;
use hardy_proto::cla::*;
use std::{collections::VecDeque, net::SocketAddr};
use thiserror::Error;
use tokio::sync::mpsc::*;
use tokio_util::bytes::{Bytes, BytesMut};
use utils::settings;

#[derive(Error, Debug)]
pub enum Error {
    #[error(transparent)]
    Codec(#[from] codec::Error),

    #[error("Peer closed the connection")]
    Hangup,

    #[error("Timed out waiting for message from peer")]
    Timeout,

    #[error("Cancelled")]
    Cancelled,

    #[error("Invalid contact header")]
    InvalidContactHeader,

    #[error("Message {0:?} was rejected by peer")]
    Rejected(codec::MessageRejectMessage),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Bpa(#[from] tonic::Status),

    #[error("BPA response channel closed")]
    Closed,
}

const DEFAULT_KEEPALIVE_INTERVAL: u16 = 60;
const DEFAULT_SEGMENT_MRU: u64 = 16384;
const DEFAULT_TRANSFER_MRU: u64 = 0x4000_0000; // 4GiB

#[derive(Clone)]
pub struct Config {
    pub keepalive_interval: u16,
    pub segment_mru: u64,
    pub transfer_mru: u64,
    pub node_id: Option<bpv7::Eid>,
}

impl Config {
    pub fn new(config: &config::Config) -> Self {
        let config = Self {
            keepalive_interval: settings::get_with_default(
                config,
                "keepalive_interval",
                DEFAULT_KEEPALIVE_INTERVAL,
            )
            .trace_expect("Invalid 'keepalive_interval' value in configuration"),
            segment_mru: settings::get_with_default(config, "segment_mru", DEFAULT_SEGMENT_MRU)
                .trace_expect("Invalid 'segment_mru' value in configuration"),
            transfer_mru: settings::get_with_default(config, "transfer_mru", DEFAULT_TRANSFER_MRU)
                .trace_expect("Invalid 'transfer_mru' value in configuration"),
            node_id: settings::get_with_default::<String, _>(config, "node_id", String::new())
                .map(|s| {
                    if s.is_empty() {
                        None
                    } else {
                        Some(
                            s.parse::<bpv7::Eid>()
                                .trace_expect("Invalid 'node_id' value in configuration"),
                        )
                    }
                })
                .trace_expect("Invalid 'node_id' value in configuration"),
        };

        if config.keepalive_interval == 0 {
            info!("Session keepalive disabled");
        }

        if let Some(node_id) = &config.node_id {
            match node_id {
                bpv7::Eid::Ipn2 { .. } | bpv7::Eid::Ipn3 { .. } => {}
                bpv7::Eid::Dtn { node_name, .. } if !node_name.starts_with('~') => {}
                _ => {
                    // Fatal!
                    error!("Invalid 'node_id' value in configuration: {node_id}");
                    panic!("Invalid 'node_id' value in configuration: {node_id}");
                }
            }
        }
        config
    }
}

struct XferAck {
    flags: codec::TransferSegmentMessageFlags,
    transfer_id: u64,
    acknowledged_length: usize,
}

enum SendSegmentResult {
    Ok,
    Terminate(codec::SessionTermMessage),
    Refused(codec::TransferRefuseReasonCode),
}

enum SendResult {
    Ok,
    Terminate(codec::SessionTermMessage),
    Shutdown(codec::SessionTermReasonCode),
}

struct Session<T>
where
    T: futures::StreamExt<Item = Result<codec::Message, codec::Error>>
        + futures::SinkExt<codec::Message>
        + std::marker::Unpin,
    session::Error: From<<T as futures::Sink<codec::Message>>::Error>,
{
    transport: T,
    bpa: bpa::Bpa,
    keepalive_interval: u16,
    last_sent: tokio::time::Instant,
    segment_mtu: usize,
    transfer_mru: usize,
    rcv: Receiver<Vec<u8>>,
    snd: UnboundedSender<Result<ForwardBundleResponse, tonic::Status>>,
    transfer_id: u64,
    acks: VecDeque<XferAck>,
    ingress_bundle: Option<BytesMut>,
}

impl<T> Session<T>
where
    T: futures::StreamExt<Item = Result<codec::Message, codec::Error>>
        + futures::SinkExt<codec::Message>
        + std::marker::Unpin,
    session::Error: From<<T as futures::Sink<codec::Message>>::Error>,
{
    fn new(
        transport: T,
        bpa: bpa::Bpa,
        keepalive_interval: u16,
        segment_mtu: usize,
        transfer_mru: usize,
        rcv: Receiver<Vec<u8>>,
        snd: UnboundedSender<Result<ForwardBundleResponse, tonic::Status>>,
    ) -> Self {
        Self {
            transport,
            bpa,
            keepalive_interval,
            last_sent: tokio::time::Instant::now(),
            segment_mtu,
            transfer_mru,
            rcv,
            snd,
            transfer_id: 0,
            acks: VecDeque::new(),
            ingress_bundle: None,
        }
    }

    async fn process_msg(
        &mut self,
        msg: Option<Result<codec::Message, codec::Error>>,
    ) -> Result<(), Error> {
        match msg {
            Some(Ok(codec::Message::SessionInit(_))) => {
                self.unexpected(codec::MessageType::SESS_INIT).await
            }
            Some(Ok(codec::Message::SessionTerm(_))) => unreachable!(),
            Some(Ok(codec::Message::Keepalive)) => todo!(),
            Some(Ok(codec::Message::TransferSegment(msg))) => self.recv(msg).await,
            Some(Ok(codec::Message::TransferAck(ack))) => self.ack_segment(ack).await,
            Some(Ok(codec::Message::TransferRefuse(refusal))) => self.refuse(refusal).await,
            Some(Ok(codec::Message::Reject(msg))) => Err(Error::Rejected(msg)),
            Some(Err(codec::Error::InvalidMessageType(rejected_message))) => {
                // Send a rejection (best effort)
                let _ = self
                    .reject(
                        codec::MessageRejectionReasonCode::UnknownType,
                        rejected_message,
                    )
                    .await;

                Err(Error::Codec(codec::Error::InvalidMessageType(
                    rejected_message,
                )))
            }
            Some(Err(e)) => Err(Error::Codec(e)),
            None => Err(Error::Hangup),
        }
    }

    async fn reject(
        &mut self,
        reason_code: codec::MessageRejectionReasonCode,
        rejected_message: u8,
    ) -> Result<(), Error> {
        self.transport
            .send(codec::Message::Reject(codec::MessageRejectMessage {
                reason_code,
                rejected_message,
            }))
            .await
            .map_err(Into::into)
            .map(|_| self.last_sent = tokio::time::Instant::now())
    }

    async fn unexpected(&mut self, rejected_message: codec::MessageType) -> Result<(), Error> {
        self.transport
            .send(codec::Message::Reject(codec::MessageRejectMessage {
                reason_code: codec::MessageRejectionReasonCode::Unexpected,
                rejected_message: rejected_message as u8,
            }))
            .await
            .map_err(Into::into)
            .map(|_| self.last_sent = tokio::time::Instant::now())
    }

    async fn recv(&mut self, msg: codec::TransferSegmentMessage) -> Result<(), Error> {
        if msg.message_flags.start {
            if self.ingress_bundle.is_some() {
                // Out of order bundle!
                self.ingress_bundle = None;
            } else {
                self.ingress_bundle = Some(BytesMut::with_capacity(msg.data.len()));
            }
        }

        let Some(bundle) = &mut self.ingress_bundle else {
            return self.unexpected(codec::MessageType::XFER_SEGMENT).await;
        };

        if msg.data.len() + bundle.len() > self.transfer_mru {
            // Bundle beyond negotiated MRU
            self.ingress_bundle = None;

            return self
                .reject(
                    codec::MessageRejectionReasonCode::Unsupported,
                    codec::MessageType::XFER_SEGMENT as u8,
                )
                .await;
        }

        bundle.extend_from_slice(&msg.data);
        let acknowledged_length = bundle.len() as u64;

        if msg.message_flags.end {
            // Send the bundle to the BPA
            self.bpa.send(bundle.to_vec()).await?;

            // Clear the ingress bundle
            self.ingress_bundle = None;
        }

        // Acknowledge the transfer
        self.transport
            .feed(codec::Message::TransferAck(codec::TransferAckMessage {
                transfer_id: msg.transfer_id,
                message_flags: msg.message_flags,
                acknowledged_length,
            }))
            .await
            .map_err(Into::into)
            .map(|_| self.last_sent = tokio::time::Instant::now())
    }

    fn respond(
        &mut self,
        response: Result<ForwardBundleResponse, tonic::Status>,
    ) -> Result<(), Error> {
        self.snd.send(response).map_err(|_| Error::Closed)
    }

    async fn ack_segment(&mut self, msg: codec::TransferAckMessage) -> Result<(), Error> {
        let Some(ack) = self.acks.front() else {
            // No acknowledgement expected!
            return self.unexpected(codec::MessageType::XFER_ACK).await;
        };

        if ack.transfer_id != msg.transfer_id {
            // Out of order acknowledgement!
            return self.unexpected(codec::MessageType::XFER_ACK).await;
        }

        // Remove the ack from the queue
        let ack = self.acks.pop_front().unwrap();

        if ack.flags != msg.message_flags {
            warn!(
                "Mismatched flags in TransferAck message: {:?}",
                msg.message_flags
            );
        }

        if ack.acknowledged_length as u64 != msg.acknowledged_length {
            warn!(
                "Mismatched acknowledged_length in TransferAck message: expected {} got {}",
                ack.acknowledged_length, msg.acknowledged_length
            );
            self.unexpected(codec::MessageType::XFER_ACK).await
        } else if ack.flags.end {
            // Let the client know send is complete
            self.respond(Ok(ForwardBundleResponse {
                result: forward_bundle_response::ForwardingResult::Sent as i32,
                delay: None,
            }))
        } else {
            Ok(())
        }
    }

    async fn refuse_segment(
        &mut self,
        msg: codec::TransferRefuseMessage,
    ) -> Result<Option<codec::TransferRefuseReasonCode>, Error> {
        let Some(ack) = self.acks.front() else {
            // Unexpected refusal
            return self
                .unexpected(codec::MessageType::XFER_REFUSE)
                .await
                .map(|_| None);
        };

        if ack.transfer_id != msg.transfer_id {
            // Out of order refusal!
            self.unexpected(codec::MessageType::XFER_REFUSE)
                .await
                .map(|_| None)
        } else {
            // Remove the ack from the queue
            self.acks.pop_front();
            Ok(Some(msg.reason_code))
        }
    }

    async fn refuse(&mut self, msg: codec::TransferRefuseMessage) -> Result<(), Error> {
        let Some(ack) = self.acks.front() else {
            // Unexpected refusal
            return self.unexpected(codec::MessageType::XFER_REFUSE).await;
        };

        if ack.transfer_id != msg.transfer_id {
            // Out of order refusal!
            return self.unexpected(codec::MessageType::XFER_REFUSE).await;
        }

        // Remove the ack from the queue
        self.acks.pop_front();

        let response = match msg.reason_code {
            codec::TransferRefuseReasonCode::Completed => Ok(ForwardBundleResponse {
                result: forward_bundle_response::ForwardingResult::Sent as i32,
                delay: None,
            }),
            codec::TransferRefuseReasonCode::SessionTerminating => {
                Err(tonic::Status::aborted("Session is terminating"))
            }
            codec::TransferRefuseReasonCode::NoResources => {
                Ok(ForwardBundleResponse {
                    result: forward_bundle_response::ForwardingResult::Congested as i32,
                    delay: /* TODO - Configurable backoff! */ Some(grpc::to_timestamp(
                        time::OffsetDateTime::now_utc() + time::Duration::seconds(5),
                    )),
                })
            }
            codec::TransferRefuseReasonCode::Retransmit => {
                /* Send again, but we can't as we have dropped the bundle,
                 * Report 'congestion' with an immediate retry */
                Ok(ForwardBundleResponse {
                    result: forward_bundle_response::ForwardingResult::Congested as i32,
                    delay: None,
                })
            }
            codec::TransferRefuseReasonCode::NotAcceptable => {
                Err(tonic::Status::invalid_argument("Not acceptable"))
            }
            reason => Err(tonic::Status::unknown(format!(
                "Peer refused bundle with reason code: {reason:?}"
            ))),
        };
        self.respond(response)
    }

    async fn send_segment(
        &mut self,
        flags: codec::TransferSegmentMessageFlags,
        data: Bytes,
        acknowledged_length: usize,
    ) -> Result<SendSegmentResult, Error> {
        // Inc transfer id
        let transfer_id = self.transfer_id;
        self.transfer_id += 1;

        // Add new Xfer to queue of Acks
        self.acks.push_back(XferAck {
            flags: flags.clone(),
            transfer_id,
            acknowledged_length,
        });

        self.transport
            .feed(codec::Message::TransferSegment(
                codec::TransferSegmentMessage {
                    message_flags: flags,
                    transfer_id,
                    data,
                    ..Default::default()
                },
            ))
            .await?;

        self.last_sent = tokio::time::Instant::now();

        // Use a biased select! to check for incoming messages before the next segment is sent
        while !self.acks.is_empty() {
            tokio::select! {
                biased;
                r = self.transport.next() => match r {
                    Some(Ok(codec::Message::SessionTerm(msg))) => {
                        trace!("Peer has started to end the session: {msg:?}");
                        return Ok(SendSegmentResult::Terminate(msg))
                    },
                    Some(Ok(codec::Message::TransferRefuse(refusal))) => {
                        if let Some(refusal) = self.refuse_segment(refusal).await? {
                            return Ok(SendSegmentResult::Refused(refusal))
                        }
                    }
                    msg => self.process_msg(msg).await?,
                },
                _ = std::future::ready(()) => {
                    // No messages pending
                    break
                },
            }
        }
        Ok(SendSegmentResult::Ok)
    }

    async fn send_once(&mut self, mut bundle: Bytes) -> Result<SendSegmentResult, Error> {
        let mut start = true;

        // Segment if needed
        let mut acknowledged_length = 0;
        while bundle.len() > self.segment_mtu {
            acknowledged_length += self.segment_mtu;
            match self
                .send_segment(
                    codec::TransferSegmentMessageFlags {
                        start,
                        end: false,
                        ..Default::default()
                    },
                    bundle.split_to(self.segment_mtu),
                    acknowledged_length,
                )
                .await?
            {
                SendSegmentResult::Ok => {}
                r => return Ok(r),
            }

            start = false;
        }

        // Send the last segment
        acknowledged_length += self.segment_mtu;
        self.send_segment(
            codec::TransferSegmentMessageFlags {
                start,
                end: true,
                ..Default::default()
            },
            bundle,
            acknowledged_length,
        )
        .await
    }

    async fn send(&mut self, bundle: Vec<u8>) -> Result<SendResult, Error> {
        /* TODO:  We currently report retry-able transfer failures as 'congestion',
         * but we need a configurable fixed delay, but there has to be a better feedback mechanism */

        // Check we can send the segments without rolling over the transfer id
        if self
            .transfer_id
            .saturating_add((bundle.len() / self.segment_mtu) as u64)
            == u64::MAX
        {
            // Nope - need to shutdown the session, report as congestion
            return self
                .respond(Ok(ForwardBundleResponse {
                    result: forward_bundle_response::ForwardingResult::Congested as i32,
                    delay: /* TODO - Configurable backoff! */ Some(grpc::to_timestamp(
                        time::OffsetDateTime::now_utc() + time::Duration::seconds(5),
                    )),
                }))
                .map(|_| SendResult::Shutdown(codec::SessionTermReasonCode::ResourceExhaustion));
        }

        let bundle = Bytes::from(bundle.into_boxed_slice());
        loop {
            match self.send_once(bundle.clone()).await? {
                SendSegmentResult::Ok => return Ok(SendResult::Ok),
                SendSegmentResult::Terminate(msg) => return Ok(SendResult::Terminate(msg)),
                SendSegmentResult::Refused(codec::TransferRefuseReasonCode::Completed) => {
                    break self.respond(Ok(ForwardBundleResponse {
                        result: forward_bundle_response::ForwardingResult::Sent as i32,
                        delay: None,
                    }))
                }
                SendSegmentResult::Refused(codec::TransferRefuseReasonCode::SessionTerminating) => {
                    break self.respond(Err(tonic::Status::aborted("Session is terminating")))
                }
                SendSegmentResult::Refused(codec::TransferRefuseReasonCode::NoResources) => {
                    break self.respond(Ok(ForwardBundleResponse {
                        result: forward_bundle_response::ForwardingResult::Congested as i32,
                        delay: /* TODO - Configurable backoff! */ Some(grpc::to_timestamp(
                            time::OffsetDateTime::now_utc() + time::Duration::seconds(5),
                        )),
                    }));
                }
                SendSegmentResult::Refused(codec::TransferRefuseReasonCode::Retransmit) => { /* Send again */ }
                SendSegmentResult::Refused(codec::TransferRefuseReasonCode::NotAcceptable) => {
                    break self.respond(Err(tonic::Status::invalid_argument("Not acceptable")))
                }
                SendSegmentResult::Refused(reason) => {
                    break self.respond(Err(tonic::Status::unknown(format!(
                        "Peer refused bundle with reason code: {reason:?}"
                    ))))
                }
            }
        }
        .map(|_| SendResult::Ok)
    }

    async fn send_keepalive(&mut self) -> Result<(), Error> {
        self.transport
            .feed(codec::Message::Keepalive)
            .await
            .map_err(Into::into)
            .map(|_| self.last_sent = tokio::time::Instant::now())
    }

    async fn shutdown(mut self, reason_code: codec::SessionTermReasonCode) -> Result<(), Error> {
        // The local client has closed the channel

        // Send a SESS_TERM message
        let msg = codec::SessionTermMessage {
            reason_code,
            ..Default::default()
        };
        let mut expected_reply = msg.clone();
        expected_reply.message_flags.reply = true;

        self.transport
            .send(codec::Message::SessionTerm(msg))
            .await?;

        // Process any remaining messages
        loop {
            match if self.keepalive_interval != 0 {
                // Read the next message with timeout
                tokio::time::timeout(
                    tokio::time::Duration::from_secs(self.keepalive_interval as u64)
                        .saturating_mul(2),
                    self.transport.next(),
                )
                .await
                .map_err(|_| Error::Timeout)?
            } else {
                self.transport.next().await
            } {
                Some(Ok(codec::Message::SessionTerm(mut msg))) => {
                    if !msg.message_flags.reply {
                        // Terminations pass in the night...
                        msg.message_flags.reply = true;
                        self.transport
                            .feed(codec::Message::SessionTerm(msg))
                            .await?;
                        continue;
                    } else if msg != expected_reply {
                        trace!(
                            "Mismatched SESS_TERM message: {:?}, expected {:?}",
                            msg,
                            expected_reply
                        );
                    }
                    break;
                }
                Some(msg) => self.process_msg(Some(msg)).await?,
                None => {
                    /* The peer has just hung-up */
                    trace!("Peer has hung-up without sending a SESS_TERM");
                    break;
                }
            }
        }

        // Graceful shutdown - Check this calls shutdown()
        self.transport.flush().await?;
        self.transport.close().await.map_err(Into::into)
    }

    async fn terminate(&mut self, mut msg: codec::SessionTermMessage) -> Result<(), Error> {
        // The remote end has started to end the session

        // Stop allowing more transfers
        self.rcv.close();

        // Drain the rcv channel
        while let Some(bundle) = self.rcv.recv().await {
            self.send(bundle).await?;
        }

        // Send our SESSION_TERM reply
        msg.message_flags.reply = true;
        self.transport
            .feed(codec::Message::SessionTerm(msg))
            .await?;

        // Wait for transfers to complete
        while !self.acks.is_empty() {
            match if self.keepalive_interval != 0 {
                // Read the next message with timeout
                tokio::time::timeout(
                    tokio::time::Duration::from_secs(self.keepalive_interval as u64)
                        .saturating_mul(2),
                    self.transport.next(),
                )
                .await
                .map_err(|_| Error::Timeout)?
            } else {
                self.transport.next().await
            } {
                Some(Ok(codec::Message::SessionTerm(_))) => {
                    /* Just ignore extra SESS_TERM */
                    let _ = self.unexpected(codec::MessageType::SESS_TERM).await;
                }
                Some(Ok(codec::Message::TransferSegment(msg))) if msg.message_flags.start => {
                    // Peer has started a new transfer in the 'Ending' state
                    self.transport
                        .send(codec::Message::TransferRefuse(
                            codec::TransferRefuseMessage {
                                transfer_id: msg.transfer_id,
                                reason_code: codec::TransferRefuseReasonCode::SessionTerminating,
                            },
                        ))
                        .await?;
                }
                msg => self.process_msg(msg).await?,
            }
        }

        // Close the connection
        self.transport.flush().await?;
        self.transport.close().await.map_err(Into::into)
    }

    async fn run(mut self) -> Result<(), Error> {
        // Different loops depending on if we need keepalives
        if self.keepalive_interval != 0 {
            let keepalive = tokio::time::Duration::from_secs(self.keepalive_interval as u64);
            loop {
                tokio::select! {
                    r = tokio::time::timeout(
                        keepalive.saturating_sub(self.last_sent.elapsed()),
                        self.rcv.recv(),
                    ) => match r {
                        Ok(Some(bundle)) => match self.send(bundle).await? {
                            SendResult::Ok => {},
                            SendResult::Terminate(msg) => return self.terminate(msg).await,
                            SendResult::Shutdown(code) => return self.shutdown(code).await,
                        }
                        Ok(None) => return self.shutdown(codec::SessionTermReasonCode::Unknown).await,
                        Err(_) => {
                            /* Nothing sent for a while, send a KEEP_ALIVE */
                            self.send_keepalive().await?;
                        }
                    },
                    msg = tokio::time::timeout(
                        keepalive.saturating_mul(2),
                        self.transport.next(),
                    ) => match msg {
                        Ok(Some(Ok(codec::Message::SessionTerm(msg)))) => return self.terminate(msg).await,
                        Ok(msg) => self.process_msg(msg).await?,
                        Err(_) => return Err(Error::Timeout),
                    },
                }
            }
        } else {
            loop {
                tokio::select! {
                    bundle = self.rcv.recv() => match bundle {
                        Some(bundle) => match self.send(bundle).await? {
                            SendResult::Ok => {},
                            SendResult::Terminate(msg) => return self.terminate(msg).await,
                            SendResult::Shutdown(code) => return self.shutdown(code).await,
                        }
                        None => return self.shutdown(codec::SessionTermReasonCode::Unknown).await,
                    },
                    msg = self.transport.next() => match msg {
                        Some(Ok(codec::Message::SessionTerm(msg))) => return self.terminate(msg).await,
                        msg => self.process_msg(msg).await?,
                    },
                }
            }
        }
    }
}

pub async fn new_passive<T>(
    config: Config,
    bpa: bpa::Bpa,
    addr: SocketAddr,
    segment_mtu: Option<usize>,
    mut transport: T,
    cancel_token: tokio_util::sync::CancellationToken,
) -> Result<(), Error>
where
    T: futures::StreamExt<Item = Result<codec::Message, codec::Error>>
        + futures::SinkExt<codec::Message>
        + std::marker::Unpin,
    session::Error: From<<T as futures::Sink<codec::Message>>::Error>,
{
    // Read the SESS_INIT message with timeout
    let peer_init = loop {
        match next_with_timeout(&mut transport, config.keepalive_interval * 2, &cancel_token)
            .await?
        {
            codec::Message::SessionInit(init) => break init,
            msg => {
                warn!("Unexpected message while waiting for SESS_INIT: {msg:?}");

                // Send a MSG_REJECT/Unexpected message
                transport
                    .send(codec::Message::Reject(codec::MessageRejectMessage {
                        reason_code: codec::MessageRejectionReasonCode::Unexpected,
                        rejected_message: codec::MessageType::from(msg) as u8,
                    }))
                    .await?;
            }
        };
    };

    // Send our SESS_INIT message
    transport
        .feed(codec::Message::SessionInit(codec::SessionInitMessage {
            keepalive_interval: config.keepalive_interval,
            segment_mru: config.segment_mru,
            transfer_mru: config.transfer_mru,
            node_id: config.node_id.clone(),
            ..Default::default()
        }))
        .await?;

    let keepalive_interval = peer_init.keepalive_interval.min(config.keepalive_interval);

    // Check peer init
    for i in &peer_init.session_extensions {
        if i.flags.critical {
            // We just don't support extensions!
            return terminate(
                &mut transport,
                codec::SessionTermMessage {
                    reason_code: codec::SessionTermReasonCode::ContactFailure,
                    ..Default::default()
                },
                keepalive_interval * 2,
                &cancel_token,
            )
            .await;
        }
    }

    let (send_request, recv_request) = channel::<Vec<u8>>(1);
    let (send_response, recv_response) =
        unbounded_channel::<Result<ForwardBundleResponse, tonic::Status>>();

    // Register the client for addr
    register_client(
        connection::new_client(send_request, recv_response),
        addr,
        peer_init.node_id,
    )
    .await?;

    // And finally process session messages
    let r = Session::new(
        transport,
        bpa,
        keepalive_interval,
        segment_mtu
            .map(|mtu| mtu.min(peer_init.segment_mru as usize))
            .unwrap_or(peer_init.segment_mru as usize),
        config.transfer_mru as usize,
        recv_request,
        send_response,
    )
    .run()
    .await
    .inspect(|_| trace!("Session with {addr} closed gracefully"))
    .inspect_err(|e| error!("Session with {addr} failed: {e}"));

    // Unregister the client for addr, whatever happens
    unregister_client(addr).await?;

    r
}

async fn register_client(
    _client: connection::Client,
    _addr: SocketAddr,
    _node_id: Option<bpv7::Eid>,
) -> Result<(), Error> {
    todo!()
}

async fn unregister_client(_addr: SocketAddr) -> Result<(), Error> {
    todo!()
}

pub async fn next_with_timeout<T>(
    transport: &mut T,
    timeout: u16,
    cancel_token: &tokio_util::sync::CancellationToken,
) -> Result<codec::Message, Error>
where
    T: futures::StreamExt<Item = Result<codec::Message, codec::Error>> + std::marker::Unpin,
{
    // Read the next message with timeout
    tokio::select! {
        r = tokio::time::timeout(
            tokio::time::Duration::from_secs(timeout as u64),
            transport.next(),
        ) => match r {
            Ok(Some(Ok(m))) => {
                Ok(m)
            }
            Ok(Some(Err(e))) => {
                Err(e.into())
            }
            Ok(None) => Err(Error::Hangup),
            Err(_) => Err(Error::Timeout)
        },
        _ = cancel_token.cancelled() => Err(Error::Cancelled)
    }
}

pub async fn terminate<T>(
    transport: &mut T,
    msg: codec::SessionTermMessage,
    timeout: u16,
    cancel_token: &tokio_util::sync::CancellationToken,
) -> Result<(), session::Error>
where
    T: futures::StreamExt<Item = Result<codec::Message, codec::Error>>
        + futures::SinkExt<codec::Message>
        + std::marker::Unpin,
    session::Error: From<<T as futures::Sink<codec::Message>>::Error>,
{
    let mut expected_reply = msg.clone();
    expected_reply.message_flags.reply = true;

    // Send the SESS_TERM message
    transport.send(codec::Message::SessionTerm(msg)).await?;

    // Read the SESS_TERM reply message with timeout
    loop {
        match session::next_with_timeout(transport, timeout, cancel_token).await? {
            codec::Message::SessionTerm(mut msg) => {
                if !msg.message_flags.reply {
                    // Terminations pass in the night...
                    msg.message_flags.reply = true;
                    transport.send(codec::Message::SessionTerm(msg)).await?;
                } else if msg != expected_reply {
                    trace!(
                        "Mismatched SESS_TERM message: {:?}, expected {:?}",
                        msg,
                        expected_reply
                    );
                }
                break;
            }
            msg => {
                warn!("Unexpected message while waiting for SESS_TERM reply: {msg:?}");

                // Send a MSG_REJECT/Unexpected message
                transport
                    .send(codec::Message::Reject(codec::MessageRejectMessage {
                        reason_code: codec::MessageRejectionReasonCode::Unexpected,
                        rejected_message: codec::MessageType::from(msg) as u8,
                    }))
                    .await?;
            }
        }
    }

    transport.close().await.map_err(Into::into)
}
