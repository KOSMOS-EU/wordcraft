//! The EDMS commands (kosmos) against a recording transport: nothing touches the network, the
//! answers are what cloud.xwork.cloud answered on 2026-11-16 (fixtures in craft-opencore).

use craft_opencore::edms::{Client, HttpResponse, Method, RecordingTransport};
use serde_json::{Value, json};

use crate::{Session, blank_document};

const DRIVE: &str = "b894e89e-a7fd-4385-92fc-b5e0bb2659fd$8c9cde70-836d-4eda-81bf-99102e838af1";
const FILE_ID: &str = "b894e89e-a7fd-4385-92fc-b5e0bb2659fd$8c9cde70-836d-4eda-81bf-99102e838af1!f820fbb2-e279-4fa4-9938-c63d07c733f8";
const DRIVES: &[u8] = include_bytes!("../../../../../opencraft/crates/craft-opencore/tests/fixtures/xwork/drives.json");

fn session() -> (Session, RecordingTransport) {
    let transport = RecordingTransport::default();
    let mut s = Session::new(blank_document(&wordcraft_geom::Region::default()));
    s.edms.install(Box::new(Client::new(transport.clone())));
    (s, transport)
}

fn connected() -> (Session, RecordingTransport) {
    let (mut s, transport) = session();
    s.run("edms.connect", &json!({"origin": "https://cloud.xwork.cloud", "token": "tok", "initiatorId": "client-1"})).unwrap();
    (s, transport)
}

/// A docx as the server would hand it over: a blank document with one paragraph of text.
fn docx_bytes(text: &str) -> Vec<u8> {
    let mut s = Session::new(blank_document(&wordcraft_geom::Region::default()));
    s.run("text.insert", &json!({"text": text})).unwrap();
    crate::io::save_bytes("x.docx", &s.doc).unwrap()
}

fn pending_id(result: &Value) -> u64 {
    result["pending"].as_u64().unwrap()
}

fn poll_one(s: &mut Session) -> Value {
    let polled = s.run("edms.poll", &json!({})).unwrap();
    let results = polled["results"].as_array().unwrap();
    assert_eq!(results.len(), 1, "{polled}");
    results[0].clone()
}

#[test]
fn without_a_transport_or_a_connection_the_commands_refuse() {
    let mut s = Session::new(blank_document(&wordcraft_geom::Region::default()));
    let status = s.run("edms.status", &json!({})).unwrap();
    assert_eq!((status["transport"].as_bool(), status["connected"].as_bool()), (Some(false), Some(false)));
    assert!(s.run("edms.drives", &json!({})).is_err(), "no transport");
    let (mut s, _) = session();
    assert!(s.run("edms.drives", &json!({})).unwrap_err().to_string().contains("not connected"));
    assert!(s.run("edms.connect", &json!({"origin": "ftp://x", "token": "t"})).is_err());
    assert!(s.run("edms.connect", &json!({"origin": "https://x"})).is_err(), "bearer or app token");
    assert!(s.run("edms.save", &json!({})).is_err(), "nothing opened from the EDMS");
}

#[test]
fn connect_then_drives_round_trip_through_the_transport() {
    let (mut s, transport) = connected();
    let status = s.run("edms.status", &json!({})).unwrap();
    assert_eq!(status["connected"], json!(true));
    assert_eq!(status["origin"], json!("https://cloud.xwork.cloud"));
    assert!(status["document"].is_null());

    let id = pending_id(&s.run("edms.drives", &json!({})).unwrap());
    let (started_id, request) = transport.last_started().unwrap();
    assert_eq!(started_id, id);
    assert_eq!(request.method, Method::Get);
    assert_eq!(request.url, "https://cloud.xwork.cloud/graph/v1.0/me/drives");
    assert_eq!(request.header_value("authorization"), Some("Bearer tok"));
    assert_eq!(request.header_value("initiator-id"), Some("client-1"));
    assert_eq!(s.run("edms.poll", &json!({})).unwrap()["results"].as_array().map(Vec::len), Some(0), "nothing answered yet");
    transport.answer(id, HttpResponse::new(200).with_body(DRIVES));
    let result = poll_one(&mut s);
    assert_eq!((result["id"].as_u64(), result["kind"].as_str(), result["ok"].as_bool()), (Some(id), Some("drives"), Some(true)));
    let drives = result["drives"].as_array().unwrap();
    assert!(drives.iter().any(|d| d["type"] == "personal" && d["id"] == DRIVE));
}

