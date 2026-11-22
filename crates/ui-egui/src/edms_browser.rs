//! kosmos: the EDMS location picker. Open, Save As, Export, Insert Picture and recipient lists
//! from the document's home in OpenCore/OpenCloud, through the `edms.*` commands, beside the
//! system's file dialog (or instead of it, when the host allows no local files: `Embed::local_files`).
//!
//! Left the spaces (`edms.drives`), right the folder (`edms.list`) with Up, a file name field
//! and the file types, then `edms.open`, `edms.fetch`, `edms.saveAs` or `edms.export`. Nothing
//! here waits: every answer arrives on a later frame through [`poll`], which takes the picker's
//! own results by id and leaves the rest for `edms.poll`. The picked file goes to the same
//! follow-up as the system dialog's ([`AfterPick`]).

use egui::{Align2, Rect, Sense, Ui, pos2, vec2};
use serde_json::{Value, json};

use crate::file_dialogs::{AfterPick, FileDialogRequest, open_filters};
use crate::theme::{Tokens, regular};
use crate::{WordApp, icons};

const ROW: f32 = 24.0;
/// At most this many entries are listed; a bigger folder shows the first ones and says so.
const MAX_LISTED: usize = 5000;

/// The picker while it is open, and the place it showed last (where it opens next time).
#[derive(Default)]
pub(crate) struct State {
    pub(crate) open: Option<Box<EdmsBrowser>>,
    last: Option<(String, String)>,
    /// File › Export: a copy goes into the EDMS rather than to this computer. `None` follows the
    /// document: into the EDMS when it lives there.
    pub(crate) export_to_edms: Option<bool>,
}

/// A space the user can browse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Drive {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) kind: String,
}

/// One subfolder or file in the listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) name: String,
    /// The path in the drive (`/Folder/name.docx`).
    pub(crate) path: String,
    pub(crate) folder: bool,
}

/// A message under the listing.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Note {
    None,
    NotFound(String),
    Replace(String),
    Error(String),
}

/// What the picker is waiting for from the EDMS.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Waiting {
    Drives,
    Listing {
        drive: usize,
        path: String,
    },
    /// The pick itself: open, fetch, save or export.
    Pick,
}

/// What OK (or Enter, or a double click) did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// The picker stays open (it moved to a folder, is loading, or has something to say).
    Stay,
    /// The command to run for the pick.
    Pick { command: &'static str, params: Value },
}

/// The open picker.
pub(crate) struct EdmsBrowser {
    req: FileDialogRequest,
    pub(crate) after: AfterPick,
    pub(crate) drives: Vec<Drive>,
    /// The space shown, an index into `drives`.
    pub(crate) drive: Option<usize>,
    /// The folder shown, in the drive (`/`, `/Berichte`).
    pub(crate) path: String,
    pub(crate) entries: Vec<Entry>,
    truncated: bool,
    /// The highlighted entry, an index into `entries`.
    selected: Option<usize>,
    /// The file name field.
    pub(crate) name: String,
    /// The file types, as (label, extensions).
    filters: Vec<(String, Vec<String>)>,
    filter: usize,
    waiting: Option<(u64, Waiting)>,
    note: Note,
    focus_name: bool,
}

/// Whether `name` has one of `exts` (case-insensitive); `*` takes any file.
fn matches(name: &str, exts: &[String]) -> bool {
    exts.iter().any(|e| e == "*") || name.rsplit_once('.').is_some_and(|(_, e)| exts.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

/// `folder` + `name` as a path in the drive.
fn join(folder: &str, name: &str) -> String {
    let folder = folder.trim_end_matches('/');
    format!("{folder}/{name}")
}

/// The folder above `path`; the root stays the root.
fn parent(path: &str) -> String {
    match path.trim_end_matches('/').rsplit_once('/') {
        Some((above, _)) if !above.is_empty() => above.to_string(),
        _ => "/".to_string(),
    }
}

/// The spaces of `edms.drives`' result: personal first, then projects, then the rest; the
/// virtual ones (the shares jail) are left out.
fn parse_drives(result: &Value) -> Vec<Drive> {
    let mut drives: Vec<Drive> = result
        .get("drives")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|d| {
                    let id = d.get("id").and_then(Value::as_str)?.to_string();
                    let kind = d.get("type").and_then(Value::as_str).unwrap_or("").to_string();
                    if kind == "virtual" {
                        return None;
                    }
                    let name = d.get("name").and_then(Value::as_str).filter(|n| !n.is_empty()).unwrap_or(id.as_str()).to_string();
                    Some(Drive { id, name, kind })
                })
                .collect()
        })
        .unwrap_or_default();
    let rank = |k: &str| match k {
        "personal" => 0,
        "project" => 1,
        _ => 2,
    };
    drives.sort_by_cached_key(|d| (rank(&d.kind), d.name.to_lowercase()));
    drives
}

/// The entries of `edms.list`'s result: folders first, then the files of the chosen types, each
/// by name; at most `cap`.
fn parse_entries(result: &Value, exts: &[String], cap: usize) -> (Vec<Entry>, bool) {
    let mut entries: Vec<Entry> = result
        .get("entries")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|e| {
                    let name = e.get("name").and_then(Value::as_str).filter(|n| !n.is_empty())?.to_string();
                    let folder = e.get("folder").and_then(Value::as_bool).unwrap_or(false);
                    if !folder && !matches(&name, exts) {
                        return None;
                    }
                    let path = e.get("path").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| format!("/{name}"));
                    Some(Entry { name, path, folder })
                })
                .collect()
        })
        .unwrap_or_default();
    entries.sort_by_cached_key(|e| (!e.folder, e.name.to_lowercase()));
    let truncated = entries.len() > cap;
    entries.truncate(cap);
    (entries, truncated)
}

