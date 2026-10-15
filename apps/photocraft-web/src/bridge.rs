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
//! - out `{type: "state", revision}`: the document, the layers picked, the tool or the open
//!   dialogs changed (estudio's panels re-read then).
//! - `{type: "control", id, method, params}`: one control request; the answer comes back as
//!   `reply {id, result}`.
//!
//! To the embedding page:
//! - `{type: "ready", version}` once the app runs;
//! - `{type: "saved", name, bytes, warnings}` after `save`, and after the user's own File › Save,
//!   which no longer downloads a file while embedded; `{type: "save-failed", error}`;
//! - `{type: "changed", unsaved}` whenever the app gains or loses unsaved changes;
//! - `{type: "reply", id, result}`.
//! - in `{type: "tool-options", options}` → out `{type: "tool-options", ok}`: merges fields into
//!   the tool options (the options bar estudio draws itself; most have no `ui.set` field).
//! - in `{type: "thumbnails", id, side}` → out `{type: "thumbnails", id, document, layers}`: RGBA
//!   thumbnails (`{width, height, pixels}`) of the document and of each layer with pixels (`id`).

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
    /// Fields to merge into the tool options (the options bar's state), as `ui.inspect` names them.
    ToolOptions(serde_json::Value),
    /// Thumbnails of the document and of every layer with pixels, at most `side` pixels.
    Thumbnails(wasm_bindgen::JsValue, u32),
}

pub struct Bridge {
    origin: String,
    incoming: Arc<Mutex<Vec<Incoming>>>,
    outbox: Outbox,
    control: Sender<ControlRequest>,
    waiting: Vec<(wasm_bindgen::JsValue, Receiver<serde_json::Value>)>,
    unsaved: Option<bool>,
    /// A language to switch the interface to on the next frame. It goes into the app's
    /// `interface.language` preference: the app applies that preference every frame, so setting
    /// only the current language would last one frame.
    language: Arc<Mutex<Option<String>>>,
    /// To ask for another frame while control requests wait: the app reads its control channel
    /// at the start of a frame and the bridge feeds it at the end, and the app draws a new frame
    /// only when something happens, so without this a request could wait for the next mouse move.
    ctx: egui::Context,
    /// What the last `state` message described (document revision, active and selected layers,
    /// tool, open dialogs), so a new one goes out only when one of them moves.
    state: String,
}

/// Whether this page is an embedded editor that should talk to its parent.
pub fn wanted(query: &str) -> bool {
    let embedded = web_sys::window().and_then(|w| w.parent().ok().flatten()).is_some_and(|parent| {
        web_sys::window().is_some_and(|me| !js_sys::Object::is(&parent, &me))
    });
    embedded && query.split(['?', '&']).any(|p| p == "bridge")
}

/// The interface language asked for in the URL (`?lang=es`), if any.
pub fn language_from(query: &str) -> Option<String> {
    query.split(['?', '&']).find_map(|p| p.strip_prefix("lang=")).map(str::to_string)
}

/// A language the app knows, or `None` (logged).
fn known_language(code: &str) -> Option<String> {
    if i18n::Lang::from_code(code).is_some() {
        Some(code.to_ascii_lowercase())
    } else {
        log::warn!("bridge: unknown language {code}");
        None
    }
}

fn get(value: &wasm_bindgen::JsValue, key: &str) -> wasm_bindgen::JsValue {
    js_sys::Reflect::get(value, &key.into()).unwrap_or(wasm_bindgen::JsValue::UNDEFINED)
}

