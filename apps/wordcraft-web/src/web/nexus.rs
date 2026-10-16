//! Quantivus modification: bounded browser transport for the SAME WebShell WordApp.
//! No stdio process, TCP port, bearer token, filesystem path or second Session is created.

use std::cell::RefCell;
use std::collections::{HashSet, VecDeque};
use std::rc::Rc;

use js_sys::{Function, Object, Promise, Reflect};
use serde_json::{Value, json};
use wasm_bindgen::JsCast as _;
use wasm_bindgen::prelude::*;
use wordcraft_mcp::Backend;
use wordcraft_mcp::nexus::{self, LiveBackend, RequestGate};
use wordcraft_ui_egui::WordApp;
use wordcraft_ui_egui::control::{ControlRequest, Outcome};

const MAX_QUEUE: usize = 32;
const FACADE: &str = "QuantivusNexusWordCraftV1";

type Callback = Closure<dyn FnMut(JsValue) -> Promise>;
struct Pending {
    kind: &'static str,
    value: Value,
    deadline: f64,
    resolve: Function,
    reject: Function,
}
#[derive(Default)]
struct State {
    queue: VecDeque<Pending>,
    cancelled: HashSet<String>,
    closed: bool,
}

fn reject(function: &Function, code: &str) {
    let _ = function.call1(&JsValue::UNDEFINED, &js_sys::Error::new(code));
}
fn to_js(value: &Value) -> Result<JsValue, String> {
    let text = nexus::bounded_json(value)?;
    js_sys::JSON::parse(&text).map_err(|_| "nexus_result_serialization_failed".to_string())
}
fn from_js(value: &JsValue) -> Result<Value, String> {
    let text = js_sys::JSON::stringify(value).map_err(|_| "nexus_arguments_invalid")?.as_string().ok_or("nexus_arguments_invalid")?;
    if text.len() > nexus::MAX_REQUEST_BYTES {
        return Err("nexus_payload_limit".into());
    }
    serde_json::from_str(&text).map_err(|_| "nexus_arguments_invalid".into())
}
fn close(state: &Rc<RefCell<State>>) {
    if let Ok(mut state) = state.try_borrow_mut() {
        state.closed = true;
        for pending in state.queue.drain(..) {
            reject(&pending.reject, "nexus_editor_closed");
        }
    }
}

pub struct NexusBridge {
    state: Rc<RefCell<State>>,
    runtime_id: String,
    gate: RequestGate,
    callbacks: Vec<Callback>,
    pagehide: Closure<dyn FnMut(web_sys::Event)>,
}

