#![allow(clippy::unwrap_used)]

use std::io::Write;

use super::*;
use crate::ipc::TimelineToolStatus::{Completed, Failed, Running};

fn append(file: &mut tempfile::NamedTempFile, line: &str) {
    writeln!(file, "{line}").unwrap();
}

#[test]
fn claude_folds_tool_results_and_keeps_only_the_main_conversation() {
    let mut file = tempfile::Builder::new()
        .suffix(".jsonl")
        .tempfile()
        .unwrap();
    append(
        &mut file,
        r#"{"type":"user","isMeta":true,"message":{"content":"a hook injected this"}}"#,
    );
    append(
        &mut file,
        r#"{"type":"user","message":{"content":"<system-reminder>hidden</system-reminder>\nThe first real prompt\nand the rest"}}"#,
    );
    append(
        &mut file,
        r#"{"type":"assistant","timestamp":"2024-01-01T00:00:00Z","message":{"content":[{"type":"thinking","thinking":"Think"},{"type":"tool_use","id":"a","name":"Bash","input":{"command":"pwd"}},{"type":"tool_use","id":"b","name":"Read","input":{"file_path":"notes"}}]}}"#,
    );
    append(
        &mut file,
        r#"{"type":"assistant","isSidechain":true,"message":{"content":[{"type":"text","text":"child"}]}}"#,
    );
    append(
        &mut file,
        r#"{"type":"user","isSidechain":true,"message":{"content":[{"type":"tool_result","tool_use_id":"b","content":"child data"}]}}"#,
    );
    let large_output = "é".repeat(9 * 1024 * 1024);
    append(&mut file, &serde_json::json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"a","content":large_output}]}}).to_string());
    append(
        &mut file,
        r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"todos","name":"TodoWrite","input":{"todos":[{"content":"Finish","status":"in_progress"}]}}]}}"#,
    );
    append(
        &mut file,
        r#"{"type":"user","message":{"content":"[senate] Preamble\n\n[senate · testing · seq 7 · from [senator::app] · answer in thread 5 · for you as chair of testing]\nrefs: internal pointer\n│ Hi\n│\n│ Again\n↳ footnote"}}"#,
    );
    append(
        &mut file,
        r#"{"type":"system","subtype":"compact_boundary"}"#,
    );
    write!(file, "{{\"type\":\"user\"").unwrap();

    let titles = TitleCache::default().refresh(vec![(
        "test".to_string(),
        Runtime::Claude,
        file.path().into(),
    )]);
    assert_eq!(
        titles["test"].title.as_deref(),
        Some("The first real prompt")
    );
    let mut reader = Transcript::open(file.path().into(), Runtime::Claude).unwrap();
    let page = reader.page(TimelineDirection::Tail, None, 60).unwrap();
    assert_eq!(
        page.items.iter().map(|item| item.seq).collect::<Vec<_>>(),
        (1..=7).collect::<Vec<_>>()
    );
    assert!(matches!(&page.items[1].kind, TimelineKind::Reasoning { text } if text == "Think"));
    assert_eq!(
        page.items[2].ts.unwrap().to_rfc3339(),
        "2024-01-01T00:00:00+00:00"
    );
    assert!(matches!(&page.items[2].kind, TimelineKind::ToolCall {
        status: Completed, detail: TimelineToolDetail::Shell { output: Some(output), .. }, ..
    } if output.len() == 64 * 1024));
    assert!(page.items[2].truncated);
    assert!(matches!(
        &page.items[3].kind,
        TimelineKind::ToolCall {
            status: Running,
            ..
        }
    ));
    assert!(
        matches!(&page.items[4].kind, TimelineKind::Todo { items } if items[0].text == "Finish")
    );
    assert!(matches!(&page.items[5].kind, TimelineKind::SenateMessage {
        seq: Some(7), thread: Some(5), from: Some(from), text, ..
    } if from == "[senator::app]" && text == "Hi\n\nAgain"));
    assert!(matches!(&page.items[6].kind, TimelineKind::Notice { .. }));
    let wire = serde_json::to_value(&page.items[5]).unwrap();
    assert_eq!(wire["seq"], 6);
    assert_eq!(wire["senate_seq"], 7);
    assert_eq!(wire["kind"], "senate_message");
    assert_eq!(
        serde_json::from_value::<TimelineItem>(wire).unwrap(),
        page.items[5]
    );
}