impl Bridge {
    /// Starts listening to the parent. `inbox` is where documents to open go (the app opens them
    /// on its next frame); `control` feeds the app's control channel (`PhotocraftApp::with_control`).
    pub fn start(inbox: Arc<Mutex<Vec<(String, Vec<u8>)>>>, outbox: Outbox, control: Sender<ControlRequest>, language: Option<String>, ctx: egui::Context) -> Option<Self> {
        let window = web_sys::window()?;
        let origin = window.location().origin().ok()?;
        let incoming: Arc<Mutex<Vec<Incoming>>> = Arc::default();
        let queue = incoming.clone();
        let language = Arc::new(Mutex::new(language.as_deref().and_then(known_language)));
        let wanted_language = language.clone();
        let accepted = origin.clone();
        let repaint = ctx.clone();
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
                    if let Some(code) = get(&data, "language").as_string().as_deref().and_then(known_language) {
                        *wanted_language.lock().unwrap_or_else(|e| e.into_inner()) = Some(code);
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
                "tool-options" => {
                    let options = js_sys::JSON::stringify(&get(&data, "options"))
                        .ok()
                        .and_then(|s| s.as_string())
                        .and_then(|s| serde_json::from_str(&s).ok())
                        .unwrap_or(serde_json::Value::Null);
                    queue.lock().unwrap_or_else(|e| e.into_inner()).push(Incoming::ToolOptions(options));
                }
                "thumbnails" => {
                    let side = get(&data, "side").as_f64().unwrap_or(48.0).clamp(16.0, 256.0) as u32;
                    queue.lock().unwrap_or_else(|e| e.into_inner()).push(Incoming::Thumbnails(get(&data, "id"), side));
                }
                _ => return,
            }
            repaint.request_repaint();
        });
        window.add_event_listener_with_callback("message", listener.as_ref().unchecked_ref()).ok()?;
        // The listener lives as long as the page.
        listener.forget();
        let bridge = Self { origin, incoming, outbox, control, waiting: Vec::new(), unsaved: None, language, ctx, state: String::new() };
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
        if let Some(code) = self.language.lock().unwrap_or_else(|e| e.into_inner()).take()
            && app.session.prefs().interface.language != code
        {
            app.session.edit_prefs(|p| p.interface.language = code);
        }
        let incoming = std::mem::take(&mut *self.incoming.lock().unwrap_or_else(|e| e.into_inner()));
        for message in incoming {
            match message {
                Incoming::Save(name) => {
                    if let Err(error) = app.save_automation(Some(name)) {
                        self.post(&[("type", "save-failed".into()), ("error", error.into())]);
                    }
                }
                Incoming::ToolOptions(patch) => {
                    let mut ok = false;
                    if let (Ok(serde_json::Value::Object(mut current)), serde_json::Value::Object(patch)) = (serde_json::to_value(&app.ui.tool_options), patch) {
                        for (key, value) in patch {
                            current.insert(key, value);
                        }
                        if let Ok(options) = serde_json::from_value(serde_json::Value::Object(current)) {
                            app.ui.tool_options = options;
                            ok = true;
                        }
                    }
                    self.post(&[("type", "tool-options".into()), ("ok", ok.into())]);
                }
                Incoming::Thumbnails(id, side) => {
                    let Some(state) = app.session.active() else { continue };
                    let doc = &state.doc;
                    let image = |width: u32, height: u32, pixels: &[u8]| {
                        let o = js_sys::Object::new();
                        let _ = js_sys::Reflect::set(&o, &"width".into(), &(width as f64).into());
                        let _ = js_sys::Reflect::set(&o, &"height".into(), &(height as f64).into());
                        let _ = js_sys::Reflect::set(&o, &"pixels".into(), &js_sys::Uint8Array::from(pixels).into());
                        o
                    };
                    let whole = photocraft_compose::thumbnail(doc, side);
                    let document = image(whole.width, whole.height, &whole.pixels);
                    // Each layer with pixels, laid out in document space like the Layers panel's own
                    // thumbnails (`thumb_image` in photocraft-ui-egui): a `side`-square, centred.
                    let layers = js_sys::Array::new();
                    let (w, h) = (doc.size.width.max(1) as f32, doc.size.height.max(1) as f32);
                    let scale = w.max(h) / side as f32;
                    let (ox, oy) = ((side as f32 - w / scale) / 2.0, (side as f32 - h / scale) / 2.0);
                    let mut px = [0.0f32; 8];
                    for (_, _, layer) in doc.walk() {
                        let Some(surface) = layer.surface() else { continue };
                        let n = surface.channels().min(px.len());
                        let mut pixels = vec![0u8; (side * side * 4) as usize];
                        for ty in 0..side {
                            for tx in 0..side {
                                let dx = (tx as f32 - ox + 0.5) * scale;
                                let dy = (ty as f32 - oy + 0.5) * scale;
                                if dx < 0.0 || dy < 0.0 || dx >= w || dy >= h {
                                    continue;
                                }
                                surface.read_pixel(dx as i32, dy as i32, &mut px[..n]);
                                let c = photocraft_raster::to_rgba(&surface.format(), &px[..n]);
                                let at = ((ty * side + tx) * 4) as usize;
                                for k in 0..4 {
                                    pixels[at + k] = (c[k].clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
                                }
                            }
                        }
                        let o = image(side, side, &pixels);
                        let _ = js_sys::Reflect::set(&o, &"id".into(), &(layer.id.0 as f64).into());
                        layers.push(&o);
                    }
                    self.post(&[("type", "thumbnails".into()), ("id", id), ("document", document.into()), ("layers", layers.into())]);
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
        if !self.waiting.is_empty() {
            self.ctx.request_repaint();
        }
        // `{type: "state", revision}` whenever the document, the layers picked, the tool or the
        // open dialogs change: the embedding page re-reads only then (`document.inspect`,
        // `ui.inspect`) instead of asking every frame. Selecting a layer is not a revision step.
        let doc = app.session.active();
        let state = format!(
            "{}|{:?}|{:?}|{:?}|{}",
            doc.map_or(0, |d| d.revision),
            doc.and_then(|d| d.active_layer),
            doc.map(|d| d.selected_layers.clone()),
            app.ui.tool,
            app.ui.dialogs.len()
        );
        if state != self.state {
            self.state = state;
            let revision = doc.map_or(0.0, |d| d.revision as f64);
            self.post(&[("type", "state".into()), ("revision", revision.into())]);
        }
        let unsaved = app.has_unsaved_work();
        if self.unsaved != Some(unsaved) {
            self.unsaved = Some(unsaved);
            self.post(&[("type", "changed".into()), ("unsaved", unsaved.into())]);
        }
    }
}