#[test]
fn open_save_and_a_rejected_save_follow_the_tracker() {
    let (mut s, transport) = connected();
    let id = pending_id(&s.run("edms.open", &json!({"drive": DRIVE, "path": "/craft-test.docx"})).unwrap());
    let (_, request) = transport.last_started().unwrap();
    assert_eq!(request.method, Method::Get);
    assert!(request.url.ends_with("/craft-test.docx"));
    transport.answer(id, HttpResponse::new(200).with_header("ETag", "\"e1\"").with_header("OC-FileId", FILE_ID).with_body(docx_bytes("Hello EDMS")));
    let opened = poll_one(&mut s);
    assert_eq!((opened["kind"].as_str(), opened["opened"].as_bool(), opened["etag"].as_str()), (Some("open"), Some(true), Some("\"e1\"")));
    assert!(s.doc.plain_text(wordcraft_doc::StoryRef::Body).contains("Hello EDMS"));
    assert_eq!(s.file_name.as_deref(), Some("craft-test.docx"));
    assert!(!s.dirty);
    let status = s.run("edms.status", &json!({})).unwrap();
    assert_eq!(status["document"]["state"], json!("clean"));
    assert_eq!(status["document"]["id"], json!(FILE_ID));
    assert!(s.ui_requests.iter().any(|r| r["edms"]["event"] == "opened"));

    // Typing makes it dirty; the save PUTs with If-Match and clears dirty on the new etag.
    s.run("text.insert", &json!({"text": " more"})).unwrap();
    assert!(s.dirty);
    assert_eq!(s.run("edms.status", &json!({})).unwrap()["document"]["state"], json!("localChanged"));
    let id = pending_id(&s.run("edms.save", &json!({})).unwrap());
    let (_, request) = transport.last_started().unwrap();
    assert_eq!(request.method, Method::Put);
    assert_eq!(request.header_value("if-match"), Some("\"e1\""));
    assert!(request.body.len() > 100, "a docx");
    transport.answer(id, HttpResponse::new(204).with_header("OC-ETag", "\"e2\""));
    let saved = poll_one(&mut s);
    assert_eq!((saved["kind"].as_str(), saved["saved"].as_bool()), (Some("save"), Some(true)));
    assert!(!s.dirty);
    let status = s.run("edms.status", &json!({})).unwrap();
    assert_eq!((status["document"]["etag"].as_str(), status["document"]["state"].as_str()), (Some("\"e2\""), Some("clean")));

    // Someone else saved meanwhile: the next save is rejected and the front end is asked.
    s.run("text.insert", &json!({"text": " again"})).unwrap();
    s.ui_requests.clear();
    let id = pending_id(&s.run("edms.save", &json!({})).unwrap());
    transport.answer(id, HttpResponse::new(412));
    let rejected = poll_one(&mut s);
    assert_eq!(
        (rejected["ok"].as_bool(), rejected["error"]["code"].as_str(), rejected["advice"].as_str()),
        (Some(false), Some("conflict"), Some("conflict"))
    );
    assert!(s.dirty, "the document keeps its changes");
    assert_eq!(s.run("edms.status", &json!({})).unwrap()["document"]["state"], json!("conflict"));
    assert!(s.ui_requests.iter().any(|r| r["edms"]["advice"] == "conflict"));
}

/// kosmos: OpenCloud's New menu makes an empty file; WordCraft begins a blank document there, and the first save
/// writes a real docx to that place.
#[test]
fn an_empty_file_from_the_new_menu_becomes_a_blank_document_saved_in_its_place() {
    let (mut s, transport) = connected();
    let id = pending_id(&s.run("edms.open", &json!({"drive": DRIVE, "path": "/Neu.docx"})).unwrap());
    transport.answer(id, HttpResponse::new(200).with_header("ETag", "\"e0\"").with_header("OC-FileId", FILE_ID).with_body(Vec::new()));
    let opened = poll_one(&mut s);
    assert_eq!((opened["ok"].as_bool(), opened["new"].as_bool()), (Some(true), Some(true)), "{opened}");
    assert_eq!(s.doc.plain_text(wordcraft_doc::StoryRef::Body), "");
    assert_eq!(s.file_name.as_deref(), Some("Neu.docx"));
    s.run("text.insert", &json!({"text": "Erster Text"})).unwrap();
    let id = pending_id(&s.run("edms.save", &json!({})).unwrap());
    let (_, request) = transport.last_started().unwrap();
    assert_eq!((request.method, request.header_value("if-match")), (Method::Put, Some("\"e0\"")));
    assert!(request.body.starts_with(b"PK"), "a real docx goes to the empty file's place");
    transport.answer(id, HttpResponse::new(204).with_header("OC-ETag", "\"e1\""));
    assert_eq!(poll_one(&mut s)["saved"].as_bool(), Some(true));
}

