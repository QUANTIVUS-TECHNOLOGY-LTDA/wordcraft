//! Quantivus modification: guarded browser adapter over the original MCP dispatcher.
//!
//! This module owns no document, socket, filesystem root or credential. The browser supplies
//! a backend borrowing the visible editor for exactly one synchronous turn. Original tool
//! implementations remain in `tools`; unsupported browser commands fail closed here.

use std::collections::HashSet;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use wordcraft_engine::Session;

use crate::{Backend, tools};

pub const API_VERSION: u32 = 1;
pub const MAX_REQUEST_BYTES: usize = 1_048_576;
pub const MAX_TEXT_BYTES: usize = 131_072;
pub const MAX_BATCH: usize = 32;
pub const MAX_REQUEST_IDS: usize = 16_384;
pub const MAX_RESULT_BYTES: usize = 1_048_576;
/// Bound serialization before crossing the Rust/JavaScript boundary, including escaped UTF-8.
pub fn bounded_json(value: &Value) -> Result<String, String> {
    struct Limited(Vec<u8>);
    impl std::io::Write for Limited {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.0.len().checked_add(bytes.len()).is_none_or(|len| len > MAX_RESULT_BYTES) {
                return Err(std::io::Error::other("nexus_result_limit"));
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut output = Limited(Vec::with_capacity(4096));
    serde_json::to_writer(&mut output, value).map_err(|_| "nexus_result_limit".to_string())?;
    String::from_utf8(output.0).map_err(|_| "nexus_result_serialization_failed".to_string())
}

/// A backend of an actual editor, never a second headless session.
pub trait LiveBackend: Backend {
    fn snapshot(&self) -> Result<Value, String>;
    fn has_unsaved_changes(&self) -> bool;
}

/// Include the real document generation AND editing context. A user moving the caret or
/// changing pending formatting must invalidate an earlier request even without a text edit.
pub fn document_snapshot(session: &Session, runtime_id: &str, has_document: bool) -> Result<Value, String> {
    let state = serde_json::to_vec(&json!({
        "selection": session.sel,
        "pending": session.pending,
        "view": session.view,
        "author": session.author,
        "initials": session.initials,
        "cells": format!("{:?}", session.cell_range()),
        "column": format!("{:?}", session.column_segments()),
        "hasDocument": has_document,
    }))
    .map_err(|_| "nexus_snapshot_unavailable".to_string())?;
    let digest: String = Sha256::digest(state).iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(json!({
        "documentIdentity": format!("wordcraft-web/{runtime_id}/{}", session.document_id()),
        "revision": format!("{}:{digest}", session.rev()),
    }))
}

/// Reviewed browser-safe engine operations. New upstream commands are NOT implicitly exposed.
/// `engine.commands` is filtered with this same policy, so discovery and execution agree.
pub fn command_allowed(id: &str) -> bool {
    matches!(
        id,
        "document.text"
            | "document.inspect"
            | "file.new"
            | "file.setAuthor"
            | "file.save"
            | "file.saveAs"
            | "file.exportPdf"
            | "file.exportPng"
            | "text.insert"
            | "text.newParagraph"
            | "text.lineBreak"
            | "text.tab"
            | "text.backspace"
            | "text.delete"
            | "text.deleteWord"
            | "text.deleteWordBack"
            | "select.text"
            | "select.all"
            | "select.range"
            | "select.none"
            | "format.set"
            | "format.bold"
            | "format.italic"
            | "format.underline"
            | "format.strike"
            | "format.clear"
            | "format.font"
            | "format.size"
            | "format.color"
            | "format.highlight"
            | "format.superscript"
            | "format.subscript"
            | "format.grow"
            | "format.shrink"
            | "format.changeCase"
            | "para.set"
            | "para.alignLeft"
            | "para.alignCenter"
            | "para.alignRight"
            | "para.justify"
            | "para.borders"
            | "para.shading"
            | "para.indent"
            | "para.outdent"
            | "styles.apply"
            | "styles.modify"
            | "styles.create"
            | "styles.delete"
            | "styles.list"
            | "insert.table"
            | "insert.pageBreak"
            | "insert.symbol"
            | "insert.textBox"
            | "insert.bookmark"
            | "insert.hyperlink"
            | "insert.footnote"
            | "insert.endnote"
            | "table.insertRow"
            | "table.insertColumn"
            | "table.deleteRow"
            | "table.deleteColumn"
            | "table.mergeCells"
            | "table.splitCells"
            | "table.borders"
            | "table.shading"
            | "table.autoFit"
            | "table.properties"
            | "table.select"
            | "table.newStyle"
            | "table.modifyStyle"
            | "review.trackChanges"
            | "review.accept"
            | "review.reject"
            | "review.acceptAll"
            | "review.rejectAll"
            | "review.newComment"
            | "review.deleteComment"
            | "review.nextChange"
            | "review.previousChange"
            | "page.margins"
            | "page.size"
            | "page.orientation"
            | "page.columns"
            | "layout.margins"
            | "layout.size"
            | "layout.orientation"
            | "layout.columns"
            | "edit.undo"
            | "edit.redo"
    )
}

fn filename_allowed(value: &Value) -> bool {
    let Some(name) = value.as_str() else { return false };
    !name.is_empty()
        && name.len() <= 200
        && !name.chars().any(|c| c.is_control() || matches!(c, '/' | '\\' | ':' | '\0'))
        && name != "."
        && name != ".."
        && name.rsplit_once('.').is_some_and(|(_, extension)| {
            matches!(extension.to_ascii_lowercase().as_str(), "docx" | "odt" | "rtf" | "html" | "md" | "tex" | "txt" | "json" | "pdf" | "png")
        })
}

fn is_save(id: &str) -> bool {
    matches!(id, "file.save" | "file.saveAs" | "file.exportPdf" | "file.exportPng")
}

fn command_valid(id: &str, params: &Value) -> bool {
    if !command_allowed(id) || !params.is_object() {
        return false;
    }
    // No pass-through macros, original/native filesystem paths, recovery files or remote URLs.
    // Hyperlink targets are inert document content; this adapter never navigates/fetches them.
    if let Some(path) = params.get("path")
        && (!is_save(id) || !filename_allowed(path))
    {
        return false;
    }
    if params.get("original").is_some() || params.get("commands").is_some() || params.get("file").is_some() {
        return false;
    }
    if id == "file.new" && params.get("template").is_some_and(|v| !matches!(v.as_str(), Some("blank" | "sample" | "letter" | "resume" | "report"))) {
        return false;
    }
    if id == "insert.table" {
        let rows = params.get("rows").and_then(Value::as_u64).unwrap_or(0);
        let cols = params.get("cols").and_then(Value::as_u64).unwrap_or(0);
        if rows == 0 || cols == 0 || rows > 100 || cols > 50 || rows.saturating_mul(cols) > 1000 {
            return false;
        }
    }
    true
}

fn supported(name: &str) -> bool {
    matches!(
        name,
        "list_commands"
            | "execute"
            | "batch"
            | "new_document"
            | "save_document"
            | "type_text"
            | "select_text"
            | "get_text"
            | "inspect_document"
            | "parity"
            | "render_page"
    )
}

pub fn capabilities() -> Value {
    let Value::Array(descriptors) = tools::tool_definitions() else { return json!([]) };
    Value::Array(descriptors.into_iter().filter_map(|mut descriptor| {
        let name = descriptor.get("name")?.as_str()?.to_string();
        if !supported(&name) { return None; }
        // These are the original tool dispatcher/schema with an explicit browser restriction,
        // not a claim that the upstream Web release exported a vendor MCP catalog.
        let previous = descriptor.get("description").and_then(Value::as_str).unwrap_or("").to_string();
        descriptor["description"] = json!(format!("Quantivus browser adapter v1. {previous} Only reviewed in-document commands are available. Native filesystem paths are refused. Saves return a browser download receipt, not proof of persistence. New documents are refused when unsaved changes exist."));
        descriptor["_meta"] = json!({"quantivus/provenance":"guarded-upstream-dispatcher", "quantivus/bridgeVersion":API_VERSION});
        Some(json!({"kind":"tool", "name":name, "enabled":true, "toolDescriptor":descriptor}))
    }).collect())
}

fn validate_tool(name: &str, args: &Value) -> Result<(), String> {
    if !supported(name) || !args.is_object() {
        return Err("nexus_tool_unsupported".into());
    }
    let invalid = || "nexus_arguments_invalid".to_string();
    match name {
        "execute" => {
            let id = args.get("command").and_then(Value::as_str).ok_or_else(invalid)?;
            if !command_valid(id, args.get("params").unwrap_or(&json!({}))) {
                return Err("nexus_command_unsupported".into());
            }
        }
        "batch" => {
            let commands = args.get("commands").and_then(Value::as_array).ok_or_else(invalid)?;
            if commands.is_empty() || commands.len() > MAX_BATCH {
                return Err(invalid());
            }
            // Validate ALL commands first: a hidden file operation late in a batch must not
            // allow earlier writes to execute. Original dispatcher retains partial failures.
            for item in commands {
                validate_tool("execute", item)?;
                let id = item.get("command").and_then(Value::as_str).unwrap_or("");
                if id == "file.new" || is_save(id) {
                    return Err("nexus_lifecycle_batch_unsupported".into());
                }
            }
        }
        "new_document" => {
            if !command_valid("file.new", args) {
                return Err(invalid());
            }
        }
        "save_document" => {
            if args.get("path").is_some_and(|p| !filename_allowed(p)) {
                return Err("nexus_filename_invalid".into());
            }
        }
        "type_text" | "select_text" => {
            let text = args.get("text").and_then(Value::as_str).ok_or_else(invalid)?;
            if text.len() > MAX_TEXT_BYTES || text.bytes().filter(|c| *c == b'\n').count() > 2048 {
                return Err("nexus_text_limit".into());
            }
            if args.get("paragraphs").is_some_and(|v| !v.is_boolean()) {
                return Err(invalid());
            }
            if args.get("occurrence").is_some_and(|v| v.as_u64().is_none_or(|n| n == 0 || n > 100_000)) {
                return Err(invalid());
            }
        }
        "render_page" => {
            if args.get("page").is_some_and(|v| v.as_u64().is_none_or(|n| n == 0 || n > 10_000)) {
                return Err(invalid());
            }
            if args.get("scale").is_some_and(|v| v.as_f64().is_none_or(|n| !n.is_finite() || !(0.1..=2.0).contains(&n))) {
                return Err(invalid());
            }
        }
        _ => {}
    }
    Ok(())
}

struct Guarded<'a, B: LiveBackend> {
    inner: &'a mut B,
}
impl<B: LiveBackend> Backend for Guarded<'_, B> {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        match method {
            "engine.execute" => {
                let id = params.get("command").and_then(Value::as_str).ok_or("nexus_command_unsupported")?;
                let args = params.get("params").cloned().unwrap_or(json!({}));
                if !command_valid(id, &args) {
                    return Err("nexus_command_unsupported".into());
                }
                if id == "file.new" && self.inner.has_unsaved_changes() {
                    return Err("nexus_unsaved_changes".into());
                }
                self.inner.call(method, params)
            }
            "engine.commands" => {
                let Value::Array(commands) = self.inner.call(method, params)? else { return Err("nexus_catalog_invalid".into()) };
                Ok(Value::Array(commands.into_iter().filter(|c| c.get("id").and_then(Value::as_str).is_some_and(command_allowed)).collect()))
            }
            "document.inspect" | "ui.parity" | "ui.render" => self.inner.call(method, params),
            _ => Err("nexus_method_unsupported".into()),
        }
    }
    fn has_ui(&self) -> bool {
        true
    }
    fn describe(&self) -> String {
        "Quantivus visible WordCraft Web editor".into()
    }
}