impl EdmsBrowser {
    pub(crate) fn new(req: FileDialogRequest, after: AfterPick) -> Self {
        let (name, filters) = match &req {
            FileDialogRequest::Open { purpose } | FileDialogRequest::OpenMany { purpose } => {
                (String::new(), open_filters(purpose).iter().map(|(l, e)| ((*l).to_string(), e.iter().map(|x| (*x).to_string()).collect())).collect())
            }
            FileDialogRequest::Save { name, .. } => {
                let ext = name.rsplit_once('.').map(|(_, e)| e).filter(|e| !e.is_empty());
                let mut f: Vec<(String, Vec<String>)> = ext.map(|e| (format!("*.{e}"), vec![e.to_string()])).into_iter().collect();
                f.push(("All files".into(), vec!["*".into()]));
                (name.clone(), f)
            }
        };
        EdmsBrowser {
            req,
            after,
            drives: Vec::new(),
            drive: None,
            path: "/".into(),
            entries: Vec::new(),
            truncated: false,
            selected: None,
            name,
            filters,
            filter: 0,
            waiting: None,
            note: Note::None,
            focus_name: true,
        }
    }

    fn exts(&self) -> &[String] {
        self.filters.get(self.filter).map_or(&[], |(_, e)| e.as_slice())
    }

    fn saving(&self) -> bool {
        matches!(self.req, FileDialogRequest::Save { .. })
    }

    fn title(&self) -> &'static str {
        match &self.after {
            AfterPick::Picture => "Insert Picture from the EDMS",
            AfterPick::Recipients => "Select Recipients from the EDMS",
            AfterPick::Export { .. } => "Export to the EDMS",
            AfterPick::SaveAs { .. } => "Save As in the EDMS",
            _ => "Open from the EDMS",
        }
    }

    fn ok_label(&self) -> &'static str {
        match &self.after {
            AfterPick::Picture => "Insert",
            AfterPick::Export { .. } => "Export",
            AfterPick::SaveAs { .. } => "Save",
            _ => "Open",
        }
    }

    fn drive_id(&self) -> Option<&str> {
        self.drive.and_then(|i| self.drives.get(i)).map(|d| d.id.as_str())
    }

    /// The listing is being fetched, or the pick is under way.
    pub(crate) fn busy(&self) -> bool {
        self.waiting.is_some()
    }

    /// The highlighted entry, if it is a folder.
    fn selected_folder(&self) -> Option<&Entry> {
        self.selected.and_then(|i| self.entries.get(i)).filter(|e| e.folder)
    }

    /// A click on an entry: highlight it, and a file's name goes into the file name field.
    fn select(&mut self, i: usize) {
        self.selected = Some(i);
        if let Some(e) = self.entries.get(i).filter(|e| !e.folder) {
            self.name = e.name.clone();
        }
        self.note = Note::None;
    }

    /// OK: a highlighted folder, or a typed folder, is opened; otherwise the typed name is the
    /// pick. Save adds the chosen type's extension to a name without one and asks once more
    /// before replacing a file; Open wants a file that is listed.
    pub(crate) fn accept(&mut self) -> Step {
        if self.busy() {
            return Step::Stay;
        }
        if let Some(folder) = self.selected_folder().map(|e| e.path.clone()) {
            return self.go(folder);
        }
        let Some(drive) = self.drive_id().map(str::to_string) else { return Step::Stay };
        let typed = self.name.trim().to_string();
        if typed.is_empty() {
            return Step::Stay;
        }
        if let Some(folder) = self.entries.iter().find(|e| e.folder && e.name == typed).map(|e| e.path.clone()) {
            self.name.clear();
            return self.go(folder);
        }
        let mut name = typed;
        if self.saving() {
            if !name.contains('.')
                && let Some(ext) = self.exts().first().filter(|e| e.as_str() != "*")
            {
                name = format!("{name}.{ext}");
            }
            // Replacing takes a second Save on the same name: the listing says it exists, or the
            // server said so (conflict) to the first attempt.
            let confirmed = matches!(&self.note, Note::Replace(n) if *n == name);
            let exists = self.entries.iter().any(|e| !e.folder && e.name == name);
            if exists && !confirmed {
                self.note = Note::Replace(name);
                return Step::Stay;
            }
            let (command, params) = match &self.after {
                AfterPick::Export { .. } => ("edms.export", json!({"drive": drive, "path": self.path, "name": name, "overwrite": confirmed})),
                _ => ("edms.saveAs", json!({"drive": drive, "path": self.path, "name": name, "overwrite": confirmed, "switch": true})),
            };
            return Step::Pick { command, params };
        }
        if !self.entries.iter().any(|e| !e.folder && e.name == name) {
            self.note = Note::NotFound(name);
            return Step::Stay;
        }
        let path = join(&self.path, &name);
        let command = match &self.after {
            AfterPick::Picture | AfterPick::Recipients => "edms.fetch",
            _ => "edms.open",
        };
        Step::Pick { command, params: json!({"drive": drive, "path": path}) }
    }

    /// Move to `folder` in the shown drive: the listing is asked for.
    fn go(&mut self, folder: String) -> Step {
        self.path = folder;
        self.selected = None;
        self.note = Note::None;
        self.entries.clear();
        Step::Stay
    }
}

