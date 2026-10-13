//! The estudio bridge: lets the page that embeds PhotoCraft in an iframe open a document in it,
//! save it back, hear about unsaved changes, and drive the app through its control interface
//! (`photocraft_ui_egui::control`), all with `postMessage`.
//!
//! Only active when the editor is loaded inside a frame with `?bridge` in its URL; otherwise the
//! web build behaves exactly as upstream's. Messages are accepted only from the embedding page's
//! own origin (the editor is served from the same origin as the app); anything else is ignored.
//!
//! From the embedding page:
//! - `{type: "open", name, bytes, language?}`: opens the document (through the inbox, like a dropped
//!   file) and switches the interface language when one is given (`"es"`, `"en"`, …);
//! - `{type: "save", name?}`: saves the active document as a layered file named `name` (default
//!   `document.psd`); the bytes come back as `saved`;
//! - `{type: "control", id, method, params}`: one control request; the answer comes back as
//!   `reply {id, result}`.
//!
//! To the embedding page:
//! - `{type: "ready", version}` once the app runs;
//! - `{type: "saved", name, bytes, warnings}` after `save`, and after the user's own File › Save,
//!   which no longer downloads a file while embedded; `{type: "save-failed", error}`;
//! - `{type: "changed", unsaved}` whenever the app gains or loses unsaved changes;
//! - `{type: "reply", id, result}`.

use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

use photocraft_ui_egui::{ControlRequest, PhotocraftApp, i18n};
use wasm_bindgen::JsCast as _;
use wasm_bindgen::prelude::Closure;

/// Saved files waiting to be posted: (name, bytes).
pub type Outbox = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

enum Incoming {
    Save(String),
    Control(wasm_bindgen::JsValue, String, serde_json::Value),
}

pub struct Bridge {
    origin: String,
    incoming: Arc<Mutex<Vec<Incoming>>>,
    outbox: Outbox,
    control: Sender<ControlRequest>,
    waiting: Vec<(wasm_bindgen::JsValue, Receiver<serde_json::Value>)>,
    unsaved: Option<bool>,
}

/// Whether this page is an embedded editor that should talk to its parent.
pub fn wanted(query: &str) -> bool {
    let embedded = web_sys::window().and_then(|w| w.parent().ok().flatten()).is_some_and(|parent| {
        web_sys::window().is_some_and(|me| !js_sys::Object::is(&parent, &me))
    });
    embedded && query.split(['?', '&']).any(|p| p == "bridge")
}

/// The interface language from the URL (`?lang=es`), so the first frame is already in it.
pub fn language_from(query: &str) {
    if let Some(code) = query.split(['?', '&']).find_map(|p| p.strip_prefix("lang=")) {
        set_language(code);
    }
}

fn set_language(code: &str) {
    match i18n::Lang::from_code(code) {
        Some(lang) => i18n::set_current(lang),
        None => log::warn!("bridge: unknown language {code}"),
    }
}

fn get(value: &wasm_bindgen::JsValue, key: &str) -> wasm_bindgen::JsValue {
    js_sys::Reflect::get(value, &key.into()).unwrap_or(wasm_bindgen::JsValue::UNDEFINED)
}