#[test]
fn a_foreign_event_offers_a_reload_an_own_one_does_not() {
    let (mut s, transport) = connected();
    let id = pending_id(&s.run("edms.open", &json!({"drive": DRIVE, "path": "/craft-test.docx"})).unwrap());
    transport.answer(id, HttpResponse::new(200).with_header("ETag", "\"e1\"").with_header("OC-FileId", FILE_ID).with_body(docx_bytes("x")));
    poll_one(&mut s);
    let own = s
        .run("edms.event", &json!({"type": "postprocessing-finished", "data": {"itemid": FILE_ID, "initiatorid": "client-1", "etag": "\"e1\""}}))
        .unwrap();
    assert_eq!((own["concerns"].as_bool(), own["foreign"].as_bool(), own["advice"].as_str()), (Some(true), Some(false), Some("none")));
    let other_file = s.run("edms.event", &json!({"type": "file-touched", "data": {"itemid": "someone$else!x", "initiatorid": "client-9"}})).unwrap();
    assert_eq!(other_file["concerns"], json!(false));
    s.ui_requests.clear();
    let foreign = s
        .run("edms.event", &json!({"type": "postprocessing-finished", "data": {"itemid": FILE_ID, "initiatorid": "client-9", "etag": "\"e2\""}}))
        .unwrap();
    assert_eq!(foreign["advice"], json!("reload"));
    assert!(s.ui_requests.iter().any(|r| r["edms"]["advice"] == "reload"));
    assert_eq!(s.run("edms.status", &json!({})).unwrap()["document"]["state"], json!("remoteChanged"));
    // Reload fetches the server's version and the tracker is clean on it.
    let id = pending_id(&s.run("edms.reload", &json!({})).unwrap());
    transport.answer(id, HttpResponse::new(200).with_header("ETag", "\"e2\"").with_body(docx_bytes("server version")));
    let reloaded = poll_one(&mut s);
    assert_eq!(reloaded["kind"], json!("reload"));
    assert!(s.doc.plain_text(wordcraft_doc::StoryRef::Body).contains("server version"));
    assert_eq!(s.run("edms.status", &json!({})).unwrap()["document"]["state"], json!("clean"));
    // Trashed elsewhere: only Save As makes sense.
    let gone = s.run("edms.event", &json!({"type": "item-trashed", "data": {"itemid": FILE_ID, "initiatorid": "client-9"}})).unwrap();
    assert_eq!(gone["advice"], json!("saveAs"));
    assert!(s.run("edms.save", &json!({})).unwrap_err().to_string().contains("edms.saveAs"));
}

#[test]
fn save_as_moves_the_document_and_export_does_not() {
    let (mut s, transport) = connected();
    s.run("text.insert", &json!({"text": "fresh"})).unwrap();
    // A new document saved into a folder: PUT with If-None-Match, then it lives there.
    let id = pending_id(&s.run("edms.saveAs", &json!({"drive": DRIVE, "path": "/Ordner A", "name": "Neu.docx"})).unwrap());
    let (_, request) = transport.last_started().unwrap();
    assert_eq!(request.method, Method::Put);
    assert!(request.url.ends_with("/Ordner%20A/Neu.docx"));
    assert_eq!(request.header_value("if-none-match"), Some("*"));
    transport.answer(id, HttpResponse::new(201).with_header("OC-ETag", "\"n1\"").with_header("OC-FileId", "s$p!n"));
    let saved = poll_one(&mut s);
    assert_eq!(
        (saved["kind"].as_str(), saved["switched"].as_bool(), saved["path"].as_str()),
        (Some("saveAs"), Some(true), Some("/Ordner A/Neu.docx"))
    );
    assert!(!s.dirty);
    assert_eq!(s.file_name.as_deref(), Some("Neu.docx"));
    let status = s.run("edms.status", &json!({})).unwrap();
    assert_eq!((status["document"]["path"].as_str(), status["document"]["etag"].as_str()), (Some("/Ordner A/Neu.docx"), Some("\"n1\"")));
    assert!(s.ui_requests.iter().any(|r| r["edms"]["event"] == "relocated"));

    // An export is a copy in another format: nothing moves, the document stays clean.
    let id = pending_id(&s.run("edms.export", &json!({"drive": DRIVE, "path": "/", "name": "Neu.pdf", "overwrite": true})).unwrap());
    let (_, request) = transport.last_started().unwrap();
    assert!(request.url.ends_with("/Neu.pdf"));
    assert_eq!(request.header_value("if-none-match"), None);
    assert!(request.body.starts_with(b"%PDF"), "a PDF");
    transport.answer(id, HttpResponse::new(201).with_header("OC-ETag", "\"p1\""));
    let exported = poll_one(&mut s);
    assert_eq!(exported["kind"], json!("export"));
    assert_eq!(s.run("edms.status", &json!({})).unwrap()["document"]["path"], json!("/Ordner A/Neu.docx"));

    // A copy without switching keeps the document where it is.
    let id = pending_id(&s.run("edms.saveAs", &json!({"drive": DRIVE, "path": "/", "name": "Kopie.docx", "switch": false})).unwrap());
    transport.answer(id, HttpResponse::new(201).with_header("OC-ETag", "\"k1\""));
    let copy = poll_one(&mut s);
    assert_eq!(copy["switched"], json!(false));
    assert_eq!(s.run("edms.status", &json!({})).unwrap()["document"]["path"], json!("/Ordner A/Neu.docx"));

    // Transport failures and a renewed token.
    let id = pending_id(&s.run("edms.list", &json!({"drive": DRIVE})).unwrap());
    transport.fail(id, "fetch: network down");
    let failed = poll_one(&mut s);
    assert_eq!((failed["ok"].as_bool(), failed["error"]["code"].as_str()), (Some(false), Some("transport")));
    s.run("edms.connect", &json!({"origin": "https://cloud.xwork.cloud", "token": "tok2"})).unwrap();
    s.run("edms.list", &json!({"drive": DRIVE, "path": "Ordner A"})).unwrap();
    let (_, request) = transport.last_started().unwrap();
    assert_eq!(request.header_value("authorization"), Some("Bearer tok2"));
    assert_eq!(request.header_value("depth"), Some("1"));
    assert!(request.url.ends_with("/Ordner%20A"), "{}", request.url);
}

