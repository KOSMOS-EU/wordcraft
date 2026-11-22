//! kosmos: the browser transport of Craft Signal (`craft-opencore`, area `signal`): the ticket by `fetch`, the
//! frames over the browser's `WebSocket`. Nothing blocks the frame: what arrives is queued, a
//! repaint is asked for, and the client takes it on the next frame.
//!
//! The socket's callbacks hold a generation number: a `close` of a socket that was already
//! replaced (reconnect) is dropped instead of closing the new connection in the client's eyes.
//! The bulk channel is a second socket of the same kind, so the bytes of a picture never hold up
//! a command.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use craft_opencore::signal::transport::{Event, Transport};
use craft_opencore::signal::wire::TicketRequest;
use wasm_bindgen::JsCast as _;
use wasm_bindgen::closure::Closure;
use wasm_bindgen_futures::JsFuture;

/// Longest answer to the ticket request that is read.
const MAX_TICKET_BYTES: u32 = 64 * 1024;

type Events = Rc<RefCell<Vec<Event>>>;

/// One open socket with the callbacks that must stay alive as long as it does.
struct Socket {
    ws: web_sys::WebSocket,
    _on_open: Closure<dyn FnMut()>,
    _on_message: Closure<dyn FnMut(web_sys::MessageEvent)>,
    _on_close: Closure<dyn FnMut(web_sys::CloseEvent)>,
    _on_error: Closure<dyn FnMut(web_sys::Event)>,
}

/// Which of the two sockets a callback serves: it decides how its events are reported.
#[derive(Clone, Copy)]
enum Channel {
    Control,
    Bulk,
}

impl Channel {
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

pub struct WebSocketTransport {
    ctx: egui::Context,
    events: Events,
    /// The generation of the socket in `socket`; callbacks of older sockets are ignored.
    generation: Rc<Cell<u32>>,
    socket: Option<Socket>,
    /// The same for the bulk channel.
    bulk_generation: Rc<Cell<u32>>,
    bulk_socket: Option<Socket>,
}

impl WebSocketTransport {
    pub fn new(ctx: egui::Context) -> Self {
        Self {
            ctx,
            events: Rc::default(),
            generation: Rc::new(Cell::new(0)),
            socket: None,
            bulk_generation: Rc::new(Cell::new(0)),
            bulk_socket: None,
        }
    }

    fn push(events: &Events, ctx: &egui::Context, event: Event) {
        events.borrow_mut().push(event);
        ctx.request_repaint();
    }

    /// Open a socket to `url`; what it sees is reported as `channel`'s events while `counter`
    /// still holds the generation it gets here.
    fn open_socket(&self, channel: Channel, url: &str, counter: &Rc<Cell<u32>>) -> Result<Socket, String> {
        let generation = counter.get().wrapping_add(1);
        counter.set(generation);
        let ws = web_sys::WebSocket::new(url).map_err(js_error)?;
        ws.set_binary_type(web_sys::BinaryType::Arraybuffer);
        let current = counter.clone();
        let live = move || current.get() == generation;

        let (events, ctx, is_live) = (self.events.clone(), self.ctx.clone(), live.clone());
        let on_open = Closure::<dyn FnMut()>::new(move || {
            if is_live() {
                Self::push(&events, &ctx, channel.opened());
            }
        });
        let (events, ctx, is_live) = (self.events.clone(), self.ctx.clone(), live.clone());
        let on_message = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |message: web_sys::MessageEvent| {
            if !is_live() {
                return;
            }
            match message.data().dyn_into::<js_sys::ArrayBuffer>() {
                Ok(buffer) => Self::push(&events, &ctx, channel.frame(js_sys::Uint8Array::new(&buffer).to_vec())),
                Err(_) => log::warn!("signal: a text message from the relay was dropped"),
            }
        });
        let (events, ctx, is_live) = (self.events.clone(), self.ctx.clone(), live.clone());
        let on_close = Closure::<dyn FnMut(web_sys::CloseEvent)>::new(move |close: web_sys::CloseEvent| {
            if !is_live() {
                return;
            }
            let reason = close.reason();
            let reason = if reason.is_empty() { format!("closed ({})", close.code()) } else { reason };
            Self::push(&events, &ctx, channel.closed(reason));
        });
        let on_error = Closure::<dyn FnMut(web_sys::Event)>::new(move |_: web_sys::Event| {
            // The browser tells nothing about the cause; the close event that follows is reported.
            if live() {
                log::warn!("signal: socket error");
            }
        });
        ws.set_onopen(Some(on_open.as_ref().unchecked_ref()));
        ws.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        ws.set_onclose(Some(on_close.as_ref().unchecked_ref()));
        ws.set_onerror(Some(on_error.as_ref().unchecked_ref()));
        Ok(Socket { ws, _on_open: on_open, _on_message: on_message, _on_close: on_close, _on_error: on_error })
    }

