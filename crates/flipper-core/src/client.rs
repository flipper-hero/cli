//! The RPC client: sends `PB.Main` requests over a transport and matches
//! responses by command id, mirroring `FlipperRPCClient` in the iOS FlipperKit.
//!
//! Calls are serialized: the Flipper RPC session is single-threaded. The
//! transport is owned by a single reader task that also services outgoing
//! writes, so transport implementations only need interior mutability for
//! their own state.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::sync::{broadcast, mpsc, watch, Mutex, Notify};
use tokio::task::JoinHandle;

use crate::error::{Error, Result};
use crate::frame::{self, Decoder};
use crate::pb::main::Content;
use crate::pb::{CommandStatus, Main};

/// A byte pipe to a Flipper. Implementations own chunking and flow control.
pub trait Transport: Send + 'static {
    /// Resolves with the next bytes as they arrive (any chunking), or an error
    /// when the link is lost or closed.
    fn recv(&mut self) -> impl Future<Output = Result<Vec<u8>>> + Send;

    /// Sends raw bytes. Implementations block until the bytes are handed to
    /// the link (flow control included).
    fn send(&mut self, data: &[u8]) -> impl Future<Output = Result<()>> + Send;

    /// Closes the link. Must not hang: called during teardown.
    fn close(&mut self) -> impl Future<Output = ()> + Send;
}

/// Progress callback: (units done, units total) frames for long transfers.
pub type Progress<'a> = &'a (dyn Fn(usize, usize) + Send + Sync);

struct PendingEntry {
    parts: Vec<Main>,
    tx: oneshot::Sender<Result<Vec<Main>>>,
}

struct WriteRequest {
    data: Vec<u8>,
    done: oneshot::Sender<Result<()>>,
}

struct Shared {
    pending: Mutex<HashMap<u32, PendingEntry>>,
    next_id: AtomicU32,
    unsolicited_tx: broadcast::Sender<Main>,
    shutdown: watch::Sender<bool>,
}

/// Result of one RPC exchange. Response parts arrive in order; a request with
/// `has_next` frames collects every part before resolving.
pub type Response = Vec<Main>;

pub struct Client<T: Transport> {
    shared: Arc<Shared>,
    write_tx: mpsc::Sender<WriteRequest>,
    reader: Mutex<Option<JoinHandle<()>>>,
    default_timeout: Duration,
    call_lock: Mutex<()>,
    pub(crate) screen_users: Mutex<usize>,
    _transport: std::marker::PhantomData<T>,
}

