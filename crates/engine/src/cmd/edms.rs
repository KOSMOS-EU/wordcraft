//! EDMS and Craft Signal commands (kosmos): the document's home in OpenCore/OpenCloud and its
//! collaboration, driven by the same ids from the ribbon, the page API, the control channel and MCP. The
//! OpenCore kit (`craft-opencore`, `session` and `session::collab`) runs them with their labels and
//! parameters. See `opencraft/EDMS_NATIVE.md` §9 and `opencraft/SIGNAL.md`.

use craft_opencore::session::{self, collab};
use serde_json::Value;

use crate::edms::to_cmd;
use crate::{CmdError, CmdResult, CommandSpec, Session};

const EDMS: &str = "File › EDMS";
const SIGNAL: &str = "Review › Collaborate";

pub fn specs() -> Vec<CommandSpec> {
    // Each command of the kit, run by the kit.
    macro_rules! kit {
        ($($id:literal),* $(,)?) => {
            vec![$(spec($id, |s, v| run(s, $id, v))),*]
        };
    }
    kit![
        "edms.connect",
        "edms.disconnect",
        "edms.status",
        "edms.poll",
        "edms.me",
        "edms.drives",
        "edms.list",
        "edms.stat",
        "edms.search",
        "edms.open",
        "edms.fetch",
        "edms.pwd",
        "edms.sidecar",
        "edms.runWith",
        "edms.adopt",
        "edms.save",
        "edms.autosave",
        "edms.saveAs",
        "edms.export",
        "edms.reload",
        "edms.lock",
        "edms.unlock",
        "edms.event",
        "signal.connect",
        "signal.join",
        "signal.accept",
        "signal.decline",
        "signal.auto",
        "signal.disconnect",
        "signal.status",
        "signal.mode",
        "signal.poll",
        "signal.saved",
    ]
}

/// The command `id` with the kit's label and parameters.
fn spec(id: &'static str, run: fn(&mut Session, &Value) -> CmdResult) -> CommandSpec {
    let (label, params, needs_document) = collab::command(id).map_or((id, "{}", false), |known| (known.label, known.params, known.needs_document));
    let group = if id.starts_with("signal.") { SIGNAL } else { EDMS };
    let spec = CommandSpec::new(id, label, group, run).params(params).pure();
    if needs_document { spec.when(needs_edms_document) } else { spec }
}

fn run(s: &mut Session, id: &str, v: &Value) -> CmdResult {
    collab::execute(s, id, v).map_err(to_cmd)
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
