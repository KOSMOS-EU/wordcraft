//! kosmos: the server's events for the EDMS module in the web build. The app reads the user's
//! SSE stream of OpenCore/OpenCloud itself (a streaming `fetch` with the connection's
//! authorization) and hands every event to `edms.event`: the engine's change tracker decides
//! what a new version, a move or a delete means for the open document. No host page takes part.
//!
//! Nothing blocks the frame: the stream's task queues what arrives and asks for a repaint, the
//! next frame feeds it to the engine. A stream that ends or fails is opened again after a pause,
//! with the authorization the connection holds by then (the server checks it when the stream
//! opens, not while it runs).

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use serde_json::json;
use wasm_bindgen::{JsCast as _, JsValue};
use wasm_bindgen_futures::JsFuture;
use wordcraft_ui_egui::WordApp;

/// The stream of the signed-in user, behind the instance's proxy.
const STREAM_PATH: &str = "/ocs/v2.php/apps/notifications/api/v1/notifications/sse";
/// Pause before a stream that ended or failed is opened again, in milliseconds.
const RETRY_MS: f64 = 5_000.0;
/// Most events kept between two frames (a tab in the background runs no frames).
const MAX_QUEUED: usize = 256;
/// Most bytes of one event; a longer one is dropped.
const MAX_EVENT_BYTES: usize = 256 * 1024;

/// One event of the stream: its `event:` name and its `data:` lines.
#[derive(Clone, Debug, PartialEq, Eq)]
struct StreamEvent {
    kind: String,
    data: String,
}

/// What the stream's task and the frame share.
#[derive(Default)]
struct Shared {
    events: RefCell<Vec<StreamEvent>>,
    /// The generation of the stream that may queue; older tasks stop when they notice.
    generation: Cell<u32>,
    running: Cell<bool>,
    /// When the last stream ended (`Date.now()`), for the pause before the next.
    ended_at: Cell<f64>,
}

/// The app's own reader of the server's events.
pub struct EdmsEvents {
    ctx: egui::Context,
    shared: Rc<Shared>,
    /// The origin the running stream belongs to.
    origin: Option<String>,
}

impl EdmsEvents {
    pub fn new(ctx: egui::Context) -> Self {
        Self { ctx, shared: Rc::default(), origin: None }
    }

    /// Once a frame: keep a stream open while the EDMS is connected and feed what arrived.
    pub fn frame(&mut self, app: &mut WordApp) {
        let origin = app.session.edms.origin().filter(|_| app.session.edms.connected());
        if origin != self.origin {
            // Another instance, or no connection any more: the running stream is not ours.
            self.shared.generation.set(self.shared.generation.get().wrapping_add(1));
            self.shared.running.set(false);
            self.shared.ended_at.set(0.0);
            self.shared.events.borrow_mut().clear();
            self.origin = origin.clone();
        }
        if let Some(origin) = origin
            && !self.shared.running.get()
        {
            let waited = js_sys::Date::now() - self.shared.ended_at.get();
            if waited >= RETRY_MS {
                if let Some(authorization) = app.session.edms.authorization() {
                    self.start(&origin, authorization, app.session.edms.initiator_id().unwrap_or_default());
                }
            } else {
                self.ctx.request_repaint_after(std::time::Duration::from_millis((RETRY_MS - waited).max(0.0) as u64 + 50));
            }
        }
        let events = std::mem::take(&mut *self.shared.events.borrow_mut());
        for event in events {
            if let Err(error) = app.run("edms.event", json!({"type": event.kind, "data": event.data})) {
                log::debug!("edms events: {} not taken: {error}", event.kind);
            }
        }
    }

    fn start(&mut self, origin: &str, authorization: String, initiator_id: String) {
        let generation = self.shared.generation.get().wrapping_add(1);
        self.shared.generation.set(generation);
        self.shared.running.set(true);
        let shared = self.shared.clone();
        let ctx = self.ctx.clone();
        let url = format!("{origin}{STREAM_PATH}");
        wasm_bindgen_futures::spawn_local(async move {
            if let Err(error) = read_stream(&url, &authorization, &initiator_id, generation, &shared, &ctx).await {
                log::warn!("edms events: stream ended: {error}");
            }
            if shared.generation.get() == generation {
                shared.running.set(false);
                shared.ended_at.set(js_sys::Date::now());
                ctx.request_repaint();
            }
        });
    }
}

fn js_error(value: JsValue) -> String {
    value
        .as_string()
        .or_else(|| js_sys::Reflect::get(&value, &"message".into()).ok().and_then(|m| m.as_string()))
        .unwrap_or_else(|| format!("{value:?}"))
}

/// Call a method without arguments on a JS object (the stream reader's API, which `web-sys`
/// only offers behind further features).
fn call_method(target: &JsValue, name: &str) -> Result<JsValue, String> {
    let method: js_sys::Function =
        js_sys::Reflect::get(target, &name.into()).map_err(js_error)?.dyn_into().map_err(|_| format!("no method {name}"))?;
    method.call0(target).map_err(js_error)
}

