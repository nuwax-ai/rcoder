//! Observe the original turn's real terminal before testing a later connection.
use std::{cell::RefCell, time::Instant};

use super::sse::{self, SseEvent};
use tokio::sync::watch;

pub struct Observation {
    pub events: Vec<SseEvent>,
    pub terminal_seq: Option<u64>,
    pub ended_reason: String,
}

fn successful_terminal(event: &SseEvent, sid: &str, request_id: &str) -> bool {
    event.seq.is_some_and(|seq| seq > 0)
        && event.event == "end_turn"
        && event.data["sessionId"] == sid
        && event.data["messageType"] == "sessionPromptEnd"
        && event.data["subType"] == "end_turn"
        && event.data["data"]["request_id"] == request_id
        && event.data["data"]["reason"] == "EndTurn"
        && event.data["data"]["error_message"].is_null()
}

pub async fn observe<F>(
    http: &reqwest::Client,
    base: &str,
    sid: &str,
    request_id: &str,
    deadline: Instant,
    mut on_event: F,
) -> Observation
where
    F: FnMut(&SseEvent),
{
    let captured = RefCell::new(Vec::new());
    let terminal_seq = RefCell::new(None);
    let (terminal, mut terminal_rx) = watch::channel(false);
    let ended_reason = {
        let stream = sse::collect_until(http, base, sid, deadline, None, false, |event| {
            on_event(event);
            captured.borrow_mut().push(event.clone());
            if Instant::now() < deadline && successful_terminal(event, sid, request_id) {
                *terminal_seq.borrow_mut() = event.seq;
                terminal.send_replace(true);
            }
        });
        tokio::pin!(stream);
        tokio::select! {
            (_, reason) = &mut stream => match reason {
                sse::EndedReason::Error(detail) => format!("error: {detail}"),
                other => other.as_str().to_owned(),
            },
            _ = terminal_rx.changed() => "original_turn_terminal_observed".to_owned(),
        }
    };
    Observation {
        events: captured.into_inner(),
        terminal_seq: terminal_seq.into_inner(),
        ended_reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, response::Sse, response::sse::Event, routing::get};
    use serde_json::{Value, json};
    use std::{
        collections::VecDeque,
        convert::Infallible,
        sync::{Arc, Mutex},
        time::Duration,
    };

    struct Server {
        base: String,
        requests: Arc<Mutex<usize>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn server(replies: Vec<Vec<Event>>, handshake_delay: Duration) -> Server {
        let replies = Arc::new(Mutex::new(VecDeque::from(replies)));
        let requests = Arc::new(Mutex::new(0));
        let observed = requests.clone();
        let router = Router::new().route(
            "/computer/progress/original-session",
            get(move || {
                let replies = replies.clone();
                let requests = observed.clone();
                async move {
                    *requests.lock().unwrap() += 1;
                    tokio::time::sleep(handshake_delay).await;
                    let events = replies.lock().unwrap().pop_front().unwrap();
                    Sse::new(futures_util::stream::iter(
                        events.into_iter().map(Ok::<_, Infallible>),
                    ))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Server {
            base,
            requests,
            task,
        }
    }

    fn terminal() -> Value {
        json!({"sessionId":"original-session","messageType":"sessionPromptEnd",
            "subType":"end_turn","data":{"request_id":"original-request","reason":"EndTurn"}})
    }

    fn event(body: &Value, seq: Option<u64>, name: &str) -> Event {
        let event = Event::default().event(name).data(body.to_string());
        match seq {
            Some(seq) => event.id(seq.to_string()),
            None => event,
        }
    }

    #[tokio::test]
    async fn wrong_identity_unsequenced_and_nonterminal_frames_cannot_authorize_reconnect() {
        let mut wrong_request = terminal();
        wrong_request["data"]["request_id"] = json!("another-request");
        let mut wrong_session = terminal();
        wrong_session["sessionId"] = json!("another-session");
        let mut failed = terminal();
        failed["data"]["error_message"] = json!("prompt failed");
        for (body, seq, name) in [
            (wrong_request, Some(1), "end_turn"),
            (wrong_session, Some(1), "end_turn"),
            (terminal(), None, "end_turn"),
            (terminal(), Some(0), "end_turn"),
            (terminal(), Some(1), "prompt_start"),
            (terminal(), None, "stream_ended"),
            (failed, Some(1), "end_turn"),
        ] {
            let server = server(vec![vec![event(&body, seq, name)]], Duration::ZERO).await;
            let observation = observe(
                &reqwest::Client::new(),
                &server.base,
                "original-session",
                "original-request",
                Instant::now() + Duration::from_secs(1),
                |_| {},
            )
            .await;
            assert_eq!(observation.events.len(), 1);
            assert!(observation.terminal_seq.is_none(), "{name}: {body}");
            assert_eq!(*server.requests.lock().unwrap(), 1);
        }
    }

    #[tokio::test]
    async fn exact_terminal_then_empty_reconnect_satisfies_original_clear_assertion() {
        let server = server(
            vec![vec![event(&terminal(), Some(7), "end_turn")], vec![]],
            Duration::ZERO,
        )
        .await;
        let http = reqwest::Client::new();
        let observation = observe(
            &http,
            &server.base,
            "original-session",
            "original-request",
            Instant::now() + Duration::from_secs(1),
            |_| {},
        )
        .await;
        assert_eq!(observation.terminal_seq, Some(7));
        let (events, reason) = sse::collect_until(
            &http,
            &server.base,
            "original-session",
            Instant::now() + Duration::from_secs(1),
            None,
            true,
            |_| {},
        )
        .await;
        assert_eq!(reason, sse::EndedReason::StreamEnded);
        assert!(sse::message_events(&events).is_empty());
        assert_eq!(*server.requests.lock().unwrap(), 2);
    }

    #[tokio::test]
    async fn empty_stream_and_expired_handshake_cannot_authorize_a_later_subscription() {
        let http = reqwest::Client::new();
        for delay in [Duration::ZERO, Duration::from_millis(300)] {
            let server = server(vec![vec![]], delay).await;
            let observation = observe(
                &http,
                &server.base,
                "original-session",
                "original-request",
                Instant::now() + Duration::from_millis(50),
                |_| {},
            )
            .await;
            assert!(observation.events.is_empty());
            assert!(observation.terminal_seq.is_none());
            assert_eq!(*server.requests.lock().unwrap(), 1);
        }
    }

    #[tokio::test]
    async fn handshake_deadline_is_failure_not_an_empty_successful_sample() {
        let server = server(vec![vec![]], Duration::from_millis(300)).await;
        let http = reqwest::Client::new();
        let started = Instant::now();
        let (events, reason) = sse::collect_until(
            &http,
            &server.base,
            "original-session",
            started + Duration::from_millis(50),
            None,
            true,
            |_| {},
        )
        .await;
        assert!(events.is_empty());
        assert!(
            matches!(reason, sse::EndedReason::Error(detail) if detail.contains("absolute deadline"))
        );
    }
}
