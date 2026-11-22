//! The EDMS side of a session (kosmos, `opencraft/EDMS_NATIVE.md` §9): where each open document
//! lives in OpenCore/OpenCloud, the change tracker per document, the requests in flight and
//! their results. The network itself is the shell's: it installs a [`craft_opencore::edms::Port`] (a
//! `Client` over its transport), the commands `edms.*` (`cmd::edms`) submit requests through it,
//! and [`Session::edms_pump`] applies what came back: a fetched document is opened, a saved one
//! gets its new ETag, a conflict asks the front end.
//!
//! Nothing here blocks. In the browser every answer arrives in a later frame; natively the
//! transport may answer at once. Agents see the same shape either way: a command returns
//! `{pending: id}`, `edms.poll` (or the front end's frame) delivers the outcome.

use std::collections::HashMap;

use craft_opencore::edms::change::{Advice, State, Tracker};
use craft_opencore::edms::{EdmsError, Location, Lookup, Outcome, Port, Reply, Request, RequestId, Sidecars, WorkDir};
use serde_json::{Value, json};

use crate::{CmdError, Session};

/// Most outcomes kept for `edms.poll` when nobody drains them (a headless session).
pub const MAX_RESULTS: usize = 64;
/// Most bytes a fetched file (`edms.fetch`) carries into a result, base64 and all.
pub const MAX_FETCH_BYTES: usize = 64 * 1024 * 1024;

/// What to do with an outcome beyond reporting it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Report only (drives, listings, search).
    Plain,
    /// The bytes open as a document named `name`.
    Open {
        name: String,
    },
    /// The active document was saved in place.
    Save,
    /// The active document was saved to a new place; with `switch` it lives there from now on.
    SaveAs {
        name: String,
        switch: bool,
    },
    /// A copy in another format was written; nothing changes for the document.
    Export,
    /// The server's version replaces the active document.
    Reload,
    /// The bytes are handed to the front end as they are (a picture to insert, a recipient
    /// list): nothing opens, the document doesn't change.
    Fetch {
        name: String,
    },
    Lock,
    Unlock,
    /// The stat after an open: the rights (and the id, if the GET did not name it) of `document`.
    Rights {
        document: u64,
    },
}

/// Most commands kept waiting for a file beside their document.
pub const MAX_DEFERRED: usize = 16;

/// A command waiting for a file beside its document (`edms.runWith`): it has not run yet, and
/// runs once, with the file's bytes in its parameters, when the fetch is over.
#[derive(Clone, Debug, PartialEq)]
struct Deferred {
    document: u64,
    command: String,
    params: Value,
    /// The path of the file, relative to the document.
    path: String,
    /// The parameter that takes the bytes, as base64.
    key: String,
}

/// Where a document lives and how it relates to the server's version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EdmsDocument {
    pub location: Location,
    pub tracker: Tracker,
    /// The file name the bytes are written as (`Bericht.docx`): it picks the format.
    pub name: String,
}

impl EdmsDocument {
    fn new(location: Location, name: String) -> Self {
        let mut tracker = Tracker::default();
        tracker.opened(location.etag.as_deref());
        Self { location, tracker, name }
    }

    /// The document as `edms.status` reports it.
    pub fn json(&self) -> Value {
        json!({
            "drive": self.location.drive_id,
            "path": self.location.path,
            "name": self.name,
            "id": self.location.item_id,
            "etag": self.location.etag,
            "writable": self.location.writable(),
            "locked": self.location.lock.is_some(),
            "state": state_name(self.tracker.state()),
        })
    }
}

/// The EDMS state of a session.
#[derive(Default)]
pub struct EdmsState {
    port: Option<Box<dyn Port>>,
    /// By document id (`Session::document_id`): a document opened from the EDMS, or saved there.
    documents: HashMap<u64, EdmsDocument>,
    actions: Vec<(RequestId, Action)>,
    results: Vec<Value>,
    /// The files beside each document (linked pictures, data sources), fetched on demand.
    sidecars: Sidecars,
    deferred: Vec<Deferred>,
}

