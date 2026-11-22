//! EDMS commands (kosmos): the document's home in OpenCore/OpenCloud, driven by the same ids from
//! the ribbon, the page API, the control channel and MCP. A command submits a request and
//! answers `{pending: id}`; the outcome arrives through `edms.poll` (or the front end's frame,
//! `Session::edms_pump`). See `opencraft/EDMS_NATIVE.md` §9.

use craft_opencore::edms::events::RemoteEvent;
use craft_opencore::edms::{Config, EdmsError, Location, Lookup, Request};
use serde_json::{Value, json};

use crate::edms::{Action, advice_name, error_json};
use crate::{CmdError, CmdResult, CommandSpec, Session, p};

/// Default lock timeout, as the web client uses it.
const DEFAULT_LOCK_SECS: u64 = 1800;
/// Longest a search pattern may be.
const MAX_PATTERN: usize = 200;
const EDMS: &str = "File › EDMS";

pub fn specs() -> Vec<CommandSpec> {
    vec![
        CommandSpec::new("edms.connect", "Connect to the EDMS", EDMS, connect)
            .params(r#"{"origin": "https://cloud.example.org", "token"?: bearer token from the host page | "login"?: user, "appToken"?: app token (Basic), "initiatorId"?: this client's id for the server's events} → {"connected": true, "origin"}"#)
            .pure(),
        CommandSpec::new("edms.disconnect", "Disconnect from the EDMS", EDMS, disconnect).pure(),
        CommandSpec::new("edms.status", "EDMS status", EDMS, status)
            .params(r#"{} → {"connected", "origin", "inFlight", "document": {"drive", "path", "name", "id", "etag", "writable", "locked", "state": clean|localChanged|remoteChanged|conflict|gone} | null}"#)
            .pure(),
        CommandSpec::new("edms.poll", "EDMS results", EDMS, poll)
            .params(r#"{} → {"results": [{"id", "kind", "ok", ...} ...]} (the outcomes of pending requests since the last poll; applied already)"#)
            .pure(),
        CommandSpec::new("edms.me", "Who am I in the EDMS", EDMS, me).params(r#"{} → {"pending": id}; result {"me": {"id", "displayName", "mail", "preferredLanguage"}}"#).pure(),
        CommandSpec::new("edms.drives", "Spaces", EDMS, drives).params(r#"{} → {"pending": id}; result {"drives": [{"id", "name", "type", "alias"} ...]}"#).pure(),
        CommandSpec::new("edms.list", "Folder contents", EDMS, list)
            .params(r#"{"drive": space id, "path"?: "/Folder" (default "/")} → {"pending": id}; result {"entries": [{"name", "id", "path", "folder", "size", "etag", "modified", "writable", "canCreateFile"} ...]}"#)
            .pure(),
        CommandSpec::new("edms.stat", "Entry details", EDMS, stat).params(r#"{"drive", "path"} → {"pending": id}; result {"entry": {...}}"#).pure(),
        CommandSpec::new("edms.search", "Search the EDMS", EDMS, search).params(r#"{"query": text, "limit"?: 1..500} → {"pending": id}; result {"entries": [...]}"#).pure(),
        CommandSpec::new("edms.open", "Open from the EDMS", EDMS, open)
            .params(r#"{"drive", "path": "/Folder/name.docx"} → {"pending": id}; result {"opened": true, "name", "etag", "document"}. Opens beside the current document when that has unsaved changes."#)
            .pure(),
        CommandSpec::new("edms.fetch", "Fetch a file from the EDMS", EDMS, fetch)
            .params(r#"{"drive", "path": "/Folder/picture.png"} → {"pending": id}; result {"fetched": true, "name", "size", "data": base64}. The bytes as they are, for the front end (a picture to insert, a recipient list); nothing opens."#)
            .pure(),
        CommandSpec::new("edms.pwd", "Working directory of a document", EDMS, pwd)
            .params(r#"{"document"?: id (default: the active one)} → {"document", "drive", "folder": "/Folder"}. The folder the document lives in; paths of files beside it are relative to this. Fails for a document that was never saved to the EDMS."#)
            .pure(),
        CommandSpec::new("edms.sidecar", "A file beside a document", EDMS, sidecar)
            .params(r#"{"path": "pictures/logo.png" (relative to the document; ".." may go up, never out of the space), "document"?: id (default: the active one), "data"?: true (include the bytes), "refresh"?: true (fetch again)} → {"document", "path", "state": "ready" | "pending" | "failed", "size"?, "etag"?, "contentType"?, "data"?: base64, "error"?: {"code", "message"}}. The first call starts the fetch and answers pending; ask again after the UI event {"edms": {"event": "sidecar", "document"}}."#)
            .pure(),
        CommandSpec::new("edms.runWith", "Run a command with a file beside the document", EDMS, run_with)
            .params(r#"{"command": "insert.picture", "path": "pictures/logo.png" (relative to the active document), "params"?: the command's other parameters, "as"?: the parameter that takes the bytes as base64 (default "data")} → {"ran": true, "result"} when the file was at hand, else {"pending": "sidecar", "path", "document"}; then the command runs when the file has arrived, result {"kind": "deferred", "ok", "command", "path", "document", "error"?} in edms.poll. The command runs once, with the bytes; it does not run if the fetch fails."#)
            .pure(),
        CommandSpec::new("edms.adopt", "Adopt the open document's EDMS location", EDMS, adopt)
            .params(r#"{"drive", "path": "/Folder/name.docx", "id"?: file id, "etag"?: the version the host loaded, "permissions"?: "RDNVWZP"} → {"adopted": true}. For a document the host page loaded itself: from now on edms.save, edms.saveAs and edms.event work on it."#)
            .pure(),
        CommandSpec::new("edms.save", "Save to the EDMS", EDMS, save)
            .params(r#"{"incomplete"?: true (save although the collaboration still owes content: what is only a preview is saved as the preview)} → {"pending": id}; result {"saved": true, "etag"} or an error with code conflict (the server has a newer version: edms.reload or edms.saveAs). In a collaboration with content still on its way the command refuses without "incomplete" and names what is missing."#)
            .when(needs_edms_document)
            .pure(),
        CommandSpec::new("edms.saveAs", "Save As in the EDMS", EDMS, save_as)
            .params(r#"{"drive", "path": "/Folder", "name": "Copy.docx", "overwrite"?: false, "switch"?: true (the document lives at the copy from now on)} → {"pending": id, "incomplete"?: [{"command", "key", "size", "seq"}] (in a collaboration: content still on its way, the copy holds its preview)}; result {"saved": true, "drive", "path", "etag"}"#)
            .pure(),
        CommandSpec::new("edms.export", "Export to the EDMS", EDMS, export)
            .params(r#"{"drive", "path": "/Folder", "name": "Report.pdf" (the extension picks the format), "overwrite"?: false} → {"pending": id}; result {"etag", "fileId"}"#)
            .pure(),
        CommandSpec::new("edms.reload", "Reload from the EDMS", EDMS, reload)
            .params(r#"{} → {"pending": id}; the server's version replaces the document (unsaved changes are lost)"#)
            .when(needs_edms_document)
            .pure(),
        CommandSpec::new("edms.lock", "Lock in the EDMS", EDMS, lock).params(r#"{"timeout"?: seconds (1800)} → {"pending": id}; result {"lock": {"token", "owner", "timeout"}}"#).when(needs_edms_document).pure(),
        CommandSpec::new("edms.unlock", "Unlock in the EDMS", EDMS, unlock).params(r#"{} → {"pending": id}"#).when(needs_edms_document).pure(),
        CommandSpec::new("edms.event", "EDMS server event", EDMS, event)
            .params(r#"{"type": "postprocessing-finished" | "file-touched" | "file-locked" | "file-unlocked" | "item-renamed" | "item-moved" | "item-trashed", "data": the event's JSON (itemid, etag, initiatorid)} → {"concerns": bool, "foreign": bool, "advice": none|reload|conflict|saveAs}"#)
            .pure(),
    ]
}

fn needs_edms_document(s: &Session) -> Option<&'static str> {
    if s.edms.document(s.document_id()).is_some() { None } else { Some("the document was not opened from the EDMS (edms.open or edms.saveAs first)") }
}

fn require(v: &Value, key: &str) -> Result<String, CmdError> {
    p::str(v, key).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).ok_or_else(|| CmdError::Params(format!("`{key}` is required")))
}

fn connect(s: &mut Session, v: &Value) -> CmdResult {
    let origin = require(v, "origin")?;
    if !(origin.starts_with("https://") || origin.starts_with("http://")) {
        return Err(CmdError::Params("`origin` is an http(s) URL".into()));
    }
    let initiator = p::str(v, "initiatorId").map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).unwrap_or_else(|| "wordcraft".to_string());
    let config = if let Some(token) = p::str(v, "token").filter(|t| !t.trim().is_empty()) {
        Config::with_bearer(&origin, token.trim(), &initiator)
    } else {
        let login = require(v, "login").map_err(|_| CmdError::Params("`token` (bearer) or `login` and `appToken` are required".into()))?;
        let app_token = require(v, "appToken")?;
        Config::with_app_token(&origin, &login, &app_token, &initiator)
    };
    let port = s.edms.port_mut()?;
    port.connect(config);
    Ok(json!({"connected": true, "origin": s.edms.origin()}))
}

fn disconnect(s: &mut Session, _v: &Value) -> CmdResult {
    s.edms.port_mut()?.disconnect();
    Ok(json!({"connected": false}))
}

fn status(s: &mut Session, _v: &Value) -> CmdResult {
    s.edms_pump();
    let document = s.edms.document(s.document_id()).map(|d| d.json());
    Ok(json!({
        "transport": s.edms.has_port(),
        "connected": s.edms.connected(),
        "origin": s.edms.origin(),
        "inFlight": s.edms.in_flight(),
        "document": document,
    }))
}

fn poll(s: &mut Session, _v: &Value) -> CmdResult {
    s.edms_pump();
    Ok(json!({"results": s.edms.take_results(), "inFlight": s.edms.in_flight()}))
}

fn pending(s: &mut Session, request: Request, action: Action) -> CmdResult {
    let id = s.edms.submit(request, action)?;
    Ok(json!({"pending": id}))
}

fn me(s: &mut Session, _v: &Value) -> CmdResult {
    pending(s, Request::Me, Action::Plain)
}

fn drives(s: &mut Session, _v: &Value) -> CmdResult {
    pending(s, Request::Drives, Action::Plain)
}

fn drive_and_path(v: &Value) -> Result<(String, String), CmdError> {
    let drive_id = require(v, "drive")?;
    let path = p::str(v, "path").map(str::trim).filter(|s| !s.is_empty()).unwrap_or("/");
    Ok((drive_id, craft_opencore::edms::location::normalize_path(path)))
}

fn list(s: &mut Session, v: &Value) -> CmdResult {
    let (drive_id, path) = drive_and_path(v)?;
    pending(s, Request::List { drive_id, path }, Action::Plain)
}

fn stat(s: &mut Session, v: &Value) -> CmdResult {
    let (drive_id, path) = drive_and_path(v)?;
    pending(s, Request::Stat { drive_id, path }, Action::Plain)
}

fn search(s: &mut Session, v: &Value) -> CmdResult {
    let pattern: String = require(v, "query")?.chars().take(MAX_PATTERN).collect();
    let limit = p::u64(v, "limit").unwrap_or(50).clamp(1, 500) as u32;
    pending(s, Request::Search { pattern, limit }, Action::Plain)
}

fn open(s: &mut Session, v: &Value) -> CmdResult {
    let (drive_id, path) = drive_and_path(v)?;
    let name = Location::new(&drive_id, &path).name().to_string();
    if !craft_opencore::edms::location::valid_name(&name) {
        return Err(CmdError::Params("`path` must name a file".into()));
    }
    pending(s, Request::Get { drive_id, path }, Action::Open { name })
}

fn fetch(s: &mut Session, v: &Value) -> CmdResult {
    let (drive_id, path) = drive_and_path(v)?;
    let name = Location::new(&drive_id, &path).name().to_string();
    if !craft_opencore::edms::location::valid_name(&name) {
        return Err(CmdError::Params("`path` must name a file".into()));
    }
    pending(s, Request::Get { drive_id, path }, Action::Fetch { name })
}

fn edms_error(error: EdmsError) -> CmdError {
    match error {
        EdmsError::NotConnected => CmdError::Disabled(error.to_string()),
        EdmsError::Params(p) => CmdError::Params(p),
        other => CmdError::Failed(other.to_string()),
    }
}

/// The document a command is about: the one it names, else the active one.
fn document_of(s: &Session, v: &Value) -> u64 {
    p::u64(v, "document").unwrap_or_else(|| s.document_id())
}

fn pwd(s: &mut Session, v: &Value) -> CmdResult {
    let document = document_of(s, v);
    let work_dir = s.edms.work_dir(document).map_err(edms_error)?;
    Ok(json!({"document": document, "drive": work_dir.drive_id, "folder": work_dir.folder}))
}

fn sidecar(s: &mut Session, v: &Value) -> CmdResult {
    let document = document_of(s, v);
    let path = require(v, "path")?;
    if p::bool(v, "refresh").unwrap_or(false) {
        s.edms.refresh_sidecar(document, &path).map_err(edms_error)?;
    }
    let with_data = p::bool(v, "data").unwrap_or(false);
    Ok(match s.edms.sidecar(document, &path).map_err(edms_error)? {
        Lookup::Pending => json!({"document": document, "path": path, "state": "pending"}),
        Lookup::Failed(error) => json!({"document": document, "path": path, "state": "failed", "error": error_json(error)}),
        Lookup::Ready(file) => {
            let mut value = json!({"document": document, "path": path, "state": "ready", "size": file.bytes.len(), "etag": file.etag, "contentType": file.content_type});
            if with_data {
                if file.bytes.len() > crate::edms::MAX_FETCH_BYTES {
                    return Err(CmdError::Failed(format!("{path} is too big to hand over ({} bytes)", file.bytes.len())));
                }
                value["data"] = json!(super::insert::base64_encode(&file.bytes));
            }
            value
        }
    })
}

fn run_with(s: &mut Session, v: &Value) -> CmdResult {
    let command = require(v, "command")?;
    let path = require(v, "path")?;
    let params = v.get("params").cloned().unwrap_or_else(|| json!({}));
    s.run_with_sidecar(&command, &params, &path, p::str(v, "as").unwrap_or("data"))
}

fn adopt(s: &mut Session, v: &Value) -> CmdResult {
    let (drive_id, path) = drive_and_path(v)?;
    let mut location = Location::new(&drive_id, &path);
    let name = location.name().to_string();
    if !craft_opencore::edms::location::valid_name(&name) {
        return Err(CmdError::Params("`path` must name a file".into()));
    }
    location.item_id = p::str(v, "id").map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    location.etag = p::str(v, "etag").map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    if let Some(letters) = p::str(v, "permissions") {
        location.permissions = craft_opencore::edms::Permissions::from_letters(letters);
    }
    let id = s.document_id();
    s.edms.adopt(id, location, name.clone());
    s.path = None;
    s.file_name = Some(name);
    Ok(json!({"adopted": true, "document": id}))
}

/// The document's bytes in the format its name says, with the save stamps applied (modified,
/// revision). Mirrors `file.save` without the file system.
fn document_bytes(s: &mut Session, name: &str) -> Result<Vec<u8>, CmdError> {
    if !craft_opencore::edms::location::valid_name(name) {
        return Err(CmdError::Params(format!("invalid file name: {name:?}")));
    }
    s.stamp_save();
    let password = s.password.as_ref().map(crate::io::Password::as_str);
    let opts = super::file::export_options(s, &Value::Null);
    let copy = super::insert::saved_copy(s);
    crate::io::save_bytes_with(name, copy.as_ref().unwrap_or(&s.doc), password, &opts).map_err(CmdError::Failed)
}

/// A copy's bytes: no stamps, the document is unchanged (export).
pub(crate) fn copy_bytes(s: &Session, name: &str) -> Result<Vec<u8>, CmdError> {
    if !craft_opencore::edms::location::valid_name(name) {
        return Err(CmdError::Params(format!("invalid file name: {name:?}")));
    }
    let opts = super::file::export_options(s, &Value::Null);
    let copy = super::insert::saved_copy(s);
    crate::io::save_bytes_opts(name, copy.as_ref().unwrap_or(&s.doc), &opts).map_err(CmdError::Failed)
}

/// What the collaboration still owes this document (SIGNAL.md, Nachlieferliste), for a message.
fn incomplete(s: &Session) -> Vec<Value> {
    s.signal.outstanding().iter().map(|o| json!({"command": o.command, "key": o.key, "size": o.size, "seq": o.seq})).collect()
}

fn save(s: &mut Session, v: &Value) -> CmdResult {
    // In a collaboration only the master writes: a participant asks it (SIGNAL.md, Speichern).
    if s.signal.collaborates() && s.edms.document(s.document_id()).is_some() {
        let local = s.signal_request_save()?;
        return Ok(json!({"requested": local}));
    }
    // In sync or not at all: with content still on its way, saving needs a second yes.
    let missing = incomplete(s);
    if !missing.is_empty() && !p::bool(v, "incomplete").unwrap_or(false) {
        s.push_ui_request(json!({"edms": {"event": "saveIncomplete", "missing": missing}}));
        return Err(CmdError::Disabled(format!(
            "{} of the collaboration is still on its way (only its preview is here): save with \"incomplete\": true to keep the preview",
            if missing.len() == 1 { "1 item".to_string() } else { format!("{} items", missing.len()) }
        )));
    }
    let id = s.document_id();
    let (location, name) = {
        let doc = s.edms.document(id).ok_or_else(|| CmdError::Disabled("not opened from the EDMS".into()))?;
        if !doc.tracker.can_save_in_place() {
            return Err(CmdError::Disabled("the file was renamed, moved or deleted on the server: use edms.saveAs".into()));
        }
        (doc.location.clone(), doc.name.clone())
    };
    let bytes = document_bytes(s, &name)?;
    let answer = pending(s, Request::Put { location, bytes }, Action::Save)?;
    // The master reports the version to the session once it is through (`SAVED`).
    s.signal.begin_save();
    Ok(answer)
}

fn target(v: &Value) -> Result<(Location, String, bool), CmdError> {
    let (drive_id, folder) = drive_and_path(v)?;
    let name = require(v, "name")?;
    if !craft_opencore::edms::location::valid_name(&name) {
        return Err(CmdError::Params(format!("invalid file name: {name:?}")));
    }
    let overwrite = p::bool(v, "overwrite").unwrap_or(false);
    Ok((Location::in_folder(&drive_id, &folder, &name), name, overwrite))
}

fn save_as(s: &mut Session, v: &Value) -> CmdResult {
    let (location, name, overwrite) = target(v)?;
    let switch = p::bool(v, "switch").unwrap_or(true);
    let bytes = if switch { document_bytes(s, &name)? } else { copy_bytes(s, &name)? };
    let mut answer = pending(s, Request::PutNew { location, bytes, overwrite }, Action::SaveAs { name, switch })?;
    // A copy out of a collaboration holds what is here now, and says what is not.
    let missing = incomplete(s);
    if !missing.is_empty() {
        s.push_ui_request(json!({"edms": {"event": "savedIncomplete", "missing": missing}}));
        answer["incomplete"] = json!(missing);
    }
    Ok(answer)
}

fn export(s: &mut Session, v: &Value) -> CmdResult {
    let (location, name, overwrite) = target(v)?;
    let bytes = copy_bytes(s, &name)?;
    pending(s, Request::PutNew { location, bytes, overwrite }, Action::Export)
}

fn current_location(s: &Session) -> Result<Location, CmdError> {
    s.edms.document(s.document_id()).map(|d| d.location.clone()).ok_or_else(|| CmdError::Disabled("not opened from the EDMS".into()))
}

fn reload(s: &mut Session, _v: &Value) -> CmdResult {
    let location = current_location(s)?;
    pending(s, Request::Get { drive_id: location.drive_id, path: location.path }, Action::Reload)
}

fn lock(s: &mut Session, v: &Value) -> CmdResult {
    let location = current_location(s)?;
    let timeout_secs = p::u64(v, "timeout").unwrap_or(DEFAULT_LOCK_SECS).clamp(1, 7 * 24 * 3600) as u32;
    let owner = s.author.clone();
    pending(s, Request::Lock { drive_id: location.drive_id, path: location.path, owner, timeout_secs }, Action::Lock)
}

fn unlock(s: &mut Session, _v: &Value) -> CmdResult {
    let location = current_location(s)?;
    let token = location.lock.and_then(|l| l.token).ok_or_else(|| CmdError::Disabled("no lock token for this document".into()))?;
    pending(s, Request::Unlock { drive_id: location.drive_id, path: location.path, token }, Action::Unlock)
}

fn event(s: &mut Session, v: &Value) -> CmdResult {
    let kind = require(v, "type")?;
    let data = match v.get("data") {
        Some(Value::String(text)) => text.clone(),
        Some(other) if !other.is_null() => other.to_string(),
        _ => return Err(CmdError::Params("`data` is the event's JSON".into())),
    };
    let remote = RemoteEvent::from_parts(&kind, &data).ok_or_else(|| CmdError::Params("`data` is not an event".into()))?;
    let own = s.edms.initiator_id().unwrap_or_default();
    let id = s.document_id();
    let Some(doc) = s.edms.document_mut(id) else {
        return Ok(json!({"concerns": false, "foreign": remote.foreign(&own), "advice": "none"}));
    };
    let concerns = doc.location.item_id.as_deref().is_some_and(|item| remote.concerns(item));
    let foreign = remote.foreign(&own);
    if !concerns || !foreign {
        return Ok(json!({"concerns": concerns, "foreign": foreign, "advice": "none"}));
    }
    // In a collaboration the master writes the new versions and announces them (`SAVED`); the
    // server's event about them is no foreign change here (the document is locked for anyone else).
    if s.signal.follows_master() && remote.kind.is_new_version() {
        return Ok(json!({"concerns": true, "foreign": true, "advice": "none", "collaboration": true}));
    }
    let advice = if remote.kind.is_new_version() {
        doc.tracker.remote_changed(remote.etag.as_deref())
    } else if remote.kind.is_gone() {
        doc.tracker.gone()
    } else {
        craft_opencore::edms::change::Advice::Nothing
    };
    let name = advice_name(advice);
    if advice != craft_opencore::edms::change::Advice::Nothing {
        s.push_ui_request(json!({"edms": {"advice": name, "reason": kind, "document": id}}));
    }
    Ok(json!({"concerns": true, "foreign": true, "advice": name, "kind": kind}))
}