impl<T: Transport> Client<T> {
    /// Starts the reader task. `default_timeout` applies to every call without
    /// an explicit one (the iOS app uses 20 seconds).
    pub fn start(mut transport: T, default_timeout: Duration) -> Self {
        let (unsolicited_tx, _) = broadcast::channel(64);
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let shared = Arc::new(Shared {
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU32::new(1),
            unsolicited_tx,
            shutdown: shutdown_tx,
        });
        let (write_tx, mut write_rx) = mpsc::channel::<WriteRequest>(16);
        let reader_shared = Arc::clone(&shared);
        let reader = tokio::spawn(async move {
            let mut decoder = Decoder::new();
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown_rx.changed() => break,
                    write = write_rx.recv() => match write {
                        Some(request) => {
                            let result = transport.send(&request.data).await;
                            let failed = result.is_err();
                            let _ = request.done.send(result);
                            if failed {
                                break;
                            }
                        }
                        None => break,
                    },
                    chunk = transport.recv() => match chunk {
                        Ok(bytes) => match decoder.push(&bytes) {
                            Ok(messages) => {
                                for message in messages {
                                    Self::dispatch(&reader_shared, message).await;
                                }
                            }
                            Err(_) => break,
                        },
                        Err(_) => break,
                    },
                }
            }
            let _ = reader_shared.shutdown.send(true);
            for (_, entry) in reader_shared.pending.lock().await.drain() {
                let _ = entry.tx.send(Err(Error::NotConnected));
            }
            write_rx.close();
            while let Some(request) = write_rx.recv().await {
                let _ = request.done.send(Err(Error::NotConnected));
            }
            transport.close().await;
        });
        Self {
            shared,
            write_tx,
            reader: Mutex::new(Some(reader)),
            default_timeout,
            call_lock: Mutex::new(()),
            screen_users: Mutex::new(0),
            _transport: std::marker::PhantomData,
        }
    }

    /// Aborts the reader task so the transport's link (a USB fd, a BLE
    /// connection) is released deterministically instead of whenever the
    /// owning task happens to be dropped. `stop` is the graceful variant.
    fn detach_link(&mut self) {
        let _ = self.shared.shutdown.send(true);
        if let Some(reader) = self.reader.get_mut().take() {
            reader.abort();
        }
    }

    /// Messages that do not answer any pending request (screen frames use
    /// command id 0). Each call returns an independent subscription.
    pub fn unsolicited(&self) -> broadcast::Receiver<Main> {
        self.shared.unsolicited_tx.subscribe()
    }

    /// Resolves once the link is lost or the client was closed.
    pub async fn closed(&self) {
        let mut rx = self.shared.shutdown.subscribe();
        if *rx.borrow_and_update() {
            return;
        }
        let _ = rx.changed().await;
    }

    /// Stops the reader and closes the transport.
    pub async fn stop(self) {
        let _ = self.shared.shutdown.send(true);
        if let Some(reader) = self.reader.lock().await.take() {
            let _ = reader.await;
        }
    }

    fn take_id(&self) -> u32 {
        // Mirrors the iOS client: wrap to 1 instead of reusing 0 (stream id).
        loop {
            let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
            if id != u32::MAX {
                return id;
            }
            self.shared.next_id.store(1, Ordering::Relaxed);
        }
    }

    fn frame(id: u32, content: &Content, has_next: bool) -> Vec<u8> {
        frame::encode(&Main {
            command_id: id,
            command_status: CommandStatus::Ok as i32,
            has_next,
            content: Some(content.clone()),
        })
    }

    async fn dispatch(shared: &Shared, message: Main) {
        let id = message.command_id;
        let mut pending = shared.pending.lock().await;
        match pending.remove(&id) {
            Some(mut entry) => {
                if message.command_status() != CommandStatus::Ok {
                    let _ = entry.tx.send(Err(Error::Rpc(
                        message.command_status().as_str_name().to_owned(),
                    )));
                } else {
                    let done = !message.has_next;
                    entry.parts.push(message);
                    if done {
                        let _ = entry.tx.send(Ok(entry.parts));
                    } else {
                        pending.insert(id, entry);
                    }
                }
            }
            None => {
                let _ = shared.unsolicited_tx.send(message);
            }
        }
    }

    /// Sends the given contents as one request (all but the last with
    /// `has_next`) and resolves with all response parts.
    pub async fn call(&self, contents: Vec<Content>) -> Result<Response> {
        self.call_with(contents, None, None).await
    }

    /// Like [`call`](Self::call) with an explicit timeout and a progress
    /// callback that fires after each request frame goes out. Cancelling the
    /// future stops sending.
    pub async fn call_with(
        &self,
        contents: Vec<Content>,
        timeout: Option<Duration>,
        progress: Option<Progress<'_>>,
    ) -> Result<Response> {
        assert!(
            !contents.is_empty(),
            "at least one request message is required"
        );
        let _guard = self.call_lock.lock().await;

        let (tx, rx) = tokio::sync::oneshot::channel();
        let id = self.take_id();
        {
            let mut pending = self.shared.pending.lock().await;
            if *self.shared.shutdown.borrow() {
                return Err(Error::NotConnected);
            }
            pending.insert(
                id,
                PendingEntry {
                    parts: Vec::new(),
                    tx,
                },
            );
        }

        let total = contents.len();
        let mut send_result = Ok(());
        for (index, content) in contents.iter().enumerate() {
            let bytes = Self::frame(id, content, index < total - 1);
            let (done_tx, done_rx) = tokio::sync::oneshot::channel();
            if self
                .write_tx
                .send(WriteRequest {
                    data: bytes,
                    done: done_tx,
                })
                .await
                .is_err()
            {
                send_result = Err(Error::NotConnected);
                break;
            }
            if let Ok(Err(error)) = done_rx.await {
                send_result = Err(error);
                break;
            }
            if let Some(progress) = progress {
                progress(index + 1, total);
            }
        }
        if let Err(error) = send_result {
            Self::remove_pending(&self.shared, id).await;
            return Err(error);
        }

        match tokio::time::timeout(timeout.unwrap_or(self.default_timeout), rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_recv)) => Err(Error::NotConnected),
            Err(_elapsed) => {
                Self::remove_pending(&self.shared, id).await;
                Err(Error::Timeout)
            }
        }
    }

    async fn remove_pending(shared: &Shared, id: u32) {
        if let Some(entry) = shared.pending.lock().await.remove(&id) {
            let _ = entry.tx.send(Err(Error::Cancelled));
        }
    }

    /// Sends independent single-message requests back to back in one write and
    /// waits for all answers. The Flipper handles them in order, so a button
    /// press costs one round trip instead of one per message.
    pub async fn call_pipelined(&self, contents: Vec<Content>) -> Result<()> {
        self.call_pipelined_with(contents, None).await
    }

    pub async fn call_pipelined_with(
        &self,
        contents: Vec<Content>,
        timeout: Option<Duration>,
    ) -> Result<()> {
        assert!(
            !contents.is_empty(),
            "at least one request message is required"
        );
        let _guard = self.call_lock.lock().await;
        if *self.shared.shutdown.borrow() {
            return Err(Error::NotConnected);
        }

        let limit = timeout.unwrap_or(self.default_timeout);
        let mut ids = Vec::with_capacity(contents.len());
        let mut rxs = Vec::with_capacity(contents.len());
        let mut data = Vec::new();
        for content in &contents {
            let (tx, rx) = tokio::sync::oneshot::channel();
            let id = self.take_id();
            data.extend_from_slice(&Self::frame(id, content, false));
            let mut pending = self.shared.pending.lock().await;
            pending.insert(
                id,
                PendingEntry {
                    parts: Vec::new(),
                    tx,
                },
            );
            ids.push(id);
            rxs.push(rx);
        }

        let write_result = async {
            let (done_tx, done_rx) = tokio::sync::oneshot::channel();
            self.write_tx
                .send(WriteRequest {
                    data,
                    done: done_tx,
                })
                .await
                .map_err(|_| Error::NotConnected)?;
            done_rx.await.map_err(|_| Error::NotConnected)?
        }
        .await;
        if let Err(error) = write_result {
            for id in ids {
                Self::remove_pending(&self.shared, id).await;
            }
            return Err(error);
        }

        // The Flipper answers in order; wait for each answer under one deadline
        // budget so a long chain cannot accumulate per-message timeouts.
        let deadline = tokio::time::Instant::now() + limit;
        let mut result = Ok(());
        for rx in rxs {
            match tokio::time::timeout_at(deadline, rx).await {
                Ok(Ok(Ok(_))) => {}
                Ok(Ok(Err(error))) => {
                    result = Err(error);
                    break;
                }
                Ok(Err(_)) | Err(_) => {
                    result = Err(Error::Timeout);
                    break;
                }
            }
        }
        for id in ids {
            Self::remove_pending(&self.shared, id).await;
        }
        result
    }
}