impl EdmsState {
    /// The shell installs its client once; the commands refuse until then.
    pub fn install(&mut self, port: Box<dyn Port>) {
        self.port = Some(port);
    }

    pub fn has_port(&self) -> bool {
        self.port.is_some()
    }

    pub fn connected(&self) -> bool {
        self.port.as_ref().is_some_and(|p| p.connected())
    }

    pub fn origin(&self) -> Option<String> {
        self.port.as_ref().and_then(|p| p.config()).map(|c| c.origin.clone())
    }

    pub fn in_flight(&self) -> usize {
        self.port.as_ref().map_or(0, |p| p.in_flight())
    }

    pub fn port_mut(&mut self) -> Result<&mut (dyn Port + '_), CmdError> {
        match self.port.as_deref_mut() {
            Some(port) => Ok(port),
            None => Err(CmdError::Disabled("this build has no EDMS transport".into())),
        }
    }

    /// The `Authorization` value of the connection, for the front end's event stream and for the
    /// ticket of a document's collaboration (the same instance, the same sign-in).
    pub fn authorization(&self) -> Option<String> {
        self.port.as_ref().and_then(|p| p.config()).map(|c| c.authorization())
    }

    /// This client's id for the server's events (`Initiator-ID`), once connected.
    pub fn initiator_id(&self) -> Option<String> {
        self.port.as_ref().and_then(|p| p.config()).map(|c| c.initiator_id.clone())
    }

    /// Give a session document (one the host page loaded) its EDMS location.
    pub fn adopt(&mut self, id: u64, location: Location, name: String) {
        self.documents.insert(id, EdmsDocument::new(location, name));
    }

    /// The EDMS document for a session document, if it has one.
    pub fn document(&self, id: u64) -> Option<&EdmsDocument> {
        self.documents.get(&id)
    }

    pub fn document_mut(&mut self, id: u64) -> Option<&mut EdmsDocument> {
        self.documents.get_mut(&id)
    }

    /// The working directory of session document `id`: the folder it lives in. A document that
    /// was never saved to the EDMS has none.
    pub fn work_dir(&self, id: u64) -> Result<WorkDir, EdmsError> {
        WorkDir::of(self.documents.get(&id).map(|d| &d.location))
    }

    /// The file `relative` beside session document `id`: there, on its way, or failed. The
    /// first call starts the fetch; the front end hears `{"edms": {"event": "sidecar"}}` when
    /// something arrived for the document and asks again. Never blocks.
    pub fn sidecar(&mut self, id: u64, relative: &str) -> Result<Lookup<'_>, EdmsError> {
        let port = self.port.as_deref_mut().ok_or(EdmsError::NotConnected)?;
        self.sidecars.lookup(port, id, self.documents.get(&id).map(|d| &d.location), relative)
    }

    /// Forget a sidecar so the next [`sidecar`](Self::sidecar) fetches it again.
    pub fn refresh_sidecar(&mut self, id: u64, relative: &str) -> Result<(), EdmsError> {
        self.sidecars.refresh(id, self.documents.get(&id).map(|d| &d.location), relative)
    }

    /// The content of document `from` was replaced and it is `to` now (a collaborator took the
    /// master's state): where it lives moves along, what was fetched beside the old content goes.
    pub(crate) fn follow_replaced(&mut self, from: u64, to: u64) {
        if let Some(lived_at) = self.documents.remove(&from) {
            self.documents.insert(to, lived_at);
        }
        self.forget_sidecars(from);
    }

    /// A session document was closed or replaced: its sidecars go.
    pub fn forget_sidecars(&mut self, id: u64) {
        self.sidecars.forget_document(id);
        self.deferred.retain(|waiting| waiting.document != id);
    }

    /// Commands waiting for a file beside their document.
    pub fn deferred(&self) -> usize {
        self.deferred.len()
    }

    fn defer(&mut self, waiting: Deferred) {
        if self.deferred.len() >= MAX_DEFERRED {
            self.deferred.remove(0);
        }
        self.deferred.push(waiting);
    }

    pub fn sidecar_bytes(&self) -> usize {
        self.sidecars.held_bytes()
    }

    /// Submit a request and remember what to do with its answer.
    pub(crate) fn submit(&mut self, request: Request, action: Action) -> Result<RequestId, CmdError> {
        let port = self.port_mut()?;
        let id = port.submit(request).map_err(|e| match e {
            EdmsError::NotConnected => CmdError::Disabled(e.to_string()),
            EdmsError::Params(p) => CmdError::Params(p),
            other => CmdError::Failed(other.to_string()),
        })?;
        self.actions.push((id, action));
        Ok(id)
    }

    /// The outcomes since the last call, oldest first.
    pub fn take_results(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.results)
    }

    /// The outcome of one request, if it has arrived: the front end's own requests (the location
    /// picker) take theirs here and leave the rest for `edms.poll`.
    pub fn take_result(&mut self, id: RequestId) -> Option<Value> {
        let at = self.results.iter().position(|r| r.get("id").and_then(Value::as_u64) == Some(id))?;
        Some(self.results.remove(at))
    }

    fn push_result(&mut self, result: Value) {
        if self.results.len() >= MAX_RESULTS {
            self.results.remove(0);
        }
        self.results.push(result);
    }

    fn take_action(&mut self, id: RequestId) -> Action {
        match self.actions.iter().position(|(pending, _)| *pending == id) {
            Some(position) => self.actions.swap_remove(position).1,
            None => Action::Plain,
        }
    }
}