#[test]
fn bad_parameters_never_reach_the_transport() {
    let (mut s, transport) = connected();
    assert!(s.run("edms.open", &json!({"drive": DRIVE, "path": "/"})).is_err(), "a folder is no file");
    assert!(s.run("edms.saveAs", &json!({"drive": DRIVE, "path": "/", "name": "a/b.docx"})).is_err());
    assert!(s.run("edms.saveAs", &json!({"drive": "", "path": "/", "name": "a.docx"})).is_err());
    assert!(s.run("edms.search", &json!({"query": "   "})).is_err());
    assert!(s.run("edms.event", &json!({"type": "x"})).is_err());
    assert!(s.run("edms.event", &json!({"type": "x", "data": "nope"})).is_err());
    assert!(s.run("edms.lock", &json!({})).is_err(), "no EDMS document");
    assert!(transport.started().is_empty());
}

#[test]
fn a_host_loaded_document_is_adopted_and_saved_through_the_module() {
    let (mut s, transport) = connected();
    s.run("text.insert", &json!({"text": "from the host"})).unwrap();
    let adopted =
        s.run("edms.adopt", &json!({"drive": DRIVE, "path": "/craft-test.docx", "id": FILE_ID, "etag": "\"h1\"", "permissions": "RDNVWZP"})).unwrap();
    assert_eq!(adopted["adopted"], json!(true));
    let status = s.run("edms.status", &json!({})).unwrap();
    assert_eq!(
        (status["document"]["path"].as_str(), status["document"]["etag"].as_str(), status["document"]["writable"].as_bool()),
        (Some("/craft-test.docx"), Some("\"h1\""), Some(true))
    );
    assert_eq!(s.file_name.as_deref(), Some("craft-test.docx"));
    let id = pending_id(&s.run("edms.save", &json!({})).unwrap());
    let (_, request) = transport.last_started().unwrap();
    assert_eq!(request.header_value("if-match"), Some("\"h1\""));
    transport.answer(id, HttpResponse::new(204).with_header("OC-ETag", "\"h2\""));
    poll_one(&mut s);
    assert!(!s.dirty);
    assert!(s.run("edms.adopt", &json!({"drive": DRIVE, "path": "/"})).is_err(), "a folder is no file");
}