async fn read_stream(
    url: &str,
    authorization: &str,
    initiator_id: &str,
    generation: u32,
    shared: &Shared,
    ctx: &egui::Context,
) -> Result<(), String> {
    let init = web_sys::RequestInit::new();
    init.set_method("GET");
    let headers = web_sys::Headers::new().map_err(js_error)?;
    headers.append("Authorization", authorization).map_err(js_error)?;
    headers.append("Accept", "text/event-stream").map_err(js_error)?;
    if !initiator_id.is_empty() {
        headers.append("Initiator-ID", initiator_id).map_err(js_error)?;
    }
    init.set_headers(&headers);
    let window = web_sys::window().ok_or("no window")?;
    let response = JsFuture::from(window.fetch_with_str_and_init(url, &init)).await.map_err(js_error)?;
    let response: web_sys::Response = response.dyn_into().map_err(|_| "fetch gave no Response".to_string())?;
    if !response.ok() {
        return Err(format!("status {}", response.status()));
    }
    let body = js_sys::Reflect::get(&response, &"body".into()).map_err(js_error)?;
    if body.is_null() || body.is_undefined() {
        return Err("answer without a body".into());
    }
    let reader = call_method(&body, "getReader")?;
    let mut pending: Vec<u8> = Vec::new();
    loop {
        let chunk = JsFuture::from(js_sys::Promise::from(call_method(&reader, "read")?)).await.map_err(js_error)?;
        if shared.generation.get() != generation {
            let _ = call_method(&reader, "cancel");
            return Ok(());
        }
        if js_sys::Reflect::get(&chunk, &"done".into()).map_err(js_error)?.as_bool().unwrap_or(false) {
            return Err("the server closed the stream".into());
        }
        let value = js_sys::Reflect::get(&chunk, &"value".into()).map_err(js_error)?;
        pending.extend_from_slice(&js_sys::Uint8Array::new(&value).to_vec());
        let mut arrived = false;
        while let Some((block, rest)) = split_block(&pending) {
            if let Some(event) = parse_block(&block) {
                let mut events = shared.events.borrow_mut();
                if events.len() >= MAX_QUEUED {
                    events.remove(0);
                }
                events.push(event);
                arrived = true;
            }
            pending = rest;
        }
        if pending.len() > MAX_EVENT_BYTES {
            log::warn!("edms events: dropped an event of more than {MAX_EVENT_BYTES} bytes");
            pending.clear();
        }
        if arrived {
            ctx.request_repaint();
        }
    }
}

/// The first complete event block (ended by an empty line) and what follows it.
fn split_block(bytes: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let mut line_start = 0;
    for (index, &byte) in bytes.iter().enumerate() {
        if byte != b'\n' {
            continue;
        }
        let line = &bytes[line_start..index];
        if line.is_empty() || line == b"\r" {
            return Some((bytes[..line_start].to_vec(), bytes[index + 1..].to_vec()));
        }
        line_start = index + 1;
    }
    None
}

/// An event block's name and data; comments (keep-alives) and blocks without a name are nothing.
fn parse_block(block: &[u8]) -> Option<StreamEvent> {
    let text = String::from_utf8_lossy(block);
    let mut kind = String::new();
    let mut data: Vec<&str> = Vec::new();
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("event:") {
            kind = value.trim().to_string();
        } else if let Some(value) = line.strip_prefix("data:") {
            data.push(value.strip_prefix(' ').unwrap_or(value));
        }
    }
    (!kind.is_empty() && !data.is_empty()).then(|| StreamEvent { kind, data: data.join("\n") })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_split_at_empty_lines_and_keep_the_rest() {
        let stream =
            b"event: postprocessing-finished\ndata: {\"itemid\":\"a\"}\n\n: keep-alive\n\nevent: item-trashed\r\ndata: {}\r\n\r\nevent: half";
        let (first, rest) = split_block(stream).unwrap();
        assert_eq!(parse_block(&first), Some(StreamEvent { kind: "postprocessing-finished".into(), data: "{\"itemid\":\"a\"}".into() }));
        let (comment, rest) = split_block(&rest).unwrap();
        assert_eq!(parse_block(&comment), None, "a keep-alive is no event");
        let (second, rest) = split_block(&rest).unwrap();
        assert_eq!(parse_block(&second), Some(StreamEvent { kind: "item-trashed".into(), data: "{}".into() }));
        assert_eq!(split_block(&rest), None, "an unfinished block waits for more bytes");
        assert_eq!(rest, b"event: half");
    }

    #[test]
    fn data_lines_join_and_a_block_without_data_is_nothing() {
        assert_eq!(parse_block(b"event: x\ndata: a\ndata:b"), Some(StreamEvent { kind: "x".into(), data: "a\nb".into() }));
        assert_eq!(parse_block(b"event: x"), None);
        assert_eq!(parse_block(b"data: only"), None);
    }
}
