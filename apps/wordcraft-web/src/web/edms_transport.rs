//! The EDMS transport of the web build (kosmos): every request is a browser `fetch`, answered in
//! a later frame. The engine's `edms.*` commands plan the requests and read the answers
//! (`craft-opencore`, area `edms`); this file only moves bytes and asks for a repaint when one arrived.

use std::cell::RefCell;
use std::rc::Rc;

use craft_opencore::edms::{HttpRequest, HttpResponse, RequestId, Transport};
use wasm_bindgen::JsCast as _;
use wasm_bindgen_futures::JsFuture;

/// Most bytes of an answer kept (a document from the EDMS); larger answers are an error.
const MAX_BODY: usize = 512 << 20;

/// The answer headers the module reads; the browser hides the rest of a cross-origin answer
/// anyway, and the EDMS is same-origin.
const WANTED_HEADERS: [&str; 7] = ["etag", "oc-etag", "oc-fileid", "content-type", "lock-token", "last-modified", "content-length"];

/// Requests in flight, answers waiting for the next `finished`.
#[derive(Clone)]
pub struct FetchTransport {
    ctx: egui::Context,
    done: Rc<RefCell<Vec<(RequestId, Result<HttpResponse, String>)>>>,
}

impl FetchTransport {
    pub fn new(ctx: egui::Context) -> Self {
        Self { ctx, done: Rc::default() }
    }
}

impl Transport for FetchTransport {
    fn start(&mut self, id: RequestId, request: HttpRequest) {
        let done = self.done.clone();
        let ctx = self.ctx.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let result = fetch(request).await;
            done.borrow_mut().push((id, result));
            ctx.request_repaint();
        });
    }

    fn finished(&mut self) -> Vec<(RequestId, Result<HttpResponse, String>)> {
        std::mem::take(&mut *self.done.borrow_mut())
    }
}

fn js_error(value: wasm_bindgen::JsValue) -> String {
    value
        .as_string()
        .or_else(|| js_sys::Reflect::get(&value, &"message".into()).ok().and_then(|m| m.as_string()))
        .unwrap_or_else(|| format!("{value:?}"))
}

async fn fetch(request: HttpRequest) -> Result<HttpResponse, String> {
    let init = web_sys::RequestInit::new();
    init.set_method(request.method.as_str());
    let headers = web_sys::Headers::new().map_err(js_error)?;
    for (name, value) in &request.headers {
        headers.append(name, value).map_err(js_error)?;
    }
    init.set_headers(&headers);
    if !request.body.is_empty() {
        let body = js_sys::Uint8Array::from(request.body.as_slice());
        init.set_body(&body);
    }
    let window = web_sys::window().ok_or("no window")?;
    let response = JsFuture::from(window.fetch_with_str_and_init(&super::uncached(&request.url, &init), &init)).await.map_err(js_error)?;
    let response: web_sys::Response = response.dyn_into().map_err(|_| "fetch gave no Response".to_string())?;
    let status = response.status();
    let mut answer = HttpResponse::new(status);
    for name in WANTED_HEADERS {
        if let Ok(Some(value)) = response.headers().get(name) {
            answer.headers.push((name.to_string(), value));
        }
    }
    let buffer = JsFuture::from(response.array_buffer().map_err(js_error)?).await.map_err(js_error)?;
    let bytes = js_sys::Uint8Array::new(&buffer);
    let length = bytes.length() as usize;
    if length > MAX_BODY {
        return Err(format!("answer of {length} bytes is larger than {MAX_BODY}"));
    }
    answer.body = bytes.to_vec();
    Ok(answer)
}