/// A dispatched request id is never reused within this editor lifetime, including failed writes.
/// Requests rejected before dispatch do not consume the bounded replay ledger.
/// Exhaustion disables remote writes instead of evicting old ids and enabling replay.
#[derive(Default)]
pub struct RequestGate {
    seen: HashSet<String>,
}
impl RequestGate {
    pub fn invoke<B: LiveBackend>(&mut self, backend: &mut B, request: &Value, deadline_ms: f64, now_ms: f64) -> Result<Value, String> {
        if serde_json::to_vec(request).map_err(|_| "nexus_arguments_invalid")?.len() > MAX_REQUEST_BYTES {
            return Err("nexus_payload_limit".into());
        }
        let map = request.as_object().ok_or("nexus_arguments_invalid")?;
        const KEYS: &[&str] =
            &["operation", "originalName", "arguments", "expectedDocumentIdentity", "expectedDocumentRevision", "requestId", "deadlineAt"];
        if map.len() != KEYS.len() || map.keys().any(|k| !KEYS.contains(&k.as_str())) {
            return Err("nexus_arguments_invalid".into());
        }
        let id = request.get("requestId").and_then(Value::as_str).ok_or("nexus_request_id_invalid")?;
        if id.len() < 16 || id.len() > 128 || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
            return Err("nexus_request_id_invalid".into());
        }
        if self.seen.contains(id) {
            return Err("nexus_request_replayed".into());
        }
        if !now_ms.is_finite() || !deadline_ms.is_finite() || deadline_ms <= now_ms || deadline_ms - now_ms > 60_000.0 {
            return Err("nexus_request_expired".into());
        }
        if request.get("operation").and_then(Value::as_str) != Some("tools/call") {
            return Err("nexus_operation_unsupported".into());
        }
        let before = backend.snapshot()?;
        for (expected, actual) in [("expectedDocumentIdentity", "documentIdentity"), ("expectedDocumentRevision", "revision")] {
            if request.get(expected).and_then(Value::as_str).is_none() || request.get(expected) != before.get(actual) {
                return Err("nexus_document_stale".into());
            }
        }
        let name = request.get("originalName").and_then(Value::as_str).ok_or("nexus_tool_unsupported")?;
        let args = request.get("arguments").ok_or("nexus_arguments_invalid")?;
        validate_tool(name, args)?;
        if self.seen.len() >= MAX_REQUEST_IDS {
            return Err("nexus_request_limit".into());
        }
        self.seen.insert(id.to_string());
        let result = tools::call_tool(&mut Guarded { inner: backend }, name, args).to_value();
        let after = backend.snapshot()?;
        Ok(json!({"result":result, "snapshot":after, "documentChanged":before.get("documentIdentity") != after.get("documentIdentity")}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Headless;

    #[derive(Default)]
    struct Editor {
        inner: Headless,
    }
    impl Backend for Editor {
        fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
            self.inner.call(method, params)
        }
        fn has_ui(&self) -> bool {
            true
        }
        fn describe(&self) -> String {
            "real engine test backend".into()
        }
    }
    impl LiveBackend for Editor {
        fn snapshot(&self) -> Result<Value, String> {
            document_snapshot(&self.inner.session, "test-runtime-001", true)
        }
        fn has_unsaved_changes(&self) -> bool {
            self.inner.session.dirty
        }
    }
    fn request(editor: &Editor, id: &str, name: &str, args: Value) -> Value {
        let s = editor.snapshot().unwrap();
        json!({"operation":"tools/call", "originalName":name, "arguments":args,
            "requestId":id, "deadlineAt":"2030-01-01T00:00:00Z",
            "expectedDocumentIdentity":s["documentIdentity"], "expectedDocumentRevision":s["revision"]})
    }
    #[test]
    fn type_and_read_use_the_existing_engine() {
        let mut editor = Editor::default();
        let mut gate = RequestGate::default();
        let r = request(&editor, "request-type-00001", "type_text", json!({"text":"Nexus visible document"}));
        let answer = gate.invoke(&mut editor, &r, 1000.0, 0.0).unwrap();
        assert_eq!(answer["result"]["isError"], false);
        let r = request(&editor, "request-read-00001", "get_text", json!({}));
        let answer = gate.invoke(&mut editor, &r, 1000.0, 0.0).unwrap();
        assert!(answer.to_string().contains("Nexus visible document"));
    }
    #[test]
    fn manual_edit_invalidates_request_without_remote_write() {
        let mut editor = Editor::default();
        let mut gate = RequestGate::default();
        let r = request(&editor, "request-stale-0001", "type_text", json!({"text":"must not appear"}));
        editor.inner.session.run("text.insert", &json!({"text":"manual"})).unwrap();
        let rev = editor.inner.session.rev();
        assert_eq!(gate.invoke(&mut editor, &r, 1000.0, 0.0).unwrap_err(), "nexus_document_stale");
        assert_eq!(editor.inner.session.rev(), rev);
    }
    #[test]
    fn replay_is_rejected() {
        let mut editor = Editor::default();
        let mut gate = RequestGate::default();
        let r = request(&editor, "request-replay-001", "get_text", json!({}));
        gate.invoke(&mut editor, &r, 1000.0, 0.0).unwrap();
        assert_eq!(gate.invoke(&mut editor, &r, 1000.0, 0.0).unwrap_err(), "nexus_request_replayed");
    }
    #[test]
    fn expiration_does_not_modify_document() {
        let mut editor = Editor::default();
        let mut gate = RequestGate::default();
        let r = request(&editor, "request-expired-01", "type_text", json!({"text":"no"}));
        let rev = editor.inner.session.rev();
        assert_eq!(gate.invoke(&mut editor, &r, 10.0, 11.0).unwrap_err(), "nexus_request_expired");
        assert_eq!(editor.inner.session.rev(), rev);
    }
    #[test]
    fn batch_is_fully_validated_before_dispatch() {
        let mut editor = Editor::default();
        let mut gate = RequestGate::default();
        let r = request(
            &editor,
            "request-batch-0001",
            "batch",
            json!({"commands":[
                {"command":"text.insert","params":{"text":"no"}}, {"command":"file.open","params":{"path":"C:\\secret.docx"}}
            ]}),
        );
        let rev = editor.inner.session.rev();
        assert!(gate.invoke(&mut editor, &r, 1000.0, 0.0).is_err());
        assert_eq!(editor.inner.session.rev(), rev);
    }
    #[test]
    fn unsaved_document_is_not_replaced() {
        let mut editor = Editor::default();
        let mut gate = RequestGate::default();
        editor.inner.session.run("text.insert", &json!({"text":"keep me"})).unwrap();
        let document = editor.inner.session.document_id();
        let r = request(&editor, "request-newdoc-001", "new_document", json!({}));
        let answer = gate.invoke(&mut editor, &r, 1000.0, 0.0).unwrap();
        assert_eq!(answer["result"]["isError"], true);
        assert_eq!(editor.inner.session.document_id(), document);
    }
    #[test]
    fn document_replacement_changes_identity() {
        let mut editor = Editor::default();
        let before = editor.snapshot().unwrap();
        editor.inner.session.set_document(wordcraft_doc::Document::new());
        assert_ne!(editor.snapshot().unwrap()["documentIdentity"], before["documentIdentity"]);
    }
    #[test]
    fn browser_paths_and_unreviewed_tools_are_refused() {
        for filename in ["../secret.docx", "C:\\secret.docx", "https://a/x.docx", "a/b.docx", "..", "a.exe"] {
            assert!(!filename_allowed(&json!(filename)));
        }
        assert!(filename_allowed(&json!("brief.docx")));
        assert!(!command_allowed("macro.run"));
        assert!(!command_allowed("review.compare"));
        assert!(!supported("open_document"));
        assert!(!supported("screenshot"));
    }
    #[test]
    fn malformed_tables_and_oversized_batch_are_refused() {
        assert!(!command_valid("insert.table", &json!({"rows":1000000000u64,"cols":1000000000u64})));
        assert!(validate_tool("batch", &json!({"commands":vec![json!({"command":"format.bold"});MAX_BATCH+1]})).is_err());
    }
    #[test]
    fn rejected_requests_do_not_exhaust_the_dispatch_ledger() {
        let mut editor = Editor::default();
        let mut gate = RequestGate::default();
        let mut r = request(&editor, "request-not-sent-001", "get_text", json!({}));
        assert!(gate.invoke(&mut editor, &r, 10.0, 11.0).is_err());
        r["expectedDocumentRevision"] = json!("stale");
        assert!(gate.invoke(&mut editor, &r, 1000.0, 0.0).is_err());
        assert!(gate.seen.is_empty());
        let valid = request(&editor, "request-not-sent-001", "get_text", json!({}));
        assert!(gate.invoke(&mut editor, &valid, 1000.0, 0.0).is_ok());
        assert_eq!(gate.seen.len(), 1);
    }
    #[test]
    fn result_encoding_is_byte_bounded_and_preserves_small_results() {
        let small = json!({"text":"Nexus / UTF-8", "content":[1,2,3]});
        assert_eq!(bounded_json(&small).unwrap(), small.to_string());
        assert_eq!(bounded_json(&json!({"text":"x".repeat(MAX_RESULT_BYTES)})).unwrap_err(), "nexus_result_limit");
    }
    #[test]
    fn capabilities_retain_original_schema_and_label_provenance() {
        let c = capabilities();
        assert_eq!(c.as_array().unwrap().len(), 11);
        for capability in c.as_array().unwrap() {
            assert_eq!(capability["name"], capability["toolDescriptor"]["name"]);
            assert!(capability["toolDescriptor"]["inputSchema"].is_object());
            assert_eq!(capability["toolDescriptor"]["_meta"]["quantivus/bridgeVersion"], API_VERSION);
        }
    }
}
