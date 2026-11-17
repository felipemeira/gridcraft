//! Verto: the page side of co-editing (`gridcraft_engine::collab`). The host page carries the
//! updates between the people editing (in Verto, through Firebase); GridCraft only hands them
//! over and takes them in, over the same `postMessage` channel as the host messages.
//!
//! From the host:
//! - `gridcraft:collab-start` `{ role: "seed" }`: share the open workbook (the first person);
//!   GridCraft answers with `gridcraft:collab-snapshot`. `{ role: "join", bytes }`: join a shared
//!   workbook from its snapshot (the same file must be open).
//! - `gridcraft:collab-update` `{ bytes }`: someone else's changes.
//! - `gridcraft:collab-snapshot-request`: the whole shared document, for someone joining.
//! - `gridcraft:collab-stop`.
//!
//! To the host: `gridcraft:collab-update` `{ bytes }` with this person's changes,
//! `gridcraft:collab-snapshot` `{ bytes }`, and `gridcraft:collab-state` `{ active, error? }`.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use gridcraft_ui_egui::SheetApp;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen::closure::Closure;

enum Incoming {
    Seed,
    Join(Vec<u8>),
    Update(Vec<u8>),
    SnapshotRequest,
    Stop,
}

pub struct Link {
    queue: Rc<RefCell<VecDeque<Incoming>>>,
    _listener: Closure<dyn FnMut(web_sys::MessageEvent)>,
}

fn field(data: &JsValue, key: &str) -> Option<JsValue> {
    js_sys::Reflect::get(data, &JsValue::from_str(key)).ok().filter(|v| !v.is_undefined() && !v.is_null())
}

impl Link {
    /// Listens to the host page for co-editing messages; `None` without a same-origin parent.
    pub fn install(ctx: egui::Context) -> Option<Link> {
        let window = web_sys::window()?;
        window.parent().ok()??;
        let origin = window.location().origin().ok()?;
        let queue: Rc<RefCell<VecDeque<Incoming>>> = Rc::default();
        let q = queue.clone();
        let listener = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |ev: web_sys::MessageEvent| {
            if ev.origin() != origin {
                return;
            }
            let data = ev.data();
            let Some(kind) = field(&data, "type").and_then(|v| v.as_string()) else { return };
            let bytes = || field(&data, "bytes").map(|v| js_sys::Uint8Array::new(&v).to_vec());
            let message = match kind.as_str() {
                "gridcraft:collab-start" => match field(&data, "role").and_then(|v| v.as_string()).as_deref() {
                    Some("join") => bytes().map(Incoming::Join),
                    _ => Some(Incoming::Seed),
                },
                "gridcraft:collab-update" => bytes().map(Incoming::Update),
                "gridcraft:collab-snapshot-request" => Some(Incoming::SnapshotRequest),
                "gridcraft:collab-stop" => Some(Incoming::Stop),
                _ => None,
            };
            if let Some(m) = message {
                q.borrow_mut().push_back(m);
                ctx.request_repaint();
            }
        });
        window.add_event_listener_with_callback("message", listener.as_ref().unchecked_ref()).ok()?;
        Some(Link { queue, _listener: listener })
    }

    /// Takes in what the host sent since the last frame, then hands the host this person's
    /// changes.
    pub fn process(&self, app: &mut SheetApp) {
        loop {
            let next = self.queue.borrow_mut().pop_front();
            let Some(message) = next else { break };
            match message {
                Incoming::Seed => match app.session.collab_seed() {
                    Ok(snapshot) => {
                        post("gridcraft:collab-snapshot", Some(&snapshot), None);
                        post_state(true, None);
                    }
                    Err(e) => post_state(false, Some(&e.to_string())),
                },
                Incoming::Join(snapshot) => match app.session.collab_join(&snapshot) {
                    Ok(()) => post_state(true, None),
                    Err(e) => post_state(false, Some(&e.to_string())),
                },
                Incoming::Update(update) => {
                    if let Err(e) = app.session.collab_receive(&update) {
                        log::warn!("{e}");
                        post_state(app.session.collab_active(), Some(&e.to_string()));
                    }
                }
                Incoming::SnapshotRequest => match app.session.collab_snapshot() {
                    Ok(snapshot) => post("gridcraft:collab-snapshot", Some(&snapshot), None),
                    Err(e) => post_state(false, Some(&e.to_string())),
                },
                Incoming::Stop => {
                    app.session.collab_stop();
                    post_state(false, None);
                }
            }
        }
        self.flush(app);
    }

    /// Hands the host the changes made since the last call.
    pub fn flush(&self, app: &mut SheetApp) {
        for update in app.session.collab_outbox() {
            post("gridcraft:collab-update", Some(&update), None);
        }
    }
}

fn post_state(active: bool, error: Option<&str>) {
    post("gridcraft:collab-state", None, Some((active, error)));
}

fn post(kind: &str, bytes: Option<&[u8]>, state: Option<(bool, Option<&str>)>) {
    let Some(window) = web_sys::window() else { return };
    let Ok(Some(parent)) = window.parent() else { return };
    let Ok(origin) = window.location().origin() else { return };
    let message = js_sys::Object::new();
    let set = |k: &str, v: &JsValue| {
        let _ = js_sys::Reflect::set(&message, &JsValue::from_str(k), v);
    };
    set("type", &JsValue::from_str(kind));
    if let Some(bytes) = bytes {
        set("bytes", &js_sys::Uint8Array::from(bytes).buffer().into());
    }
    if let Some((active, error)) = state {
        set("active", &JsValue::from_bool(active));
        if let Some(error) = error {
            set("error", &JsValue::from_str(error));
        }
    }
    if let Err(e) = parent.post_message(&message, &origin) {
        log::warn!("co-editing: couldn't post {kind}: {e:?}");
    }
}
