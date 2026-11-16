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
    // A NAK takes the own command back: the snapshot from before it comes back.
    s.run("text.insert", &json!({"text": "!"})).unwrap();
    assert_eq!(s.run("signal.status", &json!({})).unwrap()["journal"], json!(1), "the pending proposal is journaled");
    let op2 = transport.take_sent().into_iter().find(|f| f.kind == FrameType::Op).unwrap();
    let mut nak = Frame::new(FrameType::Nak).with_payload(b"stale".to_vec());
    nak.sender = 2;
    nak.local = op2.local;
    transport.deliver(nak);
    s.signal_pump();
    let status = s.run("signal.status", &json!({})).unwrap();
    assert_eq!((status["rejected"].as_u64(), status["rolledBack"].as_u64(), status["journal"].as_u64()), (Some(1), Some(1), Some(0)));
    assert_eq!(text(&s), "Theirs Mine", "the ! is gone");
    let requests = std::mem::take(&mut s.ui_requests);
    assert!(requests.iter().any(|r| r["signal"]["event"] == "rejected" && r["signal"]["undone"] == json!(true)), "{requests:?}");
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
    // The unconfirmed "Pending" was taken back and, as the new master, committed with seq 8.
    let status = s.run("signal.status", &json!({})).unwrap();
    assert_eq!(
        (status["isMaster"].as_bool(), status["seq"].as_u64(), status["rejected"].as_u64(), status["resent"].as_u64(), status["pending"].as_u64()),
        (Some(true), Some(8), Some(0), Some(1), Some(0)),
        "{status}"
    );
    assert_eq!(text(&s), "Pending");
    let resent = transport.take_sent().into_iter().find(|f| f.kind == FrameType::Commit).expect("the resent command as a COMMIT");
    assert_eq!(resent.seq, 8);
    let resent: OpPayload = wire::decode(&resent.payload).unwrap();
    assert_eq!(resent.params, json!({"text": "Pending"}));
    let requests = std::mem::take(&mut s.ui_requests);
    assert!(
        requests.iter().any(|r| r["signal"]["event"] == "master" && r["signal"]["me"] == json!(true) && r["signal"]["resent"] == json!(1)),
        "{requests:?}"
    );
    // Now the master: the next edit is a COMMIT with seq 9.
    s.run("text.insert", &json!({"text": "!"})).unwrap();
    let commit = transport.take_sent().into_iter().find(|f| f.kind == FrameType::Commit).unwrap();
    assert_eq!(commit.seq, 9);
    transport.push(Event::Closed("relay gone".into()));
    s.signal_pump();
    assert_eq!(s.run("signal.status", &json!({})).unwrap()["state"], json!("disconnected"));
    assert!(std::mem::take(&mut s.ui_requests).iter().any(|r| r["signal"]["event"] == "closed"));
    s.run("text.insert", &json!({"text": "?"})).unwrap();
    assert!(transport.take_sent().is_empty(), "local again");
    assert_eq!(s.run("signal.disconnect", &json!({})).unwrap()["connected"], json!(false));
}

/// A proposal from the master's side of the table: `op` by `sender` on `base`, anchored at
/// paragraph `block` of the body.
fn proposal(sender: u16, local: u32, base: u32, block: usize, command: &str, params: Value) -> Frame {
    let op = OpPayload {
        command: command.into(),
        params,
        anchor: serde_json::to_value(crate::Selection::caret(wordcraft_doc::Pos::body(block, 0))).unwrap(),
        time: 0,
        author: 0,
    };
    let mut frame = Frame::new(FrameType::Op).with_payload(wire::encode(&op).unwrap());
    frame.sender = sender;
    frame.local = local;
    frame.base = base;
    frame
}

fn answer(transport: &RecordingTransport, local: u32) -> FrameType {
    let sent = transport.take_sent();
    let frame = sent.iter().find(|f| f.kind == FrameType::Commit && f.local == local || f.kind == FrameType::Nak && f.local == local);
    frame.map(|f| f.kind).unwrap_or_else(|| panic!("no answer to local {local}: {sent:?}"))
}

