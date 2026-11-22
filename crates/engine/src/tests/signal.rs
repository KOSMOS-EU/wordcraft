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

/// Connect and play the relay's answers until the session is in: `id`, `rank` 0 = master. Who
/// joins a session someone else leads asks for the master's document first; here the master
/// has the same document, and its answer is played in ([`joined_waiting_for_the_state`] stops
/// before that).
fn joined(s: &mut Session, transport: &RecordingTransport, id: u16, rank: u16, master: u16, seq: u32) {
    joined_waiting_for_the_state(s, transport, id, rank, master, seq);
    if id != master {
        let state = crate::cmd::edms::copy_bytes(s, "state.docx").unwrap();
        answer_with_state(s, transport, id, seq, &state);
        s.signal.take_log();
        s.ui_requests.clear();
    }
    let status = s.run("signal.status", &json!({})).unwrap();
    assert_eq!((status["state"].as_str(), status["syncing"].as_bool()), (Some("joined"), Some(false)), "{status}");
}

/// The master's answer to the sync request of `id`: the state on the bulk channel, the `SYNC`
/// naming it on the control channel.
fn answer_with_state(s: &mut Session, transport: &RecordingTransport, id: u16, seq: u32, state: &[u8]) {
    deliver_blob(transport, state);
    let payload = craft_signal::SyncPayload { format: "docx".into(), state: Attachment::whole("state", state) };
    let mut answer = Frame::new(FrameType::Sync).with_payload(wire::encode(&payload).unwrap());
    answer.session = 1;
    answer.sender = id;
    answer.seq = seq;
    transport.deliver(answer);
    s.signal_pump();
}