fn state_name(state: Option<State>) -> &'static str {
    match state {
        None => "unknown",
        Some(State::Clean) => "clean",
        Some(State::LocalChanged) => "localChanged",
        Some(State::RemoteChanged) => "remoteChanged",
        Some(State::Conflict) => "conflict",
        Some(State::Gone) => "gone",
    }
}

/// What the front end should offer, as `edms.event` and the outcomes report it.
pub fn advice_name(advice: Advice) -> &'static str {
    match advice {
        Advice::Nothing => "none",
        Advice::OfferReload => "reload",
        Advice::OfferConflict => "conflict",
        Advice::OfferSaveAs => "saveAs",
    }
}

/// An error as the outcomes carry it.
pub fn error_json(error: &EdmsError) -> Value {
    let code = match error {
        EdmsError::NotConnected => "notConnected",
        EdmsError::Params(_) => "params",
        EdmsError::Unauthorized => "unauthorized",
        EdmsError::Forbidden => "forbidden",
        EdmsError::NotFound => "notFound",
        EdmsError::Conflict => "conflict",
        EdmsError::Locked => "locked",
        EdmsError::Server { .. } => "server",
        EdmsError::Transport(_) => "transport",
        EdmsError::Malformed(_) => "malformed",
    };
    json!({"code": code, "message": error.to_string()})
}

/// A listing entry as the outcomes carry it.
pub fn entry_json(entry: &craft_opencore::edms::Entry) -> Value {
    json!({
        "name": entry.name,
        "id": entry.id,
        "drive": entry.drive_id,
        "path": entry.path_in_drive(),
        "folder": entry.folder,
        "size": entry.size,
        "etag": entry.etag,
        "modified": entry.modified,
        "contentType": entry.content_type,
        "permissions": entry.permissions.letters(),
        "writable": entry.permissions.can_write(),
        "canCreateFile": entry.permissions.can_create_file(),
        "locked": entry.lock.is_some(),
    })
}

fn drive_json(drive: &craft_opencore::edms::Drive) -> Value {
    json!({"id": drive.id, "name": drive.name, "type": drive.drive_type, "alias": drive.alias, "davUrl": drive.dav_url})
}