/// `edms.fetch` hands the bytes over as they are, base64 in the result, and leaves the document
/// alone: a picture or a recipient list for the front end to place.
#[test]
fn fetch_hands_the_bytes_to_the_front_end_without_opening_them() {
    let (mut s, transport) = connected();
    s.run("text.insert", &json!({"text": "Stays"})).unwrap();
    let rev = s.rev();
    assert!(s.run("edms.fetch", &json!({"drive": DRIVE, "path": "/"})).is_err(), "a folder isn't a file");
    let id = pending_id(&s.run("edms.fetch", &json!({"drive": DRIVE, "path": "/Bilder/logo.png"})).unwrap());
    let (_, request) = transport.last_started().unwrap();
    assert_eq!(request.method, Method::Get);
    assert!(request.url.ends_with("/Bilder/logo.png"), "{}", request.url);
    let png = [0x89u8, b'P', b'N', b'G', 0, 1, 2, 3];
    transport.answer(id, HttpResponse::new(200).with_header("Content-Type", "image/png").with_body(png));
    let fetched = poll_one(&mut s);
    assert_eq!((fetched["kind"].as_str(), fetched["fetched"].as_bool(), fetched["name"].as_str()), (Some("fetch"), Some(true), Some("logo.png")));
    assert_eq!(fetched["size"].as_u64(), Some(png.len() as u64));
    assert_eq!(crate::cmd::insert::base64_decode(fetched["data"].as_str().unwrap()).unwrap(), png);
    assert_eq!(s.rev(), rev, "the document is untouched");
    assert_eq!(s.doc.plain_text(wordcraft_doc::StoryRef::Body), "Stays");
    // The front end's own request is taken by id; the rest stays for edms.poll.
    let a = pending_id(&s.run("edms.drives", &json!({})).unwrap());
    let b = pending_id(&s.run("edms.drives", &json!({})).unwrap());
    transport.answer(a, HttpResponse::new(200).with_body(DRIVES));
    transport.answer(b, HttpResponse::new(200).with_body(DRIVES));
    s.edms_pump();
    assert_eq!(s.edms.take_result(b).and_then(|r| r["id"].as_u64()), Some(b));
    assert!(s.edms.take_result(b).is_none(), "taken once");
    let rest = s.run("edms.poll", &json!({})).unwrap();
    assert_eq!(rest["results"].as_array().map(Vec::len), Some(1));
    assert_eq!(rest["results"][0]["id"].as_u64(), Some(a));
}

/// `edms.open` needs nobody to hand the rights in: a stat follows the GET, and the document is
/// writable once it answers. A launcher names the place and is done.
#[test]
fn an_opened_document_gets_its_rights_from_a_stat_of_its_own() {
    let (mut s, transport) = connected();
    let id = pending_id(&s.run("edms.open", &json!({"drive": DRIVE, "path": "/craft-test.docx"})).unwrap());
    transport.answer(id, HttpResponse::new(200).with_header("ETag", "\"e1\"").with_body(docx_bytes("Hello EDMS")));
    poll_one(&mut s);
    assert_eq!(s.run("edms.status", &json!({})).unwrap()["document"]["writable"], json!(false), "no rights before the stat");
    let (stat_id, request) = transport.last_started().unwrap();
    assert_eq!(request.method, Method::Propfind);
    assert_eq!(request.header_value("depth"), Some("0"));
    assert!(request.url.ends_with("/craft-test.docx"));
    let multistatus = format!(
        "<?xml version=\"1.0\"?><d:multistatus xmlns:d=\"DAV:\" xmlns:oc=\"http://owncloud.org/ns\"><d:response>\
         <d:href>/dav/spaces/{DRIVE}/craft-test.docx</d:href><d:propstat><d:prop><d:getetag>\"e1\"</d:getetag>\
         <oc:fileid>{FILE_ID}</oc:fileid><oc:permissions>RDNVWZP</oc:permissions><oc:name>craft-test.docx</oc:name>\
         </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"
    );
    transport.answer(stat_id, HttpResponse::new(207).with_body(multistatus));
    let rights = poll_one(&mut s);
    assert_eq!((rights["kind"].as_str(), rights["rights"].as_bool(), rights["writable"].as_bool()), (Some("rights"), Some(true), Some(true)));
    let status = s.run("edms.status", &json!({})).unwrap();
    assert_eq!((status["document"]["writable"].as_bool(), status["document"]["id"].as_str()), (Some(true), Some(FILE_ID)));
}

/// An open that fails tells the front end, so a launch that only named the place shows why
/// nothing came.
#[test]
fn a_failed_open_is_reported_to_the_front_end() {
    let (mut s, transport) = connected();
    let id = pending_id(&s.run("edms.open", &json!({"drive": DRIVE, "path": "/missing.docx"})).unwrap());
    transport.answer(id, HttpResponse::new(404));
    let failed = poll_one(&mut s);
    assert_eq!(failed["ok"], json!(false));
    assert!(s.ui_requests.iter().any(|r| r["edms"]["event"] == "openFailed"));
}

