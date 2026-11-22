//! Craft Signal in the session (kosmos, `opencraft/SIGNAL.md`): the seat of this document in a
//! collaboration. The shell installs a [`craft_signal::Port`] (a `Client` over its transport);
//! every edit of the document goes out as a proposal ([`Session::signal_record`], called from
//! `Session::run`), what the master confirms comes back and is applied ([`Session::signal_pump`],
//! once per frame), and as the master this session applies the others' proposals and confirms
//! them. Nothing here blocks; answers arrive in later frames.
//!
//! **Conflict rule (master):** a proposal made on an old `base` is accepted unless a commit since
//! then by someone else touched one of the paragraphs of its anchor, or moved them (changed the
//! paragraph count of their story before them). The sender's own commits never conflict: it made
//! them on the same state. A proposal without an anchor, or with a `base` older than the commits
//! the master still keeps, is rejected. [`SignalState::conflict`].
//!
//! **Rollback on `NAK` (collaborator):** the session keeps a snapshot from before each own
//! proposal and a journal of everything applied since the oldest unconfirmed one. A rejected
//! proposal is taken back by restoring its snapshot and replaying the journal after it
//! ([`Session::signal_rollback`]). `MASTER` is a blanket `NAK`: every unconfirmed proposal is
//! taken back and sent again, in order ([`Session::signal_resend_pending`]).
//!
//! Still the master's: the clock and author of a commit (determinism of the engine is the next
//! step). Undo stays local.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use craft_signal::blobs::MAX_PREVIEW_BYTES;
use craft_signal::frame::MAX_CONTROL_PAYLOAD;
use craft_signal::wire::Mode;
use craft_signal::{Attachment, ConnectRequest, Hash, Incoming, OpPayload, Outstanding, Port, PreviewKind, SignalError};
use serde_json::{Value, json};
use wordcraft_doc::{Document, Path, StoryRef};

use crate::signal_frame::{self, FrameFormat};
use crate::snapshot::EditSnapshot;
use crate::{CmdError, Selection, Session};

/// Most events kept for `signal.poll` when nobody drains them.
pub const MAX_LOG: usize = 64;
/// Most commits the master keeps for the conflict rule; a proposal based further back is rejected.
pub const MAX_TRACES: usize = 1024;
/// The reason the client gives a proposal that a master change returned.
const MASTER_CHANGED: &str = "master changed";

/// A text parameter at least this long leaves a command that doesn't fit the control channel
/// and travels as a blob on the bulk channel (SIGNAL.md, "Vorschau vor Bestätigung").
const MIN_DETACHED_BYTES: usize = 256;

/// How long a participant waits for the master's answer to its sync request, and then for the
/// bytes of the state, before it goes on with the document it has (a master that never answers).
const SYNC_ANSWER_MS: u64 = 10_000;
const SYNC_BYTES_MS: u64 = 60_000;
/// After this long without the bytes of a named state they are asked for (they were already in
/// the relay's buffer, so nobody sent them again).
const SYNC_ASK_BYTES_MS: u64 = 2_000;
/// Most commits kept while the state is on its way; more than that and the wait is given up.
const MAX_SYNC_COMMITS: usize = 4096;

/// The state a master answers a sync request with: the document as a `.docx` copy.
const SYNC_STATE_FORMAT: &str = "docx";
const SYNC_STATE_NAME: &str = "state.docx";

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

/// What a commit touched (kept by the master): its paragraphs, and whether it changed the
/// paragraph count of its story, which moves every paragraph after it.
#[derive(Clone, Debug)]
struct CommitTrace {
    seq: u32,
    sender: u16,
    touched: Option<Touched>,
    structural: bool,
}

/// What a collaborator applied since its oldest unconfirmed proposal, in order.
enum Journal {
    /// An own proposal: the snapshot from before it (as long as it is unconfirmed) and whether the
    /// master confirmed it.
    Own { local: u32, op: OpPayload, before: Option<Box<EditSnapshot>>, confirmed: bool },
    /// Someone else's commit, applied.
    Remote { seq: u32, op: OpPayload },
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

/// A participant bringing its document up to date with the session it joined: it asked the
/// master for the state and keeps what is confirmed meanwhile, to apply it on top.
struct Syncing {
    asked_ms: u64,
    /// The state the master named, once its `SYNC` is here.
    state: Option<NamedState>,
    /// Commits that came meanwhile, in order.
    commits: Vec<(u32, u16, u32, OpPayload)>,
}

struct NamedState {
    hash: Hash,
    /// The last commit the state includes.
    seq: u32,
    named_ms: u64,
    bytes_asked: bool,
}

/// A frame `attached` put into a command's parameters, before the command ran.
struct FramedParameter {
    hash: Hash,
    stand_in: Vec<u8>,
}

/// The collaboration state of a session.
#[derive(Default)]
pub struct SignalState {
    port: Option<Box<dyn Port>>,
    /// A remote command is being applied: `run` must not propose it again.
    applying: bool,
    /// What happened since the last `signal.poll`, oldest first.
    log: Vec<Value>,
    /// The master's commits, oldest first, for the conflict rule.
    traces: VecDeque<CommitTrace>,
    /// The collaborator's journal since its oldest unconfirmed proposal.
    journal: Vec<Journal>,
    /// The frames in the document whose pictures are still on their way.
    frames: Vec<Frame>,
    /// What Picture Format made of those frames.
    derived: Vec<Derived>,
    /// Set while this participant waits for the master's state after joining.
    syncing: Option<Syncing>,
    /// Proposals a master change sent again; their `NAK`s are expected and not errors.
    resent_locals: HashSet<u32>,
    pub applied: u64,
    pub confirmed: u64,
    pub rejected: u64,
    /// Rejected own proposals taken back (restored and replayed).
    pub rolled_back: u64,
    /// Own proposals sent again after a master change.
    pub resent: u64,
}

impl SignalState {
    /// The shell installs its client once; the commands refuse until then.
    pub fn install(&mut self, port: Box<dyn Port>) {
        self.port = Some(port);
    }