fn joined_waiting_for_the_state(s: &mut Session, transport: &RecordingTransport, id: u16, rank: u16, master: u16, seq: u32) {
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
    let sent = transport.take_sent();
    assert_eq!(
        sent.iter().filter(|frame| frame.kind == FrameType::SyncReq).count(),
        usize::from(id != master),
        "only who joins someone else's session asks for its state"
    );
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
        attachments: Vec::new(),
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
        attachments: Vec::new(),
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
    let bad = OpPayload { command: "file.new".into(), params: Value::Null, anchor: Value::Null, time: 0, author: 0, attachments: Vec::new() };
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
        attachments: Vec::new(),
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
    let bad =
        OpPayload { command: "text.insert".into(), params: json!({"text": "?"}), anchor: Value::Null, time: 0, author: 0, attachments: Vec::new() };
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
        attachments: Vec::new(),
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
        wire::encode(&OpPayload {
            command: "text.insert".into(),
            params: json!({"text": "!"}),
            anchor: Value::Null,
            time: 2,
            author: 2,
            attachments: Vec::new(),
        })
        .unwrap(),
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
        attachments: Vec::new(),
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

// --- Large content: the preview before the confirmation (SIGNAL.md, bulk channel) ---

use craft_signal::Attachment;
use craft_signal::blobs::{self, MAX_PREVIEW_BYTES, PART_BYTES};

/// Bring the bulk channel of a joined session up.
fn bulk_ready(s: &mut Session, transport: &RecordingTransport) {
    transport.push(Event::BulkOpened);
    s.signal_pump();
    transport.deliver_bulk(Frame::new(FrameType::Welcome));
    s.signal_pump();
    transport.take_bulk_sent();
}

/// The frames the relay hands a blob on with.
fn deliver_blob(transport: &RecordingTransport, bytes: &[u8]) {
    let hash = blobs::hash_of(bytes);
    for (index, part) in bytes.chunks(PART_BYTES).enumerate() {
        transport.deliver_bulk(Frame::new(FrameType::BlobPart).with_payload(blobs::encode_part(&hash, (index * PART_BYTES) as u64, part)));
    }
    transport.deliver_bulk(Frame::new(FrameType::BlobDone).with_payload(hash.to_vec()));
}

fn commit_frame(sender: u16, local: u32, seq: u32, op: &OpPayload) -> Frame {
    let mut frame = Frame::new(FrameType::Commit).with_payload(wire::encode(op).unwrap());
    frame.session = 1;
    frame.sender = sender;
    frame.local = local;
    frame.seq = seq;
    frame
}

#[test]
fn a_command_too_big_for_the_control_channel_sends_its_content_as_a_blob() {
    let (mut s, transport) = session();
    joined(&mut s, &transport, 2, 1, 1, 0);
    bulk_ready(&mut s, &transport);

    // Fits a preview: the blob is its own preview.
    let pasted = "Lorem ipsum dolor sit amet. ".repeat(100);
    s.run("text.insert", &json!({"text": pasted})).unwrap();
    let bulk = transport.take_bulk_sent();
    assert_eq!(bulk.iter().map(|f| f.kind).collect::<Vec<_>>(), [FrameType::BlobPart, FrameType::BlobDone]);
    let sent = transport.take_sent();
    assert_eq!((sent.len(), sent[0].kind), (1, FrameType::Op));
    let op: OpPayload = wire::decode(&sent[0].payload).unwrap();
    assert!(op.params.get("text").is_none(), "{}", op.params);
    assert_eq!(op.attachments, [Attachment::original("text", pasted.as_bytes())]);

    // Past the preview limit: the whole blob first, then the command counts.
    let long = "x".repeat(MAX_PREVIEW_BYTES + 1);
    s.run("text.insert", &json!({"text": long})).unwrap();
    let op: OpPayload = wire::decode(&transport.take_sent()[0].payload).unwrap();
    assert_eq!(op.attachments, [Attachment::whole("text", long.as_bytes())]);
    assert_eq!(transport.take_bulk_sent().len(), 3);

    // A small command goes as before.
    s.run("text.insert", &json!({"text": "!"})).unwrap();
    let op: OpPayload = wire::decode(&transport.take_sent()[0].payload).unwrap();
    assert!(op.attachments.is_empty() && op.params["text"] == json!("!"));
    assert!(transport.take_bulk_sent().is_empty());
}

#[test]
fn a_confirmed_command_gets_its_content_back_from_the_blob() {
    let (mut s, transport) = session();
    joined(&mut s, &transport, 2, 1, 1, 0);
    bulk_ready(&mut s, &transport);
    let pasted = "Lorem ipsum dolor sit amet. ".repeat(100);
    let theirs =
        OpPayload { command: "text.insert".into(), attachments: vec![Attachment::original("text", pasted.as_bytes())], ..OpPayload::default() };

    // The commit overtakes the blob: nothing is applied yet, the blob is asked for.
    transport.deliver(commit_frame(3, 1, 1, &theirs));
    s.signal_pump();
    assert_eq!(text(&s), "");
    assert_eq!(transport.take_bulk_sent()[0].kind, FrameType::BlobReq);
    deliver_blob(&transport, pasted.as_bytes());
    s.signal_pump();
    assert_eq!(text(&s), pasted);
    assert!(s.signal.outstanding().is_empty());
}

#[test]
fn with_content_still_on_its_way_saving_needs_a_second_yes_and_a_copy_says_what_is_missing() {
    use craft_edms::{Client as EdmsClient, RecordingTransport as EdmsTransport};
    let (mut s, transport) = session();
    let edms = EdmsTransport::default();
    s.edms.install(Box::new(EdmsClient::new(edms.clone())));
    s.run("edms.connect", &json!({"origin": "https://cloud.xwork.cloud", "token": "tok", "initiatorId": "client-1"})).unwrap();
    s.run("edms.adopt", &json!({"drive": "d1", "path": "/Bericht.docx", "etag": "\"h1\""})).unwrap();
    joined(&mut s, &transport, 1, 0, 1, 0);
    bulk_ready(&mut s, &transport);

    // Another participant's picture: its preview is here, the picture itself is not.
    const PIXEL: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
        0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01,
        0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];
    let preview = crate::cmd::insert::base64_encode(PIXEL);
    let original = "the big picture, as base64, still on its way".repeat(4000);
    let attachment = Attachment::represented("data", original.as_bytes(), preview.as_bytes(), "image/png", (800, 600));
    let picture = OpPayload { command: "insert.picture".into(), attachments: vec![attachment], ..OpPayload::default() };
    deliver_blob(&transport, preview.as_bytes());
    let mut proposal = commit_frame(2, 1, 0, &picture);
    proposal.kind = FrameType::Op;
    transport.deliver(proposal);
    s.signal_pump();
    assert_eq!(s.doc.media.len(), 1, "shown as its preview");
    assert_eq!(transport.take_sent().iter().map(|f| f.kind).collect::<Vec<_>>(), [FrameType::Commit]);
    assert_eq!(s.signal.outstanding().len(), 1);

    let refused = s.run("edms.save", &json!({})).unwrap_err().to_string();
    assert!(refused.contains("still on its way"), "{refused}");
    assert!(s.ui_requests.iter().any(|r| r["edms"]["event"] == "saveIncomplete" && r["edms"]["missing"][0]["command"] == "insert.picture"));
    assert!(edms.started().is_empty(), "nothing was written");
    assert!(s.run("edms.save", &json!({"incomplete": true})).unwrap()["pending"].is_u64());

    let copy = s.run("edms.saveAs", &json!({"drive": "d1", "path": "/", "name": "Kopie.docx", "switch": false})).unwrap();
    assert_eq!((copy["incomplete"][0]["key"].as_str(), copy["incomplete"][0]["size"].as_u64()), (Some("data"), Some(original.len() as u64)));
    assert!(s.ui_requests.iter().any(|r| r["edms"]["event"] == "savedIncomplete"));

    // The picture arrives: nothing is owed any more, saving is as always.
    deliver_blob(&transport, original.as_bytes());
    s.signal_pump();
    assert!(s.signal.outstanding().is_empty());
    assert!(s.ui_requests.iter().any(|r| r["signal"]["event"] == "blobComplete"));
    assert!(s.run("edms.save", &json!({})).unwrap()["pending"].is_u64());
}

