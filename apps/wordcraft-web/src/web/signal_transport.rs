//! kosmos: the browser transport of Craft Signal (`craft-signal`): the ticket by `fetch`, the
//! frames over the browser's `WebSocket`. Nothing blocks the frame: what arrives is queued, a
//! repaint is asked for, and the client takes it on the next frame.
//!
//! The socket's callbacks hold a generation number: a `close` of a socket that was already
//! replaced (reconnect) is dropped instead of closing the new connection in the client's eyes.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use craft_signal::transport::{Event, Transport};
use craft_signal::wire::TicketRequest;
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

pub struct WebSocketTransport {
    ctx: egui::Context,
    events: Events,
    /// The generation of the socket in `socket`; callbacks of older sockets are ignored.
    generation: Rc<Cell<u32>>,
    socket: Option<Socket>,
}

impl WebSocketTransport {
    pub fn new(ctx: egui::Context) -> Self {
        Self { ctx, events: Rc::default(), generation: Rc::new(Cell::new(0)), socket: None }
    }

    fn push(events: &Events, ctx: &egui::Context, event: Event) {
        events.borrow_mut().push(event);
        ctx.request_repaint();
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
        let generation = self.generation.get().wrapping_add(1);
        self.generation.set(generation);
        let ws = web_sys::WebSocket::new(url).map_err(js_error)?;
        ws.set_binary_type(web_sys::BinaryType::Arraybuffer);
        let current = self.generation.clone();
        let live = move || current.get() == generation;

        let (events, ctx, is_live) = (self.events.clone(), self.ctx.clone(), live.clone());
        let on_open = Closure::<dyn FnMut()>::new(move || {
            if is_live() {
                Self::push(&events, &ctx, Event::Opened);
            }
        });
        let (events, ctx, is_live) = (self.events.clone(), self.ctx.clone(), live.clone());
        let on_message = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |message: web_sys::MessageEvent| {
            if !is_live() {
                return;
            }
            match message.data().dyn_into::<js_sys::ArrayBuffer>() {
                Ok(buffer) => Self::push(&events, &ctx, Event::Frame(js_sys::Uint8Array::new(&buffer).to_vec())),
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
            Self::push(&events, &ctx, Event::Closed(reason));
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
        self.socket = Some(Socket { ws, _on_open: on_open, _on_message: on_message, _on_close: on_close, _on_error: on_error });
        Ok(())
    }

    fn send(&mut self, frame: Vec<u8>) -> Result<(), String> {
        let Some(socket) = &self.socket else { return Err("not open".into()) };
        if socket.ws.ready_state() != web_sys::WebSocket::OPEN {
            return Err("socket is not open".into());
        }
        socket.ws.send_with_u8_array(&frame).map_err(js_error)
    }

    fn close(&mut self) {
        if let Some(socket) = self.socket.take() {
            // Make the old socket's callbacks stale before closing, so its close is not reported
            // as the end of a connection the client already gave up.
            self.generation.set(self.generation.get().wrapping_add(1));
            if let Err(e) = socket.ws.close() {
                log::debug!("signal: close: {}", js_error(e));
            }
        }
    }

    fn poll(&mut self) -> Vec<Event> {
        std::mem::take(&mut *self.events.borrow_mut())
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
    let response = JsFuture::from(window.fetch_with_str_and_init(url, &init)).await.map_err(js_error)?;
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