/// kosmos: bytes that are no document (a PDF named .docx) are a failed open too, and the front
/// end learns which file it was.
#[test]
fn bytes_that_are_no_document_are_reported_as_a_failed_open() {
    let (mut s, transport) = connected();
    let id = pending_id(&s.run("edms.open", &json!({"drive": DRIVE, "path": "/Scan.docx"})).unwrap());
    transport.answer(id, HttpResponse::new(200).with_body(b"%PDF-1.7 not a zip".to_vec()));
    let failed = poll_one(&mut s);
    assert_eq!(failed["ok"], json!(false));
    let request = s.ui_requests.iter().find(|r| r["edms"]["event"] == "openFailed").expect("the front end is told");
    assert_eq!(request["edms"]["name"], json!("Scan.docx"));
}

#[test]
fn a_sidecar_is_fetched_relative_to_its_document_and_announced() {
    let (mut s, transport) = connected();
    assert!(s.run("edms.pwd", &json!({})).is_err(), "a document without a place has no working directory");
    assert!(s.run("edms.sidecar", &json!({"path": "logo.png"})).is_err());
    s.run("edms.adopt", &json!({"drive": DRIVE, "path": "/Projekte/2026/Bericht.docx", "etag": "\"h1\""})).unwrap();
    let document = s.document_id();
    let pwd = s.run("edms.pwd", &json!({})).unwrap();
    assert_eq!((pwd["drive"].as_str(), pwd["folder"].as_str(), pwd["document"].as_u64()), (Some(DRIVE), Some("/Projekte/2026"), Some(document)));

    assert!(s.run("edms.sidecar", &json!({"path": "/abs/logo.png"})).is_err());
    assert!(s.run("edms.sidecar", &json!({"path": "../../../logo.png"})).is_err(), "out of the space");
    assert!(s.run("edms.sidecar", &json!({"path": "logo.png", "document": document + 1000})).is_err(), "no such document");
    let first = s.run("edms.sidecar", &json!({"path": "bilder/logo.png", "document": document})).unwrap();
    assert_eq!(first["state"], json!("pending"));
    let (request_id, request) = transport.last_started().unwrap();
    assert_eq!(request.method, Method::Get);
    assert!(request.url.ends_with("/Projekte/2026/bilder/logo.png"), "{}", request.url);
    s.run("edms.sidecar", &json!({"path": "bilder/logo.png"})).unwrap();
    assert_eq!(transport.started().len(), 1, "asked twice, fetched once");

    transport
        .answer(request_id, HttpResponse::new(200).with_header("etag", "\"p1\"").with_header("content-type", "image/png").with_body(&b"PNG"[..]));
    let polled = s.run("edms.poll", &json!({})).unwrap();
    assert!(polled["results"].as_array().unwrap().is_empty(), "a sidecar is no command result: {polled}");
    assert!(s.ui_requests.iter().any(|r| r["edms"]["event"] == "sidecar" && r["edms"]["document"] == json!(document)), "{:?}", s.ui_requests);
    let ready = s.run("edms.sidecar", &json!({"path": "bilder/logo.png", "data": true})).unwrap();
    assert_eq!((ready["state"].as_str(), ready["size"].as_u64(), ready["data"].as_str()), (Some("ready"), Some(3), Some("UE5H")));
    assert_eq!(ready["contentType"], json!("image/png"));

    // A missing file is a state, not a command error; `refresh` tries again.
    s.run("edms.sidecar", &json!({"path": "fehlt.csv"})).unwrap();
    transport.answer(transport.last_started().unwrap().0, HttpResponse::new(404));
    s.edms_pump();
    let failed = s.run("edms.sidecar", &json!({"path": "fehlt.csv"})).unwrap();
    assert_eq!((failed["state"].as_str(), failed["error"]["code"].as_str()), (Some("failed"), Some("notFound")));
    assert_eq!(transport.started().len(), 2);
    assert_eq!(s.run("edms.sidecar", &json!({"path": "fehlt.csv", "refresh": true})).unwrap()["state"], json!("pending"));
    assert_eq!(transport.started().len(), 3);

    // Closing the document drops what it held.
    assert_eq!(s.edms.sidecar_bytes(), 3);
    s.close_document(document).unwrap();
    assert_eq!(s.edms.sidecar_bytes(), 0);
}

/// A 1×1 PNG.
const PIXEL: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01,
    0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
];

fn pictures(s: &mut Session) -> usize {
    crate::io::save_bytes("x.docx", &s.doc).map(|bytes| bytes.windows(4).filter(|w| *w == &PIXEL[..4]).count()).unwrap_or(0)
}

