//! The EDMS commands (kosmos) against a recording transport: nothing touches the network, the
//! answers are what cloud.xwork.cloud answered on 2026-11-16 (fixtures in craft-edms).

use craft_edms::{Client, HttpResponse, Method, RecordingTransport};
use serde_json::{Value, json};

use crate::{Session, blank_document};

const DRIVE: &str = "b894e89e-a7fd-4385-92fc-b5e0bb2659fd$8c9cde70-836d-4eda-81bf-99102e838af1";
const FILE_ID: &str = "b894e89e-a7fd-4385-92fc-b5e0bb2659fd$8c9cde70-836d-4eda-81bf-99102e838af1!f820fbb2-e279-4fa4-9938-c63d07c733f8";
const DRIVES: &[u8] = include_bytes!("../../../../../opencraft/crates/craft-edms/tests/fixtures/xwork/drives.json");

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
