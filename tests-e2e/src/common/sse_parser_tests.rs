use super::*;

#[tokio::test]
async fn actual_axum_data_before_id_keeps_id_on_its_frame_not_the_heartbeat() {
    use axum::response::{IntoResponse, Sse, sse::Event};
    use std::convert::Infallible;

    let stream = futures_util::stream::iter([
        Ok::<_, Infallible>(
            Event::default()
                .event("prompt_start")
                .data(r#"{"request_id":"original-request"}"#)
                .id("4"),
        ),
        Ok(Event::default().event("ping").data("{}")),
    ]);
    let response = Sse::new(stream).into_response();
    let body = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    let wire = std::str::from_utf8(&body).unwrap();
    eprintln!("actual Axum SSE wire: {wire:?}");
    assert!(wire.starts_with("event: prompt_start\ndata: "));
    assert!(wire.contains("\nid: 4\n\n"));

    let events = SseParser::default().push(&body, 17);
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].event, "prompt_start");
    assert_eq!(events[0].seq, Some(4));
    assert_eq!(events[0].data["request_id"], "original-request");
    assert_eq!(events[1].event, "ping");
    assert_eq!(events[1].seq, None, "a heartbeat has no explicit frame ID");
    assert_eq!(events[0].t_ms, 17);
}

#[test]
fn all_legal_event_data_id_orders_produce_the_same_frame() {
    let fields = ["event: update\n", "data: {\"x\":1}\n", "id: 7\n"];
    for order in [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ] {
        let wire = format!(
            "{}{}{}\n",
            fields[order[0]], fields[order[1]], fields[order[2]]
        );
        let events = SseParser::default().push(wire.as_bytes(), 0);
        assert_eq!(events.len(), 1, "{order:?}");
        assert_eq!(events[0].event, "update", "{order:?}");
        assert_eq!(events[0].seq, Some(7), "{order:?}");
        assert_eq!(events[0].data, serde_json::json!({"x":1}), "{order:?}");
    }
}

#[test]
fn data_does_not_dispatch_before_the_empty_line() {
    let mut parser = SseParser::default();
    assert!(
        parser
            .push(b"event: update\ndata: {}\nid: 9\n", 0)
            .is_empty()
    );
    let events = parser.push(b"\n", 5);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].seq, Some(9));
    assert_eq!(events[0].t_ms, 5);
}

#[test]
fn multiple_data_fields_are_joined_before_json_decoding() {
    let events = SseParser::default().push(
        "event: update\ndata: {\"text\":\ndata: \"中文\"}\nid: 8\n\n".as_bytes(),
        0,
    );
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data, serde_json::json!({"text":"中文"}));
    assert_eq!(events[0].seq, Some(8));
}

#[test]
fn data_whitespace_and_an_explicit_empty_data_field_are_preserved() {
    let events = SseParser::default().push(b"data:  first  \ndata: second\n\ndata:\n\n", 0);
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].event, "message");
    assert_eq!(events[0].data, Value::String(" first  \nsecond".into()));
    assert_eq!(events[1].data, Value::String(String::new()));
}

#[test]
fn frames_without_data_do_not_dispatch_or_lend_ids_to_the_next_frame() {
    let events =
        SseParser::default().push(b": keep-alive\n\nevent: ignored\nid: 10\n\ndata: {}\n\n", 0);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event, "message");
    assert_eq!(events[0].seq, None);
}

#[test]
fn all_line_endings_and_arbitrary_utf8_chunk_boundaries_are_supported() {
    for ending in ["\n", "\r\n", "\r"] {
        let wire = format!("event: update{ending}data: 中文{ending}id: 12{ending}{ending}");
        let mut parser = SseParser::default();
        let mut events = Vec::new();
        for byte in wire.as_bytes() {
            events.extend(parser.push(std::slice::from_ref(byte), 0));
        }
        assert_eq!(events.len(), 1, "{ending:?}");
        assert_eq!(events[0].data, Value::String("中文".into()));
        assert_eq!(events[0].seq, Some(12));
    }
}

#[test]
fn eof_without_an_empty_line_discards_the_pending_frame() {
    let events = SseParser::default().push(b"event: end_turn\ndata: {}\nid: 6\n", 0);
    assert!(events.is_empty(), "EOF is not an SSE event delimiter");
    let mut parser = SseParser::default();
    let events = parser.push(b"event: done\ndata: {}\n\nevent: unfinished\ndata: tail", 0);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event, "done");
}

#[test]
fn repeated_fields_and_comments_keep_the_last_explicit_event_and_id() {
    let events = SseParser::default().push(
        b"event: old\nid: 1\n: comment\nevent: update\ndata: {}\nid: 2\nretry: 100\n\n",
        0,
    );
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event, "update");
    assert_eq!(events[0].seq, Some(2));
}