    pub fn has_port(&self) -> bool {
        self.port.is_some()
    }

    pub fn joined(&self) -> bool {
        self.port.as_ref().is_some_and(|p| p.joined())
    }

    pub fn is_master(&self) -> bool {
        self.port.as_ref().is_some_and(|p| p.is_master())
    }

    pub fn applying(&self) -> bool {
        self.applying
    }

    /// Whether `run` must keep a snapshot from before command `id`: an own edit about to be
    /// proposed (not the master's: it commits itself and nothing takes that back).
    fn wants_snapshot(&self, id: &str) -> bool {
        if self.applying || !transmits(id) {
            return false;
        }
        self.port.as_ref().is_some_and(|p| p.joined() && p.mode() == Mode::Edit && !p.is_master())
    }

    /// The delivery list of the collaboration: what is valid here but not complete yet (a
    /// picture shown as its preview). Empty outside a session.
    pub fn outstanding(&self) -> Vec<Outstanding> {
        self.port.as_ref().filter(|port| port.joined()).map(|port| port.outstanding()).unwrap_or_default()
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

    /// Whether the document is waiting for the master's state: it is about to be replaced, so it
    /// can't be edited.
    pub fn syncing(&self) -> bool {
        self.syncing.is_some()
    }

    /// How many pictures in the document are shown as their frame.
    pub fn framed(&self) -> usize {
        self.frames.len()
    }

    /// Move what makes `op` too big for the control channel out of its parameters: each long
    /// text parameter (a picture's base64, pasted text) becomes a blob offered on the bulk
    /// channel and an attachment naming it. What fits a preview is its own preview. A bigger
    /// picture goes with its pixel size and format, so the others show a frame until it is
    /// there ([`signal_frame`]); anything else that big has to be at the master whole before
    /// the command counts. A reduced preview of a big picture is not built yet.
    fn detach(&mut self, op: &mut OpPayload) -> Result<(), SignalError> {
        let fits = |op: &OpPayload| craft_signal::wire::encode(op).map(|payload| payload.len() <= MAX_CONTROL_PAYLOAD);
        if fits(op)? {
            return Ok(());
        }
        let Some(port) = self.port.as_mut() else { return Ok(()) };
        let Some(params) = op.params.as_object_mut() else { return Ok(()) };
        let long: Vec<String> = params
            .iter()
            .filter(|(_, value)| value.as_str().is_some_and(|text| text.len() >= MIN_DETACHED_BYTES))
            .map(|(key, _)| key.clone())
            .collect();
        for key in long {
            let Some(Value::String(text)) = params.remove(&key) else { continue };
            let framed =
                if text.len() > MAX_PREVIEW_BYTES && op.command == "insert.picture" && key == "data" { signal_frame::describe(&text) } else { None };
            let bytes = text.into_bytes();
            let attachment = match framed {
                _ if bytes.len() <= MAX_PREVIEW_BYTES => Attachment::original(&key, &bytes),
                Some((format, pixels)) => {
                    let mut attachment = Attachment::generic(&key, &bytes, pixels);
                    attachment.preview_type = format.mime().to_string();
                    attachment
                }
                None => Attachment::whole(&key, &bytes),
            };
            port.offer(bytes)?;
            op.attachments.push(attachment);
        }
        Ok(())
    }

    /// The parameters of `op` with its attachments back in them: the blob the command is valid
    /// with (the content itself, or its preview while the content is on its way). A picture
    /// that is only announced by its size gets a frame; those come back with the parameters.
    /// A picture `doc` already holds is taken from there, as the one who brought it in did.
    fn attached(&self, op: &OpPayload, doc: &Document) -> Result<(Value, Vec<FramedParameter>), String> {
        let mut framed = Vec::new();
        if op.attachments.is_empty() {
            return Ok((op.params.clone(), framed));
        }
        let port = self.port.as_ref().ok_or_else(|| "no Craft Signal transport".to_string())?;
        let mut params = if op.params.is_object() { op.params.clone() } else { json!({}) };
        for attachment in &op.attachments {
            let bytes = match port.blob(&attachment.hash) {
                Some(bytes) => bytes,
                None => match attachment.required() {
                    Ok(Some(preview)) => port.blob(&preview).ok_or_else(|| format!("the preview of `{}` is not here", attachment.key))?,
                    Ok(None) => {
                        if let Some(held) = held_picture(doc, attachment) {
                            params[attachment.key.as_str()] = json!(held);
                            continue;
                        }
                        let format = FrameFormat::from_mime(&attachment.preview_type)
                            .filter(|_| attachment.kind() == PreviewKind::Generic && op.command == "insert.picture")
                            .ok_or_else(|| {
                                format!(
                                    "`{}` has no preview that could be shown (a box of {}×{})",
                                    attachment.key, attachment.width, attachment.height
                                )
                            })?;
                        let stand_in = signal_frame::build(format, (attachment.width, attachment.height), &attachment.hash)?;
                        params[attachment.key.as_str()] = json!(crate::cmd::insert::base64_encode(&stand_in));
                        framed.push(FramedParameter { hash: attachment.hash, stand_in });
                        continue;
                    }
                    Err(reason) => return Err(reason),
                },
            };
            let text = std::str::from_utf8(bytes).map_err(|_| format!("the content of `{}` is not text", attachment.key))?;
            params[attachment.key.as_str()] = json!(text);
        }
        Ok((params, framed))
    }

    pub fn port_mut(&mut self) -> Result<&mut (dyn Port + '_), CmdError> {
        match self.port.as_deref_mut() {
            Some(port) => Ok(port),
            None => Err(CmdError::Disabled("this build has no Craft Signal transport".into())),
        }
    }

    /// The state as `signal.status` reports it.
    pub fn status(&self) -> Value {
        let mut status = self.port.as_ref().map(|p| p.status()).unwrap_or_else(|| json!({"state": "disconnected"}));
        status["transport"] = json!(self.port.is_some());
        status["applied"] = json!(self.applied);
        status["confirmed"] = json!(self.confirmed);
        status["rejected"] = json!(self.rejected);
        status["rolledBack"] = json!(self.rolled_back);
        status["resent"] = json!(self.resent);
        status["journal"] = json!(self.journal.len());
        status["traces"] = json!(self.traces.len());
        status["framed"] = json!(self.framed());
        status["syncing"] = json!(self.syncing());
        status
    }

    pub fn take_log(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.log)
    }