impl NexusBridge {
    pub fn install(ctx: egui::Context) -> Result<Self, JsValue> {
        let window = web_sys::window().ok_or_else(|| js_sys::Error::new("nexus_window_unavailable"))?;
        if Reflect::has(window.as_ref(), &JsValue::from_str(FACADE))? {
            return Err(js_sys::Error::new("nexus_bridge_already_installed").into());
        }
        let mut random = [0u8; 16];
        window.crypto()?.get_random_values_with_u8_array(&mut random)?;
        let runtime_id = random.iter().map(|n| format!("{n:02x}")).collect::<String>();
        let state = Rc::new(RefCell::new(State::default()));
        let facade = Object::new();
        Reflect::set(&facade, &"apiVersion".into(), &JsValue::from_f64(f64::from(nexus::API_VERSION)))?;
        Reflect::set(&facade, &"applicationId".into(), &"wordcraft".into())?;
        Reflect::set(&facade, &"applicationVersion".into(), &env!("CARGO_PKG_VERSION").into())?;
        let mut callbacks = Vec::new();
        for (method, kind) in [("getCapabilities", "capabilities"), ("getDocumentSnapshot", "snapshot"), ("invoke", "invoke"), ("cancel", "cancel")] {
            let state = state.clone();
            let repaint = ctx.clone();
            let callback: Callback = Closure::new(move |input: JsValue| {
                let state = state.clone();
                let repaint = repaint.clone();
                Promise::new(&mut move |resolve: Function, failure: Function| {
                    let Ok(mut state) = state.try_borrow_mut() else {
                        reject(&failure, "nexus_editor_busy");
                        return;
                    };
                    if state.closed {
                        reject(&failure, "nexus_editor_closed");
                        return;
                    }
                    if kind == "cancel" {
                        let Some(id) = input.as_string().filter(|s| s.len() >= 16 && s.len() <= 128) else {
                            reject(&failure, "nexus_request_id_invalid");
                            return;
                        };
                        if state.cancelled.len() >= nexus::MAX_REQUEST_IDS {
                            reject(&failure, "nexus_request_limit");
                            return;
                        }
                        let index = state.queue.iter().position(|p| p.value.get("requestId").and_then(Value::as_str) == Some(&id));
                        if index.is_some() {
                            state.cancelled.insert(id.clone());
                        }
                        if let Some(index) = index
                            && let Some(pending) = state.queue.remove(index)
                        {
                            reject(&pending.reject, "nexus_request_cancelled");
                        }
                        let _ = resolve.call1(&JsValue::UNDEFINED, &JsValue::from_bool(index.is_some()));
                        return;
                    }
                    if state.queue.len() >= MAX_QUEUE {
                        reject(&failure, "nexus_queue_full");
                        return;
                    }
                    let value = if kind == "capabilities" {
                        json!({})
                    } else if kind == "snapshot" {
                        if input.is_undefined() || input.is_null() {
                            json!({})
                        } else if let Some(identity) = input.as_string().filter(|s| s.len() <= 1024) {
                            json!({"identity":identity})
                        } else {
                            reject(&failure, "nexus_arguments_invalid");
                            return;
                        }
                    } else {
                        match from_js(&input) {
                            Ok(value) => value,
                            Err(e) => {
                                reject(&failure, &e);
                                return;
                            }
                        }
                    };
                    let now = js_sys::Date::now();
                    let deadline = if kind == "invoke" {
                        let Some(iso) = value.get("deadlineAt").and_then(Value::as_str) else {
                            reject(&failure, "nexus_deadline_invalid");
                            return;
                        };
                        let deadline = js_sys::Date::parse(iso);
                        if !deadline.is_finite() || deadline <= now || deadline - now > 60_000.0 {
                            reject(&failure, "nexus_request_expired");
                            return;
                        }
                        let Some(id) = value.get("requestId").and_then(Value::as_str) else {
                            reject(&failure, "nexus_request_id_invalid");
                            return;
                        };
                        if state.cancelled.contains(id) {
                            reject(&failure, "nexus_request_cancelled");
                            return;
                        }
                        if state.queue.iter().any(|p| p.value.get("requestId").and_then(Value::as_str) == Some(id)) {
                            reject(&failure, "nexus_request_replayed");
                            return;
                        }
                        deadline
                    } else {
                        now + 5_000.0
                    };
                    state.queue.push_back(Pending { kind, value, deadline, resolve, reject: failure });
                    repaint.request_repaint();
                })
            });
            Reflect::set(&facade, &method.into(), callback.as_ref())?;
            callbacks.push(callback);
        }
        Object::freeze(&facade);
        let descriptor = Object::new();
        Reflect::set(&descriptor, &"value".into(), &facade)?;
        Reflect::set(&descriptor, &"writable".into(), &JsValue::FALSE)?;
        Reflect::set(&descriptor, &"configurable".into(), &JsValue::FALSE)?;
        Reflect::set(&descriptor, &"enumerable".into(), &JsValue::TRUE)?;
        let retire = state.clone();
        let pagehide = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| close(&retire));
        window.add_event_listener_with_callback("pagehide", pagehide.as_ref().unchecked_ref())?;
        let installed = Reflect::define_property(window.as_ref(), &FACADE.into(), &descriptor);
        if !matches!(installed, Ok(true)) {
            let _ = window.remove_event_listener_with_callback("pagehide", pagehide.as_ref().unchecked_ref());
            return Err(js_sys::Error::new("nexus_bridge_install_failed").into());
        }
        Ok(Self { state, runtime_id, gate: RequestGate::default(), callbacks, pagehide })
    }

    /// Called ONLY on the editor's logic thread after normal app logic. Never borrow WordApp
    /// from a JS closure or run an independent document in the transport.
    pub fn drain(&mut self, app: &mut WordApp, ctx: &egui::Context) {
        for _ in 0..4 {
            let next = self.state.try_borrow_mut().ok().and_then(|mut state| if state.closed { None } else { state.queue.pop_front() });
            let Some(pending) = next else { break };
            let now = js_sys::Date::now();
            if pending.deadline <= now {
                reject(&pending.reject, "nexus_request_expired");
                continue;
            }
            let mut backend = VisibleBackend { app, ctx, runtime_id: &self.runtime_id };
            let result = match pending.kind {
                "capabilities" => Ok(nexus::capabilities()),
                "snapshot" => backend.snapshot().and_then(|s| {
                    if pending.value.get("identity").is_some_and(|id| Some(id) != s.get("documentIdentity")) {
                        Err("nexus_document_stale".into())
                    } else {
                        Ok(s)
                    }
                }),
                "invoke" => self.gate.invoke(&mut backend, &pending.value, pending.deadline, now),
                _ => Err("nexus_method_unsupported".into()),
            };
            match result {
                Ok(value) => match to_js(&value) {
                    Ok(value) => {
                        let _ = pending.resolve.call1(&JsValue::UNDEFINED, &value);
                    }
                    Err(code) => reject(&pending.reject, &code),
                },
                Err(code) => reject(&pending.reject, &code),
            }
            ctx.request_repaint();
        }
        if self.state.try_borrow().is_ok_and(|state| !state.queue.is_empty()) {
            ctx.request_repaint();
        }
    }
}

