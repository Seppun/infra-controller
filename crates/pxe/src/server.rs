/*
 * SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
 * SPDX-License-Identifier: Apache-2.0
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 * http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */
//! HTTP accept loop with bounded per-connection timeouts.
//!
//! `axum::serve` builds hyper connections without a timer, which disables
//! hyper's header-read timeout, and its HTTP version sniff has no timeout at
//! all. A UEFI HTTP client that holds a keep-alive connection open, or vanishes
//! before sending a request, then keeps its socket and file descriptor forever.
//! PXE clients speak HTTP/1.1 only, so this loop serves HTTP/1 directly with a
//! timer installed and closes connections that do not deliver a request head
//! within [`HEADER_READ_TIMEOUT`]. That covers idle and never-sent
//! connections. A peer that vanishes without closing never fails a read or
//! write on its own, so the kernel bounds those cases: TCP keepalive probes a
//! socket that is idle and half-open, and on Linux [`TCP_USER_TIMEOUT`] bounds
//! how long a response write may stay unacknowledged. On other platforms a
//! peer that dies mid-transfer is reaped only after the kernel exhausts its
//! retransmit budget.

use std::convert::Infallible;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Extension;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, Response};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use opentelemetry::metrics::Meter;
use socket2::{SockRef, TcpKeepalive};
use tokio::net::TcpListener;
use tower::Service;

/// How long a connection may sit without delivering a complete request head,
/// whether it is brand new or idle between keep-alive requests, before the
/// server closes it.
pub(crate) const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Kernel keepalive for accepted connections: the first probe goes out after
/// 60s without traffic and probes repeat every 10s. With Linux's default of 9
/// unanswered probes an idle half-open peer is reaped about 150s after its
/// last packet. Keepalive only probes an idle socket; a connection with
/// unacknowledged data is governed by the retransmit budget instead, which
/// [`TCP_USER_TIMEOUT`] bounds.
const TCP_KEEPALIVE: TcpKeepalive = TcpKeepalive::new()
    .with_time(Duration::from_secs(60))
    .with_interval(Duration::from_secs(10));

/// How long transmitted data may stay unacknowledged before the kernel closes
/// the connection, so a peer that vanishes while the server is writing a
/// response is reaped on the same schedule as an idle one instead of after the
/// tcp_retries2 budget (about 15 minutes). Linux only.
#[cfg(any(target_os = "linux", target_os = "android"))]
const TCP_USER_TIMEOUT: Duration = Duration::from_secs(150);

/// Connections accepted and not yet finished.
static ACTIVE_CONNECTIONS: AtomicUsize = AtomicUsize::new(0);

/// Connections currently open, as sampled by the
/// `carbide_pxe_active_connections` gauge.
fn active_connections() -> usize {
    ACTIVE_CONNECTIONS.load(Ordering::Relaxed)
}

/// Registers the point-in-time connection gauge on `meter`. The header read
/// timeout counter is an Event and needs no registration.
pub(crate) fn register_metrics(meter: &Meter) {
    meter
        .u64_observable_gauge("carbide_pxe_active_connections")
        .with_description("Number of PXE HTTP connections currently open")
        .with_callback(|observer| observer.observe(active_connections() as u64, &[]))
        .build();
}

/// Holds one slot in [`ACTIVE_CONNECTIONS`] for as long as it is alive.
struct ActiveConnection;

impl ActiveConnection {
    fn open() -> Self {
        ACTIVE_CONNECTIONS.fetch_add(1, Ordering::Relaxed);
        Self
    }
}

impl Drop for ActiveConnection {
    fn drop(&mut self) {
        ACTIVE_CONNECTIONS.fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(carbide_instrument::Event)]
#[event(
    event_name = "pxe_connection_header_read_timed_out",
    metric_name = "carbide_pxe_connection_header_read_timeouts_total",
    component = "carbide-pxe",
    log = info,
    metric = counter,
    message = "closed connection that delivered no request head within the header read timeout",
    describe = "Number of PXE HTTP connections closed because no request head arrived within the header read timeout."
)]
struct ConnectionHeaderReadTimedOut {
    #[context]
    peer_address: SocketAddr,
    #[context]
    active_connection_count: usize,
}