/// A picture that doesn't compress: `width`×`height` pixels of noise, as PNG.
fn noise_png(width: u32, height: u32, seed: u32) -> Vec<u8> {
    let mut state = seed;
    let noise = image::RgbImage::from_fn(width, height, |_, _| {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let [red, green, blue, _] = state.to_be_bytes();
        image::Rgb([red, green, blue])
    });
    let mut png = Vec::new();
    noise.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
    png
}

#[test]
fn a_big_picture_goes_out_with_its_size_and_format_so_the_others_can_frame_it() {
    let (mut s, transport) = session();
    joined(&mut s, &transport, 2, 1, 1, 0);
    bulk_ready(&mut s, &transport);
    let picture = crate::cmd::insert::base64_encode(&noise_png(240, 160, 7));
    assert!(picture.len() > blobs::MAX_PREVIEW_BYTES, "too big to be its own preview");

    s.run("insert.picture", &json!({"data": picture})).unwrap();
    let sent = transport.take_sent();
    assert_eq!((sent.len(), sent[0].kind), (1, FrameType::Op), "it does not wait for the blob");
    let op: OpPayload = wire::decode(&sent[0].payload).unwrap();
    assert!(op.params.get("data").is_none());
    let attachment = &op.attachments[0];
    assert_eq!(attachment.kind(), craft_signal::PreviewKind::Generic);
    assert_eq!((attachment.width, attachment.height, attachment.preview_type.as_str()), (240, 160, "image/png"));
    assert_eq!(attachment.hash, blobs::hash_of(picture.as_bytes()));
    assert_eq!(transport.take_bulk_sent().last().map(|f| f.kind), Some(FrameType::BlobDone), "the picture itself is on its way");
    // Here it is the picture itself from the start.
    assert_eq!(s.doc.media.values().next().map(|bytes| bytes.len()), crate::cmd::insert::base64_decode(&picture).map(|bytes| bytes.len()));
}