    fn send_on(socket: Option<&Socket>, frame: &[u8]) -> Result<(), String> {
        let Some(socket) = socket else { return Err("not open".into()) };
        if socket.ws.ready_state() != web_sys::WebSocket::OPEN {
            return Err("socket is not open".into());
        }
        socket.ws.send_with_u8_array(frame).map_err(js_error)
    }

    /// Make the old socket's callbacks stale before closing, so its close is not reported as the
    /// end of a connection the client already gave up.
    fn close_socket(socket: Option<Socket>, counter: &Rc<Cell<u32>>) {
        if let Some(socket) = socket {
            counter.set(counter.get().wrapping_add(1));
            if let Err(e) = socket.ws.close() {
                log::debug!("signal: close: {}", js_error(e));
            }
        }
    }
}

fn js_error(value: wasm_bindgen::JsValue) -> String {
    value
        .as_string()
        .or_else(|| js_sys::Reflect::get(&value, &"message".into()).ok().and_then(|m| m.as_string()))
        .unwrap_or_else(|| format!("{value:?}"))
}

impl Transport for WebSocketTransport {
    fn ticket(&mut self, relay: &str, auth: &str, request: &TicketRequest) -> Result<(), String> {
        let url = format!("{}/signal/ticket", relay.trim_end_matches('/'));
        let body = serde_json::to_string(request).map_err(|e| e.to_string())?;
        let auth = auth.to_string();
        let events = self.events.clone();
        let ctx = self.ctx.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let result = fetch_ticket(&url, &auth, &body).await;
            Self::push(&events, &ctx, Event::Ticket(result));
        });
        Ok(())
    }

    fn open(&mut self, url: &str) -> Result<(), String> {
        self.close();
        self.socket = Some(self.open_socket(Channel::Control, url, &self.generation)?);
        Ok(())
    }

    fn send(&mut self, frame: Vec<u8>) -> Result<(), String> {
        Self::send_on(self.socket.as_ref(), &frame)
    }

    fn close(&mut self) {
        Self::close_socket(self.socket.take(), &self.generation);
    }

    fn poll(&mut self) -> Vec<Event> {
        std::mem::take(&mut *self.events.borrow_mut())
    }

    fn open_bulk(&mut self, url: &str) -> Result<(), String> {
        self.close_bulk();
        self.bulk_socket = Some(self.open_socket(Channel::Bulk, url, &self.bulk_generation)?);
        Ok(())
    }

    fn send_bulk(&mut self, frame: Vec<u8>) -> Result<(), String> {
        Self::send_on(self.bulk_socket.as_ref(), &frame)
    }

    fn close_bulk(&mut self) {
        Self::close_socket(self.bulk_socket.take(), &self.bulk_generation);
    }
}

async fn fetch_ticket(url: &str, auth: &str, body: &str) -> Result<String, String> {
    let init = web_sys::RequestInit::new();
    init.set_method("POST");
    let headers = web_sys::Headers::new().map_err(js_error)?;
    headers.append("Authorization", auth).map_err(js_error)?;
    headers.append("Content-Type", "application/json").map_err(js_error)?;
    init.set_headers(&headers);
    init.set_body(&wasm_bindgen::JsValue::from_str(body));
    let window = web_sys::window().ok_or("no window")?;
    let response = JsFuture::from(window.fetch_with_str_and_init(&super::uncached(url, &init), &init)).await.map_err(js_error)?;
    let response: web_sys::Response = response.dyn_into().map_err(|_| "fetch gave no Response".to_string())?;
    let status = response.status();
    let buffer = JsFuture::from(response.array_buffer().map_err(js_error)?).await.map_err(js_error)?;
    let bytes = js_sys::Uint8Array::new(&buffer);
    if bytes.length() > MAX_TICKET_BYTES {
        return Err(format!("ticket answer of {} bytes is larger than {MAX_TICKET_BYTES}", bytes.length()));
    }
    let text = String::from_utf8_lossy(&bytes.to_vec()).into_owned();
    if !(200..300).contains(&status) {
        return Err(format!("ticket: HTTP {status} {}", text.trim()));
    }
    Ok(text)
}
