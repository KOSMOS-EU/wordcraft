//! kosmos: the EDMS location picker. Open, Save As, Export, Insert Picture and recipient lists
//! from the document's home in OpenCore/OpenCloud, through the `edms.*` commands, beside the
//! system's file dialog (or instead of it, when the host allows no local files: `Embed::local_files`).
//!
//! Left the spaces (`edms.drives`), right the folder (`edms.list`) with Up, a file name field
//! and the file types, then `edms.open`, `edms.fetch`, `edms.saveAs` or `edms.export`. Nothing
//! here waits: every answer arrives on a later frame through [`poll`], which takes the picker's
//! own results by id and leaves the rest for `edms.poll`. The picked file goes to the same
//! follow-up as the system dialog's ([`AfterPick`]).

use craft_opencore::egui::picker::{self as kit, Done, Fit, PickerColors, PickerIcon, PickerLook};
use craft_opencore::session::picker::{Picker, Purpose, own_place};
use egui::{Align2, vec2};
use serde_json::Value;

use crate::file_dialogs::{AfterPick, FileDialogRequest, open_filters};
use crate::theme::{Tokens, regular};
use crate::{WordApp, icons};

/// The picker while it is open, and the place it showed last (where it opens next time).
#[derive(Default)]
pub(crate) struct State {
    pub(crate) open: Option<Box<EdmsBrowser>>,
    last: Option<(String, String)>,
    /// File › Export: a copy goes into the EDMS rather than to this computer. `None` follows the
    /// document: into the EDMS when it lives there.
    pub(crate) export_to_edms: Option<bool>,
}

/// The open picker: the kit's model and what to do with the pick.
pub(crate) struct EdmsBrowser {
    picker: Picker,
    pub(crate) after: AfterPick,
}

impl std::ops::Deref for EdmsBrowser {
    type Target = Picker;

    fn deref(&self) -> &Picker {
        &self.picker
    }
}

impl std::ops::DerefMut for EdmsBrowser {
    fn deref_mut(&mut self) -> &mut Picker {
        &mut self.picker
    }
}