#[test]
fn a_framed_picture_is_swapped_in_when_it_arrives_also_in_the_undo_steps() {
    let (mut s, transport) = session();
    joined(&mut s, &transport, 2, 1, 1, 0);
    bulk_ready(&mut s, &transport);
    let png = noise_png(240, 160, 7);
    let picture = crate::cmd::insert::base64_encode(&png);
    let mut attachment = Attachment::generic("data", picture.as_bytes(), (240, 160));
    attachment.preview_type = "image/png".into();
    let theirs = OpPayload { command: "insert.picture".into(), attachments: vec![attachment], ..OpPayload::default() };

    // Valid without any blob: the frame stands in, with the picture's pixels, format and name.
    transport.deliver(commit_frame(3, 1, 1, &theirs));
    s.signal_pump();
    let (key, frame) = s.doc.media.iter().next().map(|(key, bytes)| (key.clone(), bytes.clone())).expect("a frame");
    assert!(key.ends_with(".png"), "{key}");
    assert_eq!(wordcraft_render::image_size(&frame), Some((240, 160)));
    assert!(frame.len() < 4096, "an empty picture: {} bytes", frame.len());
    assert_eq!(s.signal.outstanding().len(), 1);
    assert_eq!(s.run("signal.status", &json!({})).unwrap()["framed"], json!(1));
    assert!(s.ui_requests.iter().any(|r| r["signal"]["event"] == "framed" && r["signal"]["media"] == json!(key)), "the front end can say so");
    let framed_size = crate::cmd::objects::original_size(&s, &key);

    // Editing goes on; the undo steps from now on hold the frame.
    s.run("text.insert", &json!({"text": "Bildunterschrift"})).unwrap();
    transport.take_sent();

    deliver_blob(&transport, picture.as_bytes());
    s.signal_pump();
    assert_eq!(s.doc.media.get(&key).map(|bytes| bytes.as_slice()), Some(png.as_slice()), "the picture under the frame's name");
    assert_eq!(s.doc.media.len(), 1);
    assert_eq!(crate::cmd::objects::original_size(&s, &key), framed_size, "the same size as its frame");
    assert!(s.signal.outstanding().is_empty());
    assert!(s.ui_requests.iter().any(|r| r["signal"]["event"] == "blobComplete" && r["signal"]["replaced"] == 1));
    assert_eq!(s.run("signal.status", &json!({})).unwrap()["framed"], json!(0));
    s.run("edit.undo", &json!({})).unwrap();
    assert!(!text(&s).contains("Bildunterschrift"));
    assert_eq!(s.doc.media.get(&key).map(|bytes| bytes.as_slice()), Some(png.as_slice()), "the undo step does not bring the frame back");
}

#[test]
fn two_pictures_of_one_size_get_a_frame_each_and_each_its_own_picture() {
    let (mut s, transport) = session();
    joined(&mut s, &transport, 2, 1, 1, 0);
    bulk_ready(&mut s, &transport);
    let pictures: Vec<Vec<u8>> = vec![noise_png(240, 160, 7), noise_png(240, 160, 8)];
    for (index, png) in pictures.iter().enumerate() {
        let picture = crate::cmd::insert::base64_encode(png);
        let mut attachment = Attachment::generic("data", picture.as_bytes(), (240, 160));
        attachment.preview_type = "image/png".into();
        let theirs = OpPayload { command: "insert.picture".into(), attachments: vec![attachment], ..OpPayload::default() };
        transport.deliver(commit_frame(3, index as u32 + 1, index as u32 + 1, &theirs));
    }
    s.signal_pump();
    assert_eq!(s.doc.media.len(), 2, "one frame a picture");
    // The second arrives first.
    for png in pictures.iter().rev() {
        deliver_blob(&transport, crate::cmd::insert::base64_encode(png).as_bytes());
        s.signal_pump();
    }
    let media: Vec<&[u8]> = s.doc.media.values().map(|bytes| bytes.as_slice()).collect();
    assert_eq!(media, pictures.iter().map(Vec::as_slice).collect::<Vec<_>>());
}

#[test]
fn a_frame_is_a_readable_picture_of_the_given_size_in_each_format() {
    use crate::signal_frame::{FrameFormat, MAX_FRAME_PIXELS, build};
    let (one, other) = (blobs::hash_of(b"one"), blobs::hash_of(b"other"));
    for format in [FrameFormat::Png, FrameFormat::Jpeg, FrameFormat::Gif] {
        let frame = build(format, (321, 123), &one).unwrap();
        assert_eq!(wordcraft_render::image_size(&frame), Some((321, 123)), "{format:?}");
        assert!(wordcraft_render::decode_pixmap(&frame).is_some(), "{format:?} draws");
        assert_eq!(FrameFormat::from_mime(format.mime()), Some(format));
        assert_ne!(frame, build(format, (321, 123), &other).unwrap(), "{format:?}: a frame is its picture's alone");
    }
    assert_eq!(crate::cmd::insert::image_ext(&build(FrameFormat::Jpeg, (2, 2), &one).unwrap()), "jpeg");
    // Sizes from the network are hostile.
    assert!(build(FrameFormat::Png, (0, 10), &one).is_err());
    assert!(build(FrameFormat::Png, (u32::MAX, u32::MAX), &one).is_err());
    assert!(u64::from(u32::MAX) > MAX_FRAME_PIXELS);
}

