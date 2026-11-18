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
//! - `gridcraft:documents` `{ json: [{ name, detail }] }`: the documents the host offers, shown in
//!   File › Open and Recent; choosing one sends `gridcraft:open-request` `{ json: "name" }`, and a
//!   file picked with Browse… goes to the host as `gridcraft:upload` `{ name, bytes }`.
//! - `gridcraft:collab-peers` `{ json }`: the others, as JSON `[{ id, name, color: "#rrggbb", sheet,
//!   ranges: ["A1:B2"], active: "A1" }]`; the grid draws their selections. `gridcraft:collab-goto`
//!   `{ json: { sheet, cell } }`: go to where someone is.
//!
//! To the host: `gridcraft:collab-update` `{ bytes }` with this person's changes,
//! `gridcraft:collab-snapshot` `{ bytes }`, `gridcraft:collab-state` `{ active, error? }`, and
//! `gridcraft:collab-presence` `{ json: { sheet, ranges, active } }` when this person's selection
//! changes.

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
    Peers(String),
    GoTo(String),
    Documents(String),
}

pub struct Link {
    queue: Rc<RefCell<VecDeque<Incoming>>>,
    /// The presence last sent, so it is sent again only when it changes.
    presence: RefCell<Option<String>>,
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
                "gridcraft:collab-peers" => field(&data, "json").and_then(|v| v.as_string()).map(Incoming::Peers),
                "gridcraft:collab-goto" => field(&data, "json").and_then(|v| v.as_string()).map(Incoming::GoTo),
                "gridcraft:documents" => field(&data, "json").and_then(|v| v.as_string()).map(Incoming::Documents),
                _ => None,
            };
            if let Some(m) = message {
                q.borrow_mut().push_back(m);
                ctx.request_repaint();
            }
        });
        window.add_event_listener_with_callback("message", listener.as_ref().unchecked_ref()).ok()?;
        Some(Link { queue, presence: RefCell::new(None), _listener: listener })
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
                Incoming::Peers(json) => app.session.collab_peers = peers(&json),
                Incoming::GoTo(json) => go_to(app, &json),
                Incoming::Documents(json) => app.host_documents = Some(documents(&json)),
            }
        }
        self.flush(app);
    }

    /// Hands the host the changes made since the last call.
    pub fn flush(&self, app: &mut SheetApp) {
        for update in app.session.collab_outbox() {
            post("gridcraft:collab-update", Some(&update), None);
        }
        let presence = app
            .session
            .collab_presence()
            .map(|(sheet, ranges, active)| serde_json::json!({"sheet": sheet, "ranges": ranges, "active": active}).to_string());
        if presence != *self.presence.borrow() {
            if let Some(json) = &presence {
                post_json("gridcraft:collab-presence", json);
            }
            *self.presence.borrow_mut() = presence;
        }
    }
}

/// The others, from the host's JSON; entries that don't read are left out.
fn peers(json: &str) -> Vec<gridcraft_engine::collab::Peer> {
    use gridcraft_engine::core::{CellRef, RangeRef};
    let list: Vec<serde_json::Value> = serde_json::from_str(json).unwrap_or_default();
    list.iter()
        .filter_map(|p| {
            let hex = p["color"].as_str()?.trim_start_matches('#');
            let rgb = u32::from_str_radix(hex, 16).ok()?;
            Some(gridcraft_engine::collab::Peer {
                id: p["id"].as_str()?.to_string(),
                name: p["name"].as_str().unwrap_or_default().chars().take(40).collect(),
                color: [(rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8],
                sheet: p["sheet"].as_u64()? as usize,
                ranges: p["ranges"].as_array()?.iter().filter_map(|r| RangeRef::parse(r.as_str()?)).take(64).collect(),
                active: CellRef::parse(p["active"].as_str()?)?,
            })
        })
        .take(50)
        .collect()
}

/// The host's documents, from `[{ name, detail }]`.
fn documents(json: &str) -> Vec<(String, String)> {
    let list: Vec<serde_json::Value> = serde_json::from_str(json).unwrap_or_default();
    list.iter()
        .filter_map(|d| Some((d["name"].as_str()?.chars().take(200).collect(), d["detail"].as_str().unwrap_or_default().chars().take(200).collect())))
        .take(500)
        .collect()
}

/// Goes to where someone is: `{ sheet, cell }`.
fn go_to(app: &mut SheetApp, json: &str) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else { return };
    let (Some(sheet), Some(cell)) = (v["sheet"].as_u64(), v["cell"].as_str()) else { return };
    let Some(name) = app.session.active().and_then(|d| d.wb.sheets.get(sheet as usize)).map(|s| s.name.clone()) else { return };
    let reference = format!("'{}'!{cell}", name.replace('\'', "''"));
    if let Err(e) = app.session.execute("edit.goTo", serde_json::json!({"reference": reference})) {
        log::warn!("co-editing: couldn't go to {reference}: {e}");
    }
}

pub(super) fn post_json(kind: &str, json: &str) {
    let Some(window) = web_sys::window() else { return };
    let Ok(Some(parent)) = window.parent() else { return };
    let Ok(origin) = window.location().origin() else { return };
    let message = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&message, &JsValue::from_str("type"), &JsValue::from_str(kind));
    let _ = js_sys::Reflect::set(&message, &JsValue::from_str("json"), &JsValue::from_str(json));
    if let Err(e) = parent.post_message(&message, &origin) {
        log::warn!("co-editing: couldn't post {kind}: {e:?}");
    }
}

/// Hands the host a file picked here (File › Open, Browse…): it keeps it with the other
/// documents and opens it.
pub(super) fn post_file(kind: &str, name: &str, bytes: &[u8]) {
    let Some(window) = web_sys::window() else { return };
    let Ok(Some(parent)) = window.parent() else { return };
    let Ok(origin) = window.location().origin() else { return };
    let message = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&message, &JsValue::from_str("type"), &JsValue::from_str(kind));
    let _ = js_sys::Reflect::set(&message, &JsValue::from_str("name"), &JsValue::from_str(name));
    let _ = js_sys::Reflect::set(&message, &JsValue::from_str("bytes"), &js_sys::Uint8Array::from(bytes).buffer().into());
    if let Err(e) = parent.post_message(&message, &origin) {
        log::warn!("couldn't post {kind}: {e:?}");
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
