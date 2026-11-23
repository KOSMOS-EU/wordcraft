//! Craft Signal in the session (kosmos, `opencraft/SIGNAL.md`): WordCraft's adapter to the collaboration of
//! the OpenCore kit (`craft-opencore`, `session::collab`). The kit keeps the journal, the conflict rule, the
//! sync with the master and saving through it; this file tells it what WordCraft's documents are: a snapshot
//! is an [`EditSnapshot`], an anchor is a selection (the paragraphs from its first to its last), the state
//! travels as a `.docx` copy, and a picture that is still on its way stands in the document as a frame.
//!
//! Still the master's: the clock and author of a commit (determinism of the engine is the next step). Undo
//! stays local.

use std::sync::Arc;

use craft_opencore::session::collab::{self, CollabHost, SignalSession};
use craft_opencore::signal::{Attachment, ConnectRequest, Hash, PreviewKind};
use serde_json::{Value, json};
use wordcraft_doc::{Document, Path, StoryRef};

use crate::edms::to_cmd;
use crate::signal_frame::{self, FrameFormat};
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

/// An empty picture standing in the document for content that is still on its way.
struct Frame {
    /// The blob it stands for (the picture as the command carries it: base64).
    hash: Hash,
    /// Where it is in the document's media.
    media: String,
    /// Its bytes, to tell it from what may have taken that name since.
    stand_in: Arc<Vec<u8>>,
}

/// A change of a picture's pixels (Picture Format), kept to be made again on the picture itself.
pub(crate) type PictureEdit = Box<dyn Fn(image::DynamicImage) -> Result<image::DynamicImage, CmdError>>;

/// A picture made from a frame by Picture Format, here or by another participant who already
/// had the picture: when the picture arrives, the same change is made to it.
struct Derived {
    /// The media it was made from (a frame, or another derived picture) and its bytes then.
    base: String,
    base_bytes: Arc<Vec<u8>>,
    /// The media it is and its bytes now.
    media: String,
    bytes: Arc<Vec<u8>>,
    edit: PictureEdit,
}

/// The frames in the document whose pictures are still on their way, and what Picture Format made of them.
#[derive(Default)]
pub struct PictureFrames {
    frames: Vec<Frame>,
    derived: Vec<Derived>,
    /// Counts the pictures whose bytes were exchanged under their name (a frame for its picture).
    /// Whoever caches drawn pages by the names of their pictures needs it in the key: the name stays.
    media_epoch: u64,
}

impl PictureFrames {
    /// How many pictures in the document are shown as their frame.
    pub fn framed(&self) -> usize {
        self.frames.len()
    }

    /// See the field: changes whenever a picture's bytes changed under the same media name.
    pub fn media_epoch(&self) -> u64 {
        self.media_epoch
    }

    /// Whether the media `key` with `bytes` is a frame, or was made from one: its picture is
    /// still on its way.
    pub(crate) fn stands_in(&self, key: &str, bytes: &[u8]) -> bool {
        self.frames.iter().any(|frame| frame.media == key && frame.stand_in.as_slice() == bytes)
            || self.derived.iter().any(|derived| derived.media == key && derived.bytes.as_slice() == bytes)
    }

    /// Picture Format made `media` from `base` while `base` stood in for a picture.
    pub(crate) fn note_derived(&mut self, base: &str, base_bytes: Arc<Vec<u8>>, media: &str, bytes: Arc<Vec<u8>>, edit: PictureEdit) {
        self.derived.push(Derived { base: base.to_string(), base_bytes, media: media.to_string(), bytes, edit });
    }
}

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