/// The commit of a big picture as another participant sends it.
fn framed_picture(png: &[u8]) -> (String, OpPayload) {
    let picture = crate::cmd::insert::base64_encode(png);
    let size = wordcraft_render::image_size(png).unwrap();
    let mut attachment = Attachment::generic("data", picture.as_bytes(), size);
    attachment.preview_type = "image/png".into();
    (picture, OpPayload { command: "insert.picture".into(), attachments: vec![attachment], ..OpPayload::default() })
}

#[test]
fn a_picture_the_document_already_holds_is_not_framed_and_keeps_its_one_name() {
    let (mut s, transport) = session();
    let png = noise_png(240, 160, 7);
    // Here before the collaboration: in the document, not among the blobs.
    s.run("insert.picture", &json!({"data": crate::cmd::insert::base64_encode(&png)})).unwrap();
    let names: Vec<String> = s.doc.media.keys().cloned().collect();
    joined(&mut s, &transport, 2, 1, 1, 0);
    bulk_ready(&mut s, &transport);

    let (_, theirs) = framed_picture(&png);
    transport.deliver(commit_frame(3, 1, 1, &theirs));
    s.signal_pump();
    assert_eq!(s.doc.media.keys().cloned().collect::<Vec<_>>(), names, "the same one entry, as with the one who brought it in");
    assert_eq!(s.run("signal.status", &json!({})).unwrap()["framed"], json!(0));
    assert_eq!(s.run("signal.status", &json!({})).unwrap()["applied"], json!(1));
}

#[test]
fn a_change_made_to_a_frame_is_made_again_on_the_picture_when_it_arrives() {
    let png = noise_png(240, 160, 7);
    // The one who brought it in has the picture and makes it grey right away.
    let mut sender = Session::new(blank_document(&wordcraft_geom::Region::default()));
    sender.run("insert.picture", &json!({"data": crate::cmd::insert::base64_encode(&png)})).unwrap();
    sender.run("select.objects", &json!({"index": 0})).unwrap();
    let anchor = serde_json::to_value(&sender.sel).unwrap();
    sender.run("picture.color", &json!({"mode": "grayscale"})).unwrap();
    assert_eq!(sender.doc.media.len(), 2, "the picture and its grey version");

    let (mut s, transport) = session();
    joined(&mut s, &transport, 2, 1, 1, 0);
    bulk_ready(&mut s, &transport);
    let (picture, theirs) = framed_picture(&png);
    transport.deliver(commit_frame(3, 1, 1, &theirs));
    let grey = OpPayload { command: "picture.color".into(), params: json!({"mode": "grayscale"}), anchor, ..OpPayload::default() };
    transport.deliver(commit_frame(3, 2, 2, &grey));
    s.signal_pump();
    assert_eq!(s.run("signal.status", &json!({})).unwrap()["applied"], json!(2), "{:?}", s.signal.take_log());
    assert_eq!(s.doc.media.len(), 2, "the frame and its grey version");
    assert!(s.doc.media.values().all(|bytes| !sender.doc.media.values().any(|theirs| theirs == bytes)));

    deliver_blob(&transport, picture.as_bytes());
    s.signal_pump();
    assert_eq!(s.doc.media, sender.doc.media, "the same names and bytes as with the one who had the picture");
    // And nothing brings the grey frame back.
    s.run("text.insert", &json!({"text": "x"})).unwrap();
    s.run("edit.undo", &json!({})).unwrap();
    assert_eq!(s.doc.media, sender.doc.media);
}