/// The master's conflict rule: a stale base is fine unless someone else's commit since then
/// touched the same paragraph; the sender's own commits never conflict; a commit that changed
/// the paragraph count moves everything after it.
#[test]
fn the_master_rejects_a_stale_proposal_only_on_overlap() {
    let (mut s, transport) = session();
    joined(&mut s, &transport, 1, 0, 1, 0);
    // The master's own edit is seq 1 in paragraph 0.
    s.run("text.insert", &json!({"text": "Base"})).unwrap();
    transport.take_sent();
    assert_eq!(s.run("signal.status", &json!({})).unwrap()["traces"], json!(1));
    // Participant 2 on base 0 at paragraph 0: overlaps seq 1.
    transport.deliver(proposal(2, 1, 0, 0, "text.insert", json!({"text": "x"})));
    s.signal_pump();
    assert_eq!(answer(&transport, 1), FrameType::Nak);
    assert_eq!(text(&s), "Base");
    // The same on the current base: committed as seq 2.
    transport.deliver(proposal(2, 2, 1, 0, "text.insert", json!({"text": "x"})));
    s.signal_pump();
    assert_eq!(answer(&transport, 2), FrameType::Commit);
    assert_eq!(text(&s), "xBase");
    // Participant 2 again on base 1 (it hasn't seen its own commit yet): its own seq 2 is no conflict.
    transport.deliver(proposal(2, 3, 1, 0, "text.insert", json!({"text": "y"})));
    s.signal_pump();
    assert_eq!(answer(&transport, 3), FrameType::Commit);
    // Participant 3 on base 1 at paragraph 0: conflicts with participant 2's seq 2.
    transport.deliver(proposal(3, 1, 1, 0, "text.insert", json!({"text": "z"})));
    s.signal_pump();
    assert_eq!(answer(&transport, 1), FrameType::Nak);
    // A second paragraph (seq 4, structural, at paragraph 0).
    transport.deliver(proposal(2, 4, 3, 0, "text.newParagraph", json!({})));
    s.signal_pump();
    assert_eq!(answer(&transport, 4), FrameType::Commit);
    assert_eq!(s.doc.paragraph_count(), 2);
    // Participant 3 on base 3 at paragraph 1: nothing of its own there, but seq 4 moved it.
    transport.deliver(proposal(3, 2, 3, 1, "text.insert", json!({"text": "z"})));
    s.signal_pump();
    assert_eq!(answer(&transport, 2), FrameType::Nak);
    // Participant 3 on base 4 at paragraph 1, then participant 2 on base 4 at paragraph 0: no
    // overlap, paragraph 1's edit is after paragraph 0 and not structural.
    transport.deliver(proposal(3, 3, 4, 1, "text.insert", json!({"text": "second"})));
    s.signal_pump();
    assert_eq!(answer(&transport, 3), FrameType::Commit);
    transport.deliver(proposal(2, 5, 4, 0, "text.insert", json!({"text": "first "})));
    s.signal_pump();
    assert_eq!(answer(&transport, 5), FrameType::Commit);
    assert_eq!(text(&s), "first \nsecondyxBase");
    // Without an anchor a stale proposal can't be judged: rejected.
    let bad = OpPayload { command: "text.insert".into(), params: json!({"text": "?"}), anchor: Value::Null, time: 0, author: 0 };
    let mut frame = Frame::new(FrameType::Op).with_payload(wire::encode(&bad).unwrap());
    frame.sender = 3;
    frame.local = 4;
    frame.base = 5;
    transport.deliver(frame);
    s.signal_pump();
    assert_eq!(answer(&transport, 4), FrameType::Nak);
    let log = s.run("signal.poll", &json!({})).unwrap();
    let reasons: Vec<&str> = log["events"].as_array().unwrap().iter().filter_map(|e| e["reason"].as_str()).filter(|r| !r.is_empty()).collect();
    assert_eq!(reasons.len(), 4, "{reasons:?}");
    assert!(
        reasons[0].contains("seq 1") && reasons[1].contains("seq 2") && reasons[2].contains("seq 4") && reasons[3].contains("seq 6"),
        "{reasons:?}"
    );
}

