//! kosmos: the native transport of Craft Signal (`craft-opencore`, area `signal`): the ticket by HTTP (ureq) and the
//! WebSocket to the relay (tungstenite), each on its own thread. Nothing blocks the window: what
//! arrives is queued and taken by the client on the next frame, which this transport asks for.
//! The bulk channel is a second socket of the same kind with its own thread, so the bytes of a
//! picture never hold up a command.

use std::io::ErrorKind;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel as channel_pair};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use craft_opencore::signal::transport::{Event, Transport};
use craft_opencore::signal::wire::TicketRequest;
use tungstenite::Message;
use tungstenite::stream::MaybeTlsStream;

/// How long the socket thread waits for a frame before it looks at the outgoing queue.
const READ_SLICE: Duration = Duration::from_millis(50);
/// Longest answer to the ticket request that is read.
const MAX_TICKET_BYTES: u64 = 64 * 1024;

enum Outgoing {
    Frame(Vec<u8>),
    Close,
}

/// Which of the two sockets a thread serves: it decides how its events are reported.
#[derive(Clone, Copy)]
enum Channel {
    Control,
    Bulk,
}

impl Channel {
    fn thread_name(self) -> &'static str {
        match self {
            Self::Control => "signal-socket",
            Self::Bulk => "signal-bulk",
        }
    }

    fn opened(self) -> Event {
        match self {
            Self::Control => Event::Opened,
            Self::Bulk => Event::BulkOpened,
        }
    }

    fn frame(self, data: Vec<u8>) -> Event {
        match self {
            Self::Control => Event::Frame(data),
            Self::Bulk => Event::BulkFrame(data),
        }
    }

    fn closed(self, reason: String) -> Event {
        match self {
            Self::Control => Event::Closed(reason),
            Self::Bulk => Event::BulkClosed(reason),
        }
    }
}

pub struct NativeTransport {
    ctx: egui::Context,
    events: Arc<Mutex<Vec<Event>>>,
    outgoing: Option<Sender<Outgoing>>,
    bulk_outgoing: Option<Sender<Outgoing>>,
}

impl NativeTransport {
    pub fn new(ctx: egui::Context) -> Self {
        Self { ctx, events: Arc::new(Mutex::new(Vec::new())), outgoing: None, bulk_outgoing: None }
    }

    fn push(events: &Arc<Mutex<Vec<Event>>>, ctx: &egui::Context, event: Event) {
        events.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(event);
        ctx.request_repaint();
    }

    /// Open a socket to `url` on its own thread; what it sees is reported as `channel`'s events.
    fn spawn_socket(&self, channel: Channel, url: &str) -> Result<Sender<Outgoing>, String> {
        let (tx, rx): (Sender<Outgoing>, Receiver<Outgoing>) = channel_pair();
        let url = url.to_string();
        let events = self.events.clone();
        let ctx = self.ctx.clone();
        std::thread::Builder::new()
            .name(channel.thread_name().into())
            .spawn(move || {
                let (mut socket, _) = match tungstenite::connect(url.as_str()) {
                    Ok(ok) => ok,
                    Err(e) => {
                        Self::push(&events, &ctx, channel.closed(format!("connect: {e}")));
                        return;
                    }
                };
                if let MaybeTlsStream::Plain(stream) = socket.get_mut()
                    && let Err(e) = stream.set_read_timeout(Some(READ_SLICE))
                {
                    log::warn!("signal: read timeout: {e}");
                }
                Self::push(&events, &ctx, channel.opened());
                loop {
                    match rx.try_recv() {
                        Ok(Outgoing::Frame(data)) => {
                            if let Err(e) = socket.send(Message::Binary(data.into())) {
                                Self::push(&events, &ctx, channel.closed(format!("send: {e}")));
                                return;
                            }
                        }
                        Ok(Outgoing::Close) | Err(TryRecvError::Disconnected) => {
                            let _ = socket.close(None);
                            let _ = socket.flush();
                            Self::push(&events, &ctx, channel.closed("closed".into()));
                            return;
                        }
                        Err(TryRecvError::Empty) => {}
                    }
                    match socket.read() {
                        Ok(Message::Binary(data)) => Self::push(&events, &ctx, channel.frame(data.to_vec())),
                        Ok(Message::Close(frame)) => {
                            let reason = frame.map(|f| f.reason.to_string()).unwrap_or_default();
                            Self::push(&events, &ctx, channel.closed(reason));
                            return;
                        }
                        Ok(_) => {}
                        Err(tungstenite::Error::Io(e)) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                        Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                            Self::push(&events, &ctx, channel.closed("connection closed".into()));
                            return;
                        }
                        Err(e) => {
                            Self::push(&events, &ctx, channel.closed(format!("read: {e}")));
                            return;
                        }
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(tx)
    }
}

impl Transport for NativeTransport {
    fn ticket(&mut self, relay: &str, auth: &str, request: &TicketRequest) -> Result<(), String> {
        let url = format!("{}/signal/ticket", relay.trim_end_matches('/'));
        let body = serde_json::to_string(request).map_err(|e| e.to_string())?;
        let auth = auth.to_string();
        let events = self.events.clone();
        let ctx = self.ctx.clone();
        std::thread::Builder::new()
            .name("signal-ticket".into())
            .spawn(move || {
                let result = ureq::post(&url)
                    .header("Authorization", &auth)
                    .header("Content-Type", "application/json")
                    .send(body.as_bytes())
                    .map_err(|e| e.to_string())
                    .and_then(|mut response| response.body_mut().with_config().limit(MAX_TICKET_BYTES).read_to_string().map_err(|e| e.to_string()));
                Self::push(&events, &ctx, Event::Ticket(result));
            })
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    fn open(&mut self, url: &str) -> Result<(), String> {
        self.outgoing = Some(self.spawn_socket(Channel::Control, url)?);
        Ok(())
    }

    fn send(&mut self, frame: Vec<u8>) -> Result<(), String> {
        match &self.outgoing {
            Some(tx) => tx.send(Outgoing::Frame(frame)).map_err(|_| "socket thread is gone".to_string()),
            None => Err("not open".into()),
        }
    }

    fn close(&mut self) {
        if let Some(tx) = self.outgoing.take() {
            let _ = tx.send(Outgoing::Close);
        }
    }

    fn poll(&mut self) -> Vec<Event> {
        std::mem::take(&mut *self.events.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
    }

    fn open_bulk(&mut self, url: &str) -> Result<(), String> {
        self.close_bulk();
        self.bulk_outgoing = Some(self.spawn_socket(Channel::Bulk, url)?);
        Ok(())
    }

    fn send_bulk(&mut self, frame: Vec<u8>) -> Result<(), String> {
        match &self.bulk_outgoing {
            Some(tx) => tx.send(Outgoing::Frame(frame)).map_err(|_| "bulk socket thread is gone".to_string()),
            None => Err("bulk channel is not open".into()),
        }
    }

    fn close_bulk(&mut self) {
        if let Some(tx) = self.bulk_outgoing.take() {
            let _ = tx.send(Outgoing::Close);
        }
    }
}
