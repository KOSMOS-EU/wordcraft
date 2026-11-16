//! Craft Signal in the session (kosmos) against the client's recording transport: nothing touches
//! the network; the relay's frames are played in by hand.

use craft_signal::frame::{Frame, FrameType};
use craft_signal::transport::Event;
use craft_signal::wire::{self, MasterPayload, OpPayload, WelcomePayload};
use craft_signal::{Client, RecordingTransport};
use serde_json::{Value, json};

use crate::{Session, blank_document};

fn session() -> (Session, RecordingTransport) {
    let transport = RecordingTransport::default();
    let mut s = Session::new(blank_document(&wordcraft_geom::Region::default()));
    s.signal.install(Box::new(Client::new(transport.clone())));
    (s, transport)
}

/// Connect and play the relay's answers until the session is in: `id`, `rank` 0 = master.
fn joined(s: &mut Session, transport: &RecordingTransport, id: u16, rank: u16, master: u16, seq: u32) {
    s.run("signal.connect", &json!({"relay": "http://127.0.0.1:8787", "doc": "space!file", "auth": "Bearer anna", "user": "Anna"})).unwrap();
    transport.push(Event::Ticket(Ok(r#"{"ticket":"t1","maxMode":1}"#.into())));
    s.signal_pump();
    transport.push(Event::Opened);
    s.signal_pump();
    let welcome = WelcomePayload {
        rank,
        master,
        mode: 1,
        app: "wordcraft".into(),
        app_version: super::super::cmd::signal::APP_VERSION.into(),
        reason: String::new(),
        lock: if rank == 0 { "L".into() } else { String::new() },
    };
    let mut frame = Frame::new(FrameType::Welcome).with_payload(wire::encode(&welcome).unwrap());
    frame.session = 1;
    frame.sender = id;
    frame.seq = seq;
    transport.deliver(frame);
    s.signal_pump();
    transport.take_sent();
    let status = s.run("signal.status", &json!({})).unwrap();
    assert_eq!(status["state"], json!("joined"), "{status}");
}

fn text(s: &Session) -> String {
    s.doc.plain_text(wordcraft_doc::StoryRef::Body)
}

#[test]
fn without_a_transport_the_commands_refuse_and_the_status_says_so() {
    let mut s = Session::new(blank_document(&wordcraft_geom::Region::default()));
    assert!(s.run("signal.connect", &json!({"relay": "http://x", "doc": "d"})).is_err());
    let status = s.run("signal.status", &json!({})).unwrap();
    assert_eq!((status["transport"].as_bool(), status["state"].as_str()), (Some(false), Some("disconnected")));
    let (mut s, _) = session();
    assert!(s.run("signal.connect", &json!({"relay": "ftp://x", "doc": "d"})).is_err(), "url scheme");
    assert!(s.run("signal.connect", &json!({"relay": "http://x"})).is_err(), "doc required");
    // Not joined: editing stays local, nothing is sent.
    s.run("text.insert", &json!({"text": "Local"})).unwrap();
    assert_eq!(text(&s), "Local");
}

/// As the master, every edit goes out as a COMMIT with the next seq, carrying command, params
/// and the selection before the command.
#[test]
fn the_master_commits_its_own_edits() {
    let (mut s, transport) = session();
    joined(&mut s, &transport, 1, 0, 1, 0);
    assert_eq!(s.run("signal.status", &json!({})).unwrap()["isMaster"], json!(true));
    s.run("text.insert", &json!({"text": "Hello"})).unwrap();
    s.run("format.bold", &json!({})).unwrap_or_default();
    let sent = transport.take_sent();
    let commits: Vec<&Frame> = sent.iter().filter(|f| f.kind == FrameType::Commit).collect();
    assert!(!commits.is_empty(), "{sent:?}");
    let first: OpPayload = wire::decode(&commits[0].payload).unwrap();
    assert_eq!((first.command.as_str(), &first.params, first.author), ("text.insert", &json!({"text": "Hello"}), 1));
    assert_eq!(first.anchor["focus"]["story"], json!("body"), "{}", first.anchor);
    assert_eq!(commits[0].seq, 1);
    // A command that changed nothing is no proposal, nor is a file or signal command.
    s.run("signal.status", &json!({})).unwrap();
    s.run("caret.end", &json!({})).unwrap_or_default();
    assert!(transport.take_sent().iter().all(|f| f.kind != FrameType::Commit));
    let polled = s.run("signal.poll", &json!({})).unwrap();
    assert!(polled["events"].as_array().unwrap().iter().any(|e| e["event"] == "confirmed"), "{polled}");
}

/// As a collaborator, an edit goes out as an OP and stays pending; the master's COMMITs of
/// others' edits are applied with their anchor while the own caret stays put.
#[test]
fn a_collaborator_proposes_and_applies_the_masters_commits() {
    let (mut s, transport) = session();
    joined(&mut s, &transport, 2, 1, 1, 0);
    s.run("text.insert", &json!({"text": "Mine"})).unwrap();
    let op = transport.take_sent().into_iter().find(|f| f.kind == FrameType::Op).expect("an OP");
    let proposed: OpPayload = wire::decode(&op.payload).unwrap();
    assert_eq!(proposed.params, json!({"text": "Mine"}));
    assert_eq!(s.run("signal.status", &json!({})).unwrap()["pending"], json!(1));
    // The master's commit of someone else's insert at the start of the body.
    let theirs = OpPayload {
        command: "text.insert".into(),
        params: json!({"text": "Theirs "}),
        anchor: serde_json::to_value(crate::Selection::caret(wordcraft_doc::Pos::body(0, 0))).unwrap(),
        time: 1,
        author: 1,
    };
    let caret_before = s.sel.clone();
    let mut commit = Frame::new(FrameType::Commit).with_payload(wire::encode(&theirs).unwrap());
    commit.sender = 1;
    commit.seq = 1;
    commit.local = 9;
    transport.deliver(commit);
    s.signal_pump();
    assert_eq!(text(&s), "Theirs Mine");
    assert_eq!(s.sel, caret_before, "the own selection is kept (clamped)");
    assert!(transport.take_sent().iter().all(|f| f.kind != FrameType::Op), "applying a commit proposes nothing");
    // The own commit confirms the pending op.
    let mut mine = Frame::new(FrameType::Commit).with_payload(op.payload.clone());
    mine.sender = 2;
    mine.seq = 2;
    mine.local = op.local;
    transport.deliver(mine);
    let status = s.run("signal.status", &json!({})).unwrap();
    assert_eq!(
        (status["pending"].as_u64(), status["confirmed"].as_u64(), status["applied"].as_u64(), status["seq"].as_u64()),
        (Some(0), Some(1), Some(1), Some(2))
    );
    // A NAK is reported, not rolled back (first stage).
    s.run("text.insert", &json!({"text": "!"})).unwrap();
    let op2 = transport.take_sent().into_iter().find(|f| f.kind == FrameType::Op).unwrap();
    let mut nak = Frame::new(FrameType::Nak).with_payload(b"stale".to_vec());
    nak.sender = 2;
    nak.local = op2.local;
    transport.deliver(nak);
    s.signal_pump();
    let status = s.run("signal.status", &json!({})).unwrap();
    assert_eq!(status["rejected"], json!(1));
    assert!(std::mem::take(&mut s.ui_requests).iter().any(|r| r["signal"]["event"] == "rejected"));
}

/// As the master, a proposal of another participant is applied and committed with time and
/// author; a proposal that can't be applied is rejected to its sender.
#[test]
fn the_master_applies_and_commits_proposals() {
    let (mut s, transport) = session();
    joined(&mut s, &transport, 1, 0, 1, 3);
    s.run("text.insert", &json!({"text": "Base"})).unwrap();
    transport.take_sent();
    let proposal = OpPayload {
        command: "text.insert".into(),
        params: json!({"text": " from B"}),
        anchor: serde_json::to_value(&s.sel).unwrap(),
        time: 0,
        author: 0,
    };
    let mut op = Frame::new(FrameType::Op).with_payload(wire::encode(&proposal).unwrap());
    op.sender = 2;
    op.local = 5;
    op.base = 4;
    transport.deliver(op);
    s.signal_pump();
    assert_eq!(text(&s), "Base from B");
    let commit = transport.take_sent().into_iter().find(|f| f.kind == FrameType::Commit).expect("a COMMIT");
    assert_eq!((commit.sender, commit.local, commit.seq), (2, 5, 5));
    let committed: OpPayload = wire::decode(&commit.payload).unwrap();
    assert_eq!(committed.author, 2);
    assert!(committed.time > 0, "the master stamps the time");
    // A command outside the collaboration (a file command) is rejected to the sender.
    let bad = OpPayload { command: "file.new".into(), params: Value::Null, anchor: Value::Null, time: 0, author: 0 };
    let mut op = Frame::new(FrameType::Op).with_payload(wire::encode(&bad).unwrap());
    op.sender = 2;
    op.local = 6;
    transport.deliver(op);
    s.signal_pump();
    let nak = transport.take_sent().into_iter().find(|f| f.kind == FrameType::Nak).expect("a NAK");
    assert_eq!((nak.sender, nak.local), (2, 6));
    assert_eq!(text(&s), "Base from B", "nothing changed");
}

/// A master change hands the lock over and returns pending commands as rejected; a closed
/// connection leaves the session local again.
#[test]
fn master_changes_and_closures_reach_the_front_end() {
    let (mut s, transport) = session();
    joined(&mut s, &transport, 2, 1, 1, 0);
    s.run("text.insert", &json!({"text": "Pending"})).unwrap();
    transport.take_sent();
    let mut master = Frame::new(FrameType::Master).with_payload(wire::encode(&MasterPayload { master: 2, lock: "L2".into() }).unwrap());
    master.seq = 7;
    transport.deliver(master);
    s.signal_pump();
    let status = s.run("signal.status", &json!({})).unwrap();
    assert_eq!((status["isMaster"].as_bool(), status["seq"].as_u64(), status["rejected"].as_u64()), (Some(true), Some(7), Some(1)));
    let requests = std::mem::take(&mut s.ui_requests);
    assert!(requests.iter().any(|r| r["signal"]["event"] == "master" && r["signal"]["me"] == json!(true)), "{requests:?}");
    // Now the master: the next edit is a COMMIT with seq 8.
    s.run("text.insert", &json!({"text": "!"})).unwrap();
    let commit = transport.take_sent().into_iter().find(|f| f.kind == FrameType::Commit).unwrap();
    assert_eq!(commit.seq, 8);
    transport.push(Event::Closed("relay gone".into()));
    s.signal_pump();
    assert_eq!(s.run("signal.status", &json!({})).unwrap()["state"], json!("disconnected"));
    assert!(std::mem::take(&mut s.ui_requests).iter().any(|r| r["signal"]["event"] == "closed"));
    s.run("text.insert", &json!({"text": "?"})).unwrap();
    assert!(transport.take_sent().is_empty(), "local again");
    assert_eq!(s.run("signal.disconnect", &json!({})).unwrap()["connected"], json!(false));
}