impl Bridge {
    /// Starts listening to the parent. `inbox` is where documents to open go (the app opens them
    /// on its next frame); `control` feeds the app's control channel (`PhotocraftApp::with_control`).
    pub fn start(inbox: Arc<Mutex<Vec<(String, Vec<u8>)>>>, outbox: Outbox, control: Sender<ControlRequest>, ctx: egui::Context) -> Option<Self> {
        let window = web_sys::window()?;
        let origin = window.location().origin().ok()?;
        let incoming: Arc<Mutex<Vec<Incoming>>> = Arc::default();
        let queue = incoming.clone();
        let accepted = origin.clone();
        let listener = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
            if event.origin() != accepted {
                return;
            }
            let data = event.data();
            let kind = get(&data, "type").as_string().unwrap_or_default();
            match kind.as_str() {
                "open" => {
                    let name = get(&data, "name").as_string().unwrap_or_else(|| "document.psd".into());
                    let bytes = js_sys::Uint8Array::new(&get(&data, "bytes")).to_vec();
                    if let Some(code) = get(&data, "language").as_string() {
                        set_language(&code);
                    }
                    inbox.lock().unwrap_or_else(|e| e.into_inner()).push((name, bytes));
                }
                "save" => {
                    let name = get(&data, "name").as_string().unwrap_or_else(|| "document.psd".into());
                    queue.lock().unwrap_or_else(|e| e.into_inner()).push(Incoming::Save(name));
                }
                "control" => {
                    let params = js_sys::JSON::stringify(&get(&data, "params"))
                        .ok()
                        .and_then(|s| s.as_string())
                        .and_then(|s| serde_json::from_str(&s).ok())
                        .unwrap_or(serde_json::Value::Null);
                    let method = get(&data, "method").as_string().unwrap_or_default();
                    queue.lock().unwrap_or_else(|e| e.into_inner()).push(Incoming::Control(get(&data, "id"), method, params));
                }
                _ => return,
            }
            ctx.request_repaint();
        });
        window.add_event_listener_with_callback("message", listener.as_ref().unchecked_ref()).ok()?;
        // The listener lives as long as the page.
        listener.forget();
        let bridge = Self { origin, incoming, outbox, control, waiting: Vec::new(), unsaved: None };
        bridge.post(&[("type", "ready".into()), ("version", env!("CARGO_PKG_VERSION").into())]);
        Some(bridge)
    }

    fn post(&self, fields: &[(&str, wasm_bindgen::JsValue)]) {
        let message = js_sys::Object::new();
        for (key, value) in fields {
            let _ = js_sys::Reflect::set(&message, &(*key).into(), value);
        }
        if let Some(parent) = web_sys::window().and_then(|w| w.parent().ok().flatten()) {
            if let Err(e) = parent.post_message(&message, &self.origin) {
                log::error!("bridge: couldn't post to the parent: {e:?}");
            }
        }
    }

    fn json(value: &serde_json::Value) -> wasm_bindgen::JsValue {
        js_sys::JSON::parse(&value.to_string()).unwrap_or(wasm_bindgen::JsValue::NULL)
    }

    /// Once per frame, with the app: run what the parent asked for, and tell it what changed.
    pub fn tick(&mut self, app: &mut PhotocraftApp) {
        let incoming = std::mem::take(&mut *self.incoming.lock().unwrap_or_else(|e| e.into_inner()));
        for message in incoming {
            match message {
                Incoming::Save(name) => {
                    if let Err(error) = app.save_automation(Some(name)) {
                        self.post(&[("type", "save-failed".into()), ("error", error.into())]);
                    }
                }
                Incoming::Control(id, method, params) => {
                    let (request, reply) = ControlRequest::new(method, params);
                    if self.control.send(request).is_ok() {
                        self.waiting.push((id, reply));
                    }
                }
            }
        }
        let saved = std::mem::take(&mut *self.outbox.lock().unwrap_or_else(|e| e.into_inner()));
        for (name, bytes) in saved {
            let array = js_sys::Uint8Array::from(bytes.as_slice());
            self.post(&[("type", "saved".into()), ("name", name.into()), ("bytes", array.into())]);
        }
        let mut still = Vec::new();
        for (id, reply) in std::mem::take(&mut self.waiting) {
            match reply.try_recv() {
                Ok(result) => self.post(&[("type", "reply".into()), ("id", id), ("result", Self::json(&result))]),
                Err(std::sync::mpsc::TryRecvError::Empty) => still.push((id, reply)),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.post(&[("type", "reply".into()), ("id", id), ("result", Self::json(&serde_json::json!({"ok": false, "error": "no reply"})))])
                }
            }
        }
        self.waiting = still;
        let unsaved = app.has_unsaved_work();
        if self.unsaved != Some(unsaved) {
            self.unsaved = Some(unsaved);
            self.post(&[("type", "changed".into()), ("unsaved", unsaved.into())]);
        }
    }
}
