// Copyright 2026 Cloudflare, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! HTTP/2 client session and connection
// TODO: this module needs a refactor

use bytes::Bytes;
use futures::FutureExt;
use h2::client::{self, ResponseFuture, SendRequest};
use h2::{Reason, RecvStream, SendStream};
use http::HeaderMap;
use log::{debug, error, warn};
use pingora_error::{Error, ErrorType, ErrorType::*, OrErr, Result, RetryType};
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_timeout::timeout;
use std::io::ErrorKind;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{ready, Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::watch;
use tokio::sync::Notify;
use tokio::time::Instant as TokioInstant;

use crate::connectors::http::v2::ConnectionRef;
use crate::protocols::http::authority::validate_request_authority_fields;
use crate::protocols::http::v1::client::RESPONSE_HEADER_TIMEDOUT;
use crate::protocols::http::v1::common::validate_content_length_without_transfer_encoding;
use crate::protocols::{Digest, SocketAddr, UniqueIDType};

pub const PING_TIMEDOUT: ErrorType = ErrorType::new("PingTimedout");

/// Validate response headers after HTTP/2 decoding but before Pingora accepts
/// them as an upstream response.
///
/// Reconciles `Content-Length` per RFC 9110 section 8.6 (hyper parity):
/// identical duplicates and comma-combined identical values are accepted while
/// differing or unparseable values are rejected, so an ambiguous H2 response is
/// not forwarded to a downstream H1 or custom session.
fn validate_response_header(header: &ResponseHeader) -> Result<()> {
    validate_content_length_without_transfer_encoding(&header.headers)
}

pub struct Http2Session {
    send_req: SendRequest<Bytes>,
    send_body: Option<SendStream<Bytes>>,
    resp_fut: Option<ResponseFuture>,
    req_sent: Option<Box<RequestHeader>>,
    response_header: Option<ResponseHeader>,
    response_body_reader: Option<RecvStream>,
    /// The read timeout, which will be applied to both reading the header and the body.
    /// The timeout is reset on every read. This is not a timeout on the overall duration of the
    /// response.
    pub read_timeout: Option<Duration>,
    /// The write timeout which will be applied to writing request body.
    /// The timeout is reset on every write. This is not a timeout on the overall duration of the
    /// request.
    pub write_timeout: Option<Duration>,
    // RCoder vendored patch: budget for awaiting the upstream's response header
    // (see `PeerOptions::response_header_timeout`), plus the absolute deadline
    // armed once END_STREAM has been sent on the request stream.
    pub response_header_timeout: Option<Duration>,
    response_header_deadline: Option<TokioInstant>,
    // RCoder vendored patch: signaled when END_STREAM was sent on the request
    // stream. The proxy driver detaches the body writer and streams DATA/
    // END_STREAM through the raw SendStream, so the session can only learn
    // about upload EOS through this signal (notify_one keeps a permit for a
    // waiter that has not reached `notified()` yet).
    end_stream_signal: Arc<Notify>,
    pub conn: ConnectionRef,
    // Indicate that whether a END_STREAM is already sent
    ended: bool,
    // Total DATA payload bytes received from upstream response
    body_recv: usize,
}

impl Drop for Http2Session {
    fn drop(&mut self) {
        self.conn.release_stream();
    }
}

impl Http2Session {
    pub(crate) fn new(send_req: SendRequest<Bytes>, conn: ConnectionRef) -> Self {
        Http2Session {
            send_req,
            send_body: None,
            resp_fut: None,
            req_sent: None,
            response_header: None,
            response_body_reader: None,
            read_timeout: None,
            write_timeout: None,
            response_header_timeout: None,
            response_header_deadline: None,
            end_stream_signal: Arc::new(Notify::new()),
            conn,
            ended: false,
            body_recv: 0,
        }
    }

    /// Prepare H2 authority fields and reject userinfo ([RFC 9113 section 8.3.1]).
    ///
    /// [RFC 9113 section 8.3.1]: https://www.rfc-editor.org/rfc/rfc9113.html#section-8.3.1
    fn sanitize_request_header(req: &mut RequestHeader) -> Result<()> {
        req.set_version(http::Version::HTTP_2);
        validate_request_authority_fields(req)?;
        if req.uri.authority().is_some() {
            return Ok(());
        }
        // use host header to populate :authority field
        let Some(authority) = req.headers.get(http::header::HOST).map(|v| v.as_bytes()) else {
            return Error::e_explain(InvalidHTTPHeader, "no authority header for h2");
        };
        let Some(path_and_query) = req.uri.path_and_query() else {
            return Error::e_explain(InvalidHTTPHeader, "no path and query for h2");
        };
        let uri = http::uri::Builder::new()
            .scheme("https") // fixed for now
            .authority(authority)
            .path_and_query(path_and_query.as_str())
            .build();
        match uri {
            Ok(uri) => {
                req.set_uri(uri);
                Ok(())
            }
            Err(_) => Error::e_explain(
                InvalidHTTPHeader,
                format!("invalid authority from host {authority:?}"),
            ),
        }
    }

    /// Write the request header to the server
    pub fn write_request_header(&mut self, mut req: Box<RequestHeader>, end: bool) -> Result<()> {
        if self.req_sent.is_some() {
            // cannot send again, TODO: warn
            return Ok(());
        }
        Self::sanitize_request_header(&mut req)?;
        let parts = req.as_owned_parts();
        let request = http::Request::from_parts(parts, ());
        // There is no write timeout for h2 because the actual write happens async from this fn
        let (resp_fut, send_body) = self
            .send_req
            .send_request(request, end)
            .or_err(H2Error, "while sending request")
            .map_err(|e| self.handle_err(e))?;
        self.req_sent = Some(req);
        self.send_body = Some(send_body);
        self.resp_fut = Some(resp_fut);
        if end {
            self.mark_end_stream();
        }

        Ok(())
    }

    /// Write a request body chunk
    pub async fn write_request_body(&mut self, data: Bytes, end: bool) -> Result<()> {
        if self.ended {
            warn!("Try to write request body after end of stream, dropping the extra data");
            return Ok(());
        }

        let body_writer = self
            .send_body
            .as_mut()
            .expect("Try to write request body before sending request header");

        super::write_body(body_writer, data, end, self.write_timeout)
            .await
            .map_err(|e| self.handle_err(e))?;
        if end {
            self.mark_end_stream();
        }
        Ok(())
    }

    // RCoder vendored patch: mark END_STREAM as sent and wake any header
    // reader waiting for upload EOS (see `end_stream_signal`).
    fn mark_end_stream(&mut self) {
        if !self.ended {
            self.ended = true;
            self.end_stream_signal.notify_one();
        }
    }

    /// RCoder vendored patch: handle to the END_STREAM notification for
    /// callers that detached the request body writer via
    /// [`Self::take_request_body_writer`] and stream DATA/END_STREAM through
    /// the raw `SendStream` themselves (the proxy driver does). They must
    /// `notify_one()` on this handle once END_STREAM was sent, otherwise a
    /// configured `response_header_timeout` cannot observe upload EOS.
    pub fn request_end_stream_signal(&self) -> Arc<Notify> {
        Arc::clone(&self.end_stream_signal)
    }

    /// Signal that the request body has ended
    pub fn finish_request_body(&mut self) -> Result<()> {
        if self.ended {
            return Ok(());
        }

        let body_writer = self
            .send_body
            .as_mut()
            .expect("Try to finish request stream before sending request header");

        // Just send an empty data frame with end of stream set
        body_writer
            .send_data("".into(), true)
            .or_err(WriteError, "while writing empty h2 request body")
            .map_err(|e| self.handle_err(e))?;
        self.mark_end_stream();
        Ok(())
    }

    /// Read the response header
    pub async fn read_response_header(&mut self) -> Result<()> {
        // TODO: how to read 1xx headers?
        // https://github.com/hyperium/h2/issues/167

        if self.response_header.is_some() {
            panic!("H2 response header is already read")
        }

        // RCoder vendored patch: arm the response header deadline once
        // END_STREAM has been sent on the request stream, so request upload
        // time never counts against the budget. The deadline is absolute;
        // re-entering this function (cancelled reads) never re-arms it.
        // When the body writer was detached, EOS is only observable through
        // the end-stream signal, so wait for that signal (or the header
        // itself, or the per-read timeout) before the bounded wait below.
        if self.response_header_timeout.is_some() && self.response_header_deadline.is_none() {
            if self.ended {
                self.response_header_deadline = self
                    .response_header_timeout
                    .map(|t| TokioInstant::now() + t);
            } else {
                let signal = Arc::clone(&self.end_stream_signal);
                let read_timeout = self.read_timeout;
                let header = std::future::poll_fn(|cx| self.poll_response_header(cx));
                tokio::select! {
                    res = header => {
                        return self.complete_response_header_read(res);
                    }
                    _ = signal.notified() => {
                        self.ended = true;
                        self.response_header_deadline = self
                            .response_header_timeout
                            .map(|t| TokioInstant::now() + t);
                    }
                    _ = tokio::time::sleep(read_timeout.unwrap_or(Duration::MAX)), if read_timeout.is_some() => {
                        return Err(self.handle_err(Error::explain(
                            ReadTimedout,
                            "while reading h2 response header",
                        )));
                    }
                }
            }
        }
        if let Some(deadline) = self.response_header_deadline {
            if TokioInstant::now() >= deadline {
                return Err(Error::explain(
                    RESPONSE_HEADER_TIMEDOUT,
                    "while awaiting the h2 response header",
                )
                .into_up());
            }
        }

        let read_timeout = self.read_timeout;
        // Each wait is bounded by both the per-read timeout (which resets) and
        // the absolute header deadline (which does not).
        let effective_timeout = match (read_timeout, self.response_header_deadline) {
            (Some(read), Some(deadline)) => {
                Some(read.min(deadline.saturating_duration_since(TokioInstant::now())))
            }
            (Some(read), None) => Some(read),
            (None, Some(deadline)) => Some(deadline.saturating_duration_since(TokioInstant::now())),
            (None, None) => None,
        };
        let res = match effective_timeout {
            Some(t) => {
                match timeout(t, std::future::poll_fn(|cx| self.poll_response_header(cx))).await {
                    Ok(res) => res,
                    Err(_) => {
                        // With no per-read timeout the absolute header deadline is
                        // the only bound, so a fired timer is that deadline (the
                        // timer wheel rounds to 10ms ticks and may fire marginally
                        // early); otherwise attribute to the header deadline only
                        // once it has actually elapsed.
                        let deadline_exhausted = match read_timeout {
                            None => self.response_header_deadline.is_some(),
                            Some(_) => self
                                .response_header_deadline
                                .is_some_and(|deadline| TokioInstant::now() >= deadline),
                        };
                        let error = if deadline_exhausted {
                            Error::explain(
                                RESPONSE_HEADER_TIMEDOUT,
                                "while awaiting the h2 response header",
                            )
                        } else {
                            Error::explain(ReadTimedout, "while reading h2 response header")
                        };
                        return Err(self.handle_err(error));
                    }
                }
            }
            None => std::future::poll_fn(|cx| self.poll_response_header(cx)).await,
        };
        self.complete_response_header_read(res)
    }

    // RCoder vendored patch: shared tail for every response-header completion
    // path (pre-arm early response and the bounded/plain wait above).
    fn complete_response_header_read(
        &mut self,
        res: std::result::Result<http::Response<RecvStream>, h2::Error>,
    ) -> Result<()> {
        let (resp, body_reader) = res.map_err(handle_read_header_error)?.into_parts();
        let response_header = ResponseHeader::from(resp);
        validate_response_header(&response_header)?;
        self.response_header = Some(response_header);
        self.response_body_reader = Some(body_reader);

        Ok(())
    }

    #[doc(hidden)]
    pub fn poll_read_response_header(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), h2::Error>> {
        let res = match ready!(self.poll_response_header(cx)) {
            Ok(res) => res,
            Err(err) => return Poll::Ready(Err(err)),
        };

        let (resp, body_reader) = res.into_parts();
        let response_header = ResponseHeader::from(resp);
        if let Err(e) = validate_response_header(&response_header) {
            warn!("invalid h2 response header: {e}");
            return Poll::Ready(Err(Reason::PROTOCOL_ERROR.into()));
        }

        self.response_header = Some(response_header);
        self.response_body_reader = Some(body_reader);

        Poll::Ready(Ok(()))
    }

    fn poll_response_header(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<std::result::Result<http::Response<RecvStream>, h2::Error>> {
        if self.response_header.is_some() {
            panic!("H2 response header is already read")
        }

        let Some(mut resp_fut) = self.resp_fut.take() else {
            panic!("Try to take response header, but it is already taken")
        };

        let res = match resp_fut.poll_unpin(cx) {
            Poll::Ready(Ok(res)) => res,
            Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
            Poll::Pending => {
                self.resp_fut = Some(resp_fut);
                return Poll::Pending;
            }
        };

        Poll::Ready(Ok(res))
    }

    /// Read the response body
    ///
    /// `None` means, no more body to read
    pub async fn read_response_body(&mut self) -> Result<Option<Bytes>> {
        let Some(body_reader) = self.response_body_reader.as_mut() else {
            // req is not sent or response is already read
            // TODO: warn
            return Ok(None);
        };

        let fut = body_reader.data();
        let res = match self.read_timeout {
            Some(t) => timeout(t, fut)
                .await
                .map_err(|_| Error::explain(ReadTimedout, "while reading h2 response body"))?,
            None => fut.await,
        };
        let body = res
            .transpose()
            .or_err(ReadError, "while read h2 response body")
            .map_err(|mut e| {
                // cannot use handle_err() because of borrow checker
                if self.conn.ping_timedout() {
                    e.etype = PING_TIMEDOUT;
                }
                e
            })?;

        if let Some(data) = body.as_ref() {
            body_reader
                .flow_control()
                .release_capacity(data.len())
                .or_err(ReadError, "while releasing h2 response body capacity")?;
            self.body_recv = self.body_recv.saturating_add(data.len());
        }

        Ok(body)
    }

    #[doc(hidden)]
    pub fn poll_read_response_body(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, h2::Error>>> {
        let Some(body_reader) = self.response_body_reader.as_mut() else {
            // req is not sent or response is already read
            // TODO: warn
            return Poll::Ready(None);
        };

        let data = match ready!(body_reader.poll_data(cx)).transpose() {
            Ok(data) => data,
            Err(err) => return Poll::Ready(Some(Err(err))),
        };

        if let Some(data) = data {
            body_reader.flow_control().release_capacity(data.len())?;
            return Poll::Ready(Some(Ok(data)));
        }

        Poll::Ready(None)
    }

    /// Whether the response has ended
    pub fn response_finished(&self) -> bool {
        // if response_body_reader doesn't exist, the response is not even read yet
        self.response_body_reader
            .as_ref()
            .is_some_and(|reader| reader.is_end_stream())
    }

    /// Check whether stream finished with error.
    /// Like `response_finished`, but also attempts to poll the h2 stream for errors that may have
    /// caused the stream to terminate, and returns them as `H2Error`s.
    pub fn check_response_end_or_error(&mut self) -> Result<bool> {
        let Some(reader) = self.response_body_reader.as_mut() else {
            // response is not even read
            return Ok(false);
        };

        if !reader.is_end_stream() {
            return Ok(false);
        }

        // https://github.com/hyperium/h2/issues/806
        // The fundamental issue is that h2::RecvStream may return `is_end_stream` true
        // when the stream was naturally closed via END_STREAM /OR/ if there was an error
        // while reading data frames that forced the closure.
        // The h2 API as-is makes it difficult to determine which situation is occurring.
        //
        // `poll_data` should be returning None after `is_end_stream`, if the stream
        // is truly expecting no more data to be sent.
        // https://docs.rs/h2/latest/h2/struct.RecvStream.html#method.is_end_stream
        // So poll the data once to check this condition. If an error is returned, that indicates
        // that the stream closed due to an error e.g. h2 protocol error.
        //
        // tokio::task::unconstrained because now_or_never may yield None when the future is ready
        match tokio::task::unconstrained(reader.data()).now_or_never() {
            Some(None) => Ok(true),
            Some(Some(Ok(_))) => Error::e_explain(H2Error, "unexpected data after end stream"),
            Some(Some(Err(e))) => Error::e_because(H2Error, "while checking end stream", e),
            None => {
                // RecvStream data() should be ready to poll after the stream ends,
                // this indicates an unexpected change in the h2 crate
                panic!("data() not ready after end stream")
            }
        }
    }

    /// Read the optional trailer headers
    pub async fn read_trailers(&mut self) -> Result<Option<HeaderMap>> {
        let Some(reader) = self.response_body_reader.as_mut() else {
            // response is not even read
            // TODO: warn
            return Ok(None);
        };
        let fut = reader.trailers();

        let res = match self.read_timeout {
            Some(t) => timeout(t, fut)
                .await
                .map_err(|_| Error::explain(ReadTimedout, "while reading h2 trailer"))
                .map_err(|e| self.handle_err(e))?,
            None => fut.await,
        };
        match res {
            Ok(t) => Ok(t),
            Err(e) => {
                // GOAWAY with no error: this is graceful shutdown, continue as if no trailer
                // RESET_STREAM with no error: https://datatracker.ietf.org/doc/html/rfc9113#section-8.1:
                // this is to signal client to stop uploading request without breaking the response.
                // TODO: should actually stop uploading
                // TODO: should we try reading again?
                // TODO: handle this when reading headers and body as well
                // https://github.com/hyperium/h2/issues/741

                if (e.is_go_away() || e.is_reset())
                    && e.is_remote()
                    && e.reason() == Some(Reason::NO_ERROR)
                {
                    Ok(None)
                } else {
                    Err(e)
                }
            }
        }
        .or_err(ReadError, "while reading h2 trailers")
    }

    /// The request header if it is already sent
    pub fn request_header(&self) -> Option<&RequestHeader> {
        self.req_sent.as_deref()
    }

    /// The response header if it is already read
    pub fn response_header(&self) -> Option<&ResponseHeader> {
        self.response_header.as_ref()
    }

    /// Give up the http session abruptly.
    pub fn shutdown(&mut self) {
        if !self.ended || !self.response_finished() {
            if let Some(send_body) = self.send_body.as_mut() {
                send_body.send_reset(h2::Reason::INTERNAL_ERROR)
            }
        }
    }

    /// Drop everything in this h2 stream. Return the connection ref.
    /// After this function the underlying h2 connection should already notify the closure of this
    /// stream so that another stream can be created if needed.
    pub(crate) fn conn(&self) -> ConnectionRef {
        self.conn.clone()
    }

    /// Whether ping timeout occurred. After a ping timeout, the h2 connection will be terminated.
    /// Ongoing h2 streams will receive an stream/connection error. The streams should check this
    /// flag to tell whether the error is triggered by the timeout.
    pub(crate) fn ping_timedout(&self) -> bool {
        self.conn.ping_timedout()
    }

    /// Return the [Digest] of the connection
    ///
    /// For reused connection, the timing in the digest will reflect its initial handshakes
    /// The caller should check if the connection is reused to avoid misuse the timing field.
    pub fn digest(&self) -> Option<&Digest> {
        Some(self.conn.digest())
    }

    /// Return a mutable [Digest] reference for the connection
    ///
    /// Will return `None` if multiple H2 streams are open.
    pub fn digest_mut(&mut self) -> Option<&mut Digest> {
        self.conn.digest_mut()
    }

    /// Return the server (peer) address recorded in the connection digest.
    pub fn server_addr(&self) -> Option<&SocketAddr> {
        self.conn
            .digest()
            .socket_digest
            .as_ref()
            .map(|d| d.peer_addr())?
    }

    /// Return the client (local) address recorded in the connection digest.
    pub fn client_addr(&self) -> Option<&SocketAddr> {
        self.conn
            .digest()
            .socket_digest
            .as_ref()
            .map(|d| d.local_addr())?
    }

    /// the FD of the underlying connection
    pub fn fd(&self) -> UniqueIDType {
        self.conn.id()
    }

    /// Upstream response body bytes received (HTTP/2 DATA payload; excludes headers/framing).
    pub fn body_bytes_received(&self) -> usize {
        self.body_recv
    }

    /// take the body sender to another task to perform duplex read and write
    pub fn take_request_body_writer(&mut self) -> Option<SendStream<Bytes>> {
        self.send_body.take()
    }

    fn handle_err(&self, mut e: Box<Error>) -> Box<Error> {
        if self.ping_timedout() {
            e.etype = PING_TIMEDOUT;
        }

        // is_go_away: retry via another connection, this connection is being teardown
        // should retry
        if self.response_header.is_none() {
            if let Some(err) = e.root_cause().downcast_ref::<h2::Error>() {
                if err.is_go_away()
                    && err.is_remote()
                    && (err.reason() == Some(h2::Reason::NO_ERROR))
                {
                    e.retry = true.into();
                }
            }
        }
        e
    }
}

/* helper functions */

/* Types of errors during h2 header read
 1. peer requests to downgrade to h1, mostly IIS server for NTLM: we will downgrade and retry
 2. peer sends invalid h2 frames, usually sending h1 only header: we will downgrade and retry
 3. peer sends GO_AWAY(NO_ERROR) connection is being shut down: we will retry
 4. peer IO error on reused conn, usually firewall kills old conn: we will retry
 5. peer sends REFUSED_STREAM on RST_STREAM, this is safe to retry
 6. All other errors will terminate the request
*/
fn handle_read_header_error(e: h2::Error) -> Box<Error> {
    if e.is_remote() && (e.reason() == Some(h2::Reason::HTTP_1_1_REQUIRED)) {
        let mut err = Error::because(H2Downgrade, "while reading h2 header", e);
        err.retry = true.into();
        err
    } else if e.is_go_away() && e.is_library() && (e.reason() == Some(h2::Reason::PROTOCOL_ERROR)) {
        // remote send invalid H2 responses
        let mut err = Error::because(InvalidH2, "while reading h2 header", e);
        err.retry = true.into();
        err
    } else if e.is_go_away() && e.is_remote() && (e.reason() == Some(h2::Reason::NO_ERROR)) {
        // is_go_away: retry via another connection, this connection is being teardown
        let mut err = Error::because(H2Error, "while reading h2 header", e);
        err.retry = true.into();
        err
    } else if e.is_reset() && e.is_remote() && (e.reason() == Some(h2::Reason::REFUSED_STREAM)) {
        // The REFUSED_STREAM error code can be included in a RST_STREAM frame to indicate
        // that the stream is being closed prior to any processing having occurred.
        // Any request that was sent on the reset stream can be safely retried.
        // https://datatracker.ietf.org/doc/html/rfc9113#section-8.7
        let mut err = Error::because(H2Error, "while reading h2 header", e);
        err.retry = true.into();
        err
    } else if e.is_io() {
        // is_io: typical if a previously reused connection silently drops it
        // only retry if the connection is reused
        // safety: e.get_io() will always succeed if e.is_io() is true
        let io_err = e.get_io().expect("checked is io");

        // for h2 hyperium raw_os_error() will be None unless this is a new connection
        // where we handshake() and from_io() is called, check ErrorKind explicitly with true_io_error
        let true_io_error = io_err.raw_os_error().is_some()
            || matches!(
                io_err.kind(),
                ErrorKind::ConnectionReset | ErrorKind::TimedOut | ErrorKind::BrokenPipe
            );
        let mut err = Error::because(ReadError, "while reading h2 header", e);
        if true_io_error {
            err.retry = RetryType::ReusedOnly;
        } // else could be TLS error, which is unsafe to retry
        err
    } else {
        Error::because(H2Error, "while reading h2 header", e)
    }
}

use tokio::sync::oneshot;

pub async fn drive_connection<S>(
    mut c: client::Connection<S>,
    id: UniqueIDType,
    closed: watch::Sender<bool>,
    ping_interval: Option<Duration>,
    ping_timeout_occurred: Arc<AtomicBool>,
) where
    S: AsyncRead + AsyncWrite + Send + Unpin,
{
    let interval = ping_interval.unwrap_or(Duration::ZERO);
    if !interval.is_zero() {
        // for ping to inform this fn to drop the connection
        let (tx, rx) = oneshot::channel::<()>();
        // for this fn to inform ping to give up when it is already dropped
        let dropped = Arc::new(AtomicBool::new(false));
        let dropped2 = dropped.clone();

        if let Some(ping_pong) = c.ping_pong() {
            pingora_runtime::current_handle().spawn(async move {
                do_ping_pong(ping_pong, interval, tx, dropped2, id).await;
            });
        } else {
            warn!("Cannot get ping-pong handler from h2 connection");
        }

        tokio::select! {
            r = c => match r {
                Ok(_) => debug!("H2 connection finished fd: {id}"),
                Err(e) => debug!("H2 connection fd: {id} errored: {e:?}"),
            },
            r = rx => match r {
                Ok(_) => {
                    ping_timeout_occurred.store(true, Ordering::Relaxed);
                    warn!("H2 connection Ping timeout/Error fd: {id}, closing conn");
                },
                Err(e) => warn!("H2 connection Ping Rx error {e:?}"),
            },
        };

        dropped.store(true, Ordering::Relaxed);
    } else {
        match c.await {
            Ok(_) => debug!("H2 connection finished fd: {id}"),
            Err(e) => debug!("H2 connection fd: {id} errored: {e:?}"),
        }
    }
    let _ = closed.send(true);
}

const PING_TIMEOUT: Duration = Duration::from_secs(5);

async fn do_ping_pong(
    mut ping_pong: h2::PingPong,
    interval: Duration,
    tx: oneshot::Sender<()>,
    dropped: Arc<AtomicBool>,
    id: UniqueIDType,
) {
    // delay before sending the first ping, no need to race with the first request
    tokio::time::sleep(interval).await;
    loop {
        if dropped.load(Ordering::Relaxed) {
            break;
        }
        let ping_fut = ping_pong.ping(h2::Ping::opaque());
        debug!("H2 fd: {id} ping sent");
        match tokio::time::timeout(PING_TIMEOUT, ping_fut).await {
            Err(_) => {
                error!("H2 fd: {id} ping timeout");
                let _ = tx.send(());
                break;
            }
            Ok(r) => match r {
                Ok(_) => {
                    debug!("H2 fd: {} pong received", id);
                    tokio::time::sleep(interval).await;
                }
                Err(e) => {
                    if dropped.load(Ordering::Relaxed) {
                        // drive_connection() exits first, no need to error again
                        break;
                    }
                    error!("H2 fd: {id} ping error: {e}");
                    let _ = tx.send(());
                    break;
                }
            },
        }
    }
}

#[cfg(test)]
mod tests_h2 {
    use super::*;
    use bytes::Bytes;
    use http::{Response, StatusCode};
    use tokio::io::duplex;
    use tokio::sync::oneshot;

    #[test]
    fn sanitize_request_header_rejects_invalid_authority_fields() {
        let mut host = RequestHeader::build_no_case("GET", b"/test", None).unwrap();
        host.insert_header(http::header::HOST, "user@evil.example")
            .unwrap();
        assert!(Http2Session::sanitize_request_header(&mut host).is_err());

        let mut uri = RequestHeader::from(
            http::Request::builder()
                .uri("https://user@evil.example/test")
                .body(())
                .unwrap()
                .into_parts()
                .0,
        );
        assert!(Http2Session::sanitize_request_header(&mut uri).is_err());

        let mut mismatch = RequestHeader::from(
            http::Request::builder()
                .uri("https://authority.example/test")
                .header(http::header::HOST, "other.example")
                .body(())
                .unwrap()
                .into_parts()
                .0,
        );
        assert!(Http2Session::sanitize_request_header(&mut mismatch).is_err());

        let mut duplicate = RequestHeader::build_no_case("GET", b"/test", None).unwrap();
        duplicate
            .append_header(http::header::HOST, "authority.example")
            .unwrap();
        duplicate
            .append_header(http::header::HOST, "authority.example")
            .unwrap();
        assert!(Http2Session::sanitize_request_header(&mut duplicate).is_err());
    }

    async fn session_with_delayed_response() -> (
        Http2Session,
        oneshot::Sender<()>,
        tokio::task::JoinHandle<()>,
        tokio::task::JoinHandle<()>,
    ) {
        let (client_io, server_io) = duplex(65536);
        let (request_accepted_tx, request_accepted_rx) = oneshot::channel();
        let (release_response_tx, release_response_rx) = oneshot::channel();

        let server_task = tokio::spawn(async move {
            let mut conn = h2::server::handshake(server_io).await.unwrap();
            if let Some(result) = conn.accept().await {
                let (req, mut send_resp) = result.unwrap();
                assert_eq!(req.method(), http::Method::GET);
                let _ = request_accepted_tx.send(());
                let _ = release_response_rx.await;

                let resp = Response::builder().status(StatusCode::OK).body(()).unwrap();
                send_resp.send_response(resp, true).unwrap();
                conn.graceful_shutdown();
            }
            while let Some(_result) = conn.accept().await {}
        });

        let (send_req, connection) = h2::client::handshake(client_io).await.unwrap();
        let (closed_tx, closed_rx) = tokio::sync::watch::channel(false);
        let ping_timeout = Arc::new(AtomicBool::new(false));
        let connection_task = tokio::spawn(async move {
            let _ = connection.await;
            let _ = closed_tx.send(true);
        });

        let conn_ref = crate::connectors::http::v2::ConnectionRef::new(
            send_req.clone(),
            closed_rx,
            ping_timeout,
            0,
            1,
            Digest::default(),
        );
        let mut h2s = Http2Session::new(send_req, conn_ref);
        let mut req = RequestHeader::build("GET", b"/", None).unwrap();
        req.insert_header(http::header::HOST, "example.com")
            .unwrap();
        h2s.write_request_header(Box::new(req), true).unwrap();

        request_accepted_rx
            .await
            .expect("server should accept the request before the response-header read");

        (h2s, release_response_tx, server_task, connection_task)
    }

    #[tokio::test]
    async fn response_header_read_can_resume_after_read_timeout() {
        let (mut h2s, release_response, server_task, connection_task) =
            session_with_delayed_response().await;
        h2s.read_timeout = Some(Duration::from_millis(1));

        let err = h2s
            .read_response_header()
            .await
            .expect_err("delayed response header should hit the read timeout");
        assert!(
            matches!(err.etype, ReadTimedout),
            "unexpected first read error: {err:?}"
        );
        assert!(h2s.response_header().is_none());
        assert!(
            h2s.resp_fut.is_some(),
            "timing out must not drop the pending response future"
        );

        h2s.read_timeout = None;
        release_response.send(()).unwrap();
        h2s.read_response_header().await.unwrap();
        assert_eq!(h2s.response_header().unwrap().status, StatusCode::OK);

        server_task.abort();
        connection_task.abort();
    }

    #[tokio::test]
    async fn response_header_read_can_resume_after_external_cancellation() {
        let (mut h2s, release_response, server_task, connection_task) =
            session_with_delayed_response().await;

        let first_read =
            tokio::time::timeout(Duration::from_millis(1), h2s.read_response_header()).await;
        assert!(
            first_read.is_err(),
            "external timeout should cancel the pending header read"
        );
        assert!(h2s.response_header().is_none());
        assert!(
            h2s.resp_fut.is_some(),
            "cancelling the read must not drop the pending response future"
        );

        release_response.send(()).unwrap();
        h2s.read_response_header().await.unwrap();
        assert_eq!(h2s.response_header().unwrap().status, StatusCode::OK);

        server_task.abort();
        connection_task.abort();
    }

    #[tokio::test]
    async fn h2_body_bytes_received_multi_frames() {
        let (client_io, server_io) = duplex(65536);

        // Server: respond with two DATA frames "a" and "bc"
        tokio::spawn(async move {
            let mut conn = h2::server::handshake(server_io).await.unwrap();
            if let Some(result) = conn.accept().await {
                let (req, mut send_resp) = result.unwrap();
                assert_eq!(req.method(), http::Method::GET);
                let resp = Response::builder().status(StatusCode::OK).body(()).unwrap();
                let mut send_stream = send_resp.send_response(resp, false).unwrap();
                send_stream.send_data(Bytes::from("a"), false).unwrap();
                send_stream.send_data(Bytes::from("bc"), true).unwrap();
                // Signal graceful shutdown so the accept loop can exit after the client finishes
                conn.graceful_shutdown();
            }
            // Drive the server connection until the client closes
            while let Some(_res) = conn.accept().await {}
        });

        // Client: build Http2Session and read response
        let (send_req, connection) = h2::client::handshake(client_io).await.unwrap();
        let (closed_tx, closed_rx) = tokio::sync::watch::channel(false);
        let ping_timeout = Arc::new(AtomicBool::new(false));
        tokio::spawn(async move {
            let _ = connection.await;
            let _ = closed_tx.send(true);
        });

        let digest = Digest::default();
        let conn_ref = crate::connectors::http::v2::ConnectionRef::new(
            send_req.clone(),
            closed_rx,
            ping_timeout,
            0,
            1,
            digest,
        );
        let mut h2s = Http2Session::new(send_req, conn_ref);

        // minimal request
        let mut req = RequestHeader::build("GET", b"/", None).unwrap();
        req.insert_header(http::header::HOST, "example.com")
            .unwrap();
        h2s.write_request_header(Box::new(req), true).unwrap();
        h2s.read_response_header().await.unwrap();

        let mut total = 0;
        while let Some(chunk) = h2s.read_response_body().await.unwrap() {
            total += chunk.len();
        }
        assert_eq!(total, 3);
        assert_eq!(h2s.body_bytes_received(), 3);
    }

    #[test]
    fn h2_response_conflicting_content_length_rejected() {
        let mut response = ResponseHeader::build(StatusCode::OK, None).unwrap();
        response
            .append_header(http::header::CONTENT_LENGTH, "5")
            .unwrap();
        response
            .append_header(http::header::CONTENT_LENGTH, "6")
            .unwrap();

        let err = validate_response_header(&response).unwrap_err();
        assert_eq!(err.etype(), &InvalidHTTPHeader);
    }

    /// RCoder vendored-patch regression: the proxy driver detaches the request
    /// body writer (`take_request_body_writer`) and streams DATA/END_STREAM
    /// through the raw `SendStream`, so the session-side `ended` flag never
    /// flips on that path. A configured `response_header_timeout` must still
    /// bound the header wait once upload EOS was sent from the detached
    /// writer (the driver notifies the session's end-stream signal).
    #[tokio::test]
    async fn detached_upload_end_stream_still_bounds_header_deadline() {
        use tokio::io::duplex;

        let (client_io, server_io) = duplex(65536);
        let (request_body_done_tx, request_body_done_rx) = oneshot::channel();
        let server_task = tokio::spawn(async move {
            let mut conn = h2::server::handshake(server_io).await.unwrap();
            if let Some(result) = conn.accept().await {
                let (req, _send_resp) = result.unwrap();
                assert_eq!(req.method(), http::Method::POST);
                let mut body = req.into_body();
                while let Some(chunk) = body.data().await {
                    let chunk = chunk.unwrap();
                    body.flow_control().release_capacity(chunk.len()).unwrap();
                }
                let _ = request_body_done_tx.send(());
                // blackhole: never send the response header
                std::future::pending::<()>().await;
            }
        });

        let (send_req, connection) = h2::client::handshake(client_io).await.unwrap();
        let (closed_tx, closed_rx) = tokio::sync::watch::channel(false);
        let ping_timeout = Arc::new(AtomicBool::new(false));
        let connection_task = tokio::spawn(async move {
            let _ = connection.await;
            let _ = closed_tx.send(true);
        });
        let conn_ref = crate::connectors::http::v2::ConnectionRef::new(
            send_req.clone(),
            closed_rx,
            ping_timeout,
            0,
            1,
            Digest::default(),
        );
        let mut h2s = Http2Session::new(send_req, conn_ref);

        // mirror the real driver: header without end, then the detached writer
        // carries the body and END_STREAM
        let mut req = RequestHeader::build("POST", b"/", None).unwrap();
        req.insert_header(http::header::HOST, "example.com")
            .unwrap();
        h2s.write_request_header(Box::new(req), false).unwrap();
        let end_stream_signal = h2s.request_end_stream_signal();
        let mut writer = h2s.take_request_body_writer().expect("writer present");
        writer
            .send_data(Bytes::from_static(b"payload"), true)
            .unwrap();
        // the driver notifies the session once END_STREAM was sent on the
        // detached writer (see the vendored proxy_h2 wiring)
        end_stream_signal.notify_one();
        request_body_done_rx
            .await
            .expect("server should consume the request body");

        h2s.response_header_timeout = Some(Duration::from_millis(100));
        let err = tokio::time::timeout(Duration::from_millis(400), h2s.read_response_header())
            .await
            .expect("header deadline must bound the detached-upload wait (4x budget)")
            .expect_err("blackhole upstream must fail the header read");
        assert!(
            matches!(
                err.etype,
                crate::protocols::http::v1::client::RESPONSE_HEADER_TIMEDOUT
            ),
            "unexpected error type: {err:?}"
        );

        server_task.abort();
        connection_task.abort();
    }

    /// RCoder vendored-patch regression: a header-deadline timeout on one
    /// stream must only cancel that stream — a concurrent stream on the same
    /// connection keeps working, and the timed-out stream is actually reset
    /// (the server can no longer respond on it).
    #[tokio::test]
    async fn header_deadline_timeout_isolates_the_stream() {
        use tokio::io::duplex;

        let (client_io, server_io) = duplex(65536);
        let (a_body_done_tx, a_body_done_rx) = oneshot::channel::<()>();
        let (probe_a_reset_tx, probe_a_reset_rx) = oneshot::channel::<()>();
        let (a_reset_observed_tx, a_reset_observed_rx) = oneshot::channel::<bool>();
        let (b_done_tx, b_done_rx) = oneshot::channel::<bool>();
        let mut probe_a_reset_rx = Some(probe_a_reset_rx);
        let server_task = tokio::spawn(async move {
            let mut conn = h2::server::handshake(server_io).await.unwrap();
            let mut a_body_done_tx = Some(a_body_done_tx);
            let mut probe_rx = probe_a_reset_rx.take();
            let mut a_reset_observed_tx = Some(a_reset_observed_tx);
            let mut b_done_tx = Some(b_done_tx);
            while let Some(result) = conn.accept().await {
                let (req, mut send_resp) = result.unwrap();
                if req.method() == http::Method::POST {
                    let body_done_tx = a_body_done_tx.take().expect("one POST stream");
                    let mut probe_rx = probe_rx.take().expect("one POST stream");
                    let observed_tx = a_reset_observed_tx.take().expect("one POST stream");
                    tokio::spawn(async move {
                        let mut body = req.into_body();
                        while let Some(chunk) = body.data().await {
                            let chunk = chunk.unwrap();
                            body.flow_control().release_capacity(chunk.len()).unwrap();
                        }
                        let _ = body_done_tx.send(());
                        // blackhole until the client reports the timeout, then
                        // prove the stream was reset: poll_reset yields the
                        // reset Reason once the peer cancels this stream
                        let _ = probe_rx.await;
                        let reset_reason =
                            std::future::poll_fn(|cx| send_resp.poll_reset(cx)).await;
                        let reset_observed =
                            matches!(reset_reason, Ok(reason) if reason == h2::Reason::CANCEL);
                        let _ = observed_tx.send(reset_observed);
                    });
                } else {
                    let done_tx = b_done_tx.take().expect("one GET stream");
                    tokio::spawn(async move {
                        let resp = Response::builder().status(StatusCode::OK).body(()).unwrap();
                        let _ = done_tx.send(send_resp.send_response(resp, true).is_ok());
                    });
                }
            }
        });

        let (send_req, connection) = h2::client::handshake(client_io).await.unwrap();
        let (closed_tx, closed_rx) = tokio::sync::watch::channel(false);
        let ping_timeout = Arc::new(AtomicBool::new(false));
        let connection_task = tokio::spawn(async move {
            let _ = connection.await;
            let _ = closed_tx.send(true);
        });
        let conn_ref = crate::connectors::http::v2::ConnectionRef::new(
            send_req.clone(),
            closed_rx,
            ping_timeout,
            0,
            2,
            Digest::default(),
        );

        // stream A: detached upload + blackhole, with a 100ms header budget
        let mut h2s_a = Http2Session::new(send_req.clone(), conn_ref.clone());
        let mut req = RequestHeader::build("POST", b"/a", None).unwrap();
        req.insert_header(http::header::HOST, "example.com")
            .unwrap();
        h2s_a.write_request_header(Box::new(req), false).unwrap();
        let end_stream_signal = h2s_a.request_end_stream_signal();
        let mut writer_a = h2s_a.take_request_body_writer().expect("writer present");
        writer_a
            .send_data(Bytes::from_static(b"payload"), true)
            .unwrap();
        end_stream_signal.notify_one();
        h2s_a.response_header_timeout = Some(Duration::from_millis(100));
        a_body_done_rx
            .await
            .expect("server should consume stream A body");

        // stream B: normal request on the same connection while A blackholes
        let mut h2s_b = Http2Session::new(send_req, conn_ref);
        let mut req_b = RequestHeader::build("GET", b"/b", None).unwrap();
        req_b
            .insert_header(http::header::HOST, "example.com")
            .unwrap();
        h2s_b.write_request_header(Box::new(req_b), true).unwrap();
        h2s_b.response_header_timeout = Some(Duration::from_millis(100));
        h2s_b
            .read_response_header()
            .await
            .expect("concurrent stream B must keep working");
        assert_eq!(h2s_b.response_header().unwrap().status, StatusCode::OK);
        assert!(b_done_rx.await.expect("stream B server handler"));

        // stream A times out with the dedicated error type
        let a_read = tokio::time::timeout(Duration::from_secs(1), h2s_a.read_response_header())
            .await
            .expect("stream A deadline must fire within 1s")
            .expect_err("blackhole stream A must fail");
        assert!(
            matches!(
                a_read.etype,
                crate::protocols::http::v1::client::RESPONSE_HEADER_TIMEDOUT
            ),
            "unexpected stream A error type: {a_read:?}"
        );
        // dropping the session (with the taken writer) resets stream A
        drop(h2s_a);
        drop(writer_a);
        probe_a_reset_tx
            .send(())
            .expect("server still probing stream A");
        let reset_observed = tokio::time::timeout(Duration::from_secs(2), a_reset_observed_rx)
            .await
            .expect("server must report the stream A probe within 2s")
            .expect("reset observation channel intact");
        assert!(
            reset_observed,
            "the timed-out stream must be reset for the server (poll_reset)"
        );

        server_task.abort();
        connection_task.abort();
    }

    #[test]
    fn h2_response_duplicate_identical_content_length_accepted() {
        // RFC 9110 section 8.6 / hyper: identical duplicate (or comma-combined
        // identical) Content-Length values are reconciled to a single value.
        let mut response = ResponseHeader::build(StatusCode::OK, None).unwrap();
        response
            .append_header(http::header::CONTENT_LENGTH, "5")
            .unwrap();
        response
            .append_header(http::header::CONTENT_LENGTH, "5")
            .unwrap();
        assert!(validate_response_header(&response).is_ok());

        let mut response = ResponseHeader::build(StatusCode::OK, None).unwrap();
        response
            .append_header(http::header::CONTENT_LENGTH, "5, 5")
            .unwrap();
        assert!(validate_response_header(&response).is_ok());
    }
}
