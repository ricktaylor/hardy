use super::*;
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

pub struct Session<T>
where
    T: futures::StreamExt<Item = Result<codec::Message, codec::Error>>
        + futures::SinkExt<codec::Message>
        + std::marker::Unpin,
    <T as futures::Sink<codec::Message>>::Error: Into<session::Error> + std::fmt::Debug,
{
    transport: T,
    sink: Arc<dyn hardy_bpa::cla::Sink>,
    keepalive_interval: Option<tokio::time::Duration>,
    last_sent: tokio::time::Instant,
    segment_mtu: usize,
    transfer_mru: usize,
    from_sink: tokio::sync::mpsc::Receiver<(
        hardy_bpa::Bytes,
        tokio::sync::oneshot::Sender<Result<hardy_bpa::cla::ForwardBundleResult, hardy_bpa::Bytes>>,
    )>,
    transfer_id: u64,
    acks: VecDeque<XferAck>,
    ingress_bundle: Option<BytesMut>,
}

impl<T> Session<T>
where
    T: futures::StreamExt<Item = Result<codec::Message, codec::Error>>
        + futures::SinkExt<codec::Message>
        + std::marker::Unpin,
    <T as futures::Sink<codec::Message>>::Error: Into<session::Error> + std::fmt::Debug,
{
    pub fn new(
        transport: T,
        sink: Arc<dyn hardy_bpa::cla::Sink>,
        keepalive_interval: Option<tokio::time::Duration>,
        segment_mtu: usize,
        transfer_mru: usize,
        from_sink: tokio::sync::mpsc::Receiver<(
            hardy_bpa::Bytes,
            tokio::sync::oneshot::Sender<
                Result<hardy_bpa::cla::ForwardBundleResult, hardy_bpa::Bytes>,
            >,
        )>,
    ) -> Self {
        Self {
            transport,
            sink,
            keepalive_interval,
            last_sent: tokio::time::Instant::now(),
            segment_mtu,
            transfer_mru,
            from_sink,
            transfer_id: 0,
            acks: VecDeque::new(),
            ingress_bundle: None,
        }
    }

    async fn transport_send(&mut self, msg: codec::Message) -> Result<(), Error> {
        let msg_type = msg.message_type();
        self.transport
            .send(msg)
            .await
            .inspect_err(|e| info!("Failed to send {msg_type:?} to peer: {e:?}"))
            .map_err(Into::into)
            .map(|_| self.last_sent = tokio::time::Instant::now())
    }

    async fn transport_feed(&mut self, msg: codec::Message) -> Result<(), Error> {
        let msg_type = msg.message_type();
        self.transport
            .feed(msg)
            .await
            .inspect_err(|e| info!("Failed to feed {msg_type:?} to peer: {e:?}"))
            .map_err(Into::into)
            .map(|_| self.last_sent = tokio::time::Instant::now())
    }

    async fn reject_msg(
        &mut self,
        reason_code: codec::MessageRejectionReasonCode,
        rejected_message: u8,
    ) -> Result<(), Error> {
        self.transport_send(codec::Message::Reject(codec::MessageRejectMessage {
            reason_code,
            rejected_message,
        }))
        .await
    }

    async fn unexpected_msg(&mut self, rejected_message: codec::MessageType) -> Result<(), Error> {
        self.reject_msg(
            codec::MessageRejectionReasonCode::Unexpected,
            rejected_message as u8,
        )
        .await
    }

    async fn on_transfer(&mut self, msg: codec::TransferSegmentMessage) -> Result<(), Error> {
        if msg.message_flags.start {
            if self.ingress_bundle.is_some() {
                // Out of order bundle!
                return self.unexpected_msg(codec::MessageType::XFER_SEGMENT).await;
            }
            self.ingress_bundle = Some(BytesMut::with_capacity(msg.data.len()));
        }

        let Some(bundle) = &mut self.ingress_bundle else {
            return self.unexpected_msg(codec::MessageType::XFER_SEGMENT).await;
        };

        if msg.data.len() + bundle.len() > self.transfer_mru {
            // Bundle beyond negotiated MRU
            self.ingress_bundle = None;

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
            self.sink.dispatch(bundle.freeze()).await.map_err(|e| {
                warn!("CLA dispatch failed: {e:?}");
                Error::Shutdown(codec::SessionTermReasonCode::Unknown)
            })?;
        }

        // Acknowledge the transfer
        self.transport_send(codec::Message::TransferAck(codec::TransferAckMessage {
            transfer_id: msg.transfer_id,
            message_flags: msg.message_flags,
            acknowledged_length,
        }))
        .await
    }

    async fn send_segment(
        &mut self,
        flags: codec::TransferSegmentMessageFlags,
        data: Bytes,
        acknowledged_length: usize,
    ) -> Result<Option<codec::TransferRefuseReasonCode>, Error> {
        // Inc transfer id
        let transfer_id = self.transfer_id;
        self.transfer_id += 1;

        // Add new Xfer to queue of Acks
        self.acks.push_back(XferAck {
            flags: flags.clone(),
            transfer_id,
            acknowledged_length,
        });

        let last = flags.end;

        self.transport_feed(codec::Message::TransferSegment(
            codec::TransferSegmentMessage {
                message_flags: flags,
                transfer_id,
                data,
                ..Default::default()
            },
        ))
        .await?;

        if last {
            // Make sure we flush the transport
            self.transport.flush().await.map_err(Into::into)?;
        }

        // Use a biased select! to check for incoming messages before the next segment is sent
        loop {
            tokio::select! {
                biased;
                r = self.recv_from_peer() => match r? {
                    codec::Message::SessionTerm(msg) => {
                        trace!("Peer has started to end the session: {msg:?}");
                        return Err(Error::Terminate(msg))
                    },
                    codec::Message::TransferSegment(msg) => {
                        self.on_transfer(msg).await?;
                    },
                    codec::Message::TransferAck(msg) => {
                        let ack = self.acks.pop_front().trace_expect("Transfer acks are all out of sync");
                        if ack.transfer_id != msg.transfer_id {
                            info!(
                                "Mismatched transfer id in TransferAck message: expected {:?} got {:?}",
                                ack.transfer_id,msg.transfer_id
                            );
                        } else if ack.flags != msg.message_flags {
                            info!(
                                "Mismatched flags in TransferAck message: expected {:?} got {:?}",
                                ack.flags,msg.message_flags
                            );
                        } else if ack.acknowledged_length as u64 != msg.acknowledged_length {
                            info!(
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
                            info!(
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

    async fn send_once(
        &mut self,
        mut bundle: Bytes,
    ) -> Result<Option<codec::TransferRefuseReasonCode>, Error> {
        let mut start = true;

        // Segment if needed
        let mut acknowledged_length = 0;
        while bundle.len() > self.segment_mtu {
            acknowledged_length += self.segment_mtu;
            if let Some(refused) = self
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
                return Ok(Some(refused));
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

    async fn forward_to_peer(
        &mut self,
        bundle: Bytes,
        result: tokio::sync::oneshot::Sender<
            Result<hardy_bpa::cla::ForwardBundleResult, hardy_bpa::Bytes>,
        >,
    ) -> Result<(), Error> {
        // Check we can send the segments without rolling over the transfer id
        if self
            .transfer_id
            .saturating_add((bundle.len() / self.segment_mtu) as u64)
            == u64::MAX
        {
            // Nope - need to shutdown the session
            _ = result.send(Err(bundle));

            return Err(Error::Shutdown(
                codec::SessionTermReasonCode::ResourceExhaustion,
            ));
        }

        loop {
            match self.send_once(bundle.clone()).await? {
                None | Some(codec::TransferRefuseReasonCode::Completed) => {
                    _ = result.send(Ok(hardy_bpa::cla::ForwardBundleResult::Sent));
                }
                Some(codec::TransferRefuseReasonCode::Retransmit) => {
                    /* Send again */
                    continue;
                }
                Some(codec::TransferRefuseReasonCode::NoResources) => {
                    _ = result.send(Err(bundle));
                    return Err(Error::Shutdown(
                        codec::SessionTermReasonCode::ResourceExhaustion,
                    ));
                }
                _ => {
                    _ = result.send(Err(bundle));
                }
            }
            break Ok(());
        }
    }

    async fn shutdown(mut self, reason_code: codec::SessionTermReasonCode) {
        // We must shut down our end of the session

        // Stop allowing more transfers
        self.from_sink.close();

        // Drain the sink channel
        while let Some((bundle, result)) = self.from_sink.recv().await {
            _ = result.send(Err(bundle));
        }

        // Send a SESS_TERM message
        let msg = codec::SessionTermMessage {
            reason_code,
            ..Default::default()
        };

        if self
            .transport_send(codec::Message::SessionTerm(msg))
            .await
            .is_ok()
        {
            // Process any remaining messages
            loop {
                if match self.recv_from_peer().await {
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

        // Close the connection
        _ = self.transport.close().await;
    }

    async fn on_terminate(mut self, mut msg: codec::SessionTermMessage) {
        // The remote end has started to end the session

        // Stop allowing more transfers
        self.from_sink.close();

        // Drain the sink channel
        while let Some((bundle, result)) = self.from_sink.recv().await {
            if let Err(e) = self.forward_to_peer(bundle, result).await {
                // Fail anything left in the sink channel
                while let Some((bundle, result)) = self.from_sink.recv().await {
                    _ = result.send(Err(bundle));
                }

                if let Error::Shutdown(_) = e {
                    break;
                } else {
                    // Close the connection
                    _ = self.transport.close().await;
                    return;
                }
            }
        }

        // Send our SESSION_TERM reply
        msg.message_flags.reply = true;
        if self
            .transport_send(codec::Message::SessionTerm(msg))
            .await
            .is_ok()
        {
            // Wait for transfers to complete
            while !self.acks.is_empty() {
                if match self.recv_from_peer().await {
                    Ok(codec::Message::TransferSegment(msg)) => {
                        if msg.message_flags.start {
                            // Peer has started a new transfer in the 'Ending' state
                            if self
                                .transport_send(codec::Message::TransferRefuse(
                                    codec::TransferRefuseMessage {
                                        transfer_id: msg.transfer_id,
                                        reason_code:
                                            codec::TransferRefuseReasonCode::SessionTerminating,
                                    },
                                ))
                                .await
                                .is_ok()
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

        // Close the connection
        _ = self.transport.close().await;
    }

    async fn close(mut self) {
        // The remote end has died completely

        // Stop allowing more transfers
        self.from_sink.close();

        // Drain the rcv channel
        while let Some((bundle, result)) = self.from_sink.recv().await {
            _ = result.send(Err(bundle));
        }

        // Close the connection
        _ = self.transport.close().await;
    }

    async fn recv_from_peer(&mut self) -> Result<codec::Message, Error> {
        loop {
            match if let Some(keepalive_interval) = self.keepalive_interval {
                match tokio::time::timeout(
                    keepalive_interval.saturating_mul(2),
                    self.transport.next(),
                )
                .await
                {
                    Err(_) => {
                        return Err(Error::Shutdown(codec::SessionTermReasonCode::IdleTimeout));
                    }
                    Ok(Some(Ok(codec::Message::Keepalive))) => continue,
                    Ok(msg) => msg,
                }
            } else {
                self.transport.next().await
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

    pub async fn run(mut self) {
        loop {
            // Because we can't double &mut self
            let msg = if let Some(keepalive_interval) = self.keepalive_interval {
                tokio::select! {
                    r = tokio::time::timeout(
                        keepalive_interval.saturating_sub(self.last_sent.elapsed()),
                        self.from_sink.recv(),
                    ) => match r {
                        Ok(Some((bundle,result))) => {
                            let Err(e) = self.forward_to_peer(bundle, result).await else {
                                continue
                            };
                            Err(e)
                        }
                        Ok(None) => Err(Error::Shutdown(codec::SessionTermReasonCode::Unknown)),
                        Err(_) => {
                            // Send a KEEP_ALIVE
                            let Err(e) = self.transport_send(codec::Message::Keepalive).await else {
                                continue
                            };
                            Err(e)
                        }
                    },
                    r = tokio::time::timeout(
                        keepalive_interval.saturating_mul(2),
                        self.transport.next(),
                    ) => match r {
                        Ok(Some(Ok(codec::Message::Keepalive))) => continue,
                        Ok(Some(msg)) => msg.map_err(Into::into),
                        Ok(None) => Err(Error::Hangup),
                        Err(_)=> Err(Error::Shutdown(codec::SessionTermReasonCode::IdleTimeout)),
                    }
                }
            } else {
                tokio::select! {
                    r = self.from_sink.recv() => match r {
                        Some((bundle,result)) => {
                            let Err(e) = self.forward_to_peer(bundle, result).await else {
                                continue
                            };
                            Err(e)
                        }
                        None => Err(Error::Shutdown(codec::SessionTermReasonCode::Unknown)),
                    },
                    msg = self.transport.next() => match msg {
                        Some(msg) => msg.map_err(Into::into),
                        None => Err(Error::Hangup),
                    }
                }
            };

            let result = match msg {
                Ok(codec::Message::TransferSegment(msg)) => self.on_transfer(msg).await,
                Ok(msg) => self.unexpected_msg(msg.message_type()).await,
                Err(e) => Err(e),
            };

            match result {
                Ok(_) => {}
                Err(Error::Terminate(session_term_message)) => {
                    return self.on_terminate(session_term_message).await;
                }
                Err(Error::Shutdown(session_term_reason_code)) => {
                    return self.shutdown(session_term_reason_code).await;
                }
                Err(Error::Codec(e)) => {
                    // The other end is sending us garbage
                    info!("Peer sent invalid data: {e:?}, shutting down session");
                    return self.shutdown(codec::SessionTermReasonCode::Unknown).await;
                }
                Err(Error::Hangup) => {
                    info!("Peer hung up, ending session");
                    return self.close().await;
                }
                Err(Error::Io(e)) => {
                    info!("Session I/O failure: {e:?}, ending session");
                    return self.close().await;
                }
            }
        }
    }
}
