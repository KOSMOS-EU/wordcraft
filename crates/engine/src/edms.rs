//! kosmos: WordCraft's adapter to the OpenCore kit (`craft-opencore`, area `session`; see
//! `opencraft/PLAN_EDMS_APPS_20261122.md` §4). The kit keeps where each document lives in OpenCore/OpenCloud,
//! the requests in flight and their results, and runs the commands `edms.*`; this file only tells it about
//! WordCraft's documents. The network is the shell's: it installs a `craft_opencore::edms::Port` with
//! `session.edms.install`.

use craft_opencore::session::{CommandError, DocKey, DocumentHost, EdmsSession};
use serde_json::{Value, json};

use crate::{CmdError, Session};

/// A kit error as a WordCraft command error (the same cases, the same texts).
pub(crate) fn to_cmd(error: CommandError) -> CmdError {
    match error {
        CommandError::Unknown(command) => CmdError::Unknown(command),
        CommandError::Params(text) => CmdError::Params(text),
        CommandError::Disabled(text) => CmdError::Disabled(text),
        CommandError::Failed(text) => CmdError::Failed(text),
    }
}

impl Session {
    /// A document from the EDMS (opened, or begun in an empty file) in the active one's place or beside it; it is
    /// called `name`, has no path on this computer and counts as saved.
    fn show_from_edms(&mut self, doc: wordcraft_doc::Document, name: &str, replace_active: bool) -> Result<Value, CommandError> {
        if replace_active {
            self.set_document(doc);
        } else {
            log::info!("edms.open: opened beside document {}, not in its place", self.document_id());
            self.add_opened_document(doc);
        }
        if let Some(zoom) = self.doc.settings.zoom {
            crate::cmd::view::restore_zoom(self, zoom);
        }
        self.path = None;
        self.file_name = Some(name.to_string());
        self.dirty = false;
        Ok(json!({"paragraphs": self.doc.paragraph_count(), "words": self.doc.word_count()}))
    }
}

fn from_cmd(error: CmdError) -> CommandError {
    match error {
        CmdError::Unknown(command) => CommandError::Unknown(command),
        CmdError::Params(text) => CommandError::Params(text),
        CmdError::Disabled(text) => CommandError::Disabled(text),
        CmdError::Failed(text) => CommandError::Failed(text),
    }
}

impl DocumentHost for Session {
    fn edms(&self) -> &EdmsSession {
        &self.edms
    }

    fn edms_mut(&mut self) -> &mut EdmsSession {
        &mut self.edms
    }

    fn active_document(&self) -> DocKey {
        self.document_id()
    }

    fn active_dirty(&self) -> bool {
        self.dirty
    }

    fn may_replace_active(&self) -> bool {
        self.used_because().is_none()
    }

    fn open_document(&mut self, name: &str, bytes: &[u8], replace_active: bool) -> Result<Value, CommandError> {
        let (doc, _encrypted) = crate::io::open_bytes_in(name, bytes, None, &self.region()).map_err(CommandError::Failed)?;
        self.show_from_edms(doc, name, replace_active)
    }

    /// kosmos: an empty file (OpenCloud's New menu makes one) becomes a new blank document on the Normal template,
    /// as File › New makes it, in that file's place.
    fn new_document(&mut self, name: &str, replace_active: bool) -> Result<Value, CommandError> {
        let doc = crate::cmd::file::normal_document(self);
        self.show_from_edms(doc, name, replace_active).map(|mut summary| {
            summary["new"] = json!(true);
            summary
        })
    }

    fn document_bytes(&mut self, name: &str) -> Result<Vec<u8>, CommandError> {
        crate::cmd::edms::document_bytes(self, name).map_err(from_cmd)
    }

    fn copy_bytes(&self, name: &str) -> Result<Vec<u8>, CommandError> {
        crate::cmd::edms::copy_bytes(self, name).map_err(from_cmd)
    }

    fn take_name(&mut self, name: &str) {
        self.path = None;
        self.file_name = Some(name.to_string());
    }

    fn mark_clean(&mut self) {
        self.dirty = false;
        self.relayout();
    }

    fn notify(&mut self, event: Value) {
        self.push_ui_request(event);
    }

    fn author_name(&self) -> String {
        self.author.clone()
    }

    fn client_name(&self) -> &str {
        "wordcraft"
    }

    fn has_command(&self, command: &str) -> bool {
        self.registry.get(command).is_some()
    }

    fn run_command(&mut self, command: &str, params: &Value) -> Result<Value, CommandError> {
        self.run(command, params).map_err(from_cmd)
    }

    /// As the master of a collaboration, tell the others which version holds the state.
    fn save_finished(&mut self, etag: Option<&str>) {
        self.signal.save_finished(etag);
    }

    /// A save the master ran for a participant failed: that participant hears why.
    fn save_failed(&mut self, message: &str) {
        self.signal.save_failed(message);
    }

    /// In a collaboration the master writes the new versions and announces them (`SAVED`).
    fn versions_announced(&self) -> bool {
        self.signal.follows_master()
    }

    /// kosmos: the clock for AutoSave to the EDMS, the same as the collaboration's.
    fn clock_ms(&self) -> Option<u64> {
        let now = crate::signal::now_ms();
        (now > 0).then_some(now)
    }

    /// kosmos: every change advances `rev` (`touch`): AutoSave waits for a pause in the typing, and a save keeps
    /// the changes made while it uploads unsaved.
    fn revision(&self) -> Option<u64> {
        Some(self.rev())
    }

    /// kosmos: AutoSave only where this session writes the file: not while it joins, decides, syncs or only reads,
    /// and in a collaboration only as its master (the others' saves are the master's).
    fn may_autosave(&self) -> bool {
        !self.signal.follows_master() && craft_opencore::session::collab::lock_state(self).is_none()
    }

    /// kosmos: the fonts the document's laid-out text asks for that aren't installed (`font.missing`), from the last
    /// layout: the kit fetches them, or their metric twins, from the instance.
    fn wanted_fonts(&self) -> Vec<String> {
        let Some(layout) = self.last_layout() else { return Vec::new() };
        crate::cmd::font::missing_fonts(&self.doc, &layout).into_iter().map(|missing| missing.font).collect()
    }

    /// kosmos: a font file from the instance, for this session only, as a document's embedded fonts are (never stored
    /// as the user's font, the instance has it): the text shown in a fallback until now takes it.
    fn install_font(&mut self, family: &str, bytes: &[u8]) -> Result<(), CommandError> {
        wordcraft_fonts::FontDb::global().add_document_fonts(vec![(bytes.to_vec(), family.to_string())]);
        self.relayout();
        Ok(())
    }
}

impl Session {
    /// Advance the EDMS side: note local edits, read finished requests and apply them. The front end
    /// calls this every frame, `edms.poll` on demand.
    pub fn edms_pump(&mut self) {
        craft_opencore::session::pump(self);
    }
}