/// Whether the picker can finish this follow-up; the others (fonts, Word sources, a path for
/// another dialog, Compare) stay with local files.
pub(crate) fn supports(after: &AfterPick) -> bool {
    matches!(after, AfterPick::OpenDocument { .. } | AfterPick::Picture | AfterPick::Recipients | AfterPick::SaveAs { .. } | AfterPick::Export { .. })
}

impl WordApp {
    /// The EDMS is there to browse: the shell installed a transport and the host connected it.
    pub fn edms_available(&self) -> bool {
        self.session.edms.connected()
    }

    /// Whether the open document lives in the EDMS.
    pub fn document_in_edms(&self) -> bool {
        self.session.edms.document(self.session.document_id()).is_some()
    }

    /// File › Export (and Print › Save as PDF) go into the EDMS: always without local files,
    /// otherwise as the Export page's switch says, which follows the document by default.
    pub(crate) fn export_to_edms(&self) -> bool {
        if !self.embed.local_files {
            return self.edms_available();
        }
        self.edms_available() && self.edms_browser.export_to_edms.unwrap_or_else(|| self.document_in_edms())
    }

    /// Ask for a place in the EDMS and do `after` with the answer when the picker closes. One
    /// picker at a time; a follow-up the picker can't finish is refused with a status line.
    pub(crate) fn show_edms_browser(&mut self, req: FileDialogRequest, after: AfterPick) -> bool {
        if self.file_dialog_open() {
            self.status(tl!("A file dialog is already open."));
            return false;
        }
        if !self.edms_available() {
            self.status(tl!("There is no EDMS connection."));
            return false;
        }
        if !supports(&after) {
            self.status(tl!("This can't be picked from the EDMS."));
            return false;
        }
        let mut b = Box::new(EdmsBrowser::new(req, after));
        match self.session.run("edms.drives", &json!({})) {
            Ok(v) => b.waiting = v.get("pending").and_then(Value::as_u64).map(|id| (id, Waiting::Drives)),
            Err(e) => b.note = Note::Error(e.to_string()),
        }
        self.edms_browser.open = Some(b);
        true
    }

    /// `ui.edmsBrowser`: the picker for `purpose` (document, picture, recipients, save, or an
    /// export format such as `pdf`); true when it opened.
    pub(crate) fn browse_edms(&mut self, purpose: &str) -> bool {
        let (req, after) = match purpose {
            "picture" => (FileDialogRequest::Open { purpose: "picture".into() }, AfterPick::Picture),
            "recipients" => (FileDialogRequest::Open { purpose: "recipients".into() }, AfterPick::Recipients),
            "save" => (
                FileDialogRequest::Save { name: self.default_save_name(), dir: None },
                AfterPick::SaveAs { after: crate::file_dialogs::AfterSave::Nothing, document: self.session.document_id() },
            ),
            "document" | "" => {
                (FileDialogRequest::Open { purpose: "document".into() }, AfterPick::OpenDocument { document: self.session.document_id() })
            }
            ext => {
                let file_ext = if ext == crate::print_page::EXPORT_WORD_2003 { "xml" } else { ext };
                (
                    FileDialogRequest::Save { name: format!("{}.{file_ext}", self.export_stem()), dir: None },
                    AfterPick::Export { ext: ext.to_string() },
                )
            }
        };
        self.show_edms_browser(req, after)
    }

    /// The document's name without its extension, for a copy's name: a document from the EDMS
    /// (or the host page) has a file name and no path, and `title_stem` keeps that name whole.
    fn export_stem(&self) -> String {
        match (&self.session.path, &self.session.file_name) {
            (None, Some(name)) => std::path::Path::new(name)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| self.title_stem()),
            _ => self.title_stem(),
        }
    }

    /// Fetch the listing of the picker's folder (after a move, or when the drives arrived).
    fn edms_browser_list(&mut self) {
        let Some(b) = self.edms_browser.open.as_mut() else { return };
        let Some(drive) = b.drive else { return };
        let Some(id) = b.drives.get(drive).map(|d| d.id.clone()) else { return };
        let path = b.path.clone();
        match self.session.run("edms.list", &json!({"drive": id, "path": path})) {
            Ok(v) => b.waiting = v.get("pending").and_then(Value::as_u64).map(|id| (id, Waiting::Listing { drive, path: b.path.clone() })),
            Err(e) => b.note = Note::Error(e.to_string()),
        }
    }

    /// Close the picker without a pick and finish what it was for as cancelled.
    pub(crate) fn cancel_edms_browser(&mut self) {
        let Some(b) = self.edms_browser.open.take() else { return };
        if let Some(d) = b.drive.and_then(|i| b.drives.get(i)) {
            self.edms_browser.last = Some((d.id.clone(), b.path.clone()));
        }
        if let Err(e) = self.finish_pick(b.after, None) {
            log::warn!("edms browser: {e}");
        }
        if !self.modal_open() {
            self.canvas.want_focus = true;
        }
    }

    /// The pick's answer came back: the document is open or saved, the bytes are placed, or the
    /// picker says what went wrong and stays.
    fn edms_browser_picked(&mut self, result: Value) {
        let Some(mut b) = self.edms_browser.open.take() else { return };
        if result.get("ok").and_then(Value::as_bool) != Some(true) {
            let code = result.pointer("/error/code").and_then(Value::as_str).unwrap_or("");
            let message = result.pointer("/error/message").and_then(Value::as_str).unwrap_or("the EDMS didn't answer").to_string();
            b.note = match code {
                "conflict" if b.saving() => Note::Replace(result.get("name").and_then(Value::as_str).unwrap_or(b.name.trim()).to_string()),
                "notFound" => Note::NotFound(b.name.trim().to_string()),
                _ => Note::Error(message),
            };
            self.edms_browser.open = Some(b);
            return;
        }
        if let Some(d) = b.drive.and_then(|i| b.drives.get(i)) {
            self.edms_browser.last = Some((d.id.clone(), b.path.clone()));
        }
        let name = result.get("name").and_then(Value::as_str).unwrap_or(b.name.trim()).to_string();
        match &b.after {
            AfterPick::Picture | AfterPick::Recipients => {
                let bytes = result.get("data").and_then(Value::as_str).and_then(wordcraft_engine::cmd::insert::base64_decode).unwrap_or_default();
                // A text file asked for as a recipient list loads as one, as a picked one does.
                self.recipient_list_pending = matches!(b.after, AfterPick::Recipients);
                self.file_arrived(&name, bytes, false);
                self.recipient_list_pending = false;
            }
            AfterPick::SaveAs { .. } => {
                self.ui.backstage = false;
                if let Some(ctx) = self.ctx.clone() {
                    crate::backstage::forget_save_as_error(&ctx);
                }
                self.status(crate::i18n::fmt(tl!("Saved {name} in the EDMS"), &[("name", &name)]));
            }
            AfterPick::Export { .. } => {
                self.ui.backstage = false;
                self.status(crate::i18n::fmt(tl!("Exported {name} to the EDMS"), &[("name", &name)]));
            }
            _ => {
                // `edms.open` opened the document already.
                self.ui.backstage = false;
            }
        }
        if !self.modal_open() {
            self.canvas.want_focus = true;
        }
    }
}