impl Session {
    /// Advance the EDMS side: note local edits in the trackers, read finished requests and
    /// apply them. The front end calls this every frame, `edms.poll` on demand.
    pub fn edms_pump(&mut self) {
        let id = self.document_id();
        if self.dirty
            && let Some(doc) = self.edms.document_mut(id)
        {
            doc.tracker.edited();
        }
        let outcomes = match self.edms.port.as_mut() {
            Some(port) => port.poll(),
            None => return,
        };
        for outcome in outcomes {
            // A sidecar's answer fills the cache and says so; everything else is a command's.
            match self.edms.sidecars.absorb(outcome) {
                Ok(document) => self.push_ui_request(json!({"edms": {"event": "sidecar", "document": document}})),
                Err(outcome) => self.apply_edms_outcome(outcome),
            }
        }
        if !self.edms.deferred.is_empty() {
            self.run_deferred();
        }
    }

    /// Run `command` once with the file `path` beside the active document in its parameter
    /// `key` (base64, as a front end hands over a file the user picked). The file is fetched
    /// first; the command runs when it is there, so it runs once and carries the bytes itself.
    /// Nothing runs if the fetch fails.
    pub(crate) fn run_with_sidecar(&mut self, command: &str, params: &Value, path: &str, key: &str) -> Result<Value, CmdError> {
        if command == "edms.runWith" || self.registry.get(command).is_none() {
            return Err(CmdError::Unknown(command.to_string()));
        }
        if !(params.is_object() || params.is_null()) {
            return Err(CmdError::Params("`params` must be an object".into()));
        }
        let document = self.document_id();
        let waiting = Deferred { document, command: command.to_string(), params: params.clone(), path: path.to_string(), key: key.to_string() };
        match self.sidecar_parameters(&waiting)? {
            Some(ready) => Ok(json!({"ran": true, "result": self.run(command, &ready)?})),
            None => {
                self.edms.defer(waiting);
                Ok(json!({"pending": "sidecar", "path": path, "document": document}))
            }
        }
    }

    /// The parameters of a waiting command with its file in them; `None` while the file is on
    /// its way (the first call starts the fetch).
    fn sidecar_parameters(&mut self, waiting: &Deferred) -> Result<Option<Value>, CmdError> {
        let lookup = self.edms.sidecar(waiting.document, &waiting.path).map_err(|error| match error {
            EdmsError::NotConnected => CmdError::Disabled(error.to_string()),
            EdmsError::Params(p) => CmdError::Params(p),
            other => CmdError::Failed(other.to_string()),
        })?;
        match lookup {
            Lookup::Pending => Ok(None),
            Lookup::Ready(file) if file.bytes.len() > MAX_FETCH_BYTES => {
                Err(CmdError::Failed(format!("{} is too big to hand over ({} bytes)", waiting.path, file.bytes.len())))
            }
            Lookup::Ready(file) => {
                let mut params = waiting.params.clone();
                params[waiting.key.as_str()] = json!(crate::cmd::insert::base64_encode(&file.bytes));
                Ok(Some(params))
            }
            Lookup::Failed(error) => {
                let message = format!("{}: {error}", waiting.path);
                // The failure is reported once; the next try fetches again.
                let _ = self.edms.refresh_sidecar(waiting.document, &waiting.path);
                Err(CmdError::Failed(message))
            }
        }
    }

    /// Run the commands of the active document whose file has arrived (or failed). A command of
    /// a document in the background waits until that document is active again.
    fn run_deferred(&mut self) {
        let active = self.document_id();
        let mut waiting = std::mem::take(&mut self.edms.deferred);
        let mut due = Vec::new();
        waiting.retain(|deferred| {
            let place = self.edms.documents.get(&deferred.document).map(|d| &d.location);
            let pending = matches!(self.edms.sidecars.peek(deferred.document, place, &deferred.path), Ok(Some(Lookup::Pending)));
            let run_now = deferred.document == active && !pending;
            if run_now {
                due.push(deferred.clone());
            }
            !run_now
        });
        self.edms.deferred = waiting;
        for deferred in due {
            let result = match self.sidecar_parameters(&deferred) {
                // Dropped from the cache in between: fetched again, the command keeps waiting.
                Ok(None) => {
                    self.edms.defer(deferred);
                    continue;
                }
                Ok(Some(params)) => self.run(&deferred.command, &params),
                Err(error) => Err(error),
            };
            let report = match result {
                Ok(_) => json!({"kind": "deferred", "ok": true, "command": deferred.command, "path": deferred.path, "document": deferred.document}),
                Err(error) => {
                    self.push_ui_request(json!({"edms": {"event": "sidecarFailed", "command": deferred.command, "path": deferred.path, "document": deferred.document, "message": error.to_string()}}));
                    json!({"kind": "deferred", "ok": false, "command": deferred.command, "path": deferred.path, "document": deferred.document, "error": {"code": "apply", "message": error.to_string()}})
                }
            };
            self.edms.push_result(report);
        }
    }