impl Drop for NexusBridge {
    fn drop(&mut self) {
        close(&self.state);
        if let Some(window) = web_sys::window() {
            let _ = window.remove_event_listener_with_callback("pagehide", self.pagehide.as_ref().unchecked_ref());
        }
        // The frozen window facade cannot be removed. Keep its four tiny callbacks alive
        // until page teardown; they now reject explicitly rather than calling dropped Rust.
        for callback in self.callbacks.drain(..) {
            callback.forget();
        }
    }
}

struct VisibleBackend<'a> {
    app: &'a mut WordApp,
    ctx: &'a egui::Context,
    runtime_id: &'a str,
}
impl Backend for VisibleBackend<'_> {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let preserve_dirty = self.app.session.dirty
            && method == "engine.execute"
            && matches!(params.get("command").and_then(Value::as_str), Some("file.save" | "file.saveAs"));
        let document = self.app.session.document_id();
        let (request, _unused_reply) = ControlRequest::new(method, params);
        let outcome = wordcraft_ui_egui::control::handle(self.app, self.ctx, &request);
        // Triggering a browser download is not evidence of persistence. Never permit a later
        // automated New to discard this document solely because a download was initiated.
        if preserve_dirty && self.app.session.document_id() == document {
            self.app.session.dirty = true;
        }
        match outcome {
            Outcome::Done(value) if value.get("ok").and_then(Value::as_bool) == Some(true) => Ok(value.get("result").cloned().unwrap_or(Value::Null)),
            Outcome::Done(value) => Err(value.get("error").and_then(Value::as_str).unwrap_or("nexus_engine_error").to_string()),
            Outcome::Screenshot { .. } => Err("nexus_screenshot_unsupported".into()),
        }
    }
    fn has_ui(&self) -> bool {
        true
    }
    fn describe(&self) -> String {
        "visible WordCraft WebShell".into()
    }
}
impl LiveBackend for VisibleBackend<'_> {
    fn snapshot(&self) -> Result<Value, String> {
        nexus::document_snapshot(&self.app.session, self.runtime_id, self.app.has_document())
    }
    fn has_unsaved_changes(&self) -> bool {
        self.app.has_document() && self.app.session.dirty
    }
}
