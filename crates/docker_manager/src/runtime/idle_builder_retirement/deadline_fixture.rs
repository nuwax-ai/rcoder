//! Read accepted connections concurrently; handle complete requests serially.
use futures_util::{FutureExt, StreamExt, future::BoxFuture, stream::FuturesUnordered};
use tokio::net::{TcpListener, TcpStream};

type Request = (TcpStream, String, Vec<u8>);

pub(super) struct HttpRequests {
    listener: TcpListener,
    reading: FuturesUnordered<BoxFuture<'static, Option<Request>>>,
}

impl HttpRequests {
    pub(super) fn new(listener: TcpListener) -> Self {
        Self {
            listener,
            reading: FuturesUnordered::new(),
        }
    }

    fn register(&mut self, stream: TcpStream) {
        self.reading.push(read_connection(stream).boxed());
    }

    pub(super) async fn next(&mut self) -> (TcpStream, String, Vec<u8>) {
        loop {
            tokio::select! {
                accepted = self.listener.accept() => {
                    let (stream, _) = accepted.expect("accept fixture connection");
                    self.register(stream);
                }
                completed = self.reading.next(), if !self.reading.is_empty() => {
                    match completed.expect("a registered reader must complete") {
                        Some(request) => return request,
                        None => continue,
                    }
                }
            }
        }
    }

    /// The caller drops every client before releasing the withheld response.
    /// Drain pending readers too, so shutdown cannot hide a partial HTTP error
    /// or a complete request issued after the observed timeout.
    pub(super) async fn finish(mut self) {
        loop {
            match self.listener.accept().now_or_never() {
                Some(Ok((stream, _))) => self.register(stream),
                None => break,
                Some(Err(error)) => panic!("accept pending fixture connection: {error}"),
            }
        }
        drop(self.listener);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some(request) = self.reading.next().await {
                assert!(request.is_none(), "unhandled HTTP request after deadline");
            }
        })
        .await
        .expect("fixture clients did not close their pending connections");
    }
}

async fn read_connection(mut stream: TcpStream) -> Option<Request> {
    // Pool checkout may leave an unused connection. Only clean EOF before any
    // HTTP bytes is idle; once a byte exists, preserve the strict parser's EOF
    // assertions for incomplete headers or bodies. Peek consumes no prefix.
    let mut prefix = [0; 1];
    if stream
        .peek(&mut prefix)
        .await
        .expect("peek fixture request")
        == 0
    {
        return None;
    }
    let (first, body) = super::super::tests::request(&mut stream).await;
    Some((stream, first, body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::AsyncWriteExt;

    const COMPLETE_REQUEST: &[u8] =
        b"POST /complete HTTP/1.1\r\nHost: fixture\r\nContent-Length: 2\r\n\r\nok";

    #[tokio::test]
    async fn accepted_idle_connection_does_not_block_a_complete_request() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let mut requests = HttpRequests::new(listener);
        let idle = TcpStream::connect(address).await.unwrap();
        let (accepted, _) = requests.listener.accept().await.unwrap();
        requests.register(accepted);
        assert_eq!(requests.reading.len(), 1);

        let mut active = TcpStream::connect(address).await.unwrap();
        active.write_all(COMPLETE_REQUEST).await.unwrap();
        let (_, first, body) = tokio::time::timeout(Duration::from_secs(2), requests.next())
            .await
            .expect("an already accepted idle socket must not block actual HTTP");
        assert_eq!(first, "POST /complete HTTP/1.1");
        assert_eq!(body, b"ok");
        // Keep the first connection open until the second request has arrived;
        // dropping it earlier would hide the sequential-reader regression.
        drop(idle);
    }

    #[tokio::test]
    async fn zero_byte_clean_eof_is_not_a_request_or_a_fixture_failure() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let mut requests = HttpRequests::new(listener);
        let mut idle = TcpStream::connect(address).await.unwrap();
        let (accepted, _) = requests.listener.accept().await.unwrap();
        idle.shutdown().await.unwrap();
        assert_eq!(accepted.peek(&mut [0; 1]).await.unwrap(), 0);
        requests.register(accepted);

        let mut next = Box::pin(requests.next());
        // Consume the known-ready EOF before a second connection exists. This
        // must stay Pending for HTTP, neither panic nor invent an empty request.
        assert!(futures_util::poll!(next.as_mut()).is_pending());
        let mut active = TcpStream::connect(address).await.unwrap();
        active.write_all(COMPLETE_REQUEST).await.unwrap();
        let (_, first, body) = tokio::time::timeout(Duration::from_secs(2), next)
            .await
            .expect("clean idle EOF must not block the next complete request");
        assert_eq!(first, "POST /complete HTTP/1.1");
        assert_eq!(body, b"ok");
    }

    async fn assert_partial_eof_fails(payload: &[u8]) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let mut requests = HttpRequests::new(listener);
        let mut client = TcpStream::connect(address).await.unwrap();
        client.write_all(payload).await.unwrap();
        client.shutdown().await.unwrap();
        let reader = tokio::spawn(async move { requests.next().await });
        let failed = tokio::time::timeout(Duration::from_secs(2), reader)
            .await
            .expect("partial HTTP EOF must fail instead of waiting for another connection")
            .expect_err("partial HTTP must retain the original strict parser assertion");
        assert!(failed.is_panic());
    }

    #[tokio::test]
    async fn partial_header_eof_remains_a_fixture_failure() {
        assert_partial_eof_fails(b"GET /incomplete HTTP/1.1\r\nHost: fixture\r\n").await;
    }

    #[tokio::test]
    async fn partial_body_eof_remains_a_fixture_failure() {
        assert_partial_eof_fails(
            b"POST /incomplete HTTP/1.1\r\nHost: fixture\r\nContent-Length: 3\r\n\r\nx",
        )
        .await;
    }

    #[tokio::test]
    async fn shutdown_cannot_discard_a_partial_request() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let mut requests = HttpRequests::new(listener);
        let mut client = TcpStream::connect(address).await.unwrap();
        let (accepted, _) = requests.listener.accept().await.unwrap();
        requests.register(accepted);
        client
            .write_all(b"GET /partial HTTP/1.1\r\n")
            .await
            .unwrap();
        client.shutdown().await.unwrap();
        let reader = tokio::spawn(requests.finish());
        assert!(
            reader
                .await
                .expect_err("partial shutdown must fail")
                .is_panic()
        );
    }

    #[tokio::test]
    async fn shutdown_cannot_discard_an_extra_complete_request() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let mut requests = HttpRequests::new(listener);
        let mut client = TcpStream::connect(address).await.unwrap();
        let (accepted, _) = requests.listener.accept().await.unwrap();
        requests.register(accepted);
        client.write_all(COMPLETE_REQUEST).await.unwrap();
        client.shutdown().await.unwrap();
        let reader = tokio::spawn(requests.finish());
        assert!(
            reader
                .await
                .expect_err("extra request must fail")
                .is_panic()
        );
    }
}