    fn apply_edms_outcome(&mut self, outcome: Outcome) {
        let action = self.edms.take_action(outcome.id);
        let kind = outcome.request.kind();
        let result = match outcome.result {
            Err(error) => self.edms_failed(outcome.id, kind, &action, &error),
            Ok(reply) => match self.edms_apply(&action, &outcome.request, reply) {
                Ok(mut value) => {
                    value["id"] = json!(outcome.id);
                    value["kind"] = json!(action_name(&action, kind));
                    value["ok"] = json!(true);
                    value
                }
                Err(error) => {
                    // The bytes came but are no document (a PDF named .docx): as much a failed
                    // open as a file that isn't there.
                    self.report_failed_open(&action, &error.to_string());
                    json!({"id": outcome.id, "kind": action_name(&action, kind), "ok": false, "error": {"code": "apply", "message": error.to_string()}})
                }
            },
        };
        self.edms.push_result(result);
    }

    fn edms_failed(&mut self, id: RequestId, kind: &str, action: &Action, error: &EdmsError) -> Value {
        let mut value = json!({"id": id, "kind": action_name(action, kind), "ok": false, "error": error_json(error)});
        // kosmos: a save the master ran for a participant failed: that participant hears why.
        if matches!(action, Action::Save) {
            self.signal.save_failed(&error.to_string());
        }
        // A rejected save says the server has a newer version: the tracker decides what to offer.
        if *error == EdmsError::Conflict
            && matches!(action, Action::Save)
            && let Some(doc) = self.edms.document_mut(self.document_id())
        {
            let advice = doc.tracker.save_rejected();
            value["advice"] = json!(advice_name(advice));
            self.push_ui_request(json!({"edms": {"advice": advice_name(advice), "reason": "saveRejected"}}));
        }
        // An open nobody watches (the host named the place and left) must not fail silently.
        self.report_failed_open(action, &error.to_string());
        value
    }

    /// Tell the front end that an open or a reload brought no document, with the file's name.
    fn report_failed_open(&mut self, action: &Action, message: &str) {
        let name = match action {
            Action::Open { name } => name.clone(),
            Action::Reload => self.edms.document(self.document_id()).map(|d| d.name.clone()).unwrap_or_default(),
            _ => return,
        };
        self.push_ui_request(json!({"edms": {"event": "openFailed", "name": name, "message": message}}));
    }

