use tokio::sync::mpsc;

/// A wrapper for a flume receiver that allows peeking at the next message.
#[derive(Debug)]
pub(super) struct PeekableReceiver<T> {
    msg: Option<T>,
    recv: mpsc::Receiver<T>,
}

impl<T> PeekableReceiver<T> {
    pub fn new(recv: mpsc::Receiver<T>) -> Self {
        Self { msg: None, recv }
    }

    /// Peek at the next message.
    ///
    /// Will block if there are no messages.
    /// Returns None only if there are no more messages (sender is dropped).
    pub async fn peek(&mut self) -> Option<&T> {
        if self.msg.is_none() {
            self.msg = self.recv.recv().await;
        }
        self.msg.as_ref()
    }

    /// Receive the next message.
    ///
    /// Will block if there are no messages.
    /// Returns None only if there are no more messages (sender is dropped).
    pub async fn recv(&mut self) -> Option<T> {
        if let Some(msg) = self.msg.take() {
            return Some(msg);
        }
        self.recv.recv().await
    }

    /// Push back a message. This will only work if there is room for it.
    /// Otherwise, it will fail and return the message.
    pub fn push_back(&mut self, msg: T) -> std::result::Result<(), T> {
        if self.msg.is_none() {
            self.msg = Some(msg);
            Ok(())
        } else {
            Err(msg)
        }
    }
}