#[test]
fn a_command_runs_once_with_the_file_from_beside_the_document() {
    let (mut s, transport) = connected();
    s.run("edms.adopt", &json!({"drive": DRIVE, "path": "/Projekte/Bericht.docx", "etag": "\"h1\""})).unwrap();
    let document = s.document_id();

    // The fetch starts; the command has not run.
    let first = s.run("edms.runWith", &json!({"command": "insert.picture", "path": "bilder/pixel.png"})).unwrap();
    assert_eq!(
        (first["pending"].as_str(), first["path"].as_str(), first["document"].as_u64()),
        (Some("sidecar"), Some("bilder/pixel.png"), Some(document))
    );
    assert_eq!(s.edms.deferred(), 1);
    assert!(!s.dirty && s.last_command.is_none(), "nothing ran yet");
    let (request_id, request) = transport.last_started().unwrap();
    assert!(request.url.ends_with("/Projekte/bilder/pixel.png"), "{}", request.url);
    s.edms_pump();
    assert_eq!(s.edms.deferred(), 1, "nothing arrived yet");

    // The answer runs the command, once, with the bytes in it.
    transport.answer(request_id, HttpResponse::new(200).with_body(PIXEL));
    let done = poll_one(&mut s);
    assert_eq!(
        (done["kind"].as_str(), done["ok"].as_bool(), done["command"].as_str()),
        (Some("deferred"), Some(true), Some("insert.picture")),
        "{done}"
    );
    assert_eq!(s.edms.deferred(), 0);
    assert_eq!(pictures(&mut s), 1);
    let (ran, params) = s.last_command.clone().unwrap();
    assert_eq!(ran, "insert.picture");
    assert!(params["data"].as_str().is_some_and(|data| !data.is_empty()), "the command carries the bytes");
    s.run("edit.undo", &json!({})).unwrap();
    assert_eq!(pictures(&mut s), 0, "one command, one undo step");

    // From the cache the command runs at once.
    let again = s.run("edms.runWith", &json!({"command": "insert.picture", "path": "bilder/pixel.png"})).unwrap();
    assert_eq!(again["ran"], json!(true), "{again}");
    assert_eq!((pictures(&mut s), transport.started().len()), (1, 1));

    // A file that isn't there: the command never runs, the failure is loud, a new try fetches again.
    s.ui_requests.clear();
    s.run("edms.runWith", &json!({"command": "insert.picture", "path": "fehlt.png"})).unwrap();
    transport.answer(transport.last_started().unwrap().0, HttpResponse::new(404));
    let failed = poll_one(&mut s);
    assert_eq!((failed["kind"].as_str(), failed["ok"].as_bool()), (Some("deferred"), Some(false)));
    assert!(failed["error"]["message"].as_str().unwrap().contains("fehlt.png"), "{failed}");
    assert!(s.ui_requests.iter().any(|r| r["edms"]["event"] == "sidecarFailed"));
    assert_eq!((s.edms.deferred(), pictures(&mut s)), (0, 1));
    assert_eq!(s.run("edms.runWith", &json!({"command": "insert.picture", "path": "fehlt.png"})).unwrap()["pending"], json!("sidecar"));
    assert_eq!(transport.started().len(), 3);

    assert!(s.run("edms.runWith", &json!({"command": "no.such", "path": "a.png"})).is_err());
    assert!(s.run("edms.runWith", &json!({"command": "edms.runWith", "path": "a.png"})).is_err());
}

#[test]
fn the_other_parameters_and_the_name_of_the_bytes_are_the_callers() {
    let (mut s, transport) = connected();
    // A document outside the EDMS has no folder to read from; commands read paths as before.
    assert!(s.run("edms.runWith", &json!({"command": "insert.picture", "path": "a.png"})).is_err());
    let error = s.run("insert.picture", &json!({"path": "bilder/gibt-es-nicht.png"})).unwrap_err();
    assert!(!error.to_string().contains("EDMS"), "{error}");
    assert!(transport.started().is_empty());
    assert_eq!(s.edms.deferred(), 0);

    s.run("edms.adopt", &json!({"drive": DRIVE, "path": "/Serienbrief/Brief.docx", "etag": "\"h1\""})).unwrap();
    let first = s
        .run(
            "edms.runWith",
            &json!({"command": "mailings.recipients", "path": "daten/empfaenger.csv", "as": "data", "params": {"path": "empfaenger.csv"}}),
        )
        .unwrap();
    assert_eq!(first["pending"], json!("sidecar"), "{first}");
    let (request_id, request) = transport.last_started().unwrap();
    assert!(request.url.ends_with("/Serienbrief/daten/empfaenger.csv"), "{}", request.url);
    transport.answer(request_id, HttpResponse::new(200).with_body(&b"Name,Ort\nAda,Brandis\nBob,Leipzig\n"[..]));
    let done = poll_one(&mut s);
    assert_eq!(
        (done["kind"].as_str(), done["ok"].as_bool(), done["command"].as_str()),
        (Some("deferred"), Some(true), Some("mailings.recipients")),
        "{done}"
    );
    let again = s
        .run("edms.runWith", &json!({"command": "mailings.recipients", "path": "daten/empfaenger.csv", "params": {"path": "empfaenger.csv"}}))
        .unwrap();
    assert_eq!((again["ran"].as_bool(), again["result"]["records"].as_u64()), (Some(true), Some(2)), "{again}");
}

