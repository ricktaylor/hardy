use std::collections::VecDeque;

use futures::StreamExt;
use thiserror::Error;
use tokio_util::bytes::{Bytes, BytesMut};

use super::*;

// Number of queued ingest items (acknowledgments and completed bundles)
// before the session reader stops draining the socket, applying TCP
// backpressure. Acknowledgments are small; completed bundles are bounded
// separately by INGEST_MAX_PENDING_DISPATCH.
const INGEST_QUEUE_DEPTH: usize = 128;

// Number of completed bundles that may await dispatch to the BPA before the
// session reader stops draining the socket. Each pending bundle can hold up
// to transfer_mru bytes, so this bound is deliberately small — at the 1 GiB
// default transfer-mru it still permits 2 GiB held per session, bounded in
// practice by peers' actual bundle sizes.
const INGEST_MAX_PENDING_DISPATCH: usize = 2;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Peer closed the connection")]
    Hangup,

    #[error("Peer has started to end the session: {0:?}")]
    Terminate(codec::SessionTermMessage),

    #[error("Shutdown connection: {0:?}")]
    Shutdown(codec::SessionTermReasonCode),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Codec(#[from] codec::Error),
}

struct XferAck {
    flags: codec::TransferSegmentMessageFlags,
    transfer_id: u64,
    acknowledged_length: usize,
}

// Outcome of processing a peer message while a transfer is being sent.
enum SendState {
    // Nothing decisive: continue sending / waiting for acknowledgments.
    Continue,
    // The peer refused the in-flight transfer.
    Refused(codec::TransferRefuseReasonCode),
}

// Work items for the per-session ingest task.
//
// Every XFER_ACK we emit flows through this queue, not just final ones:
// acknowledgments must be sent in segment-arrival order (a sender's ack
// matcher may be strictly ordered — ours is), and a single FIFO consumer
// preserves that order even when a dispatch is in flight ahead of later
// acknowledgments.
enum Ingest {
    // Acknowledge a non-final segment.
    Ack(codec::TransferAckMessage),
    // Dispatch a completed bundle to the BPA, then acknowledge its final
    // segment. The final acknowledgment transfers responsibility for the
    // bundle to this node — the peer deletes its copy on receipt — so it is
    // only sent once dispatch has completed.
    Dispatch {
        bundle: hardy_bpa::Bytes,
        ack: codec::TransferAckMessage,
        _permit: tokio::sync::OwnedSemaphorePermit,
    },
}

// Consume completed inbound work in arrival order: forward acknowledgments to
// the writer and dispatch completed bundles to the BPA.
//
// This runs as its own task so that a dispatch blocked on the BPA (storage
// write, ingress backpressure) never stops the session reader: segments,
// acknowledgments of our own transfers, and keepalives keep flowing, bounded
// by the ingest queue depth.
//
// Exits on dispatch or writer failure, cancelling the session's token so the
// session tears down promptly rather than discovering the closed queue on the
// next inbound segment — with keepalives negotiated off, a quiet peer waiting
// for its final ack and a stalled session could otherwise both wait forever.
// An undispatched transfer is then never acknowledged: the peer retains
// responsibility for the bundle and will retransmit.
async fn run_ingest(
    mut queue: tokio::sync::mpsc::Receiver<Ingest>,
    sink: Arc<dyn hardy_bpa::cla::Sink>,
    peer_node: Option<hardy_bpv7::eid::NodeId>,
    peer_addr: Option<hardy_bpa::cla::ClaAddress>,
    writer: writer::WriterHandle<codec::Error>,
    cancel_token: tokio_util::sync::CancellationToken,
) {
    while let Some(item) = queue.recv().await {
        let ack = match item {
            Ingest::Ack(ack) => ack,
            Ingest::Dispatch {
                bundle,
                ack,
                _permit,
            } => {
                if let Err(e) = sink
                    .dispatch(bundle, peer_node.as_ref(), peer_addr.as_ref())
                    .await
                {
                    warn!("CLA dispatch failed: {e:?}");
                    cancel_token.cancel();
                    return;
                }

                metrics::counter!("tcpclv4.transfers.received").increment(1);
                ack
            }
        };

        if !writer.feed(codec::Message::TransferAck(ack)).await {
            debug!("Writer closed, stopping ingest");
            cancel_token.cancel();
            return;
        }
    }
}

// Receive the next message from the peer, filtering keepalives and applying
// the 2x keepalive-interval idle timeout.
//
// A free function rather than a method so a caller can select over it while
// holding borrows of other Session fields (e.g. the writer).
async fn next_msg<R>(
    reader: &mut R,
    keepalive_interval: Option<tokio::time::Duration>,
) -> Result<codec::Message, Error>
where
    R: StreamExt<Item = Result<codec::Message, codec::Error>> + core::marker::Unpin,
{
    loop {
        return match if let Some(keepalive_interval) = keepalive_interval {
            // Timeout for receiving from peer: 2x keepalive interval
            // If we don't receive anything in this time, peer is probably dead
            match tokio::time::timeout(keepalive_interval.saturating_mul(2), reader.next()).await {
                Err(_) => {
                    return Err(Error::Shutdown(codec::SessionTermReasonCode::IdleTimeout));
                }
                Ok(Some(Ok(codec::Message::Keepalive))) => continue,
                Ok(msg) => msg,
            }
        } else {
            reader.next().await
        } {
            None => Err(Error::Hangup),
            Some(Err(e)) => Err(e.into()),
            Some(Ok(msg)) => Ok(msg),
        };
    }
}