    fn note(&mut self, entry: Value) {
        if self.log.len() >= MAX_LOG {
            self.log.remove(0);
        }
        self.log.push(entry);
    }

    /// The master's conflict rule for a proposal by `sender` made on `base` while the session is
    /// at `seq_now`: `Some(reason)` rejects it.
    fn conflict(&self, sender: u16, base: u32, seq_now: u32, anchor: &Value) -> Option<String> {
        if base >= seq_now {
            return None;
        }
        let oldest = self.traces.front().map(|t| t.seq).unwrap_or(seq_now.saturating_add(1));
        if oldest > base.saturating_add(1) {
            return Some(format!("base {base} is older than the commits the master keeps (from {oldest})"));
        }
        let foreign = self.traces.iter().filter(|t| t.seq > base && t.sender != sender);
        let mine = touched(anchor);
        for trace in foreign {
            let hit = match (&mine, &trace.touched) {
                (None, _) | (_, None) => true,
                (Some(mine), Some(theirs)) => theirs.story == mine.story && theirs.from <= mine.to && (mine.from <= theirs.to || trace.structural),
            };
            if hit {
                return Some(format!("base {base}: conflicts with seq {} of participant {}", trace.seq, trace.sender));
            }
        }
        None
    }

    fn trace(&mut self, seq: u32, sender: u16, anchor: &Value, structural: bool) {
        if self.traces.len() >= MAX_TRACES {
            self.traces.pop_front();
        }
        self.traces.push_back(CommitTrace { seq, sender, touched: touched(anchor), structural });
    }

    /// Drop the journal before the oldest unconfirmed proposal; all of it when none is left.
    fn prune_journal(&mut self) {
        match self.journal.iter().position(|e| matches!(e, Journal::Own { confirmed: false, .. })) {
            Some(first) => {
                self.journal.drain(..first);
            }
            None => self.journal.clear(),
        }
    }