/// Serves `app` on `listener`. Each connection carries `ConnectInfo<SocketAddr>`
/// for the peer, matching what `Router::into_make_service_with_connect_info`
/// provides.
pub(crate) async fn serve<S>(
    listener: TcpListener,
    app: S,
    header_read_timeout: Duration,
) -> io::Result<()>
where
    S: Service<Request<Body>, Response = Response<Body>, Error = Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send,
{
    let mut builder = http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(header_read_timeout);

    loop {
        let (stream, peer_address) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(error) if is_connection_error(&error) => continue,
            Err(error) => {
                // Typically fd exhaustion; back off so the loop does not spin.
                tracing::error!(%error, "accept failed");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };

        let socket = SockRef::from(&stream);
        if let Err(error) = socket.set_tcp_keepalive(&TCP_KEEPALIVE) {
            tracing::warn!(%error, %peer_address, "unable to enable tcp keepalive");
        }
        #[cfg(any(target_os = "linux", target_os = "android"))]
        if let Err(error) = socket.set_tcp_user_timeout(Some(TCP_USER_TIMEOUT)) {
            tracing::warn!(%error, %peer_address, "unable to set tcp user timeout");
        }
        let active = ActiveConnection::open();

        let builder = builder.clone();
        let app = tower::ServiceBuilder::new()
            .map_request(|request: Request<Incoming>| request.map(Body::new))
            .layer(Extension(ConnectInfo(peer_address)))
            .service(app.clone());
        tokio::spawn(async move {
            let _active = active;
            if let Err(error) = builder
                .serve_connection(TokioIo::new(stream), TowerToHyperService::new(app))
                .await
            {
                if error.is_timeout() {
                    carbide_instrument::emit(ConnectionHeaderReadTimedOut {
                        peer_address,
                        active_connection_count: active_connections(),
                    });
                } else {
                    tracing::debug!(%error, %peer_address, "connection closed with error");
                }
            }
        });
    }
}