#[test]
fn the_same_change_to_two_frames_of_one_size_stays_two_pictures() {
    let (mut s, transport) = session();
    joined(&mut s, &transport, 2, 1, 1, 0);
    bulk_ready(&mut s, &transport);
    let pictures = [noise_png(240, 160, 7), noise_png(240, 160, 8)];
    for (index, png) in pictures.iter().enumerate() {
        let (_, theirs) = framed_picture(png);
        transport.deliver(commit_frame(3, index as u32 + 1, index as u32 + 1, &theirs));
    }
    s.signal_pump();
    for index in 0..pictures.len() {
        s.run("select.objects", &json!({"index": index})).unwrap();
        s.run("picture.color", &json!({"mode": "grayscale"})).unwrap();
    }
    assert_eq!(s.doc.media.len(), 4, "two frames, and a grey version of each");
    for png in &pictures {
        deliver_blob(&transport, crate::cmd::insert::base64_encode(png).as_bytes());
        s.signal_pump();
    }
    let sizes: std::collections::BTreeSet<usize> = s.doc.media.values().map(|bytes| bytes.len()).collect();
    assert_eq!(s.doc.media.len(), 4);
    assert!(sizes.iter().all(|size| *size > 4096), "no frame is left: {sizes:?}");
}

#[test]
fn the_master_answers_a_sync_request_with_the_document_as_it_is_now() {
    let (mut s, transport) = session();
    joined(&mut s, &transport, 1, 0, 1, 0);
    bulk_ready(&mut s, &transport);
    s.run("text.insert", &json!({"text": "Stand vor dem Beitritt"})).unwrap();
    transport.take_sent();
    transport.take_bulk_sent();

    let mut asking = Frame::new(FrameType::SyncReq);
    asking.session = 1;
    asking.sender = 3;
    transport.deliver(asking);
    s.signal_pump();

    let sent = transport.take_sent();
    assert_eq!((sent.len(), sent[0].kind, sent[0].sender, sent[0].seq), (1, FrameType::Sync, 3, 1), "to the one who asked, up to the last commit");
    let payload: craft_signal::SyncPayload = wire::decode(&sent[0].payload).unwrap();
    assert_eq!(payload.format, "docx");
    // The blob on the bulk channel is that document.
    let mut state = Vec::new();
    for frame in transport.take_bulk_sent().iter().filter(|frame| frame.kind == FrameType::BlobPart) {
        let (hash, offset, data) = blobs::decode_part(&frame.payload).unwrap();
        assert_eq!((hash, offset as usize), (payload.state.hash, state.len()));
        state.extend_from_slice(data);
    }
    assert_eq!((state.len() as u64, blobs::hash_of(&state)), (payload.state.size, payload.state.hash));
    let reopened = crate::io::open_bytes("state.docx", &state).unwrap();
    assert_eq!(reopened.plain_text(wordcraft_doc::StoryRef::Body), "Stand vor dem Beitritt");
    assert!(s.dirty, "answering is not a save: the document stays as changed as it was");
    let log = s.signal.take_log();
    assert!(log.iter().any(|entry| entry["event"] == "syncRequested" && entry["answered"] == true), "{log:?}");

    // A collaborator is not asked and does not answer.
    let (mut other, their_transport) = session();
    joined(&mut other, &their_transport, 2, 1, 1, 0);
    let mut stray = Frame::new(FrameType::SyncReq);
    stray.session = 1;
    stray.sender = 3;
    their_transport.deliver(stray);
    other.signal_pump();
    assert!(their_transport.take_sent().is_empty());
}