#[test]
fn codex_normalizes_rollout_records_and_native_commands() {
    let mut file = tempfile::Builder::new()
        .suffix(".jsonl")
        .tempfile()
        .unwrap();
    let lines = [
        r#"{"type":"session_meta","payload":{"id":"root","cwd":"/work"}}"#,
        r#"{"type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"developer context"}]}}"#,
        r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>hidden</environment_context>"},{"type":"input_text","text":"My first prompt"}]}}"#,
        r#"{"type":"event_msg","payload":{"type":"user_message","message":"duplicate prompt"}}"#,
        r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"[senate · testing · seq 9 · 来自 [senator::app] · answer，thread 3]\n│ Update"}]}}"#,
        r#"{"type":"response_item","payload":{"type":"reasoning","summary":[{"type":"summary_text","text":"Plan"}],"encrypted_content":"opaque"}}"#,
        r#"{"type":"response_item","payload":{"type":"function_call","call_id":"a","name":"exec_command","arguments":"{\"cmd\":\"false\"}"}}"#,
        r#"{"type":"response_item","payload":{"type":"function_call","call_id":"b","name":"read_file","arguments":"{\"path\":\"test.txt\"}"}}"#,
        r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"a","output":[{"type":"input_text","text":"Process exited with code 23\nOutput:\nno"}]}}"#,
        r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"CommandExecution","id":"native","command":["pwd"],"cwd":"/work","status":"completed","exit_code":0,"aggregated_output":"/work"}}}"#,
        r#"{"type":"event_msg","payload":{"type":"item_completed","thread_id":"child","item":{"type":"CommandExecution","id":"native2","command":["pwd"],"status":"completed"}}}"#,
        r#"{"type":"event_msg","payload":{"type":"turn_aborted","reason":"interrupted"}}"#,
        r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Done"}]}}"#,
    ];
    for line in lines {
        append(&mut file, line);
    }
    let mut reader = Transcript::open(file.path().into(), Runtime::Codex).unwrap();
    let page = reader.page(TimelineDirection::Tail, None, 60).unwrap();
    assert_eq!(page.items.len(), 8);
    assert!(
        matches!(&page.items[0].kind, TimelineKind::UserMessage { text } if text == "My first prompt")
    );
    assert!(matches!(
        &page.items[1].kind,
        TimelineKind::SenateMessage {
            seq: Some(9),
            thread: Some(3),
            ..
        }
    ));
    assert!(matches!(&page.items[2].kind, TimelineKind::Reasoning { text } if text == "Plan"));
    assert!(matches!(&page.items[3].kind, TimelineKind::ToolCall {
        status: Failed, detail: TimelineToolDetail::Shell { exit_code: Some(23), cwd: Some(cwd), .. }, ..
    } if cwd == "/work"));
    assert!(matches!(
        &page.items[4].kind,
        TimelineKind::ToolCall {
            status: Running,
            ..
        }
    ));
    assert!(matches!(&page.items[5].kind, TimelineKind::ToolCall {
        status: Completed, detail: TimelineToolDetail::Shell { exit_code: Some(0), output: Some(output), .. }, ..
    } if output == "/work"));
    assert!(
        matches!(&page.items[6].kind, TimelineKind::Notice { text, .. } if text == "Turn interrupted")
    );
    assert!(
        matches!(&page.items[7].kind, TimelineKind::AssistantMessage { text } if text == "Done")
    );
}

