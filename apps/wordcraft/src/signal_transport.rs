//! kosmos: the native transport of Craft Signal (`craft-signal`): the ticket by HTTP (ureq) and the
//! WebSocket to the relay (tungstenite), each on its own thread. Nothing blocks the window: what
//! arrives is queued and taken by the client on the next frame, which this transport asks for.

use std::io::ErrorKind;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use craft_signal::transport::{Event, Transport};
use craft_signal::wire::TicketRequest;
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

pub struct NativeTransport {
    ctx: egui::Context,
    events: Arc<Mutex<Vec<Event>>>,
    outgoing: Option<Sender<Outgoing>>,
}

impl NativeTransport {
    pub fn new(ctx: egui::Context) -> Self {
        Self { ctx, events: Arc::new(Mutex::new(Vec::new())), outgoing: None }
    }

    fn push(events: &Arc<Mutex<Vec<Event>>>, ctx: &egui::Context, event: Event) {
        events.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(event);
        ctx.request_repaint();
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
        let (tx, rx): (Sender<Outgoing>, Receiver<Outgoing>) = channel();
        self.outgoing = Some(tx);
        let url = url.to_string();
        let events = self.events.clone();
        let ctx = self.ctx.clone();
        std::thread::Builder::new()
            .name("signal-socket".into())
            .spawn(move || {
                let (mut socket, _) = match tungstenite::connect(url.as_str()) {
                    Ok(ok) => ok,
                    Err(e) => {
                        Self::push(&events, &ctx, Event::Closed(format!("connect: {e}")));
                        return;
                    }
                };
                if let MaybeTlsStream::Plain(stream) = socket.get_mut()
                    && let Err(e) = stream.set_read_timeout(Some(READ_SLICE))
                {
                    log::warn!("signal: read timeout: {e}");
                }
                Self::push(&events, &ctx, Event::Opened);
                loop {
                    match rx.try_recv() {
                        Ok(Outgoing::Frame(data)) => {
                            if let Err(e) = socket.send(Message::Binary(data.into())) {
                                Self::push(&events, &ctx, Event::Closed(format!("send: {e}")));
                                return;
                            }
                        }
                        Ok(Outgoing::Close) | Err(TryRecvError::Disconnected) => {
                            let _ = socket.close(None);
                            let _ = socket.flush();
                            Self::push(&events, &ctx, Event::Closed("closed".into()));
                            return;
                        }
                        Err(TryRecvError::Empty) => {}
                    }
                    match socket.read() {
                        Ok(Message::Binary(data)) => Self::push(&events, &ctx, Event::Frame(data.to_vec())),
                        Ok(Message::Close(frame)) => {
                            let reason = frame.map(|f| f.reason.to_string()).unwrap_or_default();
                            Self::push(&events, &ctx, Event::Closed(reason));
                            return;
                        }
                        Ok(_) => {}
                        Err(tungstenite::Error::Io(e)) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                        Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                            Self::push(&events, &ctx, Event::Closed("connection closed".into()));
                            return;
                        }
                        Err(e) => {
                            Self::push(&events, &ctx, Event::Closed(format!("read: {e}")));
                            return;
                        }
                    }
                }
            })
            .map_err(|e| e.to_string())?;
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
}