    fn pending_locals(&self) -> Vec<u32> {
        self.journal
            .iter()
            .filter_map(|e| match e {
                Journal::Own { local, confirmed: false, .. } => Some(*local),
                _ => None,
            })
            .collect()
    }
}

/// Commands that stay local: files, the modules themselves, the tools, the view, undo (open in the
/// concept), and the ones that replace the document.
pub fn transmits(id: &str) -> bool {
    !(id.starts_with("file.")
        || id.starts_with("edms.")
        || id.starts_with("signal.")
        || id.starts_with("tools.")
        || id.starts_with("view.")
        || id.starts_with("ui.")
        || matches!(
            id,
            "edit.undo"
                | "edit.redo"
                | "edit.undoTo"
                | "edit.repeat"
                | "review.compare"
                | "review.combine"
                | "mailings.finish"
                | "mailings.envelopes"
                | "mailings.labels"
        ))
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

impl Session {
    /// What `run` keeps from before a mutating command `id`: the snapshot a collaborator needs
    /// to take its proposal back on a `NAK`, nothing otherwise.
    pub(crate) fn signal_snapshot_before(&self, mutates: bool, id: &str) -> Option<Box<EditSnapshot>> {
        (mutates && self.signal.wants_snapshot(id)).then(|| Box::new(self.edit_snapshot()))
    }

    /// Connect to a relay (`signal.connect`).
    pub fn signal_connect(&mut self, request: ConnectRequest) -> Result<(), CmdError> {
        self.signal.port_mut()?.connect(request).map_err(signal_error)
    }

    /// An edit of this document just happened: propose it to the collaboration. Called from
    /// `Session::run` for every mutating command that changed something, with the document from
    /// before it and, for a collaborator, the snapshot `run` took ([`SignalState::wants_snapshot`]).
    pub(crate) fn signal_record(
        &mut self,
        id: &str,
        params: &Value,
        sel_before: &Selection,
        before_doc: Option<&Document>,
        snapshot: Option<Box<EditSnapshot>>,
    ) {
        if self.signal.applying || !transmits(id) {
            return;
        }
        let Some(port) = self.signal.port.as_ref() else { return };
        if !port.joined() || port.mode() != Mode::Edit {
            return;
        }
        let mut params = params.clone();
        if let Some(m) = params.as_object_mut() {
            m.remove("password");
        }
        let anchor = serde_json::to_value(sel_before).unwrap_or(Value::Null);
        let mut op = OpPayload { command: id.to_string(), params, anchor: anchor.clone(), time: 0, author: 0, attachments: Vec::new() };
        let master = port.is_master();
        let me = port.id();
        if let Err(e) = self.signal.detach(&mut op) {
            let message = format!("{id}: {e}");
            self.signal.note(json!({"event": "error", "message": message}));
            self.push_ui_request(json!({"signal": {"event": "error", "message": message}}));
            return;
        }
        let structural = master && before_doc.is_some_and(|before| paragraphs(before, &anchor) != paragraphs(&self.doc, &anchor));
        let Some(port) = self.signal.port.as_mut() else { return };
        match port.propose(op.clone()) {
            Ok(local) => {
                if master {
                    let seq = port.seq();
                    self.signal.trace(seq, me, &anchor, structural);
                } else {
                    self.signal.journal.push(Journal::Own { local, op, before: snapshot, confirmed: false });
                }
            }
            Err(e) => {
                let message = format!("{id}: {e}");
                self.signal.note(json!({"event": "error", "message": message}));
                self.push_ui_request(json!({"signal": {"event": "error", "message": format!("{id}: {e}")}}));
            }
        }
    }

    /// Advance the collaboration: read what came in and apply it. The front end calls this every
    /// frame, `signal.poll` on demand.
    pub fn signal_pump(&mut self) {
        let incoming = match self.signal.port.as_mut() {
            Some(port) => {
                // The client has no clock: with the time it gives up on a preview that never comes.
                port.set_time(now_ms());
                port.poll()
            }
            None => return,
        };
        for item in incoming {
            self.signal_apply(item);
        }
        self.signal_sync_step_at(now_ms());
    }

    /// Ask the master for its document and wait for it: until it is here nothing is applied or
    /// edited ([`SignalState::syncing`]).
    fn signal_request_state(&mut self, seq: u32) {
        let asked = match self.signal.port.as_mut() {
            Some(port) => port.request_sync(seq).map_err(|e| e.to_string()),
            None => Err("no Craft Signal transport".into()),
        };
        match asked {
            Ok(()) => {
                self.signal.syncing = Some(Syncing { asked_ms: now_ms(), state: None, commits: Vec::new() });
                self.signal.note(json!({"event": "syncing", "seq": seq}));
            }
            Err(e) => self.signal.note(json!({"event": "error", "message": format!("the master's state could not be asked for: {e}")})),
        }
    }

    /// Go on with the document that is here: the state did not come. What was confirmed meanwhile
    /// is applied to it, as before there was a state to ask for.
    fn signal_give_up_state(&mut self, reason: &str) {
        let Some(syncing) = self.signal.syncing.take() else { return };
        let entry = json!({"event": "syncFailed", "reason": reason, "commits": syncing.commits.len()});
        self.signal.note(entry.clone());
        self.push_ui_request(json!({"signal": entry}));
        for (seq, sender, local, op) in syncing.commits {
            self.signal_apply(Incoming::Commit { seq, sender, local, op });
        }
    }

    /// One step of bringing the document up to date: take the state when its bytes are here, ask
    /// for them when they don't come by themselves, give up when nothing comes at all.
    pub(crate) fn signal_sync_step_at(&mut self, now: u64) {
        let Some(syncing) = self.signal.syncing.as_mut() else { return };
        let Some(state) = syncing.state.as_mut() else {
            if now.saturating_sub(syncing.asked_ms) > SYNC_ANSWER_MS {
                self.signal_give_up_state("the master did not answer");
            }
            return;
        };
        let (hash, state_seq, waited_ms) = (state.hash, state.seq, now.saturating_sub(state.named_ms));
        let bytes = self.signal.port.as_ref().and_then(|port| port.blob(&hash)).map(<[u8]>::to_vec);
        let Some(bytes) = bytes else {
            if waited_ms > SYNC_BYTES_MS {
                self.signal_give_up_state("the bytes of the master's state did not come");
            } else if waited_ms > SYNC_ASK_BYTES_MS && !state.bytes_asked {
                state.bytes_asked = true;
                if let Some(port) = self.signal.port.as_mut() {
                    let _ = port.request(&hash);
                }
            }
            return;
        };
        if let Err(e) = self.signal_adopt_state(&bytes) {
            self.signal_give_up_state(&format!("the master's state can't be opened: {e}"));
            return;
        }
        let commits = self.signal.syncing.take().map(|syncing| syncing.commits).unwrap_or_default();
        let mut replayed = 0usize;
        for (seq, sender, local, op) in commits {
            // The state already holds what was confirmed up to its `seq`.
            if seq > state_seq {
                self.signal_apply(Incoming::Commit { seq, sender, local, op });
                replayed += 1;
            }
        }
        let entry = json!({"event": "synced", "seq": state_seq, "size": bytes.len(), "replayed": replayed});
        self.signal.note(entry.clone());
        self.push_ui_request(json!({"signal": entry}));
    }

    /// Put the master's document in the place of the one open here. Where it lives (its EDMS
    /// entry, its name) stays: only the content is the session's.
    fn signal_adopt_state(&mut self, bytes: &[u8]) -> Result<(), String> {
        let (doc, _encrypted) = crate::io::open_bytes_in(SYNC_STATE_NAME, bytes, None, &self.region())?;
        let old_id = self.document_id();
        let (file_name, path, password) = (self.file_name.clone(), self.path.clone(), self.password.clone());
        self.signal.frames.clear();
        self.signal.derived.clear();
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

    fn signal_apply(&mut self, item: Incoming) {
        match item {
            Incoming::Joined { session, id, mode, rank, master, seq, reason } => {
                self.signal.traces.clear();
                self.signal.journal.clear();
                self.signal.resent_locals.clear();
                let entry = json!({"event": "joined", "session": session, "id": id, "mode": mode.name(), "rank": if rank == craft_signal::wire::NO_RANK { Value::Null } else { json!(rank) }, "master": master, "seq": seq, "reason": reason});
                self.signal.note(entry.clone());
                self.push_ui_request(json!({"signal": entry}));
                // Joining a session someone else leads: its document is the master's, not the
                // one opened here (the master may be ahead of the file, saved or not).
                self.signal.syncing = None;
                if master != 0 && master != id {
                    self.signal_request_state(seq);
                }
            }
            Incoming::ModeGranted { mode, reason } => {
                let entry = json!({"event": "mode", "mode": mode.name(), "reason": reason});
                self.signal.note(entry.clone());
                self.push_ui_request(json!({"signal": entry}));
            }
            Incoming::Roster { members, master } => {
                let entry = json!({"event": "roster", "members": members.len(), "master": master});
                self.signal.note(entry);
            }
            Incoming::MasterChanged { master, me, lock } => {
                // The new master has never seen what was unconfirmed: take it back and send it
                // again, on the state the whole session agrees on.
                self.signal.traces.clear();
                if self.signal.syncing.is_some() {
                    // The one who was asked is gone. Another master is asked again; as the master
                    // itself this document is the session's now.
                    if me {
                        self.signal_give_up_state("this participant became the master before the state came");
                    } else {
                        let seq = self.signal.port.as_ref().map_or(0, |port| port.seq());
                        let kept = self.signal.syncing.take().map(|syncing| syncing.commits).unwrap_or_default();
                        self.signal_request_state(seq);
                        if let Some(syncing) = self.signal.syncing.as_mut() {
                            syncing.commits = kept;
                        }
                    }
                }
                let resent = self.signal_resend_pending();
                let entry = json!({"event": "master", "master": master, "me": me, "lock": !lock.is_empty(), "resent": resent});
                self.signal.note(entry.clone());
                self.push_ui_request(json!({"signal": entry}));
            }
            Incoming::Commit { seq, sender, local, op } if self.signal.syncing.is_some() => {
                // On top of the state that is on its way, not of the document that is replaced.
                let kept = self.signal.syncing.as_mut().map_or(0, |syncing| {
                    syncing.commits.push((seq, sender, local, op));
                    syncing.commits.len()
                });
                if kept > MAX_SYNC_COMMITS {
                    self.signal_give_up_state("too much happened while the state was on its way");
                }
            }
            Incoming::Commit { seq, sender, local, op } => match self.signal_run_remote(&op) {
                Ok(()) => {
                    self.signal.applied += 1;
                    if !self.signal.journal.is_empty() {
                        self.signal.journal.push(Journal::Remote { seq, op: op.clone() });
                    }
                    self.signal.note(json!({"event": "applied", "seq": seq, "sender": sender, "local": local, "command": op.command}));
                }
                Err(e) => {
                    self.signal.note(json!({"event": "applyFailed", "seq": seq, "sender": sender, "command": op.command, "error": e}));
                    self.push_ui_request(json!({"signal": {"event": "applyFailed", "seq": seq, "command": op.command, "error": e}}));
                }
            },
            Incoming::Confirmed { local, seq } => {
                self.signal.confirmed += 1;
                for entry in &mut self.signal.journal {
                    if let Journal::Own { local: l, confirmed, before, .. } = entry
                        && *l == local
                    {
                        *confirmed = true;
                        *before = None;
                    }
                }
                self.signal.prune_journal();
                self.signal.note(json!({"event": "confirmed", "local": local, "seq": seq}));
            }
            Incoming::Rejected { local, op, payload } => {
                let reason = String::from_utf8_lossy(&payload).into_owned();
                let command = op.as_ref().map(|o| o.command.clone()).unwrap_or_default();
                if reason == MASTER_CHANGED && self.signal.resent_locals.remove(&local) {
                    // Already taken back and sent again by the master change.
                    self.signal.note(json!({"event": "resent", "local": local, "command": command}));
                    return;
                }
                self.signal.rejected += 1;
                let undone = match self.signal_rollback(local) {
                    Ok(replayed) => {
                        self.signal.rolled_back += 1;
                        json!({"undone": true, "replayed": replayed})
                    }
                    Err(e) => json!({"undone": false, "error": e}),
                };
                let mut entry = json!({"event": "rejected", "local": local, "command": command, "reason": reason});
                if let (Some(entry), Some(undone)) = (entry.as_object_mut(), undone.as_object()) {
                    entry.extend(undone.clone());
                }
                self.signal.note(entry.clone());
                self.push_ui_request(json!({"signal": entry}));
            }
            Incoming::Proposed { sender, local, base, op } => {
                let seq_now = self.signal.port.as_ref().map(|p| p.seq()).unwrap_or(0);
                if let Some(reason) = self.signal.conflict(sender, base, seq_now, &op.anchor) {
                    if let Some(port) = self.signal.port.as_mut() {
                        let _ = port.reject(sender, local, &reason);
                    }
                    self.signal.note(
                        json!({"event": "rejectedProposal", "sender": sender, "local": local, "base": base, "command": op.command, "reason": reason}),
                    );
                    return;
                }
                let before = paragraphs(&self.doc, &op.anchor);
                let applied = self.signal_run_remote(&op);
                let structural = before != paragraphs(&self.doc, &op.anchor);
                let Some(port) = self.signal.port.as_mut() else { return };
                match applied {
                    Ok(()) => match port.commit(sender, local, op.clone(), now_ms()) {
                        Ok(seq) => {
                            self.signal.applied += 1;
                            self.signal.trace(seq, sender, &op.anchor, structural);
                            self.signal.note(
                                json!({"event": "committed", "seq": seq, "sender": sender, "local": local, "base": base, "command": op.command}),
                            );
                        }
                        Err(e) => self.signal.note(json!({"event": "error", "message": format!("commit: {e}")})),
                    },
                    Err(e) => {
                        let _ = port.reject(sender, local, &e);
                        self.signal.note(
                            json!({"event": "rejectedProposal", "sender": sender, "local": local, "base": base, "command": op.command, "reason": e}),
                        );
                    }
                }
            }
            Incoming::SyncRequested { sender, base } => {
                // The master's whole document as it is now, as a copy (no save stamps): the bytes
                // go as a blob, the answer names them. Commits after `base` are not sent singly.
                let answered = crate::cmd::edms::copy_bytes(self, SYNC_STATE_NAME).map_err(|e| e.to_string()).and_then(|state| {
                    let size = state.len();
                    let port = self.signal.port.as_mut().ok_or_else(|| "no Craft Signal transport".to_string())?;
                    port.answer_sync(sender, SYNC_STATE_FORMAT, state).map(|hash| (hash, size)).map_err(|e| e.to_string())
                });
                let entry = match answered {
                    Ok((hash, size)) => {
                        json!({"event": "syncRequested", "sender": sender, "base": base, "answered": true, "hash": craft_signal::blobs::hex(&hash), "size": size})
                    }
                    Err(error) => json!({"event": "syncRequested", "sender": sender, "base": base, "answered": false, "error": error}),
                };
                self.signal.note(entry);
            }
            Incoming::Sync { seq, payload } => {
                let now = now_ms();
                let Some(syncing) = self.signal.syncing.as_mut() else { return };
                match craft_signal::wire::decode::<craft_signal::SyncPayload>(&payload) {
                    Ok(sync) if sync.format == SYNC_STATE_FORMAT => {
                        syncing.state = Some(NamedState { hash: sync.state.hash, seq, named_ms: now, bytes_asked: false })
                    }
                    Ok(sync) => self.signal_give_up_state(&format!("the master sent a state as `{}`, which this app does not read", sync.format)),
                    Err(e) => self.signal_give_up_state(&format!("the master's state can't be read: {e}")),
                }
            }
            Incoming::Presence { .. } => {}
            Incoming::Saved { seq } => {
                self.signal.note(json!({"event": "saved", "seq": seq}));
            }
            Incoming::BlobComplete { hash } => {
                // The content behind a frame arrived: it takes the frame's place.
                let replaced = self.signal_fill_frames(&hash);
                let entry = json!({"event": "blobComplete", "hash": craft_signal::blobs::hex(&hash), "replaced": replaced});
                self.signal.note(entry.clone());
                self.push_ui_request(json!({"signal": entry}));
            }
            Incoming::Closed { reason } => {
                self.signal.frames.clear();
                self.signal.derived.clear();
                self.signal.syncing = None;
                self.signal.journal.clear();
                self.signal.traces.clear();
                let entry = json!({"event": "closed", "reason": reason});
                self.signal.note(entry.clone());
                self.push_ui_request(json!({"signal": entry}));
            }
            Incoming::Error { message } => {
                let entry = json!({"event": "error", "message": message});
                self.signal.note(entry.clone());
                self.push_ui_request(json!({"signal": entry}));
            }
        }
    }

    /// Run another participant's command: with their anchor as the selection, then the own
    /// selection back (clamped, the text may have moved under it).
    fn signal_run_remote(&mut self, op: &OpPayload) -> Result<(), String> {
        if !transmits(&op.command) {
            return Err(format!("{} is not a collaboration command", op.command));
        }
        let (params, framed) = self.signal.attached(op, &self.doc)?;
        let mine = self.sel.clone();
        let theirs: Option<Selection> = serde_json::from_value(op.anchor.clone()).ok();
        self.signal.applying = true;
        if let Some(sel) = theirs {
            self.sel = sel;
            self.clamp_selection();
        }
        let result = self.run(&op.command, &params);
        self.signal.applying = false;
        self.sel = mine;
        self.clamp_selection();
        if result.is_ok() {
            self.signal_note_frames(framed);
        }
        result.map(|_| ()).map_err(|e| e.to_string())
    }

    /// Remember where the frames a command just put in ended up in the document's media.
    fn signal_note_frames(&mut self, framed: Vec<FramedParameter>) {
        for FramedParameter { hash, stand_in } in framed {
            let Some((media, bytes)) = self.doc.media.iter().find(|(_, bytes)| bytes.as_slice() == stand_in.as_slice()) else { continue };
            if self.signal.frames.iter().any(|frame| frame.hash == hash && frame.media == *media) {
                continue;
            }
            let entry = json!({"event": "framed", "media": media, "hash": craft_signal::blobs::hex(&hash)});
            self.signal.frames.push(Frame { hash, media: media.clone(), stand_in: bytes.clone() });
            self.signal.note(entry.clone());
            self.push_ui_request(json!({"signal": entry}));
        }
    }

    /// [`Session::replace_media`], and the same in the snapshots the journal would restore.
    fn signal_replace_media(&mut self, key: &str, stand_in: &[u8], content: &Arc<Vec<u8>>) -> usize {
        let mut replaced = self.replace_media(key, stand_in, content);
        for entry in &mut self.signal.journal {
            if let Journal::Own { before: Some(snapshot), .. } = entry {
                replaced += snapshot.replace_media(key, stand_in, content);
            }
        }
        replaced
    }

    /// `media` got its picture (`content` where `stand_in` was): make the Picture Format changes
    /// that were made to the stand-in again on the picture, and so on down the chain.
    fn signal_redo_derived(&mut self, media: &str, stand_in: Arc<Vec<u8>>, content: Arc<Vec<u8>>) {
        let mut arrived = vec![(media.to_string(), stand_in, content)];
        while let Some((base, base_before, base_now)) = arrived.pop() {
            let (waiting, rest): (Vec<Derived>, Vec<Derived>) =
                std::mem::take(&mut self.signal.derived).into_iter().partition(|derived| derived.base == base && *derived.base_bytes == *base_before);
            self.signal.derived = rest;
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
                        self.signal_replace_media(&derived.media, &derived.bytes, &png);
                        arrived.push((derived.media, derived.bytes, png));
                    }
                    Err(e) => self.signal.note(json!({"event": "error", "message": format!("a change to a picture could not be made again on the picture that arrived: {e}")})),
                }
            }
        }
    }

