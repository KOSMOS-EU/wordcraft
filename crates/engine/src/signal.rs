//! Craft Signal in the session (kosmos, `opencraft/SIGNAL.md`): the seat of this document in a
//! collaboration. The shell installs a [`craft_signal::Port`] (a `Client` over its transport);
//! every edit of the document goes out as a proposal ([`Session::signal_record`], called from
//! `Session::run`), what the master confirms comes back and is applied ([`Session::signal_pump`],
//! once per frame), and as the master this session applies the others' proposals and confirms
//! them. Nothing here blocks; answers arrive in later frames.
//!
//! First stage: every proposal the master receives is accepted (no conflict rule yet), a rejected
//! own command is reported, not rolled back, and the clock and author of a commit are the
//! master's (determinism of the engine is the next step). Undo stays local.

use craft_signal::wire::Mode;
use craft_signal::{ConnectRequest, Incoming, OpPayload, Port, SignalError};
use serde_json::{Value, json};

use crate::{CmdError, Selection, Session};

/// Most events kept for `signal.poll` when nobody drains them.
pub const MAX_LOG: usize = 64;

/// The collaboration state of a session.
#[derive(Default)]
pub struct SignalState {
    port: Option<Box<dyn Port>>,
    /// A remote command is being applied: `run` must not propose it again.
    applying: bool,
    /// What happened since the last `signal.poll`, oldest first.
    log: Vec<Value>,
    pub applied: u64,
    pub confirmed: u64,
    pub rejected: u64,
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

impl Session {
    /// Connect to a relay (`signal.connect`).
    pub fn signal_connect(&mut self, request: ConnectRequest) -> Result<(), CmdError> {
        self.signal.port_mut()?.connect(request).map_err(signal_error)
    }

    /// An edit of this document just happened: propose it to the collaboration. Called from
    /// `Session::run` for every mutating command that changed something.
    pub(crate) fn signal_record(&mut self, id: &str, params: &Value, sel_before: &Selection) {
        if self.signal.applying || !transmits(id) {
            return;
        }
        let Some(port) = self.signal.port.as_mut() else { return };
        if !port.joined() || port.mode() != Mode::Edit {
            return;
        }
        let mut params = params.clone();
        if let Some(m) = params.as_object_mut() {
            m.remove("password");
        }
        let anchor = serde_json::to_value(sel_before).unwrap_or(Value::Null);
        let op = OpPayload { command: id.to_string(), params, anchor, time: 0, author: 0 };
        if let Err(e) = port.propose(op) {
            let message = format!("{id}: {e}");
            self.signal.note(json!({"event": "error", "message": message}));
            self.push_ui_request(json!({"signal": {"event": "error", "message": format!("{id}: {e}")}}));
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
                let entry = json!({"event": "master", "master": master, "me": me, "lock": !lock.is_empty()});
                self.signal.note(entry.clone());
                self.push_ui_request(json!({"signal": entry}));
            }
            Incoming::Commit { seq, sender, local, op } => match self.signal_run_remote(&op) {
                Ok(()) => {
                    self.signal.applied += 1;
                    self.signal.note(json!({"event": "applied", "seq": seq, "sender": sender, "local": local, "command": op.command}));
                }
                Err(e) => {
                    self.signal.note(json!({"event": "applyFailed", "seq": seq, "sender": sender, "command": op.command, "error": e}));
                    self.push_ui_request(json!({"signal": {"event": "applyFailed", "seq": seq, "command": op.command, "error": e}}));
                }
            },
            Incoming::Confirmed { local, seq } => {
                self.signal.confirmed += 1;
                self.signal.note(json!({"event": "confirmed", "local": local, "seq": seq}));
            }
            Incoming::Rejected { local, op, payload } => {
                self.signal.rejected += 1;
                let reason = String::from_utf8_lossy(&payload).into_owned();
                let command = op.as_ref().map(|o| o.command.clone()).unwrap_or_default();
                self.signal.note(json!({"event": "rejected", "local": local, "command": command, "reason": reason}));
                self.push_ui_request(json!({"signal": {"event": "rejected", "local": local, "command": command, "reason": reason}}));
            }
            Incoming::Proposed { sender, local, base, op } => {
                let applied = self.signal_run_remote(&op);
                let Some(port) = self.signal.port.as_mut() else { return };
                match applied {
                    Ok(()) => match port.commit(sender, local, op.clone(), now_ms()) {
                        Ok(seq) => {
                            self.signal.applied += 1;
                            self.signal.note(
                                json!({"event": "committed", "seq": seq, "sender": sender, "local": local, "base": base, "command": op.command}),
                            );
                        }
                        Err(e) => self.signal.note(json!({"event": "error", "message": format!("commit: {e}")})),
                    },
                    Err(e) => {
                        let _ = port.reject(sender, local, &e);
                        self.signal.note(json!({"event": "rejectedProposal", "sender": sender, "local": local, "command": op.command, "reason": e}));
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
}

fn signal_error(e: SignalError) -> CmdError {
    match e {
        SignalError::NotConnected => CmdError::Disabled(e.to_string()),
        SignalError::Params(p) => CmdError::Params(p),
        other => CmdError::Failed(other.to_string()),
    }
}