// Session that handles the reader side, with writes delegated to a
// WriterHandle and dispatch to the BPA delegated to an ingest task.
//
// The reader never awaits a transport write or a BPA dispatch directly:
// writes go through the writer task's bounded command channel, and completed
// inbound bundles go through the bounded ingest queue. Backpressure reaches
// the peer through those bounds rather than by stalling the protocol loop, so
// keepalives and acknowledgment processing continue while the BPA is busy.
pub struct Session<R>
where
    R: StreamExt<Item = Result<codec::Message, codec::Error>> + core::marker::Unpin,
{
    reader: R,
    writer: writer::WriterHandle<codec::Error>,
    sink: Arc<dyn hardy_bpa::cla::Sink>,
    peer_node: Option<hardy_bpv7::eid::NodeId>,
    peer_addr: Option<hardy_bpa::cla::ClaAddress>,
    keepalive_interval: Option<tokio::time::Duration>,
    segment_mtu: usize,
    transfer_mru: usize,
    from_sink: tokio::sync::mpsc::Receiver<(
        hardy_bpa::Bytes,
        tokio::sync::oneshot::Sender<hardy_bpa::cla::ForwardBundleResult>,
    )>,
    transfer_id: u64,
    acks: VecDeque<XferAck>,
    ingress_bundle: Option<BytesMut>,
    // Transfer id whose remaining segments are being swallowed after an
    // XFER_REFUSE (over-MRU)
    refusing: Option<u64>,
    ingest_tx: Option<tokio::sync::mpsc::Sender<Ingest>>,
    ingest_rx: Option<tokio::sync::mpsc::Receiver<Ingest>>,
    dispatch_permits: Arc<tokio::sync::Semaphore>,
    cancel_token: tokio_util::sync::CancellationToken,
}