/// The parts of a docx, by name.
fn unzip(docx: &[u8]) -> Vec<(String, Vec<u8>)> {
    use std::io::Read;
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(docx)).unwrap();
    (0..archive.len())
        .map(|index| {
            let mut part = archive.by_index(index).unwrap();
            let mut bytes = Vec::new();
            part.read_to_end(&mut bytes).unwrap();
            (part.name().to_string(), bytes)
        })
        .collect()
}

fn zip_parts(parts: &[(String, Vec<u8>)]) -> Vec<u8> {
    use std::io::Write;
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, bytes) in parts {
        writer.start_file(name.as_str(), zip::write::SimpleFileOptions::default()).unwrap();
        writer.write_all(bytes).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

/// A docx with one picture whose `a:blip` is rewritten by `blip` (given the `r:embed="…"`
/// attribute as written) and that has an external relationship `rIdLink` to `bilder/logo.png`.
fn docx_with_linked_picture(blip: impl Fn(&str) -> String) -> Vec<u8> {
    let mut s = Session::new(blank_document(&wordcraft_geom::Region::default()));
    s.run("insert.picture", &json!({"data": crate::cmd::insert::base64_encode(PIXEL)})).unwrap();
    let mut parts = unzip(&crate::io::save_bytes("x.docx", &s.doc).unwrap());
    for (name, bytes) in &mut parts {
        let mut text = match name.as_str() {
            "word/document.xml" | "word/_rels/document.xml.rels" => String::from_utf8(bytes.clone()).unwrap(),
            _ => continue,
        };
        if name == "word/document.xml" {
            let start = text.find("r:embed=\"").expect("the picture's blip");
            let end = start + "r:embed=\"".len() + text[start + "r:embed=\"".len()..].find('"').unwrap() + 1;
            let embed = text[start..end].to_string();
            text.replace_range(start..end, &blip(&embed));
        } else {
            let link = r#"<Relationship Id="rIdLink" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="bilder/logo.png" TargetMode="External"/></Relationships>"#;
            text = text.replace("</Relationships>", link);
        }
        *bytes = text.into_bytes();
    }
    zip_parts(&parts)
}

/// What WordCraft does with linked pictures today (kosmos, finding of 2026-11-21): nothing is
/// fetched, a link without a copy is dropped at open, and a link beside a copy is not written
/// back. This pins the behaviour the EDMS sidecars meet; it is not a wish.
#[test]
fn linked_pictures_are_not_kept_only_embedded_ones() {
    // Link only: no picture, reported as missing with the link's target.
    let link_only = docx_with_linked_picture(|_| r#"r:link="rIdLink""#.to_string());
    let doc = crate::io::open_bytes("x.docx", &link_only).unwrap();
    assert!(doc.media.is_empty(), "{:?}", doc.media.keys());
    let missing: Vec<_> = doc.missing_pictures.list.iter().map(|m| (m.part.as_str(), m.reason)).collect();
    assert_eq!(missing, [("bilder/logo.png", wordcraft_doc::MissingPictureReason::LinkOnly)]);

    // Link and copy: the copy is the picture; the link is gone after a save.
    let both = docx_with_linked_picture(|embed| format!(r#"{embed} r:link="rIdLink""#));
    let doc = crate::io::open_bytes("x.docx", &both).unwrap();
    assert_eq!((doc.media.len(), doc.missing_pictures.list.len()), (1, 0));
    let saved = unzip(&crate::io::save_bytes("x.docx", &doc).unwrap());
    let written: String = saved
        .iter()
        .filter(|(name, _)| name.ends_with(".xml") || name.ends_with(".rels"))
        .map(|(_, bytes)| String::from_utf8_lossy(bytes).into_owned())
        .collect();
    assert!(written.contains("r:embed=") && !written.contains("r:link=") && !written.contains("bilder/logo.png"), "the link survived");
}