impl EdmsBrowser {
    pub(crate) fn new(req: FileDialogRequest, after: AfterPick) -> Self {
        let picker = match &req {
            FileDialogRequest::Open { purpose } | FileDialogRequest::OpenMany { purpose } => {
                let filters = open_filters(purpose).iter().map(|(l, e)| ((*l).to_string(), e.iter().map(|x| (*x).to_string()).collect())).collect();
                let purpose = if matches!(after, AfterPick::Picture | AfterPick::Recipients) { Purpose::Fetch } else { Purpose::Open };
                Picker::new(purpose, String::new(), filters)
            }
            FileDialogRequest::Save { name, .. } => {
                let purpose = if matches!(after, AfterPick::Export { .. }) { Purpose::Export } else { Purpose::SaveAs };
                Picker::new(purpose, name.clone(), Picker::save_filters(name))
            }
        };
        EdmsBrowser { picker, after }
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
}

/// Whether the picker can finish this follow-up; the others (fonts, Word sources, a path for
/// another dialog, Compare) stay with local files.
pub(crate) fn supports(after: &AfterPick) -> bool {
    matches!(after, AfterPick::OpenDocument { .. } | AfterPick::Picture | AfterPick::Recipients | AfterPick::SaveAs { .. } | AfterPick::Export { .. })
}

/// The app's command path for the picker.
fn runner(session: &mut wordcraft_engine::Session) -> impl FnMut(&str, &Value) -> Result<Value, String> + '_ {
    |command, params| session.run(command, params).map_err(|e| e.to_string())
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
        b.start(&mut runner(&mut self.session));
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
        b.list(&mut runner(&mut self.session));
    }

    /// Close the picker without a pick and finish what it was for as cancelled.
    pub(crate) fn cancel_edms_browser(&mut self) {
        let Some(b) = self.edms_browser.open.take() else { return };
        if let Some(place) = b.place() {
            self.edms_browser.last = Some(place);
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
        if !b.picked(&result) {
            self.edms_browser.open = Some(b);
            return;
        }
        if let Some(place) = b.place() {
            self.edms_browser.last = Some(place);
        }
        let name = b.picked_name(&result);
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
    let Some(id) = b.waiting_for() else { return };
    let Some(result) = app.session.edms.take_result(id) else { return };
    // Where it was last, else the document's own space, else the first (personal) space.
    let start = app.edms_browser.last.clone().or_else(|| own_place(&app.session.edms, app.session.document_id()));
    let picked = b.answer(result, start, &mut runner(&mut app.session));
    if let Some(picked) = picked {
        app.edms_browser_picked(picked);
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
        let ok_label = tl!(b.ok_label());
        done = kit::body(ui, &mut b, &WordLook { tokens: &t }, &fit, ok_label);
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

/// OK in the picker: move, or run the pick and wait for its answer.
fn accept(app: &mut WordApp) {
    let Some(b) = app.edms_browser.open.as_mut() else { return };
    b.accept_and_run(&mut runner(&mut app.session));
}

/// WordCraft's look for the kit's picker: its colours, words, icons, font and reading direction.
struct WordLook<'a> {
    tokens: &'a Tokens,
}

impl PickerLook for WordLook<'_> {
    fn colors(&self) -> PickerColors {
        let t = self.tokens;
        PickerColors {
            text: t.text,
            text_dim: t.text_dim,
            text_disabled: t.text_disabled,
            input: t.input,
            input_border: t.input_border,
            checked: t.checked,
            hover: t.hover,
            icon: t.icon,
            accent: t.accent,
            warning: t.red,
        }
    }

    fn tr(&self, english: &str) -> String {
        tl!(english).to_string()
    }

    fn fmt(&self, template: &str, args: &[(&str, &str)]) -> String {
        crate::i18n::fmt(template, args)
    }

    fn icon(&self, painter: &egui::Painter, rect: egui::Rect, icon: PickerIcon, color: egui::Color32, accent: egui::Color32) {
        let name = match icon {
            PickerIcon::HomeFolder => "homeFolder",
            PickerIcon::Folder => "folder",
            PickerIcon::File => "file",
            PickerIcon::Up => "chevronUp",
        };
        icons::paint(painter, rect, name, color, accent);
    }

    fn font(&self, size: f32) -> egui::FontId {
        regular(size)
    }

    fn rtl(&self) -> bool {
        crate::rtl::on()
    }

    fn isolate(&self, name: &str) -> String {
        crate::i18n::isolate(name)
    }

    fn isolate_path(&self, path: &str) -> String {
        crate::i18n::isolate_path(path)
    }

    fn lead_x(&self, rtl: bool, rect: egui::Rect, inset: f32) -> f32 {
        crate::rtl::lead_x(rtl, rect, inset)
    }

    fn root(&self, ui: &mut egui::Ui, add: &mut dyn FnMut(&mut egui::Ui) -> Option<Done>) -> Option<Done> {
        crate::rtl::root(ui, add)
    }

    fn far_side(&self, ui: &egui::Ui, align: egui::Align) -> egui::Layout {
        crate::rtl::far_side(ui, align)
    }

    fn combo(&self, ui: &mut egui::Ui, id_salt: &str, selected: String, width: f32, accessible_name: &str, contents: &mut dyn FnMut(&mut egui::Ui)) {
        craft_ui::ComboBox::from_id_salt(id_salt)
            .selected_text(selected)
            .width(width)
            .show_ui(ui, contents)
            .response
            .accessible_name(accessible_name);
    }

    fn ok_cancel(&self, ui: &mut egui::Ui, ok: &str, cancel: &str) -> (egui::Response, egui::Response) {
        crate::dialogs::ok_cancel_responses(ui, |ui| ui.add(crate::widgets::primary_button(ui, ok)), cancel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Services;
    use craft_opencore::edms::{Client, HttpResponse, Method, RecordingTransport};
    use craft_opencore::session::picker::{Entry, Note, Step, matches};
    use serde_json::json;
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
