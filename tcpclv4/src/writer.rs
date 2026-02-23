use super::*;
use futures::SinkExt;

/// Commands sent to the session writer task.
pub enum WriteCommand<E> {
    /// Send a message (waits for write to complete, then flushes).
    Send {
        msg: codec::Message,
        result: tokio::sync::oneshot::Sender<Result<bool, E>>,
    },
    /// Feed a message (buffers without flushing).
    Feed {
        msg: codec::Message,
        result: tokio::sync::oneshot::Sender<Result<bool, E>>,
    },
    /// Flush pending messages.
    Flush {
        result: tokio::sync::oneshot::Sender<Result<bool, E>>,
    },
    /// Close the writer.
    Close,
}

/// Handle for sending commands to the writer task.
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

impl<E> WriterHandle<E> {
    pub fn new(tx: tokio::sync::mpsc::Sender<WriteCommand<E>>) -> Self {
        Self { tx }
    }

    /// Send a message and wait for acknowledgment.
    /// Returns Ok(true) on success, Ok(false) if writer closed, Err on IO error.
    pub async fn send(&self, msg: codec::Message) -> Result<bool, E> {
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
            return Ok(false);
        }
        result_rx.await.unwrap_or(Ok(false))
    }

    /// Feed a message (buffer without flushing).
    /// Returns Ok(true) on success, Ok(false) if writer closed, Err on IO error.
    pub async fn feed(&self, msg: codec::Message) -> Result<bool, E> {
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        if self
            .tx
            .send(WriteCommand::Feed {
                msg,
                result: result_tx,
            })
            .await
            .is_err()
        {
            return Ok(false);
        }
        result_rx.await.unwrap_or(Ok(false))
    }

    /// Flush pending messages.
    /// Returns Ok(true) on success, Ok(false) if writer closed, Err on IO error.
    pub async fn flush(&self) -> Result<bool, E> {
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        if self
            .tx
            .send(WriteCommand::Flush { result: result_tx })
            .await
            .is_err()
        {
            return Ok(false);
        }
        result_rx.await.unwrap_or(Ok(false))
    }

    /// Request the writer to close.
    pub async fn close(&self) {
        _ = self.tx.send(WriteCommand::Close).await;
    }
}

/// Session writer that handles keepalives independently of the main session loop.
///
/// This runs in its own task so that keepalives are sent even when the session
/// is blocked waiting for bundle dispatch (which can block on BoundedTaskPool
/// backpressure).
pub struct SessionWriter<W>
where
    W: futures::Sink<codec::Message> + std::marker::Unpin,
{
    writer: W,
    from_session: tokio::sync::mpsc::Receiver<WriteCommand<W::Error>>,
    keepalive_interval: Option<tokio::time::Duration>,
    cancel_token: tokio_util::sync::CancellationToken,
}

impl<W> SessionWriter<W>
where
    W: futures::Sink<codec::Message> + std::marker::Unpin,
    W::Error: std::fmt::Debug,
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

    /// Run the writer task.
    ///
    /// This task handles:
    /// 1. Sending messages from the session
    /// 2. Sending keepalives when idle (independent of session dispatch blocking)
    pub async fn run(mut self) {
        let mut last_sent = tokio::time::Instant::now();

        loop {
            // Calculate time until next keepalive
            let keepalive_sleep = if let Some(interval) = self.keepalive_interval {
                let elapsed = last_sent.elapsed();
                if elapsed >= interval {
                    // Send keepalive immediately
                    if self.writer.send(codec::Message::Keepalive).await.is_err() {
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
                    match cmd {
                        Some(WriteCommand::Send { msg, result }) => {
                            let msg_type = msg.message_type();
                            let r = self.writer.send(msg).await;
                            if r.is_ok() {
                                last_sent = tokio::time::Instant::now();
                            } else {
                                debug!("Failed to send {msg_type:?}");
                            }
                            _ = result.send(r.map(|()| true));
                        }
                        Some(WriteCommand::Feed { msg, result }) => {
                            let msg_type = msg.message_type();
                            let r = self.writer.feed(msg).await;
                            if r.is_ok() {
                                last_sent = tokio::time::Instant::now();
                            } else {
                                debug!("Failed to feed {msg_type:?}");
                            }
                            _ = result.send(r.map(|()| true));
                        }
                        Some(WriteCommand::Flush { result }) => {
                            let r = self.writer.flush().await;
                            _ = result.send(r.map(|()| true));
                        }
                        Some(WriteCommand::Close) | None => {
                            debug!("Writer received close command");
                            break;
                        }
                    }
                }

                // Keepalive timeout (only triggers when idle)
                _ = async {
                    if let Some(sleep_duration) = keepalive_sleep {
                        tokio::time::sleep(sleep_duration).await
                    } else {
                        std::future::pending::<()>().await
                    }
                } => {
                    if self.writer.send(codec::Message::Keepalive).await.is_err() {
                        debug!("Failed to send keepalive, closing writer");
                        break;
                    }
                    last_sent = tokio::time::Instant::now();
                }
            }
        }

        // Best-effort close
        _ = self.writer.close().await;
    }
}

/// Creates a writer task and returns the handle.
///
/// This function splits the transport setup from running, allowing the caller
/// to spawn the writer in their own task pool.
pub fn create_writer<W>(
    writer: W,
    keepalive_interval: Option<tokio::time::Duration>,
    cancel_token: tokio_util::sync::CancellationToken,
) -> (WriterHandle<W::Error>, SessionWriter<W>)
where
    W: futures::Sink<codec::Message> + std::marker::Unpin,
    W::Error: std::fmt::Debug,
{
    // Channel for session to send commands to writer
    // Bounded to 16 to provide some backpressure but allow concurrency
    let (tx, rx) = tokio::sync::mpsc::channel(16);

    let handle = WriterHandle::new(tx);
    let writer_task = SessionWriter::new(writer, rx, keepalive_interval, cancel_token);

    (handle, writer_task)
}