    fn edms_apply(&mut self, action: &Action, request: &Request, reply: Reply) -> Result<Value, CmdError> {
        match (action, reply) {
            (Action::Plain, Reply::Drives(drives)) => Ok(json!({"drives": drives.iter().map(drive_json).collect::<Vec<_>>()})),
            (Action::Plain, Reply::Me(me)) => {
                Ok(json!({"me": {"id": me.id, "displayName": me.display_name, "mail": me.mail, "preferredLanguage": me.preferred_language}}))
            }
            (Action::Plain, Reply::Entries(entries)) => Ok(json!({"entries": entries.iter().map(entry_json).collect::<Vec<_>>()})),
            (Action::Plain, Reply::Entry(entry)) => Ok(json!({"entry": entry_json(&entry)})),
            (Action::Open { name }, Reply::Bytes { bytes, etag, file_id, .. }) => {
                let opened = self.edms_open_bytes(request, name, &bytes, etag, file_id, false)?;
                // The GET carries no rights: a stat follows, so the document is writable without
                // anyone handing the rights in afterwards.
                if let Request::Get { drive_id, path } = request {
                    let document = self.document_id();
                    if let Err(error) =
                        self.edms.submit(Request::Stat { drive_id: drive_id.clone(), path: path.clone() }, Action::Rights { document })
                    {
                        log::warn!("edms.open: no stat for the rights: {error:?}");
                    }
                }
                Ok(opened)
            }
            (Action::Rights { document }, Reply::Entry(entry)) => {
                let Some(doc) = self.edms.document_mut(*document) else {
                    return Ok(json!({"rights": false, "document": document}));
                };
                doc.location.permissions = entry.permissions.clone();
                if doc.location.item_id.is_none() {
                    doc.location.item_id = entry.id.clone();
                }
                Ok(
                    json!({"rights": true, "document": document, "writable": doc.location.writable(), "permissions": doc.location.permissions.letters()}),
                )
            }
            (Action::Reload, Reply::Bytes { bytes, etag, file_id, .. }) => {
                let name = self.edms.document(self.document_id()).map(|d| d.name.clone()).unwrap_or_else(|| "document".into());
                self.edms_open_bytes(request, &name, &bytes, etag, file_id, true)
            }
            (Action::Fetch { name }, Reply::Bytes { bytes, etag, content_type, .. }) => {
                if bytes.len() > MAX_FETCH_BYTES {
                    return Err(CmdError::Failed(format!("{name} is too big to fetch ({} bytes)", bytes.len())));
                }
                Ok(
                    json!({"fetched": true, "name": name, "size": bytes.len(), "etag": etag, "contentType": content_type, "data": crate::cmd::insert::base64_encode(&bytes)}),
                )
            }
            (Action::Save, Reply::Saved { etag, file_id }) => self.edms_saved(etag, file_id),
            (Action::SaveAs { name, switch }, Reply::Saved { etag, file_id }) => self.edms_saved_as(request, name, *switch, etag, file_id),
            (Action::Export, Reply::Saved { etag, file_id }) => Ok(json!({"etag": etag, "fileId": file_id})),
            (Action::Lock, Reply::Locked(info)) => {
                let id = self.document_id();
                if let Some(doc) = self.edms.document_mut(id) {
                    doc.location.lock = Some(info.clone());
                }
                Ok(json!({"lock": {"token": info.token, "owner": info.owner, "timeout": info.timeout}}))
            }
            (Action::Unlock, Reply::Unlocked) => {
                let id = self.document_id();
                if let Some(doc) = self.edms.document_mut(id) {
                    doc.location.lock = None;
                }
                Ok(json!({"unlocked": true}))
            }
            (action, reply) => Err(CmdError::Failed(format!("unexpected {} reply for {}", reply_name(&reply), action_name(action, request.kind())))),
        }
    }