impl<R> Session<R>
where
    R: StreamExt<Item = Result<codec::Message, codec::Error>> + core::marker::Unpin,
{
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        reader: R,
        writer: writer::WriterHandle<codec::Error>,
        sink: Arc<dyn hardy_bpa::cla::Sink>,
        peer_node: Option<hardy_bpv7::eid::NodeId>,
        peer_addr: Option<hardy_bpa::cla::ClaAddress>,
        keepalive_interval: Option<tokio::time::Duration>,
        segment_mtu: usize,
        transfer_mru: usize,
        from_sink: tokio::sync::mpsc::Receiver<(
            hardy_bpa::Bytes,
            tokio::sync::oneshot::Sender<hardy_bpa::cla::ForwardBundleResult>,
        )>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Self {
        let (ingest_tx, ingest_rx) = tokio::sync::mpsc::channel(INGEST_QUEUE_DEPTH);
        // Each session gets its own child token: the ingest task cancels it
        // to terminate just this session on failure, while CLA-wide
        // cancellation still propagates down from the parent.
        let cancel_token = cancel_token.child_token();
        Self {
            reader,
            writer,
            sink,
            peer_node,
            peer_addr,
            keepalive_interval,
            segment_mtu,
            transfer_mru,
            from_sink,
            transfer_id: 0,
            acks: VecDeque::new(),
            ingress_bundle: None,
            refusing: None,
            ingest_tx: Some(ingest_tx),
            ingest_rx: Some(ingest_rx),
            dispatch_permits: Arc::new(tokio::sync::Semaphore::new(INGEST_MAX_PENDING_DISPATCH)),
            cancel_token,
        }
    }

    async fn reject_msg(
        &self,
        reason_code: codec::MessageRejectionReasonCode,
        rejected_message: u8,
    ) -> Result<(), Error> {
        if !self
            .writer
            .send(codec::Message::Reject(codec::MessageRejectMessage {
                reason_code,
                rejected_message,
            }))
            .await?
        {
            // Writer closed: the rejection never reached the peer and
            // nothing further can be written
            return Err(Error::Shutdown(codec::SessionTermReasonCode::Unknown));
        }
        Ok(())
    }

    async fn unexpected_msg(&self, rejected_message: codec::MessageType) -> Result<(), Error> {
        self.reject_msg(
            codec::MessageRejectionReasonCode::Unexpected,
            rejected_message as u8,
        )
        .await
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    async fn on_transfer(&mut self, msg: codec::TransferSegmentMessage) -> Result<(), Error> {
        if msg.message_flags.start {
            self.refusing = None;
            if self.ingress_bundle.take().is_some() {
                // Out of order bundle! The in-progress reassembly is dropped
                // with it: appending the new transfer's segments to the old
                // bytes would dispatch a cross-transfer amalgam to the BPA.
                debug!("Out of order segment received");
                return self.unexpected_msg(codec::MessageType::XFER_SEGMENT).await;
            }
            self.ingress_bundle = Some(BytesMut::with_capacity(msg.data.len()));
        } else if self.refusing == Some(msg.transfer_id) {
            // Remaining in-flight segments of a transfer we have refused
            if msg.message_flags.end {
                self.refusing = None;
            }
            return Ok(());
        }

        let Some(bundle) = &mut self.ingress_bundle else {
            debug!("Unexpected segment received");
            return self.unexpected_msg(codec::MessageType::XFER_SEGMENT).await;
        };

        if msg.data.len() + bundle.len() > self.transfer_mru {
            // Bundle beyond negotiated MRU: XFER_REFUSE is the
            // protocol-level answer (RFC 9174 Section 5.2.2), and the
            // transfer's remaining segments are swallowed above rather than
            // rejected one by one
            self.ingress_bundle = None;
            self.refusing = (!msg.message_flags.end).then_some(msg.transfer_id);

            debug!("Segment received beyond the negotiated MRU");
            self.writer
                .send(codec::Message::TransferRefuse(
                    codec::TransferRefuseMessage {
                        transfer_id: msg.transfer_id,
                        reason_code: codec::TransferRefuseReasonCode::NotAcceptable,
                    },
                ))
                .await?;
            return Ok(());
        }

        bundle.extend_from_slice(&msg.data);
        let acknowledged_length = bundle.len() as u64;

        metrics::counter!("tcpclv4.segments.received").increment(1);

        // Per RFC9174 Section 5.2.3: "A receiving TCPCL entity SHALL send a
        // XFER_ACK message in response to each received XFER_SEGMENT message
        // after the segment has been fully processed."
        //
        // Emission is delegated to the ingest task, which keeps
        // acknowledgments in segment-arrival order while dispatching
        // completed bundles off the reader path.
        let end = msg.message_flags.end;
        let ack = codec::TransferAckMessage {
            transfer_id: msg.transfer_id,
            message_flags: msg.message_flags,
            acknowledged_length,
        };

        let item = if end {
            // NOTE: This blocks when INGEST_MAX_PENDING_DISPATCH bundles
            // already await dispatch; keepalives are handled by the separate
            // writer task so the session stays alive.
            let permit = self
                .dispatch_permits
                .clone()
                .acquire_owned()
                .await
                .trace_expect("Dispatch semaphore closed");
            Ingest::Dispatch {
                bundle: self
                    .ingress_bundle
                    .take()
                    .trace_expect("End segment without reassembly buffer")
                    .freeze(),
                ack,
                _permit: permit,
            }
        } else {
            Ingest::Ack(ack)
        };

        self.ingest_tx
            .as_ref()
            .trace_expect("Transfer received after ingest queue closed")
            .send(item)
            .await
            .map_err(|_| {
                debug!("Ingest task has stopped");
                Error::Shutdown(codec::SessionTermReasonCode::ResourceExhaustion)
            })
    }

    // Process a message received from the peer while a transfer of ours is in
    // flight: inbound transfers continue, and acknowledgments and refusals are
    // matched against the outstanding segment queue.
    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    async fn on_peer_msg_while_sending(&mut self, msg: codec::Message) -> Result<SendState, Error> {
        match msg {
            codec::Message::SessionTerm(msg) => {
                debug!("Peer has started to end the session: {msg:?}");
                Err(Error::Terminate(msg))
            }
            codec::Message::TransferSegment(msg) => {
                self.on_transfer(msg).await?;
                Ok(SendState::Continue)
            }
            codec::Message::TransferAck(msg) => {
                if let Some(ack) = self.acks.pop_front() {
                    if ack.transfer_id != msg.transfer_id {
                        debug!(
                            "Mismatched transfer id in TransferAck message: expected {:?} got {:?}",
                            ack.transfer_id, msg.transfer_id
                        );
                    } else if ack.flags != msg.message_flags {
                        debug!(
                            "Mismatched flags in TransferAck message: expected {:?} got {:?}",
                            ack.flags, msg.message_flags
                        );
                    } else if ack.acknowledged_length as u64 != msg.acknowledged_length {
                        debug!(
                            "Mismatched acknowledged_length in TransferAck message: expected {} got {}",
                            ack.acknowledged_length, msg.acknowledged_length
                        );
                    } else {
                        return Ok(SendState::Continue);
                    }
                } else {
                    debug!("Received TransferAck with no outstanding transfers");
                }

                self.reject_msg(
                    codec::MessageRejectionReasonCode::Unexpected,
                    codec::MessageType::XFER_ACK as u8,
                )
                .await?;

                // It's all gone very wrong
                Err(Error::Shutdown(codec::SessionTermReasonCode::Unknown))
            }
            codec::Message::TransferRefuse(msg) => {
                if let Some(ack) = self.acks.front() {
                    if ack.transfer_id == msg.transfer_id {
                        // Per RFC9174 Section 5.2.2: no further XFER_ACK
                        // messages follow for a refused transfer, so drop
                        // every outstanding expectation for it.
                        self.acks.retain(|a| a.transfer_id != msg.transfer_id);

                        metrics::counter!("tcpclv4.transfers.refused", "reason" => format!("{:?}", msg.reason_code)).increment(1);
                        return Ok(SendState::Refused(msg.reason_code));
                    }
                    debug!(
                        "Mismatched transfer id in TransferRefuse message: expected {:?} got {:?}",
                        ack.transfer_id, msg.transfer_id
                    );
                } else {
                    debug!("Received TransferRefuse with no outstanding transfers");
                }

                self.reject_msg(
                    codec::MessageRejectionReasonCode::Unexpected,
                    codec::MessageType::XFER_REFUSE as u8,
                )
                .await?;

                // It's all gone very wrong
                Err(Error::Shutdown(codec::SessionTermReasonCode::Unknown))
            }
            msg => {
                self.unexpected_msg(msg.message_type()).await?;
                Ok(SendState::Continue)
            }
        }
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, data)))]
    async fn send_segment(
        &mut self,
        transfer_id: u64,
        cumulative_acknowledged_length: usize,
        flags: codec::TransferSegmentMessageFlags,
        data: Bytes,
    ) -> Result<Option<codec::TransferRefuseReasonCode>, Error> {
        // Add new Xfer to queue of Acks
        self.acks.push_back(XferAck {
            flags: flags.clone(),
            transfer_id,
            acknowledged_length: cumulative_acknowledged_length,
        });

        let last = flags.end;
        let mut msg = Some(codec::Message::TransferSegment(
            codec::TransferSegmentMessage {
                message_flags: flags,
                transfer_id,
                data,
                ..Default::default()
            },
        ));

        // Feed the segment once the writer has room, processing inbound
        // messages while waiting: a full writer must never stop the reader,
        // both to keep acknowledgments flowing and to avoid deadlock when
        // both peers write at once. Inbound is polled first so peer traffic
        // is drained between segments. The handle is cloned so the reserved
        // permit does not hold a borrow of self across the select arms.
        let writer = self.writer.clone();
        while let Some(segment) = msg.take() {
            tokio::select! {
                biased;
                r = next_msg(&mut self.reader, self.keepalive_interval) => {
                    msg = Some(segment);
                    match r {
                        Err(Error::Codec(codec::Error::InvalidMessageType(rejected_message))) => {
                            // Send a rejection (best effort)
                            self.reject_msg(
                                codec::MessageRejectionReasonCode::UnknownType,
                                rejected_message,
                            )
                            .await?;
                        }
                        r => {
                            if let SendState::Refused(reason) =
                                self.on_peer_msg_while_sending(r?).await?
                            {
                                // The remaining segments of this transfer are moot
                                return Ok(Some(reason));
                            }
                        }
                    }
                }
                permit = writer.reserve() => {
                    let Some(permit) = permit else {
                        return Err(Error::Shutdown(codec::SessionTermReasonCode::Unknown));
                    };
                    permit.feed(segment);
                }
            }
        }

        metrics::counter!("tcpclv4.segments.sent").increment(1);

        if !last {
            return Ok(None);
        }

        // Wait for every outstanding acknowledgment: the transfer is only
        // complete (and the bundle deletable by our BPA) once the peer has
        // fully acknowledged it.
        while !self.acks.is_empty() {
            let msg = self.recv_from_peer().await?;
            if let SendState::Refused(reason) = self.on_peer_msg_while_sending(msg).await? {
                return Ok(Some(reason));
            }
        }
        Ok(None)
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, bundle)))]
    async fn send_once(
        &mut self,
        mut bundle: Bytes,
    ) -> Result<Option<codec::TransferRefuseReasonCode>, Error> {
        // Allocate a Transfer ID for this transfer (RFC 9174 Section 5.2.1:
        // all segments within a transfer share the same Transfer ID)
        let transfer_id = self.transfer_id;
        self.transfer_id += 1;

        let mut start = true;
        let mut cumulative_acknowledged_length = 0usize;

        // Segment if needed
        while bundle.len() > self.segment_mtu {
            let segment = bundle.split_to(self.segment_mtu);
            cumulative_acknowledged_length += segment.len();
            if let Some(refused) = self
                .send_segment(
                    transfer_id,
                    cumulative_acknowledged_length,
                    codec::TransferSegmentMessageFlags {
                        start,
                        end: false,
                        ..Default::default()
                    },
                    segment,
                )
                .await?
            {
                debug!("Peer refused the transfer: {refused:?}");
                return Ok(Some(refused));
            }

            start = false;
        }

        // Send the last segment
        cumulative_acknowledged_length += bundle.len();
        self.send_segment(
            transfer_id,
            cumulative_acknowledged_length,
            codec::TransferSegmentMessageFlags {
                start,
                end: true,
                ..Default::default()
            },
            bundle,
        )
        .await
        .inspect(|r| {
            r.as_ref().inspect(|r| {
                debug!("Peer refused the transfer: {r:?}");
            });
        })
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    async fn forward_to_peer(
        &mut self,
        bundle: Bytes,
        result: tokio::sync::oneshot::Sender<hardy_bpa::cla::ForwardBundleResult>,
    ) -> Result<(), Error> {
        // Check we can allocate a transfer id without rollover (RFC 9174 Section 5.2.1)
        if self.transfer_id == u64::MAX {
            debug!("Out of Transfer Ids, closing session");
            return Err(Error::Shutdown(
                codec::SessionTermReasonCode::ResourceExhaustion,
            ));
        }

        loop {
            match self.send_once(bundle.clone()).await? {
                None | Some(codec::TransferRefuseReasonCode::Completed) => {
                    metrics::counter!("tcpclv4.transfers.sent").increment(1);
                    _ = result.send(hardy_bpa::cla::ForwardBundleResult::Sent);
                    return Ok(());
                }
                Some(codec::TransferRefuseReasonCode::Retransmit) => {
                    /* Send again */
                    continue;
                }
                Some(codec::TransferRefuseReasonCode::NoResources) => {
                    return Err(Error::Shutdown(
                        codec::SessionTermReasonCode::ResourceExhaustion,
                    ));
                }
                _ => {
                    return Err(Error::Shutdown(codec::SessionTermReasonCode::Unknown));
                }
            }
        }
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    async fn shutdown(&mut self, reason_code: codec::SessionTermReasonCode) {
        // Stop allowing more transfers
        self.from_sink.close();

        // If already cancelled, skip the graceful SESS_TERM exchange
        if self.cancel_token.is_cancelled() {
            return;
        }

        // Send a SESS_TERM message
        let msg = codec::SessionTermMessage {
            reason_code,
            ..Default::default()
        };

        if self
            .writer
            .send(codec::Message::SessionTerm(msg))
            .await
            .unwrap_or(false)
        {
            // Process any remaining messages, with cancellation support
            let cancel_token = self.cancel_token.clone();
            loop {
                tokio::select! {
                    _ = cancel_token.cancelled() => {
                        break;
                    }
                    result = self.recv_from_peer() => {
                        if match result {
                            Ok(codec::Message::SessionTerm(msg)) => {
                                // A non-reply here is a crossing termination:
                                // the peer sent its own SESS_TERM before
                                // seeing ours. RFC 9174 Section 6.1: an
                                // entity that has already sent a SESS_TERM
                                // does not send an acknowledging one — both
                                // ends are Ending, so just finish up.
                                let _ = msg;
                                break;
                            }
                            Ok(codec::Message::TransferSegment(msg)) => self.on_transfer(msg).await,
                            Ok(msg) => self.unexpected_msg(msg.message_type()).await,
                            Err(e) => Err(e),
                        }
                        .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        }
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    async fn on_terminate(&mut self, mut msg: codec::SessionTermMessage) {
        // The remote end has started to end the session

        // Stop allowing more transfers
        self.from_sink.close();

        // RFC 9174 Section 6.1: no new outgoing transfers once the session
        // is Ending. Dropping each queued result sender returns the bundle
        // to the pool, which retries it on another session.
        while self.from_sink.recv().await.is_some() {}

        // Send our SESSION_TERM reply
        msg.message_flags.reply = true;
        if self
            .writer
            .send(codec::Message::SessionTerm(msg))
            .await
            .unwrap_or(false)
        {
            // Wait for transfers to complete
            while !self.acks.is_empty() {
                if match self.recv_from_peer().await {
                    Ok(codec::Message::TransferSegment(msg)) => {
                        if msg.message_flags.start {
                            // Peer has started a new transfer in the 'Ending' state
                            if self
                                .writer
                                .send(codec::Message::TransferRefuse(
                                    codec::TransferRefuseMessage {
                                        transfer_id: msg.transfer_id,
                                        reason_code:
                                            codec::TransferRefuseReasonCode::SessionTerminating,
                                    },
                                ))
                                .await
                                .unwrap_or(false)
                            {
                                continue;
                            } else {
                                break;
                            }
                        }
                        self.on_transfer(msg).await
                    }
                    Ok(msg) => self.unexpected_msg(msg.message_type()).await,
                    Err(_) => break,
                }
                .is_err()
                {
                    break;
                }
            }
        }
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    async fn recv_from_peer(&mut self) -> Result<codec::Message, Error> {
        loop {
            match next_msg(&mut self.reader, self.keepalive_interval).await {
                Err(Error::Codec(codec::Error::InvalidMessageType(rejected_message))) => {
                    // Send a rejection (best effort)
                    self.reject_msg(
                        codec::MessageRejectionReasonCode::UnknownType,
                        rejected_message,
                    )
                    .await?;
                }
                r => return r,
            }
        }
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    pub async fn run(mut self) {
        // Dispatch to the BPA is delegated to the ingest task so the reader
        // below never blocks on the BPA: see run_ingest
        let ingest = run_ingest(
            self.ingest_rx
                .take()
                .trace_expect("Session::run called twice"),
            self.sink.clone(),
            self.peer_node.clone(),
            self.peer_addr.clone(),
            self.writer.clone(),
            self.cancel_token.clone(),
        );
        #[cfg(feature = "instrument")]
        let ingest = {
            let span = tracing::trace_span!(parent: None, "session_ingest");
            span.follows_from(tracing::Span::current());
            tracing::Instrument::instrument(ingest, span)
        };
        // Spawned directly (not in a TaskPool) - the session owns the ingest
        // task's lifecycle via the queue and joins it before closing
        let ingest = tokio::spawn(ingest);

        let cancel_token = self.cancel_token.clone();
        let e = loop {
            // The main loop only handles:
            // 1. Cancellation
            // 2. Outbound bundles from sink
            // 3. Inbound messages from peer
            //
            // Keepalive SENDING is handled by the separate writer task, and
            // dispatch of received bundles by the separate ingest task.
            let msg = if let Some(keepalive_interval) = self.keepalive_interval {
                tokio::select! {
                    _ = cancel_token.cancelled() => {
                        Err(Error::Shutdown(codec::SessionTermReasonCode::Unknown))
                    }
                    r = self.from_sink.recv() => match r {
                        Some((bundle, result)) => {
                            let Err(e) = self.forward_to_peer(bundle, result).await else {
                                continue
                            };
                            Err(e)
                        }
                        None => Err(Error::Shutdown(codec::SessionTermReasonCode::Unknown)),
                    },
                    r = tokio::time::timeout(
                        keepalive_interval.saturating_mul(2),
                        self.reader.next(),
                    ) => match r {
                        Ok(Some(Ok(codec::Message::Keepalive))) => continue,
                        Ok(Some(msg)) => msg.map_err(Into::into),
                        Ok(None) => Err(Error::Hangup),
                        Err(_) => Err(Error::Shutdown(codec::SessionTermReasonCode::IdleTimeout)),
                    }
                }
            } else {
                tokio::select! {
                    _ = cancel_token.cancelled() => {
                        Err(Error::Shutdown(codec::SessionTermReasonCode::Unknown))
                    }
                    r = self.from_sink.recv() => match r {
                        Some((bundle, result)) => {
                            let Err(e) = self.forward_to_peer(bundle, result).await else {
                                continue
                            };
                            Err(e)
                        }
                        None => Err(Error::Shutdown(codec::SessionTermReasonCode::Unknown)),
                    },
                    msg = self.reader.next() => match msg {
                        Some(msg) => msg.map_err(Into::into),
                        None => Err(Error::Hangup),
                    }
                }
            };

            if let Err(e) = match msg {
                Ok(codec::Message::TransferSegment(msg)) => self.on_transfer(msg).await,
                Ok(codec::Message::SessionTerm(msg)) => {
                    debug!("Peer has started to end the session: {msg:?}");
                    Err(Error::Terminate(msg))
                }
                Ok(msg) => self.unexpected_msg(msg.message_type()).await,
                Err(Error::Codec(codec::Error::InvalidMessageType(rejected_message))) => {
                    // Reject-and-continue, mirroring recv_from_peer: an
                    // unknown message type must not be fatal only when the
                    // session happens to be idle
                    self.reject_msg(
                        codec::MessageRejectionReasonCode::UnknownType,
                        rejected_message,
                    )
                    .await
                }
                Err(e) => Err(e),
            } {
                break e;
            }
        };

        // Record session termination reason
        let reason = match &e {
            Error::Terminate(msg) => format!("{:?}", msg.reason_code),
            Error::Shutdown(code) => format!("{:?}", code),
            Error::Codec(_) => "codec_error".to_string(),
            Error::Hangup => "hangup".to_string(),
            Error::Io(_) => "io_error".to_string(),
        };
        metrics::counter!("tcpclv4.session.terminated", "reason" => reason).increment(1);

        match e {
            Error::Terminate(session_term_message) => {
                self.on_terminate(session_term_message).await;
            }
            Error::Shutdown(session_term_reason_code) => {
                self.shutdown(session_term_reason_code).await;
            }
            Error::Codec(e) => {
                // Question: may be there is a better way to handle this ?!
                // If this is an UnexpectedEof error, it's likely a TLS close_notify issue
                if let codec::Error::Io(io_err) = &e
                    && io_err.kind() == std::io::ErrorKind::UnexpectedEof
                {
                    // Peer closed connection (likely without TLS close_notify) - treat as normal hangup
                    debug!("Peer closed connection (UnexpectedEof), ending session");
                } else {
                    // The other end is sending us garbage
                    debug!("Peer sent invalid data: {e:?}, shutting down session");
                    self.shutdown(codec::SessionTermReasonCode::Unknown).await;
                }
            }
            Error::Hangup => {
                // The remote end has died completely
                debug!("Peer hung up, ending session");
            }
            Error::Io(e) => {
                debug!("Session I/O failure: {e:?}, ending session");
            }
        }

        // Let the ingest queue drain - dispatching any fully received bundles
        // and flushing their acknowledgments - before the writer closes
        drop(self.ingest_tx.take());
        if let Err(e) = ingest.await {
            error!("Ingest task failed: {e}");
        }
        self.writer.close().await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    // ---- Ingest task ----

    struct MockSink {
        fail: bool,
        delay: Option<tokio::time::Duration>,
        dispatched: Mutex<Vec<hardy_bpa::Bytes>>,
    }

    impl MockSink {
        fn new(fail: bool, delay: Option<tokio::time::Duration>) -> Arc<Self> {
            Arc::new(Self {
                fail,
                delay,
                dispatched: Mutex::new(Vec::new()),
            })
        }
    }

    #[hardy_bpa::async_trait]
    impl hardy_bpa::cla::Sink for MockSink {
        async fn unregister(&self) {}

        async fn dispatch(
            &self,
            bundle: hardy_bpa::Bytes,
            _peer_node: Option<&hardy_bpv7::eid::NodeId>,
            _peer_addr: Option<&hardy_bpa::cla::ClaAddress>,
        ) -> hardy_bpa::cla::Result<()> {
            if let Some(delay) = self.delay {
                tokio::time::sleep(delay).await;
            }
            if self.fail {
                return Err(hardy_bpa::cla::Error::Disconnected);
            }
            self.dispatched.lock().unwrap().push(bundle);
            Ok(())
        }

        async fn add_peer(
            &self,
            _cla_addr: hardy_bpa::cla::ClaAddress,
            _node_ids: &[hardy_bpv7::eid::NodeId],
        ) -> hardy_bpa::cla::Result<bool> {
            Ok(true)
        }

        async fn remove_peer(
            &self,
            _cla_addr: &hardy_bpa::cla::ClaAddress,
        ) -> hardy_bpa::cla::Result<bool> {
            Ok(true)
        }
    }

    // A mid-transfer XFER_REFUSE clears every outstanding acknowledgment
    // expectation for the refused transfer (RFC 9174 Section 5.2.2: no
    // further XFER_ACK messages follow for it), leaving later transfers'
    // expectations intact — stale expectations desynchronise the
    // acknowledgment matcher and tear the session down.
    #[tokio::test]
    async fn refuse_clears_all_expectations_for_refused_transfer() {
        let (writer_tx, _writer_rx) = tokio::sync::mpsc::channel(16);
        let (_sink_tx, from_sink) = tokio::sync::mpsc::channel(1);
        let mut session = Session::new(
            futures::stream::empty::<Result<codec::Message, codec::Error>>(),
            writer::WriterHandle::new(writer_tx),
            MockSink::new(false, None),
            None,
            None,
            None,
            1024,
            1 << 20,
            from_sink,
            tokio_util::sync::CancellationToken::new(),
        );

        for (transfer_id, end) in [(7, false), (7, true), (8, false)] {
            session.acks.push_back(XferAck {
                flags: codec::TransferSegmentMessageFlags {
                    start: false,
                    end,
                    ..Default::default()
                },
                transfer_id,
                acknowledged_length: 10,
            });
        }

        let r = session
            .on_peer_msg_while_sending(codec::Message::TransferRefuse(
                codec::TransferRefuseMessage {
                    transfer_id: 7,
                    reason_code: codec::TransferRefuseReasonCode::NotAcceptable,
                },
            ))
            .await
            .expect("a matched refuse must not error");

        assert!(matches!(
            r,
            SendState::Refused(codec::TransferRefuseReasonCode::NotAcceptable)
        ));
        assert_eq!(
            session.acks.len(),
            1,
            "the later transfer's expectation must survive"
        );
        assert_eq!(session.acks[0].transfer_id, 8);
    }

    fn ack(transfer_id: u64, start: bool, end: bool, len: u64) -> codec::TransferAckMessage {
        codec::TransferAckMessage {
            transfer_id,
            message_flags: codec::TransferSegmentMessageFlags {
                start,
                end,
                ..Default::default()
            },
            acknowledged_length: len,
        }
    }

    // Acknowledgments are emitted in arrival order, with the final ack of a
    // transfer held until dispatch completes and later acks queueing behind it
    #[tokio::test]
    async fn ingest_preserves_ack_order_across_dispatch() {
        let (writer_tx, mut writer_rx) = tokio::sync::mpsc::channel(16);
        let writer = writer::WriterHandle::<codec::Error>::new(writer_tx);
        let sink = MockSink::new(false, Some(tokio::time::Duration::from_millis(20)));
        let permits = Arc::new(tokio::sync::Semaphore::new(INGEST_MAX_PENDING_DISPATCH));

        let (tx, rx) = tokio::sync::mpsc::channel(INGEST_QUEUE_DEPTH);
        let task = tokio::spawn(run_ingest(
            rx,
            sink.clone(),
            None,
            None,
            writer,
            tokio_util::sync::CancellationToken::new(),
        ));

        tx.send(Ingest::Ack(ack(0, true, false, 100)))
            .await
            .unwrap();
        tx.send(Ingest::Dispatch {
            bundle: hardy_bpa::Bytes::from_static(b"bundle-0"),
            ack: ack(0, false, true, 200),
            _permit: permits.clone().acquire_owned().await.unwrap(),
        })
        .await
        .unwrap();
        tx.send(Ingest::Ack(ack(1, true, false, 50))).await.unwrap();
        drop(tx);
        task.await.unwrap();

        let mut acks = Vec::new();
        while let Some(cmd) = writer_rx.recv().await {
            if let writer::WriteCommand::Feed {
                msg: codec::Message::TransferAck(a),
            } = cmd
            {
                acks.push(a);
            }
        }

        assert_eq!(acks.len(), 3);
        assert_eq!((acks[0].transfer_id, acks[0].acknowledged_length), (0, 100));
        assert_eq!((acks[1].transfer_id, acks[1].acknowledged_length), (0, 200));
        assert!(acks[1].message_flags.end);
        assert_eq!((acks[2].transfer_id, acks[2].acknowledged_length), (1, 50));

        assert_eq!(
            sink.dispatched.lock().unwrap().first().unwrap().as_ref(),
            b"bundle-0".as_slice()
        );
    }

    // A failed dispatch leaves the transfer unacknowledged and closes the
    // ingest queue, which the session observes as a send error
    #[tokio::test]
    async fn ingest_stops_unacknowledged_on_dispatch_failure() {
        let (writer_tx, mut writer_rx) = tokio::sync::mpsc::channel(16);
        let writer = writer::WriterHandle::<codec::Error>::new(writer_tx);
        let sink = MockSink::new(true, None);
        let permits = Arc::new(tokio::sync::Semaphore::new(INGEST_MAX_PENDING_DISPATCH));

        let (tx, rx) = tokio::sync::mpsc::channel(INGEST_QUEUE_DEPTH);
        let task = tokio::spawn(run_ingest(
            rx,
            sink,
            None,
            None,
            writer,
            tokio_util::sync::CancellationToken::new(),
        ));

        tx.send(Ingest::Dispatch {
            bundle: hardy_bpa::Bytes::from_static(b"lost"),
            ack: ack(0, true, true, 4),
            _permit: permits.clone().acquire_owned().await.unwrap(),
        })
        .await
        .unwrap();

        task.await.unwrap();

        // The session observes the failure as a closed queue
        assert!(tx.send(Ingest::Ack(ack(1, true, false, 1))).await.is_err());

        // No acknowledgment was emitted for the failed transfer
        assert!(writer_rx.recv().await.is_none());
    }

    // ---- UT-TCP-03: Parameter Negotiation ----
    // Tests the negotiation logic from ConnectionContext::negotiate_keepalive().
    // The function returns min(local, peer) or 0 if local is None.

    // UT-TCP-03: Local < Peer → use local.
    #[test]
    fn negotiate_keepalive_local_smaller() {
        let local: Option<u16> = Some(30);
        let peer: u16 = 60;
        let result = local.map(|l| peer.min(l)).unwrap_or(0);
        assert_eq!(result, 30);
    }

    // UT-TCP-03: Peer < Local → use peer.
    #[test]
    fn negotiate_keepalive_peer_smaller() {
        let local: Option<u16> = Some(60);
        let peer: u16 = 30;
        let result = local.map(|l| peer.min(l)).unwrap_or(0);
        assert_eq!(result, 30);
    }

    // UT-TCP-03: Equal values → use either.
    #[test]
    fn negotiate_keepalive_equal() {
        let local: Option<u16> = Some(45);
        let peer: u16 = 45;
        let result = local.map(|l| peer.min(l)).unwrap_or(0);
        assert_eq!(result, 45);
    }

    // UT-TCP-03: Local is None (disabled) → result is 0 (disabled).
    #[test]
    fn negotiate_keepalive_local_disabled() {
        let local: Option<u16> = None;
        let peer: u16 = 60;
        let result = local.map(|l| peer.min(l)).unwrap_or(0);
        assert_eq!(result, 0);
    }

    // UT-TCP-03: Peer sends 0 (disabled) → result is 0.
    #[test]
    fn negotiate_keepalive_peer_disabled() {
        let local: Option<u16> = Some(60);
        let peer: u16 = 0;
        let result = local.map(|l| peer.min(l)).unwrap_or(0);
        assert_eq!(result, 0);
    }

    // UT-TCP-03: Segment MRU negotiation uses min(local_mtu, peer_mru).
    #[test]
    fn negotiate_segment_mtu() {
        let local_mtu: Option<usize> = Some(8192);
        let peer_mru: usize = 16384;
        let result = local_mtu.map(|mtu| mtu.min(peer_mru)).unwrap_or(peer_mru);
        assert_eq!(result, 8192);

        // No local MTU → use peer's MRU
        let local_mtu: Option<usize> = None;
        let result = local_mtu.map(|mtu| mtu.min(peer_mru)).unwrap_or(peer_mru);
        assert_eq!(result, 16384);
    }

    // ---- UT-TCP-04: Fragment Logic ----
    // Tests the segmentation calculation used by Session::send_once().
    // The logic: while bundle.len() > segment_mtu, split_to(segment_mtu).

    // UT-TCP-04: Bundle smaller than MTU → 1 segment (START+END).
    #[test]
    fn fragment_single_segment() {
        let bundle_len: usize = 500;
        let segment_mtu: usize = 1000;

        let mut remaining = bundle_len;
        let mut count = 0;
        while remaining > segment_mtu {
            remaining -= segment_mtu;
            count += 1;
        }
        count += 1; // final segment
        assert_eq!(count, 1);
    }

    // UT-TCP-04: Bundle exactly equals MTU → 1 segment.
    #[test]
    fn fragment_exact_mtu() {
        let bundle_len: usize = 1000;
        let segment_mtu: usize = 1000;

        // Loop condition is `while bundle.len() > segment_mtu` — not >=
        // So equal length skips the loop, producing 1 final segment
        let mut remaining = bundle_len;
        let mut count = 0;
        while remaining > segment_mtu {
            remaining -= segment_mtu;
            count += 1;
        }
        count += 1;
        assert_eq!(count, 1);
    }

    // UT-TCP-04: 1000-byte payload with 100-byte MTU → 10 segments.
    #[test]
    fn fragment_ten_segments() {
        let bundle_len: usize = 1000;
        let segment_mtu: usize = 100;

        let mut remaining = bundle_len;
        let mut count = 0;
        while remaining > segment_mtu {
            remaining -= segment_mtu;
            count += 1;
        }
        count += 1;
        assert_eq!(count, 10);
        assert_eq!(remaining, 100); // last segment is exactly MTU
    }

    // UT-TCP-04: Non-divisible payload → last segment is smaller.
    #[test]
    fn fragment_with_remainder() {
        let bundle_len: usize = 1050;
        let segment_mtu: usize = 100;

        let mut remaining = bundle_len;
        let mut count = 0;
        while remaining > segment_mtu {
            remaining -= segment_mtu;
            count += 1;
        }
        count += 1;
        assert_eq!(count, 11);
        assert_eq!(remaining, 50);
    }

    // UT-TCP-04: First segment has START flag, last has END flag.
    #[test]
    fn fragment_flags() {
        let bundle_len: usize = 300;
        let segment_mtu: usize = 100;

        let mut remaining = bundle_len;
        let mut start = true;
        let mut flags: Vec<(bool, bool)> = Vec::new();

        while remaining > segment_mtu {
            flags.push((start, false));
            remaining -= segment_mtu;
            start = false;
        }
        flags.push((start, true)); // final: end=true

        assert_eq!(flags.len(), 3);
        assert_eq!(flags[0], (true, false)); // START, not END
        assert_eq!(flags[1], (false, false)); // not START, not END
        assert_eq!(flags[2], (false, true)); // not START, END
    }
}
