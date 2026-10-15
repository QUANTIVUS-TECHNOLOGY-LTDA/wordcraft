# Quantivus WordCraft Web bridge v1

This is a Quantivus modification, not an official upstream browser MCP API. Native
`wordcraft-cli mcp` remains unchanged and has a separate session. The bridge is implemented
in Rust; no JavaScript reaches into private WASM memory and no second document is created.

## Export and same-document ownership

`window.QuantivusNexusWordCraftV1` is a non-writable, non-configurable, frozen facade.
The shell installs it once and services its bounded Promise queue on the actual
`WebShell.app` after regular editor logic. Four requests at most are drained per frame,
with 32 outstanding requests at most. New calls fail closed after the editor is disposed.

The functions are `getCapabilities()`, `getDocumentSnapshot(identity?)`,
`invoke(request)`, and `cancel(requestId)`. Each returns a Promise. A cancellation returns
whether it removed a queued request; `false` does NOT certify that an already-dispatched
operation was undone. There is no automatic retry.

A request has exactly these fields:

```json
{
  "operation": "tools/call",
  "originalName": "type_text",
  "arguments": {"text": "A document edited through Nexus"},
  "expectedDocumentIdentity": "the exact observed documentIdentity",
  "expectedDocumentRevision": "the exact observed revision",
  "requestId": "a-new-unique-UUID-for-this-operation",
  "deadlineAt": "an ISO-8601 deadline no more than 60 seconds ahead"
}
```

The response is `{result, snapshot, documentChanged}`. `result` retains the upstream MCP
`content` and `isError` structure. `snapshot` is `{documentIdentity, revision}`. The identity
combines a cryptographically random editor-instance identifier and the engine's actual
`Session::document_id()`. The revision includes `Session::rev()` and a digest of the editing
context (selection, pending formatting, view, author and cell/column selection), so manual
edits and caret changes invalidate earlier commands. Replacing a document changes its
identity, even for another unsaved document. The Nexus host MUST retire/rebind the old
handle when `documentChanged` is true; it must not relabel the old document as the new one.

## Supported operations

The eleven wrappers use the original `wordcraft_mcp::tools` implementation and original
input schemas, with explicit Quantivus/browser provenance: `list_commands`, `execute`,
`batch`, `new_document`, `save_document`, `type_text`, `select_text`, `get_text`,
`inspect_document`, `parity`, and `render_page`.

`execute` and command discovery share a fail-closed, reviewed command allowlist in
`crates/mcp/src/nexus.rs`. Upstream additions are not automatically approved. An allowlisted
name still has to exist and be enabled in the actual engine. This does not promise all
upstream commands work on the Web. `parity` describes the upstream engine, not Web-adapter
coverage.

Native filesystem reads, URLs, macros, arbitrary commands, desktop screenshots and input
injection are not exposed. `save_document` accepts only a single filename with a supported
extension, never an absolute or relative path. It uses the editor's existing browser save
service; its download or pending-save receipt is NOT proof that a disk/cloud save finished.
A dirty document cannot be replaced by `new_document`. Browser file opening remains the
existing user-controlled picker; a future byte-import capability must have its own schema.

## Bounds, lifetime and trust

Requests are bounded to 1 MiB; inserted/selected text to 128 KiB and 2,048 newlines; batches
to 32 commands; inserted tables to 1,000 cells. The complete batch is validated before
execution. Engine-level errors can still leave a batch partially applied; the original
MCP result reports those partial outcomes. Request IDs are never evicted/replayed within
an instance; 16,384 IDs exhaust the remote capability until a fresh editor instance exists.
Queued expired/cancelled requests cannot execute on a later frame.

This facade is not authorization. The trusted Nexus parent owns OAuth credentials,
workspace/user checks, server-side command admission, single-use consent, private
MessageChannel routing and file/cloud grants. No credential is sent to this facade.
Other scripts in the same editor origin already share that editor's trust boundary;
production integration must use verified assets in the isolated Nexus frame.

## Verification

Native engine tests: `cargo test -p wordcraft-mcp nexus::tests`.
WASM compilation: `cargo check -p wordcraft-web --target wasm32-unknown-unknown --locked`.
Full upstream gates: `cargo xtask ci`.

Compilation and core unit tests do not prove a production Nexus/ChatGPT/browser round trip.
A release also requires actual Web tests (visible edit/readback, document switch, cancelled
queue, stale revision and unsaved work), a verified build digest and a host integration test.
No deployment or native-runtime activation is implied by adding this source.

## Integration merge status

The owner explicitly requested committing and merging this development implementation into the Quantivus fork main branch after being informed of the outstanding verification. This is not a production-release certification.

The latest completed focused checks were 28 MCP/automation-root tests, Clippy with warnings denied, and the actual WASM build. Full upstream CI still has two color-emoji rendering failures also reproduced on unmodified upstream. The real Chromium editor run passed nine cases but the manual keyboard-edit test was not green; authenticated Nexus deployment and independent final Cline review remain unverified.

Request and response JSON are bounded to 1 MiB. Rejected pre-dispatch requests do not consume the dispatched-request replay ledger. Initiating a browser save does not establish persistence; the bridge conservatively retains previously dirty state until the actual save flow confirms it. No compiled WASM, fonts, local test files or credentials are part of this commit.
