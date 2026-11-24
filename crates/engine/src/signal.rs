//! Craft Signal in the session (kosmos, `opencraft/SIGNAL.md`): WordCraft's adapter to the collaboration of
//! the OpenCore kit (`craft-opencore`, `session::collab`). The kit keeps the journal, the conflict rule, the
//! sync with the master and saving through it; this file tells it what WordCraft's documents are: a snapshot
//! is an [`EditSnapshot`], an anchor is a selection (the paragraphs from its first to its last), the state
//! travels as a `.docx` copy, and a picture that is still on its way stands in the document as a frame.
//!
//! Still the master's: the clock and author of a commit (determinism of the engine is the next step). Undo
//! stays local.

use std::sync::Arc;

use craft_opencore::session::collab::pictures::{self, PictureHost};
use craft_opencore::session::collab::{self, CollabHost, SignalSession};
use craft_opencore::signal::{Attachment, ConnectRequest, Hash};
use serde_json::Value;
use wordcraft_doc::{Document, Path, StoryRef};

use crate::edms::to_cmd;
use crate::snapshot::EditSnapshot;
use crate::{CmdError, Selection, Session};

/// This engine's version, the session type at the relay (same engine, same commands).
pub const APP_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "-kosmos");

/// The state a master answers a sync request with: the document as a `.docx` copy.
const SYNC_STATE_NAME: &str = "state.docx";

/// The collaboration state of a session.
pub type SignalState = SignalSession<Box<EditSnapshot>>;

/// The paragraphs a command's anchor names: from the first to the last of its selection, in one
/// story. `None` when the anchor is missing or spans stories.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Touched {
    story: StoryRef,
    from: Path,
    to: Path,
}

fn touched(anchor: &Value) -> Option<Touched> {
    let selection: Selection = serde_json::from_value(anchor.clone()).ok()?;
    let (first, last) = selection.ordered();
    (first.story == last.story).then_some(Touched { story: first.story, from: first.path, to: last.path })
}

/// A change of a picture's pixels (Picture Format), kept to be made again on the picture itself.
pub(crate) type PictureEdit = Box<dyn Fn(image::DynamicImage) -> Result<image::DynamicImage, CmdError>>;

/// The frames in the document whose pictures are still on their way, and what Picture Format made of them
/// (the kit's book of frames, `session::collab::frames`).
pub type PictureFrames = collab::PictureFrames<PictureEdit>;

/// Commands that stay local beyond the kit's (files, the modules, tools, view, undo): the ones that
/// replace the document or make other files.
fn transmits(id: &str) -> bool {
    collab::transmits_by_default(id)
        && !matches!(id, "review.compare" | "review.combine" | "mailings.finish" | "mailings.envelopes" | "mailings.labels")
}

/// Milliseconds since the epoch, UTC; 0 when the clock can't say.
pub fn now_ms() -> u64 {
    #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
    {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis().min(u128::from(u64::MAX)) as u64).unwrap_or(0)
    }
    #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
    {
        let ms = js_sys::Date::now();
        if ms.is_finite() && ms > 0.0 { ms as u64 } else { 0 }
    }
}

/// The paragraph count of the story an anchor names; `None` without one.
fn paragraphs(doc: &Document, anchor: &Value) -> Option<usize> {
    touched(anchor).map(|t| doc.para_paths(t.story).len())
}

impl CollabHost for Session {
    type Snapshot = Box<EditSnapshot>;
    type Selection = Selection;

    fn signal(&self) -> &SignalState {
        &self.signal
    }

    fn signal_mut(&mut self) -> &mut SignalState {
        &mut self.signal
    }

    fn app_version(&self) -> &str {
        APP_VERSION
    }

    fn now_ms(&self) -> u64 {
        now_ms()
    }

    fn transmits(&self, command: &str) -> bool {
        transmits(command)
    }

    fn snapshot(&self) -> Box<EditSnapshot> {
        Box::new(self.edit_snapshot())
    }

    fn restore(&mut self, snapshot: Box<EditSnapshot>) {
        Session::restore(self, *snapshot);
    }

    fn selection(&self) -> Selection {
        self.sel.clone()
    }

    fn set_selection(&mut self, selection: Selection) {
        self.sel = selection;
        self.clamp_selection();
    }

    fn run_at(&mut self, anchor: &Value, command: &str, params: &Value) -> Result<(), String> {
        let mine = self.sel.clone();
        if let Ok(theirs) = serde_json::from_value::<Selection>(anchor.clone()) {
            self.sel = theirs;
            self.clamp_selection();
        }
        let result = self.run(command, params);
        self.sel = mine;
        self.clamp_selection();
        result.map(|_| ()).map_err(|e| e.to_string())
    }

    /// The conflict rule: a commit by someone else touched one of the paragraphs of the anchor, or moved them
    /// (changed the paragraph count of their story before them).
    fn conflicts(&self, mine: &Value, theirs: &Value, structural: bool) -> bool {
        match (touched(mine), touched(theirs)) {
            (Some(mine), Some(theirs)) => theirs.story == mine.story && theirs.from <= mine.to && (mine.from <= theirs.to || structural),
            _ => true,
        }
    }

    fn extent(&self, anchor: &Value) -> Option<usize> {
        paragraphs(&self.doc, anchor)
    }

    fn state_name(&self) -> &str {
        SYNC_STATE_NAME
    }