#[test]
fn who_joins_later_takes_the_masters_document_and_what_was_confirmed_meanwhile() {
    // The master's document, ahead of the file both opened.
    let mut master = Session::new(blank_document(&wordcraft_geom::Region::default()));
    master.run("text.insert", &json!({"text": "Vom Master. "})).unwrap();
    let state = crate::cmd::edms::copy_bytes(&master, "state.docx").unwrap();

    let (mut s, transport) = session();
    s.run("text.insert", &json!({"text": "Lokaler Stand"})).unwrap();
    joined_waiting_for_the_state(&mut s, &transport, 2, 1, 1, 4);
    assert_eq!(s.run("signal.status", &json!({})).unwrap()["syncing"], json!(true));

    // Until the state is here nothing is edited and nothing confirmed is applied.
    let refused = s.run("text.insert", &json!({"text": "x"})).unwrap_err().to_string();
    assert!(refused.contains("brought up to date"), "{refused}");
    let already_in_the_state = OpPayload { command: "text.insert".into(), params: json!({"text": "ALT"}), ..OpPayload::default() };
    let after_the_state = OpPayload {
        command: "text.insert".into(),
        params: json!({"text": "Danach."}),
        anchor: serde_json::to_value(&master.sel).unwrap(),
        ..OpPayload::default()
    };
    transport.deliver(commit_frame(1, 1, 4, &already_in_the_state));
    transport.deliver(commit_frame(1, 2, 5, &after_the_state));
    s.signal_pump();
    assert_eq!(text(&s), "Lokaler Stand");
    assert!(transport.take_sent().is_empty(), "nothing went out");

    answer_with_state(&mut s, &transport, 2, 4, &state);
    assert_eq!(text(&s), "Vom Master. Danach.", "the state, then only what came after it");
    let status = s.run("signal.status", &json!({})).unwrap();
    assert_eq!((status["syncing"].as_bool(), status["applied"].as_u64()), (Some(false), Some(1)));
    assert!(s.ui_requests.iter().any(|r| r["signal"]["event"] == "synced" && r["signal"]["seq"] == 4 && r["signal"]["replayed"] == 1));
    // Editing works again, and no undo step brings the replaced document back.
    s.run("text.insert", &json!({"text": "!"})).unwrap();
    assert_eq!(transport.take_sent().iter().map(|f| f.kind).collect::<Vec<_>>(), [FrameType::Op]);
    for _ in 0..4 {
        let _ = s.run("edit.undo", &json!({}));
        assert!(!text(&s).contains("Lokaler Stand"), "{}", text(&s));
    }
}

#[test]
fn a_master_that_never_answers_does_not_hold_the_participant_forever() {
    let (mut s, transport) = session();
    joined_waiting_for_the_state(&mut s, &transport, 2, 1, 1, 3);
    let confirmed = OpPayload { command: "text.insert".into(), params: json!({"text": "Bestätigt"}), ..OpPayload::default() };
    transport.deliver(commit_frame(1, 1, 4, &confirmed));
    s.signal_pump();
    assert_eq!(text(&s), "");

    s.signal_sync_step_at(crate::signal::now_ms() + 5_000);
    assert_eq!(s.run("signal.status", &json!({})).unwrap()["syncing"], json!(true), "still within the wait");
    s.signal_sync_step_at(crate::signal::now_ms() + 11_000);
    assert_eq!(s.run("signal.status", &json!({})).unwrap()["syncing"], json!(false));
    assert_eq!(text(&s), "Bestätigt", "it goes on with its document and what was confirmed");
    assert!(s.ui_requests.iter().any(|r| r["signal"]["event"] == "syncFailed"));
    s.run("text.insert", &json!({"text": "!"})).unwrap();
}

#[test]
fn the_bytes_of_the_state_are_asked_for_when_they_do_not_come_and_the_master_does_not_ask_itself() {
    let (mut s, transport) = session();
    joined_waiting_for_the_state(&mut s, &transport, 2, 1, 1, 0);
    bulk_ready(&mut s, &transport);
    let state = crate::cmd::edms::copy_bytes(&s, "state.docx").unwrap();
    let payload = craft_signal::SyncPayload { format: "docx".into(), state: Attachment::whole("state", &state) };
    let mut answer = Frame::new(FrameType::Sync).with_payload(wire::encode(&payload).unwrap());
    answer.session = 1;
    answer.sender = 2;
    transport.deliver(answer);
    s.signal_pump();
    assert!(transport.take_bulk_sent().is_empty(), "the bytes usually come by themselves");
    s.signal_sync_step_at(crate::signal::now_ms() + 3_000);
    let asked = transport.take_bulk_sent();
    assert_eq!((asked.len(), asked[0].kind, asked[0].payload.as_slice()), (1, FrameType::BlobReq, &blobs::hash_of(&state)[..]));
    s.signal_sync_step_at(crate::signal::now_ms() + 4_000);
    assert!(transport.take_bulk_sent().is_empty(), "asked once");

    // The first of a session is its master: nobody to ask.
    let (mut first, their_transport) = session();
    joined_waiting_for_the_state(&mut first, &their_transport, 1, 0, 1, 0);
    assert_eq!(first.run("signal.status", &json!({})).unwrap()["syncing"], json!(false));
}
