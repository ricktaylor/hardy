use super::*;
use futures::StreamExt;
use std::collections::VecDeque;
use thiserror::Error;
use tokio_util::bytes::{Bytes, BytesMut};

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

/// Session that handles the reader side, with writes delegated to a WriterHandle.
///
/// This architecture allows keepalives to be sent by the writer task even when
/// the session is blocked waiting for bundle dispatch (which can block on
/// BoundedTaskPool backpressure).
pub struct Session<R>
where
    R: StreamExt<Item = Result<codec::Message, codec::Error>> + std::marker::Unpin,
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
    cancel_token: tokio_util::sync::CancellationToken,
}

impl<R> Session<R>
where
    R: StreamExt<Item = Result<codec::Message, codec::Error>> + std::marker::Unpin,
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
            cancel_token,
        }
    }

    async fn reject_msg(
        &self,
        reason_code: codec::MessageRejectionReasonCode,
        rejected_message: u8,
    ) -> Result<(), Error> {
        self.writer
            .send(codec::Message::Reject(codec::MessageRejectMessage {
                reason_code,
                rejected_message,
            }))
            .await?;
        Ok(())
    }

    async fn unexpected_msg(&self, rejected_message: codec::MessageType) -> Result<(), Error> {
        self.reject_msg(
            codec::MessageRejectionReasonCode::Unexpected,
            rejected_message as u8,
        )
        .await
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self)))]
    async fn on_transfer(&mut self, msg: codec::TransferSegmentMessage) -> Result<(), Error> {
        if msg.message_flags.start {
            if self.ingress_bundle.is_some() {
                // Out of order bundle!
                debug!("Out of order segment received");
                return self.unexpected_msg(codec::MessageType::XFER_SEGMENT).await;
            }
            self.ingress_bundle = Some(BytesMut::with_capacity(msg.data.len()));
        }

        let Some(bundle) = &mut self.ingress_bundle else {
            debug!("Unexpected segment received");
            return self.unexpected_msg(codec::MessageType::XFER_SEGMENT).await;
        };

        if msg.data.len() + bundle.len() > self.transfer_mru {
            // Bundle beyond negotiated MRU
            self.ingress_bundle = None;

            debug!("Segment received beyond the negotiated MRU");
            return self
                .reject_msg(
                    codec::MessageRejectionReasonCode::Unsupported,
                    codec::MessageType::XFER_SEGMENT as u8,
                )
                .await;
        }

        bundle.extend_from_slice(&msg.data);
        let acknowledged_length = bundle.len() as u64;

        if msg.message_flags.end {
            // Clear the ingress bundle
            let bundle = self.ingress_bundle.take().unwrap();

            // Send the bundle to the BPA
            // NOTE: This may block if BoundedTaskPool is full, but keepalives
            // are handled by the separate writer task so the session stays alive.
            self.sink
                .dispatch(
                    bundle.freeze(),
                    self.peer_node.as_ref(),
                    self.peer_addr.as_ref(),
                )
                .await
                .map_err(|e| {
                    debug!("CLA dispatch failed: {e:?}");
                    Error::Shutdown(codec::SessionTermReasonCode::Unknown)
                })?;
        }

        // Per RFC9174 Section 5.2.3: "A receiving TCPCL entity SHALL send a
        // XFER_ACK message in response to each received XFER_SEGMENT message
        // after the segment has been fully processed."
        self.writer
            .send(codec::Message::TransferAck(codec::TransferAckMessage {
                transfer_id: msg.transfer_id,
                message_flags: msg.message_flags,
                acknowledged_length,
            }))
            .await?;
        Ok(())
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self, data)))]
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

        self.writer
            .feed(codec::Message::TransferSegment(
                codec::TransferSegmentMessage {
                    message_flags: flags,
                    transfer_id,
                    data,
                    ..Default::default()
                },
            ))
            .await?;

        if last {
            // Make sure we flush
            self.writer.flush().await?;
        }

        // Use a biased select! to check for incoming messages before the next segment is sent
        loop {
            tokio::select! {
                biased;
                r = self.recv_from_peer() => match r? {
                    codec::Message::SessionTerm(msg) => {
                        debug!("Peer has started to end the session: {msg:?}");
                        return Err(Error::Terminate(msg))
                    },
                    codec::Message::TransferSegment(msg) => {
                        self.on_transfer(msg).await?;
                    },
                    codec::Message::TransferAck(msg) => {
                        let ack = self.acks.pop_front().trace_expect("Transfer acks are all out of sync");
                        if ack.transfer_id != msg.transfer_id {
                            debug!(
                                "Mismatched transfer id in TransferAck message: expected {:?} got {:?}",
                                ack.transfer_id,msg.transfer_id
                            );
                        } else if ack.flags != msg.message_flags {
                            debug!(
                                "Mismatched flags in TransferAck message: expected {:?} got {:?}",
                                ack.flags,msg.message_flags
                            );
                        } else if ack.acknowledged_length as u64 != msg.acknowledged_length {
                            debug!(
                                "Mismatched acknowledged_length in TransferAck message: expected {} got {}",
                                ack.acknowledged_length, msg.acknowledged_length
                            );
                        } else {
                            if self.acks.is_empty() {
                                return Ok(None);
                            }
                            continue;
                        }

                        self.reject_msg(codec::MessageRejectionReasonCode::Unsupported,codec::MessageType::XFER_ACK as u8).await?;

                        // It's all gone very wrong
                        return Err(Error::Shutdown(codec::SessionTermReasonCode::Unknown));
                    },
                    codec::Message::TransferRefuse(msg) => {
                        let ack = self.acks.pop_front().trace_expect("Transfer acks are all out of sync");
                        if ack.transfer_id != msg.transfer_id {
                            debug!(
                                "Mismatched transfer id in TransferRefuse message: expected {:?} got {:?}",
                                ack.transfer_id,msg.transfer_id
                            );
                        } else {
                            return Ok(Some(msg.reason_code));
                        }

                        self.reject_msg(codec::MessageRejectionReasonCode::Unsupported,codec::MessageType::XFER_REFUSE as u8).await?;

                        // It's all gone very wrong
                        return Err(Error::Shutdown(codec::SessionTermReasonCode::Unknown));
                    }
                    msg => {
                        self.unexpected_msg(msg.message_type()).await?;
                    }
                },
                _ = std::future::ready(()), if !last => {
                    // No messages pending, we can send the next
                    return Ok(None);
                },
            }
        }
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self, bundle)))]
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

    #[cfg_attr(feature = "tracing", instrument(skip_all))]
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

    #[cfg_attr(feature = "tracing", instrument(skip(self)))]
    async fn shutdown(mut self, reason_code: codec::SessionTermReasonCode) {
        // Stop allowing more transfers
        self.from_sink.close();

        // If already cancelled, skip the graceful SESS_TERM exchange
        if self.cancel_token.is_cancelled() {
            self.writer.close().await;
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
                                if !msg.message_flags.reply {
                                    // Terminations pass in the night...
                                    return self.on_terminate(msg).await;
                                }
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

        // Close the writer
        self.writer.close().await;
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self)))]
    async fn on_terminate(mut self, mut msg: codec::SessionTermMessage) {
        // The remote end has started to end the session

        // Stop allowing more transfers
        self.from_sink.close();

        // Drain the sink channel
        while let Some((bundle, result)) = self.from_sink.recv().await {
            if let Err(e) = self.forward_to_peer(bundle, result).await {
                if let Error::Shutdown(_) = e {
                    break;
                } else {
                    // Close the writer
                    self.writer.close().await;
                    return;
                }
            }
        }

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

        // Close the writer
        self.writer.close().await;
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self)))]
    async fn close(self) {
        // The remote end has died completely
        // Close the writer
        self.writer.close().await;
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self)))]
    async fn recv_from_peer(&mut self) -> Result<codec::Message, Error> {
        loop {
            match if let Some(keepalive_interval) = self.keepalive_interval {
                // Timeout for receiving from peer: 2x keepalive interval
                // If we don't receive anything in this time, peer is probably dead
                match tokio::time::timeout(keepalive_interval.saturating_mul(2), self.reader.next())
                    .await
                {
                    Err(_) => {
                        return Err(Error::Shutdown(codec::SessionTermReasonCode::IdleTimeout));
                    }
                    Ok(Some(Ok(codec::Message::Keepalive))) => continue,
                    Ok(msg) => msg,
                }
            } else {
                self.reader.next().await
            } {
                None => return Err(Error::Hangup),
                Some(Err(codec::Error::InvalidMessageType(rejected_message))) => {
                    // Send a rejection (best effort)
                    self.reject_msg(
                        codec::MessageRejectionReasonCode::UnknownType,
                        rejected_message,
                    )
                    .await?;
                }
                Some(Err(e)) => return Err(Error::Codec(e)),
                Some(Ok(msg)) => return Ok(msg),
            }
        }
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    pub async fn run(mut self) {
        let cancel_token = self.cancel_token.clone();
        let e = loop {
            // The main loop now only handles:
            // 1. Cancellation
            // 2. Outbound bundles from sink
            // 3. Inbound messages from peer
            //
            // Keepalive SENDING is handled by the separate writer task.
            // This allows keepalives to be sent even when dispatch() blocks.
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
                Err(e) => Err(e),
            } {
                break e;
            }
        };

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
                    self.close().await;
                    return;
                }
                // The other end is sending us garbage
                debug!("Peer sent invalid data: {e:?}, shutting down session");
                self.shutdown(codec::SessionTermReasonCode::Unknown).await;
            }
            Error::Hangup => {
                debug!("Peer hung up, ending session");
                self.close().await;
            }
            Error::Io(e) => {
                debug!("Session I/O failure: {e:?}, ending session");
                self.close().await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    // use super::*;

    // #[tokio::test]
    // async fn test_parameter_negotiation_ut_tcp_03() {
    //     // TODO: UT-TCP-03 Parameter Negotiation
    //     // Verify that session parameters (Keepalive, Segment Size) are correctly negotiated
    //     // (e.g., taking the minimum of local and peer values).
    //     todo!("Implement test_parameter_negotiation_ut_tcp_03");
    // }

    // #[tokio::test]
    // async fn test_fragment_logic_ut_tcp_04() {
    //     // TODO: UT-TCP-04 Fragment Logic
    //     // Verify the logic that splits a large payload into XFER_SEGMENT chunks based on the negotiated segment size.
    //     todo!("Implement test_fragment_logic_ut_tcp_04");
    // }

    // #[test]
    // fn test_reason_codes_ut_tcp_05() {
    //     // TODO: UT-TCP-05 Reason Codes
    //     // Verify mapping of internal errors (e.g., Storage Full) to correct SESS_TERM or XFER_REFUSE reason codes.
    //     todo!("Implement test_reason_codes_ut_tcp_05");
    // }
}