    /// The picture `hash` is complete here: put it where its frames are, in the document, in
    /// the undo and redo steps and in the journal's snapshots. Returns how many frames it filled.
    fn signal_fill_frames(&mut self, hash: &Hash) -> usize {
        let (waiting, rest): (Vec<Frame>, Vec<Frame>) = std::mem::take(&mut self.signal.frames).into_iter().partition(|frame| frame.hash == *hash);
        self.signal.frames = rest;
        if waiting.is_empty() {
            return 0;
        }
        let content = self
            .signal
            .port
            .as_ref()
            .and_then(|port| port.blob(hash))
            .and_then(|base64| std::str::from_utf8(base64).ok())
            .and_then(crate::cmd::insert::base64_decode)
            .map(Arc::new);
        let Some(content) = content else {
            self.signal.note(json!({"event": "error", "message": "a picture arrived that can't be read; its frame stays"}));
            return 0;
        };
        let mut filled = 0usize;
        for frame in &waiting {
            let replaced = self.signal_replace_media(&frame.media, &frame.stand_in, &content);
            self.signal_redo_derived(&frame.media, frame.stand_in.clone(), content.clone());
            filled += usize::from(replaced > 0);
        }
        filled
    }

    /// Take a rejected own proposal back: restore the snapshot from before it and replay the
    /// journal after it (others' commits, own confirmed and still pending proposals, in order).
    /// Returns how many entries were replayed.
    fn signal_rollback(&mut self, local: u32) -> Result<usize, String> {
        let index = self
            .signal
            .journal
            .iter()
            .position(|e| matches!(e, Journal::Own { local: l, .. } if *l == local))
            .ok_or_else(|| "not in the journal (confirmed, or older than the journal)".to_string())?;
        if !matches!(&self.signal.journal[index], Journal::Own { before: Some(_), .. }) {
            self.signal.journal.remove(index);
            return Err("no snapshot from before it".into());
        }
        let mut tail = self.signal.journal.drain(index..).collect::<Vec<_>>().into_iter();
        let Some(Journal::Own { before: Some(snapshot), .. }) = tail.next() else {
            return Err("no snapshot from before it".into());
        };
        let mine = self.sel.clone();
        self.restore(*snapshot);
        let mut replayed = 0usize;
        for entry in tail {
            match entry {
                Journal::Remote { seq, op } => {
                    if let Err(e) = self.signal_run_remote(&op) {
                        self.signal.note(json!({"event": "replayFailed", "seq": seq, "command": op.command, "error": e}));
                    }
                    self.signal.journal.push(Journal::Remote { seq, op });
                }
                Journal::Own { local, op, confirmed: true, .. } => {
                    if let Err(e) = self.signal_run_remote(&op) {
                        self.signal.note(json!({"event": "replayFailed", "local": local, "command": op.command, "error": e}));
                    }
                    self.signal.journal.push(Journal::Own { local, op, before: None, confirmed: true });
                }
                Journal::Own { local, op, confirmed: false, .. } => {
                    let before = Box::new(self.edit_snapshot());
                    if let Err(e) = self.signal_run_remote(&op) {
                        self.signal.note(json!({"event": "replayFailed", "local": local, "command": op.command, "error": e}));
                    }
                    self.signal.journal.push(Journal::Own { local, op, before: Some(before), confirmed: false });
                }
            }
            replayed += 1;
        }
        self.signal.prune_journal();
        self.sel = mine;
        self.clamp_selection();
        Ok(replayed)
    }

