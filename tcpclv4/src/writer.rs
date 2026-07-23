use core::ops::ControlFlow;

use futures::SinkExt;

use super::*;

// Failure modes of [`WriterHandle::send`]. Two variants instead of a
// tri-state `Result<bool, E>`: a closed writer cannot be mistaken for
// success by a caller that only propagates the error case.
#[derive(Debug, thiserror::Error)]
pub enum SendError<E> {
    // The writer has closed; the message was not sent.
    #[error("The writer has closed")]
    Closed,
    // The transport write failed.
    #[error("Transport write failed")]
    Transport(E),
}

// Commands sent to the session writer task.
pub enum WriteCommand<E> {
    // Send a message, flush, and report the result.
    Send {
        msg: codec::Message,
        result: tokio::sync::oneshot::Sender<Result<(), E>>,
    },
    // Queue a message for transmission without waiting for completion. The
    // writer flushes once its command queue runs dry; a write failure closes
    // the writer, which later commands observe as a closed channel.
    Feed {
        msg: codec::Message,
    },
    // Close the writer.
    Close,
}

// Handle for sending commands to the writer task.
pub struct WriterHandle<E> {
    tx: tokio::sync::mpsc::Sender<WriteCommand<E>>,
}

impl<E> Clone for WriterHandle<E> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
        }
    }
}

// Reserved space for feeding a single message, obtained via
// [WriterHandle::reserve].
pub struct FeedPermit<'a, E>(tokio::sync::mpsc::Permit<'a, WriteCommand<E>>);

impl<E> FeedPermit<'_, E> {
    // Queue the message on the reserved slot. Never blocks.
    pub fn feed(self, msg: codec::Message) {
        self.0.send(WriteCommand::Feed { msg });
    }
}

impl<E> WriterHandle<E> {
    pub fn new(tx: tokio::sync::mpsc::Sender<WriteCommand<E>>) -> Self {
        Self { tx }
    }

    // Send a message, wait for it to be written and flushed.
    pub async fn send(&self, msg: codec::Message) -> Result<(), SendError<E>> {
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        if self
            .tx
            .send(WriteCommand::Send {
                msg,
                result: result_tx,
            })
            .await
            .is_err()
        {
            return Err(SendError::Closed);
        }
        match result_rx.await {
            // The writer died before reporting
            Err(_) => Err(SendError::Closed),
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(SendError::Transport(e)),
        }
    }

    // Queue a message without waiting for the write to complete. Write errors
    // surface as a closed writer on a later command.
    // Returns false if the writer has closed.
    pub async fn feed(&self, msg: codec::Message) -> bool {
        self.tx.send(WriteCommand::Feed { msg }).await.is_ok()
    }

    // Reserve space for one message, waiting until the writer has room.
    // Returns None if the writer has closed.
    //
    // Reservation is cancel-safe and the returned permit queues its message
    // without blocking, so a caller can reserve inside a select that
    // concurrently processes inbound messages.
    pub async fn reserve(&self) -> Option<FeedPermit<'_, E>> {
        self.tx.reserve().await.ok().map(FeedPermit)
    }

    // Request the writer to close.
    pub async fn close(&self) {
        _ = self.tx.send(WriteCommand::Close).await;
    }
}

// Session writer that handles keepalives independently of the main session
// loop.
//
// This runs in its own task so that keepalives are sent even when the session
// is busy elsewhere, and so the session can apply backpressure at the command
// channel rather than blocking on transport writes.
pub struct SessionWriter<W>
where
    W: futures::Sink<codec::Message> + core::marker::Unpin,
{
    writer: W,
    from_session: tokio::sync::mpsc::Receiver<WriteCommand<W::Error>>,
    keepalive_interval: Option<tokio::time::Duration>,
    cancel_token: tokio_util::sync::CancellationToken,
}