/// The picture `attachment` names, as the command carries it (base64), if `doc` holds it already.
fn held_picture(doc: &Document, attachment: &Attachment) -> Option<String> {
    doc.media
        .values()
        .filter(|bytes| (bytes.len().div_ceil(3) as u64).saturating_mul(4) == attachment.size)
        .map(|bytes| crate::cmd::insert::base64_encode(bytes))
        .find(|base64| craft_opencore::signal::blobs::hash_of(base64.as_bytes()) == attachment.hash)
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

    /// A bigger picture goes with its pixel size and format, so the others show a frame until it is there.
    fn describe_large(&self, command: &str, key: &str, text: &str) -> Option<(String, (u32, u32))> {
        if command != "insert.picture" || key != "data" {
            return None;
        }
        signal_frame::describe(text).map(|(format, pixels)| (format.mime().to_string(), pixels))
    }

    fn held(&self, attachment: &Attachment) -> Option<String> {
        held_picture(&self.doc, attachment)
    }

    fn stand_in(&self, command: &str, attachment: &Attachment) -> Result<(String, Vec<u8>), String> {
        let format = FrameFormat::from_mime(&attachment.preview_type)
            .filter(|_| attachment.kind() == PreviewKind::Generic && command == "insert.picture")
            .ok_or_else(|| {
                format!("`{}` has no preview that could be shown (a box of {}×{})", attachment.key, attachment.width, attachment.height)
            })?;
        let stand_in = signal_frame::build(format, (attachment.width, attachment.height), &attachment.hash)?;
        Ok((crate::cmd::insert::base64_encode(&stand_in), stand_in))
    }

    /// Remember where the frames a command just put in ended up in the document's media.
    fn stand_ins_placed(&mut self, placed: Vec<(Hash, Vec<u8>)>) {
        for (hash, stand_in) in placed {
            let Some((media, bytes)) = self.doc.media.iter().find(|(_, bytes)| bytes.as_slice() == stand_in.as_slice()) else { continue };
            if self.signal_frames.frames.iter().any(|frame| frame.hash == hash && frame.media == *media) {
                continue;
            }
            let entry = json!({"event": "framed", "media": media, "hash": craft_opencore::signal::blobs::hex(&hash)});
            self.signal_frames.frames.push(Frame { hash, media: media.clone(), stand_in: bytes.clone() });
            self.signal.note(entry.clone());
            self.push_ui_request(json!({"signal": entry}));
        }
    }

    /// The picture `hash` is complete here: put it where its frames are, in the document, in the undo
    /// and redo steps and in the journal's snapshots. Returns how many frames it filled.
    fn content_arrived(&mut self, hash: &Hash, content: Option<&[u8]>, snapshots: &mut [&mut Box<EditSnapshot>]) -> usize {
        let (waiting, rest): (Vec<Frame>, Vec<Frame>) =
            std::mem::take(&mut self.signal_frames.frames).into_iter().partition(|frame| frame.hash == *hash);
        self.signal_frames.frames = rest;
        if waiting.is_empty() {
            return 0;
        }
        let Some(content) = content.and_then(|base64| std::str::from_utf8(base64).ok()).and_then(crate::cmd::insert::base64_decode).map(Arc::new)
        else {
            self.signal.note(json!({"event": "error", "message": "a picture arrived that can't be read; its frame stays"}));
            return 0;
        };
        let mut filled = 0usize;
        for frame in &waiting {
            let replaced = self.signal_replace_media(&frame.media, &frame.stand_in, &content, snapshots);
            self.signal_redo_derived(&frame.media, frame.stand_in.clone(), content.clone(), snapshots);
            filled += usize::from(replaced > 0);
        }
        filled
    }

    fn extend_status(&self, status: &mut Value) {
        status["framed"] = json!(self.signal_frames.framed());
    }

    /// Saved by the master on this state: clean, nothing else changes (no new stamps here).
    fn saved_by_master(&mut self) {
        self.dirty = false;
    }

    fn collab_reset(&mut self) {
        self.signal_frames.frames.clear();
        self.signal_frames.derived.clear();
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

    /// [`Session::replace_media`], and the same in the snapshots the journal would restore.
    fn signal_replace_media(&mut self, key: &str, stand_in: &[u8], content: &Arc<Vec<u8>>, snapshots: &mut [&mut Box<EditSnapshot>]) -> usize {
        let mut replaced = self.replace_media(key, stand_in, content);
        for snapshot in snapshots.iter_mut() {
            replaced += snapshot.replace_media(key, stand_in, content);
        }
        if replaced > 0 {
            self.signal_frames.media_epoch = self.signal_frames.media_epoch.wrapping_add(1);
        }
        replaced
    }

    /// `media` got its picture (`content` where `stand_in` was): make the Picture Format changes
    /// that were made to the stand-in again on the picture, and so on down the chain.
    fn signal_redo_derived(&mut self, media: &str, stand_in: Arc<Vec<u8>>, content: Arc<Vec<u8>>, snapshots: &mut [&mut Box<EditSnapshot>]) {
        let mut arrived = vec![(media.to_string(), stand_in, content)];
        while let Some((base, base_before, base_now)) = arrived.pop() {
            let (waiting, rest): (Vec<Derived>, Vec<Derived>) = std::mem::take(&mut self.signal_frames.derived)
                .into_iter()
                .partition(|derived| derived.base == base && *derived.base_bytes == *base_before);
            self.signal_frames.derived = rest;
            for derived in waiting {
                let made = wordcraft_geom::picture::decode(&base_now, wordcraft_geom::MAX_DECODED_BYTES)
                    .map_err(|e| e.to_string())
                    .and_then(|picture| (derived.edit)(picture).map_err(|e| e.to_string()))
                    .and_then(|picture| {
                        let mut png = Vec::new();
                        picture.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).map_err(|e| e.to_string())?;
                        Ok(Arc::new(png))
                    });
                match made {
                    Ok(png) => {
                        self.signal_replace_media(&derived.media, &derived.bytes, &png, snapshots);
                        arrived.push((derived.media, derived.bytes, png));
                    }
                    Err(e) => self.signal.note(json!({"event": "error", "message": format!("a change to a picture could not be made again on the picture that arrived: {e}")})),
                }
            }
        }
    }
}
