use super::*;

fn item(role: &str, text: &str) -> Value {
    serde_json::json!({
        "role": role,
        "content": [{"type": "input_text", "text": text}],
    })
}

#[test]
fn clamp_keeps_a_conversation_that_fits_untouched() {
    let items = vec![item("user", "short")];
    let (fit, dropped) = clamp_to_window(items.clone(), 1_000_000);
    assert!(dropped.is_empty());
    assert_eq!(fit, items);
}

#[test]
fn clamp_drops_oldest_never_the_recent_tail() {
    let items = vec![
        item("user", "a".repeat(10_000).as_str()),
        item("assistant", "b".repeat(10_000).as_str()),
        item("user", "c".repeat(10_000).as_str()),
        item("user", "the actual question"),
    ];
    let (fit, dropped) = clamp_to_window(items, 24_000);
    assert_eq!(dropped.len(), 2);
    assert!(dropped
        .iter()
        .any(|v| v.to_string().contains("a".repeat(10_000).as_str())));
    assert!(!dropped
        .iter()
        .any(|v| v.to_string().contains("the actual question")));
    // The most recent user turn must survive the cut.
    assert!(fit
        .last()
        .unwrap()
        .to_string()
        .contains("the actual question"));
    // The oldest turns are gone; the tail is preserved in order.
    assert!(!fit
        .iter()
        .any(|v| v.to_string().contains("a".repeat(10_000).as_str())));
}

#[test]
fn clamp_never_empties_the_conversation() {
    let items = vec![item("user", "keep me at any cost")];
    let (fit, dropped) = clamp_to_window(items.clone(), 10);
    assert!(dropped.is_empty());
    assert_eq!(fit.len(), 1);
    assert!(fit[0].to_string().contains("keep me at any cost"));
}

#[test]
fn clamp_reports_every_dropped_turn() {
    let items = vec![
        item("user", "x".repeat(20_000).as_str()),
        item("assistant", "y".repeat(20_000).as_str()),
        item("user", "z".repeat(20_000).as_str()),
        item("user", "tail"),
    ];
    let (fit, dropped) = clamp_to_window(items, 5_000);
    assert_eq!(fit.len(), 1);
    // Dropped and kept are complementary: nothing is lost or duplicated.
    assert_eq!(dropped.len(), 3);
    assert!(fit[0].to_string().contains("tail"));
}

#[test]
fn compaction_falls_back_to_truncated_text_for_oversized_item() {
    let mut provider = super::tests_routing::multi_dialect_provider();
    provider.models.iter_mut().for_each(|m| {
        if m.id == "deepseek-v4-flash" {
            m.context_window = Some(1_000_000);
        }
    });
    let payload = serde_json::json!({
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "x".repeat(4_000_000)}]},
            {"type": "compaction_trigger"}
        ],
        "stream": true
    });

    let prepared = super::dispatch::fit_compaction_input(&provider, "deepseek-v4-flash", &payload);
    let items = prepared["input"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    let text = items[0]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.len() < 3_000_000,
        "oversized item must be truncated: {}",
        text.len()
    );
    assert_eq!(
        prepared["instructions"],
        "You are a conversation summarizer."
    );
    let estimated = estimate_tokens(items) + estimate_non_input_tokens(&prepared, items);
    let budget = 1_000_000 - CONTEXT_RESERVE_TOKENS * 2;
    assert!(
        estimated <= budget,
        "compaction payload estimate {estimated} exceeds {budget}"
    );
}

#[test]
fn render_items_as_text_flattens_responses_blocks_with_roles() {
    let items = vec![
        item("user", "first turn"),
        item("assistant", "second turn"),
        item("user", "third turn"),
    ];
    let text = render_items_as_text(&items);
    assert!(text.contains("user: first turn"));
    assert!(text.contains("assistant: second turn"));
    assert!(text.contains("user: third turn"));
    assert_eq!(text.matches("user:").count(), 2);
}