impl<T: Transport> Drop for Client<T> {
    fn drop(&mut self) {
        self.detach_link();
    }
}

/// Waits until at least `want` bytes of flow-control credit are available and
/// grants up to `want` of them. Shared between the flow-control notification
/// handler and the writer in the BLE transport.
#[derive(Default)]
pub struct FlowControl {
    free: Mutex<i64>,
    updated: Notify,
}

impl FlowControl {
    /// A new credit report from the device (big-endian UInt32 of free bytes).
    pub async fn update(&self, free_bytes: u32) {
        *self.free.lock().await = i64::from(free_bytes);
        self.updated.notify_waiters();
    }

    /// Reserves between 1 and `want` bytes, waiting for credit as needed.
    pub async fn reserve(&self, want: usize) -> usize {
        loop {
            let notified = self.updated.notified();
            let mut free = self.free.lock().await;
            if *free > 0 {
                let granted = (*free).min(want as i64) as usize;
                *free -= granted as i64;
                return granted;
            }
            drop(free);
            notified.await;
        }
    }

    /// Starts with unknown credit; transports seed this from the first
    /// notification or a conservative default.
    pub async fn seed(&self, free_bytes: i64) {
        *self.free.lock().await = free_bytes;
        self.updated.notify_waiters();
    }
}