/// Pick up the picker's answers (every frame, from `logic`, after `edms_pump`).
pub(crate) fn poll(app: &mut WordApp) {
    let Some(b) = app.edms_browser.open.as_mut() else { return };
    let Some((id, what)) = b.waiting.clone() else { return };
    let Some(result) = app.session.edms.take_result(id) else { return };
    b.waiting = None;
    let ok = result.get("ok").and_then(Value::as_bool) == Some(true);
    match what {
        Waiting::Drives => {
            if !ok {
                b.note = Note::Error(result.pointer("/error/message").and_then(Value::as_str).unwrap_or("the EDMS didn't answer").to_string());
                return;
            }
            b.drives = parse_drives(&result);
            // Where it was last, else the document's own space, else the first (personal) space.
            let last = app.edms_browser.last.clone();
            let own = app.session.edms.document(app.session.document_id()).map(|d| (d.location.drive_id.clone(), parent(&d.location.path)));
            let start = last.or(own).and_then(|(drive, path)| b.drives.iter().position(|d| d.id == drive).map(|i| (i, path)));
            let (drive, path) = start.unwrap_or((0, "/".into()));
            if b.drives.is_empty() {
                b.note = Note::Error(tl!("No spaces to browse.").to_string());
                return;
            }
            b.drive = Some(drive);
            b.path = path;
            app.edms_browser_list();
        }
        Waiting::Listing { drive, path } => {
            // A listing for a folder the user has since left is dropped.
            if b.drive != Some(drive) || b.path != path {
                app.edms_browser_list();
                return;
            }
            if !ok {
                b.note = Note::Error(result.pointer("/error/message").and_then(Value::as_str).unwrap_or("this folder can't be read").to_string());
                return;
            }
            let (entries, truncated) = parse_entries(&result, b.exts(), MAX_LISTED);
            b.entries = entries;
            b.truncated = truncated;
        }
        Waiting::Pick => app.edms_browser_picked(result),
    }
}

/// Draw the picker, if it is open.
pub(crate) fn show(app: &mut WordApp, ctx: &egui::Context) {
    let Some(mut b) = app.edms_browser.open.take() else { return };
    let t = Tokens::get(ctx);
    let mut done: Option<Done> = None;
    let id = egui::Id::unique("edms_browser");
    let window = egui::Window::new(tl!(b.title())).id(id).collapsible(false).resizable(false).anchor(Align2::CENTER_CENTER, vec2(0.0, -20.0));
    let fit = Fit::of(ctx.content_rect());
    let shown = crate::window_close::window(ctx, window).show(ctx, |ui| {
        ui.set_width(fit.width);
        done = crate::rtl::root(ui, |ui| body(ui, &mut b, &t, &fit));
    });
    if let Some(s) = &shown {
        if crate::window_close::show(ctx, &s.response, &t) {
            done = Some(Done::Cancel);
        }
        if done.is_none() {
            crate::dialogs::block_behind(app, ctx, &s.response);
        }
    }
    if b.busy() {
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }
    app.edms_browser.open = Some(b);
    match done {
        Some(Done::Cancel) => {
            crate::dialogs::forget_nudge(ctx, id);
            app.cancel_edms_browser();
        }
        Some(Done::Accept) => accept(app),
        Some(Done::Refilter | Done::Moved) => app.edms_browser_list(),
        None => {}
    }
}

/// How the picker fits the surface it is drawn on: a phone-sized surface is narrower and lower than
/// the picker's desktop size.
struct Fit {
    width: f32,
    /// Height of the folder's list.
    list_height: f32,
    /// Too narrow for the spaces beside the folder: the spaces become a drop-down above it and the
    /// file type moves below the name.
    narrow: bool,
}

impl Fit {
    fn of(surface: Rect) -> Self {
        let width = (surface.width() - 32.0).clamp(240.0, 760.0);
        let narrow = width < 560.0;
        // Title bar, path row, name row, buttons and margins around the list.
        let around_list = if narrow { 290.0 } else { 230.0 };
        let list_height = (surface.height() - around_list).clamp(110.0, 300.0);
        Self { width, list_height, narrow }
    }
}