fn is_connection_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
    )
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::routing::get;
    use carbide_instrument::testing::MetricsCapture;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    use super::*;

    const TEST_TIMEOUT: Duration = Duration::from_millis(200);
    const WAIT: Duration = Duration::from_secs(5);
    const TIMEOUTS_METRIC: &str = "carbide_pxe_connection_header_read_timeouts_total";
    const ACTIVE_CONNECTIONS_METRIC: &str = "carbide_pxe_active_connections";
    /// Large enough that the server is still writing when the client leaves.
    const LARGE_BODY_LEN: usize = 64 * 1024 * 1024;

    #[derive(Debug, Clone, Copy)]
    enum Step {
        /// Send a request and expect a 200 whose body echoes the client address.
        Request,
        /// Send nothing and expect the server to close the connection.
        IdleUntilClosed,
    }

    async fn start_server() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new()
            .route(
                "/",
                get(|ConnectInfo(peer): ConnectInfo<SocketAddr>| async move { peer.to_string() }),
            )
            .route("/large", get(|| async { vec![b'x'; LARGE_BODY_LEN] }));
        tokio::spawn(serve(listener, app, TEST_TIMEOUT));
        addr
    }

    async fn wait_for_active_connections(expected: usize) {
        tokio::time::timeout(WAIT, async {
            while active_connections() != expected {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "active connections stayed at {} instead of returning to {expected}",
                active_connections()
            )
        });
    }

    /// The gauge samples the live guard count on each scrape and reports the
    /// exposed name, so a renamed or unregistered gauge fails here.
    #[test]
    fn active_connections_gauge_samples_open_guards() {
        let metrics = MetricsCapture::start();
        register_metrics(&opentelemetry::global::meter("carbide-pxe"));
        let idle = active_connections() as f64;
        let held = [ActiveConnection::open(), ActiveConnection::open()];
        assert_eq!(
            metrics.gauge_value(ACTIVE_CONNECTIONS_METRIC, &[]),
            idle + 2.0,
            "{}",
            metrics.render()
        );
        drop(held);
        assert_eq!(metrics.gauge_value(ACTIVE_CONNECTIONS_METRIC, &[]), idle);
    }

    async fn read_until(stream: &mut TcpStream, needle: &str) -> String {
        let mut buf = Vec::new();
        tokio::time::timeout(WAIT, async {
            loop {
                let mut chunk = [0u8; 1024];
                let n = stream.read(&mut chunk).await.unwrap();
                assert_ne!(n, 0, "connection closed before {needle:?} arrived");
                buf.extend_from_slice(&chunk[..n]);
                if String::from_utf8_lossy(&buf).contains(needle) {
                    break;
                }
            }
        })
        .await
        .expect("timed out waiting for response");
        String::from_utf8_lossy(&buf).into_owned()
    }

    async fn run_steps(server: SocketAddr, steps: &[Step]) {
        let mut stream = TcpStream::connect(server).await.unwrap();
        let client = stream.local_addr().unwrap().to_string();
        for step in steps {
            match step {
                Step::Request => {
                    stream
                        .write_all(b"GET / HTTP/1.1\r\nHost: pxe\r\n\r\n")
                        .await
                        .unwrap();
                    let response = read_until(&mut stream, &client).await;
                    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
                }
                Step::IdleUntilClosed => {
                    let mut buf = [0u8; 1];
                    let read = tokio::time::timeout(WAIT, stream.read(&mut buf))
                        .await
                        .expect("server did not close the idle connection");
                    assert_eq!(read.unwrap(), 0, "expected EOF from server");
                }
            }
        }
    }

    #[tokio::test]
    async fn idle_connections_are_closed_after_header_read_timeout() {
        let metrics = MetricsCapture::start();
        let server = start_server().await;
        let cases: &[(&str, &[Step])] = &[
            ("never sends a request", &[Step::IdleUntilClosed]),
            (
                "idles after keep-alive requests",
                &[Step::Request, Step::Request, Step::IdleUntilClosed],
            ),
        ];
        for (scenario, steps) in cases {
            let started = tokio::time::Instant::now();
            run_steps(server, steps).await;
            assert!(
                started.elapsed() >= TEST_TIMEOUT,
                "{scenario}: closed before the timeout elapsed"
            );
        }
        wait_for_active_connections(0).await;
        assert_eq!(
            metrics.counter_delta(TIMEOUTS_METRIC, &[]),
            cases.len() as f64,
            "each idle close is counted as a header read timeout"
        );
    }

    /// A client that leaves mid-response fails the server's next write, so
    /// the connection task ends and releases its slot without being counted
    /// as a header read timeout.
    #[tokio::test]
    async fn aborted_transfers_release_the_connection() {
        let metrics = MetricsCapture::start();
        let server = start_server().await;
        let cases: &[(&str, usize)] = &[("drops the socket after a few response bytes", 16)];
        for (scenario, bytes_before_abort) in cases {
            let mut stream = TcpStream::connect(server).await.unwrap();
            stream
                .write_all(b"GET /large HTTP/1.1\r\nHost: pxe\r\n\r\n")
                .await
                .unwrap();
            let mut buf = vec![0u8; *bytes_before_abort];
            tokio::time::timeout(WAIT, stream.read_exact(&mut buf))
                .await
                .expect("timed out waiting for the response to start")
                .unwrap();
            assert!(
                buf.starts_with(b"HTTP/1.1 200"),
                "{scenario}: {:?}",
                String::from_utf8_lossy(&buf)
            );
            drop(stream);
            wait_for_active_connections(0).await;
        }
        assert_eq!(
            metrics.counter_delta(TIMEOUTS_METRIC, &[]),
            0.0,
            "an aborted transfer is not a header read timeout"
        );
    }
}