    /// Put the master's document in the place of the one open here. Where it lives (its EDMS
    /// entry, its name) stays: only the content is the session's.
    fn adopt_state(&mut self, bytes: &[u8]) -> Result<(), String> {
        let (doc, _encrypted) = crate::io::open_bytes_in(SYNC_STATE_NAME, bytes, None, &self.region())?;
        let old_id = self.document_id();
        let (file_name, path, password) = (self.file_name.clone(), self.path.clone(), self.password.clone());
        self.set_document(doc);
        self.file_name = file_name;
        self.path = path;
        self.password = password;
        let new_id = self.document_id();
        self.edms.follow_replaced(old_id, new_id);
        if let Some(zoom) = self.doc.settings.zoom {
            crate::cmd::view::restore_zoom(self, zoom);
        }
        Ok(())
    }

    /// A bigger picture goes with its pixel size and format, so the others show a frame until it is there (the kit's
    /// frames, `session::collab::pictures`).
    fn describe_large(&self, command: &str, key: &str, text: &str) -> Option<(String, (u32, u32))> {
        pictures::describe_large(self, command, key, text)
    }

    fn held(&self, attachment: &Attachment) -> Option<String> {
        pictures::held(self, attachment)
    }

    fn stand_in(&self, command: &str, attachment: &Attachment) -> Result<(String, Vec<u8>), String> {
        pictures::stand_in(self, command, attachment)
    }

    fn stand_ins_placed(&mut self, placed: Vec<(Hash, Vec<u8>)>) {
        pictures::stand_ins_placed(self, placed);
    }

    fn content_arrived(&mut self, hash: &Hash, content: Option<&[u8]>, snapshots: &mut [&mut Box<EditSnapshot>]) -> usize {
        pictures::content_arrived(self, hash, content, snapshots)
    }

    fn extend_status(&self, status: &mut Value) {
        pictures::extend_status(self, status);
    }

    /// Saved by the master on this state: clean, nothing else changes (no new stamps here).
    fn saved_by_master(&mut self) {
        self.dirty = false;
    }

    fn collab_reset(&mut self) {
        pictures::reset(self);
    }
}

/// What WordCraft has and can for the kit's picture frames: pictures are `insert.picture`'s `data`, the document's
/// media by name, Picture Format's changes made again on the picture.
impl PictureHost for Session {
    type Edit = PictureEdit;

    fn frames(&self) -> &PictureFrames {
        &self.signal_frames
    }

    fn frames_mut(&mut self) -> &mut PictureFrames {
        &mut self.signal_frames
    }

    fn picture_param(&self, command: &str, key: &str) -> bool {
        command == "insert.picture" && key == "data"
    }

    fn media(&self) -> Vec<(String, Arc<Vec<u8>>)> {
        self.doc.media.iter().map(|(name, bytes)| (name.clone(), bytes.clone())).collect()
    }

    fn replace_media(&mut self, key: &str, old: &[u8], new: &Arc<Vec<u8>>) -> usize {
        Session::replace_media(self, key, old, new)
    }

    fn replace_media_in(&self, snapshot: &mut Box<EditSnapshot>, key: &str, old: &[u8], new: &Arc<Vec<u8>>) -> usize {
        snapshot.replace_media(key, old, new)
    }

    fn decode(&self, text: &str) -> Option<Vec<u8>> {
        crate::cmd::insert::base64_decode(text)
    }

    fn encode(&self, bytes: &[u8]) -> String {
        crate::cmd::insert::base64_encode(bytes)
    }

    fn redo_edit(&self, edit: &PictureEdit, picture: &[u8]) -> Result<Arc<Vec<u8>>, String> {
        let picture = wordcraft_geom::picture::decode(picture, wordcraft_geom::MAX_DECODED_BYTES).map_err(|e| e.to_string())?;
        let made = edit(picture).map_err(|e| e.to_string())?;
        let mut png = Vec::new();
        made.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).map_err(|e| e.to_string())?;
        Ok(Arc::new(png))
    }
}

impl Session {
    /// What `run` keeps from before a mutating command `id`: the snapshot a collaborator needs
    /// to take its proposal back on a `NAK`, nothing otherwise.
    pub(crate) fn signal_snapshot_before(&self, mutates: bool, id: &str) -> Option<Box<EditSnapshot>> {
        collab::snapshot_before(self, mutates, id)
    }

    /// Connect to a relay (`signal.connect`).
    pub fn signal_connect(&mut self, request: ConnectRequest) -> Result<(), CmdError> {
        collab::connect(self, request).map_err(to_cmd)
    }

    /// An edit of this document just happened: propose it to the collaboration. Called from
    /// `Session::run` for every mutating command that changed something, with the document from
    /// before it and, for a collaborator, the snapshot `run` took.
    pub(crate) fn signal_record(
        &mut self,
        id: &str,
        params: &Value,
        sel_before: &Selection,
        before_doc: Option<&Document>,
        snapshot: Option<Box<EditSnapshot>>,
    ) {
        let anchor = serde_json::to_value(sel_before).unwrap_or(Value::Null);
        // Only the master keeps what moved: it judges the others' proposals by it.
        let structural = self.signal.is_master() && before_doc.is_some_and(|before| paragraphs(before, &anchor) != paragraphs(&self.doc, &anchor));
        collab::record(self, id, params, anchor, structural, snapshot);
    }

    /// Advance the collaboration: read what came in and apply it. The front end calls this every
    /// frame, `signal.poll` on demand.
    pub fn signal_pump(&mut self) {
        collab::pump(self);
    }

    /// Join the collaboration of the active document (`signal.join`).
    pub fn signal_join(&mut self) -> Result<(), CmdError> {
        collab::join(self).map_err(to_cmd)
    }

    /// One step of the sync with the master at time `now` (tests set the clock).
    #[cfg(test)]
    pub(crate) fn signal_sync_step_at(&mut self, now: u64) {
        collab::sync_step_at(self, now);
    }

    /// The wait for the master's save at time `now` (tests set the clock).
    #[cfg(test)]
    pub(crate) fn signal_save_wait_at(&mut self, now: u64) {
        collab::save_wait_at(self, now);
    }
}