    fn edms_open_bytes(
        &mut self,
        request: &Request,
        name: &str,
        bytes: &[u8],
        etag: Option<String>,
        file_id: Option<String>,
        reload: bool,
    ) -> Result<Value, CmdError> {
        let Request::Get { drive_id, path } = request else {
            return Err(CmdError::Failed("bytes without a get".into()));
        };
        let (doc, _encrypted) = crate::io::open_bytes_in(name, bytes, None, &self.region()).map_err(CmdError::Failed)?;
        // Replacing the active document gives it a new id: its EDMS entry moves along (a reload
        // keeps id, rights and lock the server did not repeat), a replaced blank one is dropped.
        let old_id = self.document_id();
        let in_place = reload || self.used_because().is_none();
        let previous = if in_place { self.edms.documents.remove(&old_id) } else { None };
        if in_place {
            self.edms.forget_sidecars(old_id);
        }
        if in_place {
            self.set_document(doc);
        } else {
            log::info!("edms.open: opened beside document {old_id}, not in its place");
            self.add_opened_document(doc);
        }
        if let Some(z) = self.doc.settings.zoom {
            crate::cmd::view::restore_zoom(self, z);
        }
        self.path = None;
        self.file_name = Some(name.to_string());
        self.dirty = false;
        let mut location = Location::new(drive_id, path);
        location.item_id = file_id;
        location.etag = etag.clone();
        // Rights and lock come with the next stat; until then the server decides on the PUT.
        let id = self.document_id();
        if reload && let Some(previous) = previous {
            if location.item_id.is_none() {
                location.item_id = previous.location.item_id;
            }
            location.permissions = previous.location.permissions;
            location.lock = previous.location.lock;
        }
        self.edms.documents.insert(id, EdmsDocument::new(location, name.to_string()));
        let what = if reload { "reloaded" } else { "opened" };
        self.push_ui_request(json!({"edms": {"event": what, "document": id}}));
        Ok(json!({what: true, "name": name, "etag": etag, "document": id, "paragraphs": self.doc.paragraph_count(), "words": self.doc.word_count()}))
    }

    fn edms_saved(&mut self, etag: Option<String>, file_id: Option<String>) -> Result<Value, CmdError> {
        let id = self.document_id();
        let doc = self.edms.document_mut(id).ok_or_else(|| CmdError::Failed("saved a document that is no longer open".into()))?;
        if etag.is_some() {
            doc.location.etag = etag.clone();
        }
        if file_id.is_some() {
            doc.location.item_id = file_id.clone();
        }
        doc.tracker.saved(doc.location.etag.as_deref());
        let held = doc.location.etag.clone();
        self.dirty = false;
        self.relayout();
        self.push_ui_request(json!({"edms": {"event": "saved", "document": id}}));
        // kosmos: as the master of a collaboration, tell the others which version holds the state.
        self.signal.save_finished(held.as_deref());
        Ok(json!({"saved": true, "etag": etag, "fileId": file_id}))
    }

    fn edms_saved_as(
        &mut self,
        request: &Request,
        name: &str,
        switch: bool,
        etag: Option<String>,
        file_id: Option<String>,
    ) -> Result<Value, CmdError> {
        let Request::PutNew { location, .. } = request else {
            return Err(CmdError::Failed("saved without a putNew".into()));
        };
        let mut location = location.clone();
        location.etag = etag.clone();
        location.item_id = file_id.clone();
        let id = self.document_id();
        if switch {
            self.edms.documents.insert(id, EdmsDocument::new(location.clone(), name.to_string()));
            self.path = None;
            self.file_name = Some(name.to_string());
            self.dirty = false;
            self.relayout();
            self.push_ui_request(json!({"edms": {"event": "relocated", "document": id, "drive": location.drive_id, "path": location.path}}));
        }
        Ok(
            json!({"saved": true, "switched": switch, "drive": location.drive_id, "path": location.path, "name": name, "etag": etag, "fileId": file_id}),
        )
    }
}

fn action_name(action: &Action, kind: &str) -> String {
    match action {
        Action::Plain => kind.to_string(),
        Action::Open { .. } => "open".into(),
        Action::Save => "save".into(),
        Action::SaveAs { .. } => "saveAs".into(),
        Action::Export => "export".into(),
        Action::Reload => "reload".into(),
        Action::Fetch { .. } => "fetch".into(),
        Action::Lock => "lock".into(),
        Action::Unlock => "unlock".into(),
        Action::Rights { .. } => "rights".into(),
    }
}

fn reply_name(reply: &Reply) -> &'static str {
    match reply {
        Reply::Me(_) => "me",
        Reply::Drives(_) => "drives",
        Reply::Entries(_) => "entries",
        Reply::Entry(_) => "entry",
        Reply::Bytes { .. } => "bytes",
        Reply::Saved { .. } => "saved",
        Reply::Locked(_) => "locked",
        Reply::Unlocked => "unlocked",
    }
}