    /// A master change: take every unconfirmed proposal back (oldest first, the others and the
    /// confirmed replayed) and run each again as a new proposal, in order. As the new master that
    /// commits them right away. Returns how many were sent again.
    fn signal_resend_pending(&mut self) -> usize {
        let pending = self.signal.pending_locals();
        let Some(&oldest) = pending.first() else { return 0 };
        // Collect the pending commands before the rollback rebuilds the journal.
        let ops: Vec<OpPayload> = self
            .signal
            .journal
            .iter()
            .filter_map(|e| match e {
                Journal::Own { op, confirmed: false, .. } => Some(op.clone()),
                _ => None,
            })
            .collect();
        // Roll back to before the oldest; the replay puts the later pending ones back in, so drop
        // them from the document by rolling each of them back too, youngest last.
        if let Err(e) = self.signal_rollback(oldest) {
            self.signal.note(json!({"event": "error", "message": format!("master change: {e}")}));
            return 0;
        }
        for local in self.signal.pending_locals() {
            if let Err(e) = self.signal_rollback(local) {
                self.signal.note(json!({"event": "error", "message": format!("master change: {e}")}));
            }
        }
        self.signal.journal.clear();
        let mut resent = 0usize;
        let mine = self.sel.clone();
        for (local, op) in pending.iter().zip(ops) {
            self.signal.resent_locals.insert(*local);
            if let Ok(sel) = serde_json::from_value::<Selection>(op.anchor.clone()) {
                self.sel = sel;
                self.clamp_selection();
            }
            // The proposal lost what went out as a blob; it is still here.
            let params = match self.signal.attached(&op, &self.doc) {
                Ok((params, _)) => params,
                Err(e) => {
                    self.signal.note(json!({"event": "resendFailed", "local": local, "command": op.command, "error": e}));
                    continue;
                }
            };
            match self.run(&op.command, &params) {
                Ok(_) => resent += 1,
                Err(e) => self.signal.note(json!({"event": "resendFailed", "local": local, "command": op.command, "error": e.to_string()})),
            }
        }
        self.sel = mine;
        self.clamp_selection();
        self.signal.resent += resent as u64;
        resent
    }
}

/// The picture `attachment` names, as the command carries it (base64), if `doc` holds it already.
fn held_picture(doc: &Document, attachment: &Attachment) -> Option<String> {
    doc.media
        .values()
        .filter(|bytes| (bytes.len().div_ceil(3) as u64).saturating_mul(4) == attachment.size)
        .map(|bytes| crate::cmd::insert::base64_encode(bytes))
        .find(|base64| craft_signal::blobs::hash_of(base64.as_bytes()) == attachment.hash)
}

fn signal_error(e: SignalError) -> CmdError {
    match e {
        SignalError::NotConnected => CmdError::Disabled(e.to_string()),
        SignalError::Params(p) => CmdError::Params(p),
        other => CmdError::Failed(other.to_string()),
    }
}