#[test]
fn render_items_as_text_skips_blocks_without_text() {
    let items = vec![
        serde_json::json!({"role": "user", "content": [{"type": "input_text", "text": "kept"}]}),
        serde_json::json!({"role": "assistant", "content": [{"type": "function_call", "name": "x"}]}),
    ];
    let text = render_items_as_text(&items);
    assert!(text.contains("user: kept"));
    assert!(!text.contains("function_call"));
}

/// A minimal Responses-wire input item carrying a stable `id`, for the
/// WS history tests. The clamp tests above use `item(role, text)`; the
/// history rebuild keys off `id`, so it needs its own fixture.
fn history_item(label: &str) -> Value {
    json!({"id": label, "type": "message", "role": "user", "content": label})
}

#[test]
fn follow_up_turn_appends_delta_to_the_cached_base() {
    let mut history = WsHistory::new();
    history.insert(
        "resp-1".into(),
        vec![history_item("a"), history_item("b"), history_item("c")],
        None,
    );
    let rebuilt = rebuild_input(&history, Some("resp-1"), vec![history_item("d")]);
    assert_eq!(
        rebuilt
            .iter()
            .map(|v| v["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["a", "b", "c", "d"]
    );
}

#[test]
fn unknown_previous_response_id_degrades_to_delta_alone() {
    // A fresh conversation, or an id the cache already evicted: the old
    // pre-cache behavior, delta-only.
    let history = WsHistory::new();
    let rebuilt = rebuild_input(&history, Some("never-cached"), vec![history_item("d")]);
    assert_eq!(rebuilt.len(), 1);
    assert_eq!(rebuilt[0]["id"], "d");
}

#[test]
fn native_follow_up_replaces_previous_response_id_with_rebuilt_input() {
    let mut payload = json!({
        "model": "gpt-5.6-sol",
        "previous_response_id": "resp_1",
        "input": [history_item("new")],
    });
    let full_input = vec![history_item("old"), history_item("new")];

    replace_incremental_input(&mut payload, full_input.clone());

    assert_eq!(payload["input"], json!(full_input));
    assert!(payload.get("previous_response_id").is_none());
}

#[test]
fn native_payload_strips_the_unsupported_generate_flag() {
    let mut payload = json!({"model": "gpt-5.6-terra", "generate": true});

    sanitize_responses_payload(&mut payload);

    assert!(payload.get("generate").is_none());
    assert_eq!(payload["model"], "gpt-5.6-terra");
}

#[test]
fn a_reconnect_rebuilds_input_from_the_shared_history() {
    // The regression: Codex reconnects mid-conversation, starting a new
    // WS session. The history is shared per-process (not per-session), so
    // the new session's follow-up turn still finds the prior turns.
    let shared = Arc::new(Mutex::new(WsHistory::new()));

    // Session 1 completes a turn; the full input + output is cached under
    // the response id it echoed to Codex.
    let record = {
        let mut r = vec![history_item("a"), history_item("b")];
        r.push(
            json!({"id": "asst-1", "type": "message", "role": "assistant",
                      "content": "oi"}),
        );
        r
    };
    shared.lock().unwrap().insert("resp-1".into(), record, None);

    // Session 2 (post-reconnect) sends only the delta + previous_response_id.
    let items = {
        let history = shared.lock().unwrap_or_else(|e| e.into_inner());
        rebuild_input(&history, Some("resp-1"), vec![history_item("c")])
    };
    assert_eq!(
        items
            .iter()
            .map(|v| v["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["a", "b", "asst-1", "c"]
    );
}

#[test]
fn ws_history_evicts_oldest_first() {
    let mut history = WsHistory::new();
    for i in 0..(WS_HISTORY_MAX_ENTRIES + 10) {
        history.insert(
            format!("resp-{i}"),
            vec![history_item(&format!("m{i}"))],
            None,
        );
    }
    assert!(history.get("resp-0").is_none(), "oldest must be evicted");
    assert!(
        history
            .get(&format!("resp-{}", WS_HISTORY_MAX_ENTRIES + 9))
            .is_some(),
        "newest must survive"
    );
    assert_eq!(history.order.len(), WS_HISTORY_MAX_ENTRIES);
}

#[test]
fn an_entry_larger_than_the_byte_budget_survives() {
    // The regression behind "context reset at 304k": a long conversation's
    // rebuilt input alone serializes past WS_HISTORY_MAX_BYTES. The old
    // eviction loop treated the just-inserted entry as the oldest when it
    // was the only one, removed it, and the next turn resolved
    // `previous_response_id` against nothing — delta-only, context to zero.
    let mut history = WsHistory::new();
    let big = vec![json!({
        "id": "big",
        "type": "message",
        "role": "user",
        "content": "x".repeat(WS_HISTORY_MAX_BYTES),
    })];
    assert!(
        big.iter().map(|v| v.to_string().len()).sum::<usize>() > WS_HISTORY_MAX_BYTES,
        "fixture must exceed the byte budget"
    );
    history.insert("resp-1".into(), big, None);
    assert!(
        history.get("resp-1").is_some(),
        "the stored turn must survive"
    );
    assert_eq!(history.order.len(), 1);
}

#[test]
fn a_follow_up_replaces_the_turn_it_was_built_on() {
    // Each entry contains the whole conversation so far, so the entry the
    // follow-up was rebuilt from is fully subsumed: inserting the new turn
    // drops the old one, keeping exactly one entry per conversation.
    let mut history = WsHistory::new();
    history.insert("resp-1".into(), vec![history_item("a")], None);
    history.insert(
        "resp-2".into(),
        vec![history_item("a"), history_item("b")],
        Some("resp-1"),
    );
    assert!(
        history.get("resp-1").is_none(),
        "subsumed turn must be dropped"
    );
    assert_eq!(
        history.get("resp-2").unwrap().iter().count(),
        2,
        "the newest entry keeps the full conversation"
    );
}

#[test]
fn the_just_inserted_turn_is_never_evicted_by_its_own_insert() {
    // A burst of small entries leaves the cache at the entry cap, then a
    // single oversized turn (the conversation's newest) lands: the insert
    // that stores it must not evict it to make room. FIFO keeps it and
    // drops the oldest of the small ones instead.
    let mut history = WsHistory::new();
    for i in 0..(WS_HISTORY_MAX_ENTRIES + 10) {
        history.insert(
            format!("small-{i}"),
            vec![history_item(&format!("s{i}"))],
            None,
        );
    }
    assert_eq!(history.order.len(), WS_HISTORY_MAX_ENTRIES);
    let big = vec![json!({
        "id": "conv-1",
        "type": "message",
        "role": "user",
        "content": "y".repeat(WS_HISTORY_MAX_BYTES),
    })];
    history.insert("conv-1".into(), big, None);
    assert!(
        history.get("conv-1").is_some(),
        "a just-stored conversation turn must never be evicted by its own insert"
    );
    // The oversized turn alone exceeds the byte budget, so everything
    // older is evicted down to that one entry; the newest survives.
    assert_eq!(history.order.len(), 1);
}

#[test]
fn two_large_conversations_coexist_without_evicting_each_other() {
    // The multi-conversation gap: a byte budget of 512KB meant one
    // 304k-token conversation (~1.2MB serialized) already blew the cap,
    // so a second long conversation's insert evicted the first one's
    // entry and reset it. With the raised budget each conversation keeps
    // its own newest turn; both must survive side by side.
    let mut history = WsHistory::new();
    let conv_a = vec![json!({
        "id": "a",
        "type": "message",
        "role": "user",
        "content": "a".repeat(2 * 1024 * 1024),
    })];
    let conv_b = vec![json!({
        "id": "b",
        "type": "message",
        "role": "user",
        "content": "b".repeat(2 * 1024 * 1024),
    })];
    history.insert("resp-a".into(), conv_a, None);
    history.insert("resp-b".into(), conv_b, None);
    assert!(
        history.get("resp-a").is_some(),
        "conversation A was evicted"
    );
    assert!(
        history.get("resp-b").is_some(),
        "conversation B was evicted"
    );
    assert_eq!(history.order.len(), 2);
}
