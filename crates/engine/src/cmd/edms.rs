//! EDMS commands (kosmos): the document's home in OpenCore/OpenCloud, driven by the same ids from
//! the ribbon, the page API, the control channel and MCP. The OpenCore kit (`craft-opencore`, area
//! `session`) runs them; WordCraft adds what a collaboration needs before a save (ask the master, refuse
//! while content is still on its way). See `opencraft/EDMS_NATIVE.md` §9.

use craft_opencore::session;
use serde_json::{Value, json};

use crate::edms::to_cmd;
use crate::{CmdError, CmdResult, CommandSpec, Session, p};

const EDMS: &str = "File › EDMS";

const SAVE_PARAMS: &str = r#"{"incomplete"?: true (save although the collaboration still owes content: what is only a preview is saved as the preview)} → {"pending": id}; result {"saved": true, "etag"} or an error with code conflict (the server has a newer version: edms.reload or edms.saveAs). In a collaboration with content still on its way the command refuses without "incomplete" and names what is missing."#;
const SAVE_AS_PARAMS: &str = r#"{"drive", "path": "/Folder", "name": "Copy.docx", "overwrite"?: false, "switch"?: true (the document lives at the copy from now on)} → {"pending": id, "incomplete"?: [{"command", "key", "size", "seq"}] (in a collaboration: content still on its way, the copy holds its preview)}; result {"saved": true, "drive", "path", "etag"}"#;

pub fn specs() -> Vec<CommandSpec> {
    // Each command of the kit, run by the kit.
    macro_rules! kit {
        ($id:literal) => {
            spec($id, |s, v| run(s, $id, v))
        };
    }
    vec![
        kit!("edms.connect"),
        kit!("edms.disconnect"),
        kit!("edms.status"),
        kit!("edms.poll"),
        kit!("edms.me"),
        kit!("edms.drives"),
        kit!("edms.list"),
        kit!("edms.stat"),
        kit!("edms.search"),
        kit!("edms.open"),
        kit!("edms.fetch"),
        kit!("edms.pwd"),
        kit!("edms.sidecar"),
        kit!("edms.runWith"),
        kit!("edms.adopt"),
        spec("edms.save", save).params(SAVE_PARAMS),
        spec("edms.saveAs", save_as).params(SAVE_AS_PARAMS),
        kit!("edms.export"),
        kit!("edms.reload"),
        kit!("edms.lock"),
        kit!("edms.unlock"),
        kit!("edms.event"),
    ]
}

/// The command `id` with the kit's label and parameters.
fn spec(id: &'static str, run: fn(&mut Session, &Value) -> CmdResult) -> CommandSpec {
    let (label, params, needs_document) = session::command(id).map_or((id, "{}", false), |known| (known.label, known.params, known.needs_document));
    let spec = CommandSpec::new(id, label, EDMS, run).params(params).pure();
    if needs_document { spec.when(needs_edms_document) } else { spec }
}

fn run(s: &mut Session, id: &str, v: &Value) -> CmdResult {
    session::execute(s, id, v).map_err(to_cmd)
}

fn needs_edms_document(s: &Session) -> Option<&'static str> {
    session::needs_document(s, s.document_id())
}

/// The document's bytes in the format its name says, with the save stamps applied (modified,
/// revision). Mirrors `file.save` without the file system.
pub(crate) fn document_bytes(s: &mut Session, name: &str) -> Result<Vec<u8>, CmdError> {
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
    let answer = session::save(s).map_err(to_cmd)?;
    // The master reports the version to the session once it is through (`SAVED`).
    s.signal.begin_save();
    Ok(answer)
}

fn save_as(s: &mut Session, v: &Value) -> CmdResult {
    let mut answer = session::save_as(s, v).map_err(to_cmd)?;
    // A copy out of a collaboration holds what is here now, and says what is not.
    let missing = incomplete(s);
    if !missing.is_empty() {
        s.push_ui_request(json!({"edms": {"event": "savedIncomplete", "missing": missing}}));
        answer["incomplete"] = json!(missing);
    }
    Ok(answer)
}