/// What the body asks for once the frame is drawn.
enum Done {
    Cancel,
    Accept,
    /// The folder or drive changed: list it.
    Moved,
    /// The file type changed: list the folder again.
    Refilter,
}

/// OK in the picker: move, or run the pick and wait for its answer.
fn accept(app: &mut WordApp) {
    let Some(b) = app.edms_browser.open.as_mut() else { return };
    match b.accept() {
        Step::Stay => {
            if b.entries.is_empty() && !b.busy() && b.note == Note::None {
                app.edms_browser_list();
            }
        }
        Step::Pick { command, params } => match app.session.run(command, &params) {
            Ok(v) => b.waiting = v.get("pending").and_then(Value::as_u64).map(|id| (id, Waiting::Pick)),
            Err(e) => b.note = Note::Error(e.to_string()),
        },
    }
}

/// The picker's contents; `Some` when something is to be done once the frame is drawn.
fn body(ui: &mut Ui, b: &mut EdmsBrowser, t: &Tokens, fit: &Fit) -> Option<Done> {
    let mut done = None;
    let mut pick = false;
    let panes = |ui: &mut Ui| {
        // The spaces.
        if fit.narrow {
            let mut chosen = b.drive;
            let current = b.drive.and_then(|i| b.drives.get(i)).map(|d| d.name.clone()).unwrap_or_default();
            craft_ui::ComboBox::from_id_salt("edms_browser_drive")
                .selected_text(current)
                .width(ui.available_width())
                .show_ui(ui, |ui| {
                    for (i, d) in b.drives.iter().enumerate() {
                        ui.selectable_value(&mut chosen, Some(i), d.name.as_str());
                    }
                })
                .response
                .accessible_name(tl!("Spaces"));
            if chosen != b.drive {
                b.drive = chosen;
                b.path = "/".into();
                b.entries.clear();
                b.selected = None;
                b.note = Note::None;
                done = Some(Done::Moved);
            }
        } else {
            ui.vertical(|ui| {
                ui.set_width(200.0);
                ui.label(egui::RichText::new(tl!("Spaces")).color(t.text_dim));
                egui::Frame::NONE.fill(t.input).stroke(egui::Stroke::new(1.0, t.input_border)).inner_margin(2.0).show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .id_salt("edms_drives")
                        .max_height(fit.list_height + 40.0)
                        .min_scrolled_height(fit.list_height + 40.0)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            if b.drives.is_empty() {
                                ui.add_space((fit.list_height / 2.0).max(20.0));
                                ui.vertical_centered(|ui| {
                                    ui.label(
                                        egui::RichText::new(if b.busy() { tl!("Loading…") } else { tl!("No spaces to browse.") })
                                            .small()
                                            .color(t.text_dim),
                                    );
                                });
                            }
                            for i in 0..b.drives.len() {
                                let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW), Sense::click());
                                if b.drive == Some(i) {
                                    ui.painter().rect_filled(r, 2.0, t.checked);
                                } else if resp.hovered() {
                                    ui.painter().rect_filled(r, 2.0, t.hover);
                                }
                                let Some(d) = b.drives.get(i) else { break };
                                let rtl = crate::rtl::on();
                                let icon = Rect::from_center_size(pos2(crate::rtl::lead_x(rtl, r, 14.0), r.center().y), vec2(18.0, 18.0));
                                icons::paint(ui.painter(), icon, if d.kind == "personal" { "homeFolder" } else { "folder" }, t.icon, t.accent);
                                let name = d.name.clone();
                                let resp = if row_text(ui, r, &name, t, rtl) { resp.on_hover_text(&name) } else { resp };
                                if resp.clicked() && b.drive != Some(i) {
                                    b.drive = Some(i);
                                    b.path = "/".into();
                                    b.entries.clear();
                                    b.selected = None;
                                    b.note = Note::None;
                                    done = Some(Done::Moved);
                                }
                            }
                        });
                });
            });
        }
        ui.add_space(8.0);
        // The folder.
        ui.vertical(|ui| {
            ui.horizontal(|ui| {
                if icon_button(ui, t, "chevronUp", tl!("Up one level"), b.path != "/" && !b.busy()) {
                    b.path = parent(&b.path);
                    b.entries.clear();
                    b.selected = None;
                    b.note = Note::None;
                    done = Some(Done::Moved);
                }
                let where_ = match b.drive.and_then(|i| b.drives.get(i)) {
                    Some(d) if b.path == "/" => d.name.clone(),
                    Some(d) => format!("{}{}", d.name, b.path),
                    None => String::new(),
                };
                ui.label(egui::RichText::new(crate::i18n::isolate_path(&where_)).font(regular(13.0))).on_hover_text(&where_);
            });
            ui.add_space(4.0);
            let mut clicked: Option<(usize, bool)> = None;
            egui::Frame::NONE.fill(t.input).stroke(egui::Stroke::new(1.0, t.input_border)).inner_margin(2.0).show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("edms_entries")
                    .max_height(fit.list_height)
                    .min_scrolled_height(fit.list_height)
                    .auto_shrink([false, false])
                    .show_rows(ui, ROW, b.entries.len(), |ui, range| {
                        if b.entries.is_empty() {
                            ui.add_space((fit.list_height / 2.0 - 10.0).max(20.0));
                            ui.vertical_centered(|ui| {
                                ui.label(
                                    egui::RichText::new(if b.busy() { tl!("Loading…") } else { tl!("No items to show.") }).small().color(t.text_dim),
                                );
                            });
                        }
                        for i in range {
                            let Some(e) = b.entries.get(i) else { break };
                            let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW), Sense::click());
                            if b.selected == Some(i) {
                                ui.painter().rect_filled(r, 2.0, t.checked);
                            } else if resp.hovered() {
                                ui.painter().rect_filled(r, 2.0, t.hover);
                            }
                            let rtl = crate::rtl::on();
                            let icon = Rect::from_center_size(pos2(crate::rtl::lead_x(rtl, r, 14.0), r.center().y), vec2(18.0, 18.0));
                            icons::paint(ui.painter(), icon, if e.folder { "folder" } else { "file" }, t.icon, t.accent);
                            let resp = if row_text(ui, r, &e.name, t, rtl) { resp.on_hover_text(&e.name) } else { resp };
                            if resp.double_clicked() {
                                clicked = Some((i, true));
                            } else if resp.clicked() {
                                clicked = Some((i, false));
                            }
                        }
                    });
            });
            if let Some((i, double)) = clicked {
                b.select(i);
                if double {
                    pick = true;
                }
            }
            let message = match &b.note {
                Note::NotFound(name) => Some((crate::i18n::fmt(tl!("{name} wasn't found."), &[("name", name)]), t.red)),
                Note::Replace(name) => {
                    Some((crate::i18n::fmt(tl!("{name} already exists. Choose Save again to replace it."), &[("name", name)]), t.red))
                }
                Note::Error(e) => Some((e.clone(), t.red)),
                Note::None if b.truncated => {
                    Some((crate::i18n::fmt(tl!("Showing the first {count} items."), &[("count", &b.entries.len().to_string())]), t.text_dim))
                }
                Note::None => None,
            };
            if let Some((text, color)) = message {
                ui.label(egui::RichText::new(text).color(color));
            }
        });
    };
    if fit.narrow {
        ui.vertical(panes);
    } else {
        ui.horizontal_top(panes);
    }
    ui.add_space(6.0);
    let name_row = |ui: &mut Ui| {
        let label = ui.label(tl!("File name:"));
        // Narrow: the name takes the rest of the row, the file type wraps below it.
        let width = if fit.narrow { (ui.available_width() - 12.0).max(120.0) } else { (ui.available_width() - 170.0).max(120.0) };
        let r = ui.add(egui::TextEdit::singleline(&mut b.name).desired_width(width)).labelled_by(label.id);
        if b.focus_name {
            r.request_focus();
            b.focus_name = false;
        }
        if r.changed() {
            b.selected = None;
            b.note = Note::None;
        }
        let before = b.filter;
        let current = b.filters.get(b.filter).map(|(l, _)| tl!(l.as_str()).to_string()).unwrap_or_default();
        craft_ui::ComboBox::from_id_salt("edms_browser_type")
            .selected_text(current)
            .width(150.0)
            .show_ui(ui, |ui| {
                for (i, (l, _)) in b.filters.iter().enumerate() {
                    ui.selectable_value(&mut b.filter, i, tl!(l.as_str()));
                }
            })
            .response
            .accessible_name(tl!("File type"));
        if b.filter != before {
            done = Some(Done::Refilter);
        }
    };
    if fit.narrow {
        ui.horizontal_wrapped(name_row);
    } else {
        ui.horizontal(name_row);
    }
    ui.add_space(8.0);
    let mut cancel = ui.input(|i| i.key_pressed(egui::Key::Escape));
    let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
    ui.horizontal(|ui| {
        ui.with_layout(crate::rtl::far_side(ui, egui::Align::Center), |ui| {
            let (ok, cancelb) =
                crate::dialogs::ok_cancel_responses(ui, |ui| ui.add(crate::widgets::primary_button(ui, tl!(b.ok_label()))), tl!("Cancel"));
            cancel |= cancelb.clicked();
            pick |= ok.clicked() || enter;
        });
    });
    if cancel {
        return Some(Done::Cancel);
    }
    if pick {
        return Some(Done::Accept);
    }
    done
}