impl<W> SessionWriter<W>
where
    W: futures::Sink<codec::Message> + core::marker::Unpin,
    W::Error: core::fmt::Debug,
{
    pub fn new(
        writer: W,
        from_session: tokio::sync::mpsc::Receiver<WriteCommand<W::Error>>,
        keepalive_interval: Option<tokio::time::Duration>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Self {
        Self {
            writer,
            from_session,
            keepalive_interval,
            cancel_token,
        }
    }

    // Run the writer task.
    //
    // This task handles:
    // 1. Sending messages from the session and ingest tasks
    // 2. Flushing when the command queue runs dry, so consecutive Feeds
    //    stream into the transport without intermediate flushes
    // 3. Sending keepalives when idle
    pub async fn run(mut self) {
        let mut last_sent = tokio::time::Instant::now();
        let mut needs_flush = false;

        loop {
            // Drain any queued commands before flushing fed messages. The
            // drain checks cancellation itself: under sustained feeds it
            // never reaches the select below, and every socket write inside
            // handle_command is raced against the token.
            if needs_flush {
                if self.cancel_token.is_cancelled() {
                    debug!("Writer cancelled");
                    break;
                }
                match self.from_session.try_recv() {
                    Ok(cmd) => {
                        if self
                            .handle_command(cmd, &mut last_sent, &mut needs_flush)
                            .await
                            .is_break()
                        {
                            break;
                        }
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                        let cancel_token = self.cancel_token.clone();
                        let flushed = tokio::select! {
                            biased;
                            _ = cancel_token.cancelled() => {
                                debug!("Writer cancelled");
                                break;
                            }
                            r = self.writer.flush() => r,
                        };
                        if flushed.is_err() {
                            debug!("Failed to flush, closing writer");
                            break;
                        }
                        needs_flush = false;
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
                }
                continue;
            }

            // Calculate time until next keepalive
            let keepalive_sleep = if let Some(interval) = self.keepalive_interval {
                let elapsed = last_sent.elapsed();
                if elapsed >= interval {
                    // Send keepalive immediately, raced against cancellation
                    // like every other socket write
                    let cancel_token = self.cancel_token.clone();
                    let sent = tokio::select! {
                        biased;
                        _ = cancel_token.cancelled() => {
                            debug!("Writer cancelled");
                            break;
                        }
                        r = self.writer.send(codec::Message::Keepalive) => r,
                    };
                    if sent.is_err() {
                        debug!("Failed to send keepalive, closing writer");
                        break;
                    }
                    last_sent = tokio::time::Instant::now();
                    continue;
                }
                Some(interval - elapsed)
            } else {
                None
            };

            tokio::select! {
                biased;

                // Cancellation
                _ = self.cancel_token.cancelled() => {
                    debug!("Writer cancelled");
                    break;
                }

                // Commands from session (prioritized over keepalive)
                cmd = self.from_session.recv() => {
                    let Some(cmd) = cmd else {
                        debug!("Writer command channel closed");
                        break;
                    };
                    if self.handle_command(cmd, &mut last_sent, &mut needs_flush).await.is_break() {
                        break;
                    }
                }

                // Keepalive timeout (only triggers when idle)
                _ = async {
                    if let Some(sleep_duration) = keepalive_sleep {
                        tokio::time::sleep(sleep_duration).await
                    } else {
                        core::future::pending::<()>().await
                    }
                } => {
                    let cancel_token = self.cancel_token.clone();
                    let sent = tokio::select! {
                        biased;
                        _ = cancel_token.cancelled() => {
                            debug!("Writer cancelled");
                            break;
                        }
                        r = self.writer.send(codec::Message::Keepalive) => r,
                    };
                    if sent.is_err() {
                        debug!("Failed to send keepalive, closing writer");
                        break;
                    }
                    last_sent = tokio::time::Instant::now();
                }
            }
        }

        // Best-effort close, flushing anything still buffered. A failure here
        // drops fed-but-unflushed messages; safety holds via retransmission,
        // but leave the breadcrumb for whoever debugs the peer's retry.
        if self.writer.close().await.is_err() {
            debug!("Failed to close writer transport; unflushed messages dropped");
        }
    }

    async fn handle_command(
        &mut self,
        cmd: WriteCommand<W::Error>,
        last_sent: &mut tokio::time::Instant,
        needs_flush: &mut bool,
    ) -> ControlFlow<()> {
        let cancel_token = self.cancel_token.clone();
        match cmd {
            WriteCommand::Send { msg, result } => {
                let msg_type = msg.message_type();
                let r = tokio::select! {
                    biased;
                    _ = cancel_token.cancelled() => {
                        debug!("Writer cancelled");
                        // Dropping the result reports Closed to the caller
                        drop(result);
                        return ControlFlow::Break(());
                    }
                    r = self.writer.send(msg) => r,
                };
                // After a failed transport write the sink is almost certainly
                // dead: close, like the Feed path, rather than feeding frames
                // into a broken sink
                let failed = r.is_err();
                if failed {
                    debug!("Failed to send {msg_type:?}, closing writer");
                } else {
                    *last_sent = tokio::time::Instant::now();
                    // SinkExt::send flushes
                    *needs_flush = false;
                }
                _ = result.send(r);
                if failed {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            }
            WriteCommand::Feed { msg } => {
                let msg_type = msg.message_type();
                let r = tokio::select! {
                    biased;
                    _ = cancel_token.cancelled() => {
                        debug!("Writer cancelled");
                        return ControlFlow::Break(());
                    }
                    r = self.writer.feed(msg) => r,
                };
                if r.is_err() {
                    debug!("Failed to feed {msg_type:?}, closing writer");
                    return ControlFlow::Break(());
                }
                *last_sent = tokio::time::Instant::now();
                *needs_flush = true;
                ControlFlow::Continue(())
            }
            WriteCommand::Close => {
                debug!("Writer received close command");
                ControlFlow::Break(())
            }
        }
    }
}

// Creates a writer task and returns the handle.
//
// This function splits the transport setup from running, allowing the caller
// to spawn the writer in their own task pool.
pub fn create_writer<W>(
    writer: W,
    keepalive_interval: Option<tokio::time::Duration>,
    cancel_token: tokio_util::sync::CancellationToken,
) -> (WriterHandle<W::Error>, SessionWriter<W>)
where
    W: futures::Sink<codec::Message> + core::marker::Unpin,
    W::Error: core::fmt::Debug,
{
    // Channel for session to send commands to writer
    // Bounded to 16 to provide backpressure while allowing segments to
    // stream. The buffered bytes scale with segment size: 16 x segment_mtu
    // per session (256 KiB at the default MRU, 16 MiB at a 1 MiB
    // segment-mru).
    let (tx, rx) = tokio::sync::mpsc::channel(16);

    let handle = WriterHandle::new(tx);
    let writer_task = SessionWriter::new(writer, rx, keepalive_interval, cancel_token);

    (handle, writer_task)
}

#[cfg(test)]
mod tests {
    use super::*;

    // send() reports Closed, not success, when the writer task is gone.
    #[tokio::test]
    async fn send_reports_closed_writer() {
        let (tx, rx) = tokio::sync::mpsc::channel::<WriteCommand<codec::Error>>(1);
        drop(rx);
        let handle = WriterHandle::new(tx);
        assert!(matches!(
            handle.send(codec::Message::Keepalive).await,
            Err(SendError::Closed)
        ));
    }
}