/// A rejected proposal is taken back and what came after it (others' commits, own later
/// proposals) replayed on the restored state.
#[test]
fn a_nak_restores_the_snapshot_and_replays_the_journal() {
    let (mut s, transport) = session();
    joined(&mut s, &transport, 2, 1, 1, 0);
    s.run("text.insert", &json!({"text": "Mine"})).unwrap();
    let op1 = transport.take_sent().into_iter().find(|f| f.kind == FrameType::Op).unwrap();
    // The master's commit of participant 1's "Theirs" after it.
    let theirs = OpPayload {
        command: "text.insert".into(),
        params: json!({"text": "Theirs"}),
        anchor: serde_json::to_value(crate::Selection::caret(wordcraft_doc::Pos::body(0, 4))).unwrap(),
        time: 1,
        author: 1,
    };
    let mut commit = Frame::new(FrameType::Commit).with_payload(wire::encode(&theirs).unwrap());
    commit.sender = 1;
    commit.seq = 1;
    transport.deliver(commit);
    s.signal_pump();
    assert_eq!(text(&s), "MineTheirs");
    // A second own proposal at the start.
    s.run("caret.home", &json!({})).unwrap();
    s.run("text.insert", &json!({"text": "!"})).unwrap();
    transport.take_sent();
    assert_eq!(text(&s), "!MineTheirs");
    assert_eq!(s.run("signal.status", &json!({})).unwrap()["journal"], json!(3));
    // The master rejects the first: back to before "Mine", then "Theirs" (clamped to the start)
    // and "!" again.
    let mut nak = Frame::new(FrameType::Nak).with_payload(b"base 0: conflicts with seq 1 of participant 1".to_vec());
    nak.sender = 2;
    nak.local = op1.local;
    transport.deliver(nak);
    s.signal_pump();
    assert_eq!(text(&s), "!Theirs");
    let status = s.run("signal.status", &json!({})).unwrap();
    assert_eq!(
        (status["rejected"].as_u64(), status["rolledBack"].as_u64(), status["pending"].as_u64(), status["journal"].as_u64()),
        (Some(1), Some(1), Some(1), Some(1)),
        "{status} (the journal starts at the oldest pending proposal)"
    );
    let log = s.run("signal.poll", &json!({})).unwrap();
    let rejected = log["events"].as_array().unwrap().iter().find(|e| e["event"] == "rejected").unwrap();
    assert_eq!((rejected["undone"].as_bool(), rejected["replayed"].as_u64()), (Some(true), Some(2)), "{rejected}");
    // Nothing was proposed again by the replay.
    assert!(transport.take_sent().iter().all(|f| f.kind != FrameType::Op));
    // The confirmation of "!" empties the journal.
    let mut mine = Frame::new(FrameType::Commit).with_payload(
        wire::encode(&OpPayload { command: "text.insert".into(), params: json!({"text": "!"}), anchor: Value::Null, time: 2, author: 2 }).unwrap(),
    );
    mine.sender = 2;
    mine.seq = 2;
    mine.local = 2;
    transport.deliver(mine);
    s.signal_pump();
    let status = s.run("signal.status", &json!({})).unwrap();
    assert_eq!((status["pending"].as_u64(), status["journal"].as_u64()), (Some(0), Some(0)), "{status}");
    assert_eq!(text(&s), "!Theirs");
}

/// A master change with two pending proposals and a foreign commit in between: both are taken
/// back and proposed again, in order, on the state with the commit; the old NAKs are expected.
#[test]
fn a_master_change_resends_the_pending_proposals_in_order() {
    let (mut s, transport) = session();
    joined(&mut s, &transport, 2, 1, 1, 0);
    s.run("text.insert", &json!({"text": "A"})).unwrap();
    let theirs = OpPayload {
        command: "text.insert".into(),
        params: json!({"text": "X"}),
        anchor: serde_json::to_value(crate::Selection::caret(wordcraft_doc::Pos::body(0, 0))).unwrap(),
        time: 1,
        author: 1,
    };
    let mut commit = Frame::new(FrameType::Commit).with_payload(wire::encode(&theirs).unwrap());
    commit.sender = 1;
    commit.seq = 1;
    transport.deliver(commit);
    s.signal_pump();
    s.run("text.insert", &json!({"text": "B"})).unwrap();
    // The own caret stayed at 1 while "X" came in before it (anchors are not transformed yet).
    assert_eq!(text(&s), "XBA");
    transport.take_sent();
    // Participant 3 becomes the master; the session stays a collaborator.
    let mut master = Frame::new(FrameType::Master).with_payload(wire::encode(&MasterPayload { master: 3, lock: String::new() }).unwrap());
    master.seq = 1;
    transport.deliver(master);
    s.signal_pump();
    let sent = transport.take_sent();
    let ops: Vec<OpPayload> = sent.iter().filter(|f| f.kind == FrameType::Op).map(|f| wire::decode(&f.payload).unwrap()).collect();
    assert_eq!(ops.iter().map(|o| o.params["text"].as_str().unwrap()).collect::<Vec<_>>(), vec!["A", "B"], "{sent:?}");
    assert!(sent.iter().filter(|f| f.kind == FrameType::Op).all(|f| f.base == 1), "proposed on the agreed state");
    // Built again on the agreed state: A at its anchor 0 of "X", B at its anchor 1. That is
    // what the new master computes from the two proposals too.
    assert_eq!(text(&s), "ABX");
    let status = s.run("signal.status", &json!({})).unwrap();
    assert_eq!(
        (status["isMaster"].as_bool(), status["rejected"].as_u64(), status["resent"].as_u64(), status["pending"].as_u64()),
        (Some(false), Some(0), Some(2), Some(2)),
        "{status}"
    );
    let log = s.run("signal.poll", &json!({})).unwrap();
    assert_eq!(log["events"].as_array().unwrap().iter().filter(|e| e["event"] == "resent").count(), 2, "{log}");
}