/// A name in a row: a long one keeps its beginning and ends in an ellipsis; true then, so the
/// caller shows the whole name as the row's tooltip.
fn row_text(ui: &mut Ui, r: Rect, name: &str, t: &Tokens, rtl: bool) -> bool {
    let mut job = egui::text::LayoutJob::simple_singleline(crate::i18n::isolate(name), regular(13.0), t.text);
    job.wrap.max_width = (r.width() - 36.0).max(0.0);
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    let galley = ui.painter().layout_job(job);
    let text_x = crate::rtl::lead_x(rtl, r, 30.0);
    let at = pos2(if rtl { text_x - galley.size().x } else { text_x }, r.center().y - galley.size().y / 2.0);
    let elided = galley.elided;
    ui.painter().galley(at, galley, t.text);
    elided
}

/// A small button with a drawn icon and a tooltip; true when clicked.
fn icon_button(ui: &mut Ui, t: &Tokens, icon: &str, tip: &str, enabled: bool) -> bool {
    let (r, resp) = ui.allocate_exact_size(vec2(28.0, 24.0), if enabled { Sense::click() } else { Sense::hover() });
    if enabled && resp.hovered() {
        ui.painter().rect_filled(r, 3.0, t.hover);
    }
    let c = if enabled { t.icon } else { t.text_disabled };
    icons::paint(ui.painter(), Rect::from_center_size(r.center(), vec2(18.0, 18.0)), icon, c, if enabled { t.accent } else { c });
    resp.on_hover_text(tip).clicked()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Services;
    use craft_opencore::edms::{Client, HttpResponse, Method, RecordingTransport};
    use wordcraft_engine::Session;

    const DRIVE: &str = "b894e89e-a7fd-4385-92fc-b5e0bb2659fd$8c9cde70-836d-4eda-81bf-99102e838af1";
    const DRIVES: &[u8] = include_bytes!("../../../../opencraft/crates/craft-opencore/tests/fixtures/xwork/drives.json");
    const ROOT: &[u8] = include_bytes!("../../../../opencraft/crates/craft-opencore/tests/fixtures/xwork/propfind_root_depth1.xml");

    /// An app whose EDMS is connected through a recording transport (nothing touches the network).
    fn connected() -> (WordApp, RecordingTransport) {
        let transport = RecordingTransport::default();
        let mut session = Session::new(wordcraft_doc::Document::new());
        session.edms.install(Box::new(Client::new(transport.clone())));
        session.run("edms.connect", &json!({"origin": "https://cloud.xwork.cloud", "token": "tok", "initiatorId": "client-1"})).unwrap();
        (WordApp::new(session, Services::default()), transport)
    }

    fn status_of(a: &WordApp) -> String {
        a.status_msg.as_ref().map(|(s, _)| s.clone()).unwrap_or_default()
    }

    /// A 2×2 red PNG.
    fn tiny_png() -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        image::RgbaImage::from_pixel(2, 2, image::Rgba([255, 0, 0, 255])).write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    /// The answer to the picker's last request, then the frame that takes it.
    fn answer(a: &mut WordApp, transport: &RecordingTransport, response: HttpResponse) {
        let (id, _) = transport.last_started().unwrap();
        transport.answer(id, response);
        a.session.edms_pump();
        poll(a);
    }

    /// Without local files, Open, Insert Picture and Save As go to the EDMS picker, never to the
    /// system dialog, and the picker starts by asking for the spaces.
    #[test]
    fn server_only_sends_open_pictures_and_save_as_to_the_edms_picker() {
        let (mut a, transport) = connected();
        a.embed.local_files = false;
        a.services.open_async = Some(Box::new(|_| panic!("the system dialog must not open")));
        a.open_dialog();
        assert!(a.edms_browser.open.is_some(), "the picker is open");
        assert!(a.file_dialog_open(), "and counts as the file dialog");
        let (_, request) = transport.last_started().unwrap();
        assert_eq!(request.method, Method::Get);
        assert!(request.url.ends_with("/graph/v1.0/me/drives"), "{}", request.url);
        a.cancel_edms_browser();
        assert!(a.edms_browser.open.is_none());

        a.pick_picture();
        assert!(matches!(a.edms_browser.open.as_ref().map(|b| &b.after), Some(AfterPick::Picture)));
        a.cancel_edms_browser();
        a.run("text.insert", json!({"text": "Server only"})).unwrap();
        a.save_as_dialog();
        assert!(matches!(a.edms_browser.open.as_ref().map(|b| &b.after), Some(AfterPick::SaveAs { .. })));
        assert_eq!(a.edms_browser.open.as_ref().map(|b| b.name.as_str()), Some("Server only.docx"));
    }

    /// Without local files and without an EDMS connection nothing opens; the host page's own
    /// document, which comes through the inbox like a drop, still opens (the drops themselves
    /// are stopped at their source).
    #[test]
    fn server_only_without_a_connection_refuses_local_files_but_takes_the_hosts_document() {
        let inbox: crate::Inbox = Default::default();
        let services = Services { inbox: Some(inbox.clone()), open_async: Some(Box::new(|_| panic!("no system dialog"))), ..Default::default() };
        let mut a = WordApp::new(Session::new(wordcraft_doc::Document::new()), services);
        a.embed.local_files = false;
        a.open_dialog();
        assert!(a.edms_browser.open.is_none() && !a.file_dialog_open());
        assert!(status_of(&a).contains("no EDMS connection"), "{}", status_of(&a));
        let hosted = wordcraft_engine::io::save_bytes("x.docx", &wordcraft_doc::Document::from_text("From the host")).unwrap();
        inbox.lock().unwrap().push(("hosted.docx".into(), hosted));
        a.drain_inbox();
        assert!(a.session.doc.plain_text(wordcraft_doc::StoryRef::Body).contains("From the host"), "the host's document opens");
    }

    /// The picker lists the spaces, the personal one first, then its root folder, and a picked
    /// document goes to `edms.open`.
    #[test]
    fn the_picker_lists_spaces_then_the_folder_and_opens_the_picked_document() {
        let (mut a, transport) = connected();
        assert!(a.browse_edms("document"));
        answer(&mut a, &transport, HttpResponse::new(200).with_body(DRIVES));
        let b = a.edms_browser.open.as_ref().unwrap();
        assert!(b.drives.len() > 1, "{} spaces", b.drives.len());
        assert_eq!(b.drives[0].kind, "personal");
        assert_eq!(b.drive, Some(0));
        assert!(b.busy(), "the listing is on its way");
        let (_, request) = transport.last_started().unwrap();
        assert_eq!(request.method, Method::Propfind);
        assert!(request.url.contains(DRIVE), "{}", request.url);
        answer(&mut a, &transport, HttpResponse::new(207).with_body(ROOT));
        let b = a.edms_browser.open.as_mut().unwrap();
        assert!(!b.busy());
        assert!(b.entries.iter().any(|e| e.folder && e.name == "Test"), "folders are listed");
        assert!(b.entries.iter().any(|e| !e.folder && e.name == "craft-test.docx"), "documents are listed");
        assert!(b.entries.iter().all(|e| e.folder || e.name.rsplit_once('.').is_some()), "only documents");
        let folders = b.entries.iter().take_while(|e| e.folder).count();
        assert!(b.entries[folders..].iter().all(|e| !e.folder), "folders first");
        b.name = "nowhere.docx".into();
        assert_eq!(b.accept(), Step::Stay, "a name that isn't listed isn't opened");
        assert!(matches!(b.note, Note::NotFound(_)));
        b.name = "craft-test.docx".into();
        let Step::Pick { command, params } = b.accept() else { panic!("picked") };
        assert_eq!(command, "edms.open");
        assert_eq!(params["path"], json!("/craft-test.docx"));
        // A folder picked by name is entered, not opened: the listing is asked for again.
        b.name = "Test".into();
        assert_eq!(b.accept(), Step::Stay);
        assert_eq!(b.path, "/Test");
        assert!(b.entries.is_empty());
        accept(&mut a);
        let (_, request) = transport.last_started().unwrap();
        assert_eq!(request.method, Method::Propfind);
        assert!(request.url.ends_with("/Test"), "{}", request.url);
    }

    /// Save As in the picker: a bare name gets the type's extension, an existing file is
    /// replaced only on the second Save (then with `overwrite`), and the request is a PUT that
    /// refuses to overwrite until then.
    #[test]
    fn save_as_adds_the_extension_and_asks_before_replacing() {
        let (mut a, transport) = connected();
        a.run("text.insert", json!({"text": "Into the EDMS"})).unwrap();
        assert!(a.browse_edms("save"));
        answer(&mut a, &transport, HttpResponse::new(200).with_body(DRIVES));
        answer(&mut a, &transport, HttpResponse::new(207).with_body(ROOT));
        let b = a.edms_browser.open.as_mut().unwrap();
        b.name = "chapter".into();
        let Step::Pick { command, params } = b.accept() else { panic!("picked") };
        assert_eq!(command, "edms.saveAs");
        assert_eq!(
            (params["name"].as_str(), params["overwrite"].as_bool(), params["switch"].as_bool()),
            (Some("chapter.docx"), Some(false), Some(true))
        );
        b.name = "craft-test".into();
        assert_eq!(b.accept(), Step::Stay, "asks before replacing");
        assert!(matches!(b.note, Note::Replace(ref n) if n == "craft-test.docx"));
        let Step::Pick { params, .. } = b.accept() else { panic!("picked") };
        assert_eq!(params["overwrite"], json!(true));
        // Through the frame: the pick runs, the PUT goes out with the name, and the answer
        // closes the picker and relocates the document.
        b.name = "chapter".into();
        accept(&mut a);
        let (_, request) = transport.last_started().unwrap();
        assert_eq!(request.method, Method::Put);
        assert!(request.url.ends_with("/chapter.docx"), "{}", request.url);
        assert_eq!(request.header_value("if-none-match"), Some("*"));
        answer(&mut a, &transport, HttpResponse::new(201).with_header("ETag", "\"new\"").with_header("OC-FileId", "f!1"));
        assert!(a.edms_browser.open.is_none(), "closed after the save");
        assert!(a.document_in_edms());
        assert_eq!(a.session.file_name.as_deref(), Some("chapter.docx"));
        assert!(!a.session.dirty);
    }

    /// A picked picture is fetched and inserted; the document stays where it is.
    #[test]
    fn a_picked_picture_is_fetched_and_inserted() {
        let (mut a, transport) = connected();
        a.run("text.insert", json!({"text": "Before"})).unwrap();
        assert!(a.browse_edms("picture"));
        answer(&mut a, &transport, HttpResponse::new(200).with_body(DRIVES));
        answer(&mut a, &transport, HttpResponse::new(207).with_body(ROOT));
        let b = a.edms_browser.open.as_mut().unwrap();
        assert!(b.entries.iter().all(|e| e.folder || matches(&e.name, &["png".to_string(), "jpg".to_string(), "jpeg".to_string()])), "pictures only");
        b.entries.push(Entry { name: "logo.png".into(), path: "/logo.png".into(), folder: false });
        b.name = "logo.png".into();
        accept(&mut a);
        let (_, request) = transport.last_started().unwrap();
        assert_eq!(request.method, Method::Get);
        assert!(request.url.ends_with("/logo.png"), "{}", request.url);
        let png = tiny_png();
        answer(&mut a, &transport, HttpResponse::new(200).with_header("Content-Type", "image/png").with_body(png));
        assert!(a.edms_browser.open.is_none());
        assert!(a.session.doc.plain_text(wordcraft_doc::StoryRef::Body).contains("Before"));
        assert!(a.session.dirty, "the picture is an edit");
    }

    /// File › Export goes into the EDMS when the document lives there, to this computer
    /// otherwise, and the Export page's switch overrides either.
    #[test]
    fn export_follows_the_document_unless_switched() {
        let (mut a, _transport) = connected();
        a.run("text.insert", json!({"text": "Export me"})).unwrap();
        assert!(!a.export_to_edms(), "a local document exports locally");
        a.run("edms.adopt", json!({"drive": DRIVE, "path": "/craft-test.docx", "etag": "\"e1\""})).unwrap();
        assert!(a.export_to_edms(), "an EDMS document exports there");
        a.edms_browser.export_to_edms = Some(false);
        assert!(!a.export_to_edms());
        a.embed.local_files = false;
        assert!(a.export_to_edms(), "without local files the switch doesn't count");
        a.export_dialog("pdf");
        let b = a.edms_browser.open.as_ref().expect("the picker");
        assert!(matches!(b.after, AfterPick::Export { ref ext } if ext == "pdf"));
        assert_eq!(b.name, "craft-test.pdf", "the copy takes the document's name without its extension");
    }
}