#[test]
fn append_scans_only_new_bytes_and_detects_a_rewrite_past_the_old_offset() {
    let mut file = tempfile::Builder::new()
        .suffix(".jsonl")
        .tempfile()
        .unwrap();
    append(
        &mut file,
        r#"{"type":"user","message":{"content":"first"}}"#,
    );
    let mut reader = Transcript::open(file.path().into(), Runtime::Claude).unwrap();
    let first = reader.updates(reader.snapshot_len().unwrap(), 0).unwrap();
    let offset = reader.offset;
    assert_eq!(first[0].seq, 1);

    write!(file, r#"{{"type":"user","#).unwrap();
    assert!(
        reader
            .updates(reader.snapshot_len().unwrap(), 0)
            .unwrap()
            .is_empty()
    );
    assert!(reader.offset > offset);
    let partial_end = reader.offset;
    assert!(
        reader
            .updates(reader.snapshot_len().unwrap(), 0)
            .unwrap()
            .is_empty()
    );
    assert_eq!(reader.offset, partial_end);
    append(&mut file, r#""message":{"content":"second"}}"#);
    let next = reader.updates(reader.snapshot_len().unwrap(), 0).unwrap();
    assert_eq!(next[0].seq, 2);

    std::fs::write(file.path(), vec![b'x'; reader.offset as usize + 100]).unwrap();
    assert_eq!(
        reader.refreshed_len(file.path(), Runtime::Claude).unwrap(),
        None
    );
}

#[test]
fn large_pages_preserve_cursor_contiguity_within_the_socket_frame() {
    let mut file = tempfile::Builder::new()
        .suffix(".jsonl")
        .tempfile()
        .unwrap();
    for _ in 0..8 {
        let line =
            serde_json::json!({"type":"user","message":{"content":"\u{0001}".repeat(65_536)}});
        append(&mut file, &line.to_string());
    }
    let mut reader = Transcript::open(file.path().into(), Runtime::Claude).unwrap();
    let tail = reader.page(TimelineDirection::Tail, None, 300).unwrap();
    assert_eq!(
        tail.items.iter().map(|item| item.seq).collect::<Vec<_>>(),
        [7, 8]
    );
    assert!(tail.has_older && !tail.has_newer);

    let mut reader = Transcript::open(file.path().into(), Runtime::Claude).unwrap();
    let after = reader.page(TimelineDirection::After, Some(4), 300).unwrap();
    assert_eq!(
        after.items.iter().map(|item| item.seq).collect::<Vec<_>>(),
        [5, 6]
    );
    assert!(after.has_older && after.has_newer);
}

#[test]
fn watch_cursor_avoids_replaying_50_mib_on_append() {
    let mut file = tempfile::Builder::new()
        .suffix(".jsonl")
        .tempfile()
        .unwrap();
    // Provider bookkeeping contributes bytes but no visible timeline items.
    let metadata = serde_json::json!({"type": "api-request", "data": "x".repeat(64 * 1024)});
    let line = metadata.to_string();
    for _ in 0..800 {
        append(&mut file, &line);
    }
    let open = || Transcript::open(file.path().into(), Runtime::Claude).unwrap();
    let mut incremental = open();
    let initial_end = incremental.snapshot_len().unwrap();
    assert!(incremental.updates(initial_end, 0).unwrap().is_empty());

    let appended = r#"{"type":"user","message":{"content":"hello"}}"#;
    append(&mut file, appended);
    let end = incremental.snapshot_len().unwrap();
    let change = incremental.updates(end, 0).unwrap();
    let incremental_bytes = incremental.offset - initial_end;

    // Remove the retained cursor and reducer by opening the same snapshot
    // again. Both readers see the same change and consume different bytes.
    let mut replay = Transcript::open(file.path().into(), Runtime::Claude).unwrap();
    assert_eq!(replay.updates(end, 0).unwrap(), change);
    assert_eq!(incremental_bytes, (appended.len() + 1) as u64);
    assert_eq!(replay.offset, end);
    assert!(replay.offset - incremental_bytes >= 50 * 1024 * 1024);
    eprintln!(
        "watch append: {incremental_bytes} bytes consumed, full replay: {} bytes consumed",
        replay.offset
    );
}
