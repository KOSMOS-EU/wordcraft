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

use craft_signal::wire::Mode;
use craft_signal::{ConnectRequest, Incoming, OpPayload, Port, SignalError};
use serde_json::{Value, json};
use wordcraft_doc::{Document, Path, StoryRef};

use crate::snapshot::EditSnapshot;
use crate::{CmdError, Selection, Session};

/// Most events kept for `signal.poll` when nobody drains them.
pub const MAX_LOG: usize = 64;
/// Most commits the master keeps for the conflict rule; a proposal based further back is rejected.
pub const MAX_TRACES: usize = 1024;
/// The reason the client gives a proposal that a master change returned.
const MASTER_CHANGED: &str = "master changed";

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
        let op = OpPayload { command: id.to_string(), params, anchor: anchor.clone(), time: 0, author: 0 };
        let master = port.is_master();
        let me = port.id();
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
            Some(port) => port.poll(),
            None => return,
        };
        for item in incoming {
            self.signal_apply(item);
        }
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
                let resent = self.signal_resend_pending();
                let entry = json!({"event": "master", "master": master, "me": me, "lock": !lock.is_empty(), "resent": resent});
                self.signal.note(entry.clone());
                self.push_ui_request(json!({"signal": entry}));
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
                self.signal.note(json!({"event": "syncRequested", "sender": sender, "base": base}));
            }
            Incoming::Sync { .. } | Incoming::Presence { .. } => {}
            Incoming::Saved { seq } => {
                self.signal.note(json!({"event": "saved", "seq": seq}));
            }
            Incoming::Closed { reason } => {
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
        let mine = self.sel.clone();
        let theirs: Option<Selection> = serde_json::from_value(op.anchor.clone()).ok();
        self.signal.applying = true;
        if let Some(sel) = theirs {
            self.sel = sel;
            self.clamp_selection();
        }
        let result = self.run(&op.command, &op.params);
        self.signal.applying = false;
        self.sel = mine;
        self.clamp_selection();
        result.map(|_| ()).map_err(|e| e.to_string())
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
            match self.run(&op.command, &op.params) {
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

fn signal_error(e: SignalError) -> CmdError {
    match e {
        SignalError::NotConnected => CmdError::Disabled(e.to_string()),
        SignalError::Params(p) => CmdError::Params(p),
        other => CmdError::Failed(other.to_string()),
    }
}
