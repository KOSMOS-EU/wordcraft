//! Craft Signal commands (kosmos): the session's seat in a collaboration over a relay
//! (`opencraft/SIGNAL.md`, `opencraft/relay`). `signal.connect` takes a ticket and joins; from
//! then on every edit is proposed, the master's commits are applied each frame
//! (`Session::signal_pump`, or `signal.poll` on demand).

use craft_signal::ConnectRequest;
use craft_signal::wire::Mode;
use serde_json::{Value, json};

use crate::{CmdError, CmdResult, CommandSpec, Session, p};

const SIGNAL: &str = "Review › Collaborate";

/// This engine's version, the session type at the relay (same engine, same commands).
pub const APP_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "-kosmos");

pub fn specs() -> Vec<CommandSpec> {
    vec![
        CommandSpec::new("signal.connect", "Join a collaboration", SIGNAL, connect)
            .params(r#"{"relay": "http://127.0.0.1:8787" | "https://cloud.example.org", "doc": document id (space!file), "auth"?: Authorization value ("Bearer …"; default "Bearer dev"), "user"?: display name (default the author), "mode"?: "edit" | "readOnly" (default edit), "app"?: "wordcraft"} → {"connecting": true}; signal.status says when joined"#)
            .pure(),
        CommandSpec::new("signal.disconnect", "Leave the collaboration", SIGNAL, disconnect).pure(),
        CommandSpec::new("signal.status", "Collaboration status", SIGNAL, status)
            .params(r#"{} → {"state": disconnected|ticketing|opening|greeting|joined, "id", "mode", "rank", "master", "isMaster", "seq", "pending", "members": [...], "applied", "confirmed", "rejected"}"#)
            .pure(),
        CommandSpec::new("signal.mode", "Collaborate or read only", SIGNAL, mode).params(r#"{"value": "edit" | "readOnly"} → {"requested": mode}; the answer arrives as an event"#).pure(),
        CommandSpec::new("signal.poll", "Collaboration events", SIGNAL, poll)
            .params(r#"{} → {"events": [{"event": joined|mode|roster|master|applied|confirmed|rejected|committed|closed|error, ...} ...]} (applied already)"#)
            .pure(),
        CommandSpec::new("signal.saved", "Tell the collaboration the document is saved", SIGNAL, saved).params(r#"{} → {"seq"}; only the master"#).pure(),
    ]
}

fn require(v: &Value, key: &str) -> Result<String, CmdError> {
    p::str(v, key).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).ok_or_else(|| CmdError::Params(format!("`{key}` is required")))
}

fn connect(s: &mut Session, v: &Value) -> CmdResult {
    let relay = require(v, "relay")?;
    if !(relay.starts_with("http://") || relay.starts_with("https://") || relay.starts_with("ws://") || relay.starts_with("wss://")) {
        return Err(CmdError::Params("`relay` is an http(s) or ws(s) URL".into()));
    }
    let doc = require(v, "doc")?;
    let auth = p::str(v, "auth").map(str::trim).filter(|a| !a.is_empty()).unwrap_or("Bearer dev").to_string();
    let name = p::str(v, "user").map(str::trim).filter(|u| !u.is_empty()).map(str::to_string).unwrap_or_else(|| s.author.clone());
    let mode = match p::str(v, "mode") {
        None => Mode::Edit,
        Some(m) => Mode::parse(m).ok_or_else(|| CmdError::Params("`mode` is edit or readOnly".into()))?,
    };
    let app = p::str(v, "app").map(str::trim).filter(|a| !a.is_empty()).unwrap_or("wordcraft").to_string();
    s.signal_connect(ConnectRequest { relay, auth, doc, app, app_version: APP_VERSION.to_string(), mode, name })?;
    Ok(json!({"connecting": true, "appVersion": APP_VERSION}))
}

fn disconnect(s: &mut Session, _v: &Value) -> CmdResult {
    s.signal.port_mut()?.disconnect();
    s.signal_pump();
    Ok(json!({"connected": false}))
}

fn status(s: &mut Session, _v: &Value) -> CmdResult {
    s.signal_pump();
    Ok(s.signal.status())
}

fn mode(s: &mut Session, v: &Value) -> CmdResult {
    let wanted = Mode::parse(&require(v, "value")?).ok_or_else(|| CmdError::Params("`value` is edit or readOnly".into()))?;
    s.signal.port_mut()?.set_mode(wanted).map_err(|e| CmdError::Failed(e.to_string()))?;
    Ok(json!({"requested": wanted.name()}))
}

fn poll(s: &mut Session, _v: &Value) -> CmdResult {
    s.signal_pump();
    Ok(json!({"events": s.signal.take_log(), "state": s.signal.status()["state"]}))
}

fn saved(s: &mut Session, _v: &Value) -> CmdResult {
    let port = s.signal.port_mut()?;
    port.saved().map_err(|e| CmdError::Failed(e.to_string()))?;
    Ok(json!({"seq": port.seq()}))
}
