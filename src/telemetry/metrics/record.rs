use std::sync::{Arc, Mutex};

use telemetry::event::Event;
use super::Root;
use super::labels::{
    RequestLabels,
    ResponseLabels,
};
use super::transport;

/// Tracks Prometheus metrics
#[derive(Clone, Debug)]
pub struct Record {
    metrics: Arc<Mutex<Root>>,
    transports: transport::Registry,
}

// ===== impl Record =====

impl Record {
    pub(super) fn new(metrics: &Arc<Mutex<Root>>, transports: transport::Registry) -> Self {
        Self { metrics: metrics.clone(), transports }
    }

    #[inline]
    fn update<F: Fn(&mut Root)>(&mut self, f: F) {
        let mut lock = self.metrics.lock()
            .expect("metrics lock poisoned");
        f(&mut *lock);
    }

    /// Observe the given event.
    pub fn record_event(&mut self, event: &Event) {
        trace!("Root::record({:?})", event);
        match *event {

            Event::StreamRequestOpen(_) => {},

            Event::StreamRequestFail(ref req, _) => {
                self.update(|metrics| {
                    metrics.request(RequestLabels::new(req)).end();
                })
            },

            Event::StreamRequestEnd(ref req, _) => {
                self.update(|metrics| {
                    metrics.request(RequestLabels::new(req)).end();
                })
            },

            Event::StreamResponseOpen(_, _) => {},

            Event::StreamResponseEnd(ref res, ref end) => {
                let latency = end.response_first_frame_at - end.request_open_at;
                self.update(|metrics| {
                    metrics.response(ResponseLabels::new(res, end.grpc_status))
                        .end(latency);
                });
            },

            Event::StreamResponseFail(ref res, ref fail) => {
                // TODO: do we care about the failure's error code here?
                let first_frame_at = fail.response_first_frame_at.unwrap_or(fail.response_fail_at);
                let latency = first_frame_at - fail.request_open_at;
                self.update(|metrics| {
                    metrics.response(ResponseLabels::fail(res)).end(latency)
                });
            },

            Event::TransportOpen(ref ctx) => {
                self.transports.open(ctx);
            },

            Event::TransportClose(ref ctx, ref close) => {
                let eos = if close.clean {
                    transport::Eos::Clean
                } else {
                    transport::Eos::Error {
                        errno: close.errno.map(|e| e.into())
                    }
                };
                self.transports
                    .close(ctx, eos, close.duration, close.rx_bytes, close.tx_bytes);
            },
        };
    }
}

#[cfg(test)]
mod test {
    use telemetry::{
        event,
        metrics::{self, labels},
        Event,
    };
    use ctx::{self, test_util::*, transport::TlsStatus};
    use std::time::{Duration, Instant};
    use conditional::Conditional;
    use tls;

    const TLS_ENABLED: Conditional<(), tls::ReasonForNoTls> = Conditional::Some(());
    const TLS_DISABLED: Conditional<(), tls::ReasonForNoTls> =
        Conditional::None(tls::ReasonForNoTls::Disabled);

    fn test_record_response_end_outbound(client_tls: TlsStatus, server_tls: TlsStatus) {
        let proxy = ctx::Proxy::Outbound;
        let server = server(proxy, server_tls);

        let client = client(proxy, indexmap![
            "service".into() => "draymond".into(),
            "deployment".into() => "durant".into(),
            "pod".into() => "klay".into(),
        ], client_tls);

        let (_, rsp) = request("http://buoyant.io", &server, &client);

        let request_open_at = Instant::now();
        let response_open_at = request_open_at + Duration::from_millis(100);
        let response_first_frame_at = response_open_at + Duration::from_millis(100);
        let response_end_at = response_first_frame_at + Duration::from_millis(100);
        let end = event::StreamResponseEnd {
            grpc_status: None,
            request_open_at,
            response_open_at,
            response_first_frame_at,
            response_end_at,
            bytes_sent: 0,
            frames_sent: 0,
        };

        let (mut r, _) = metrics::new(Duration::from_secs(100), Default::default(), Default::default());
        let ev = Event::StreamResponseEnd(rsp.clone(), end.clone());
        let labels = labels::ResponseLabels::new(&rsp, None);

        assert_eq!(labels.tls_status(), client_tls);

        assert!(r.metrics.lock()
            .expect("lock")
            .responses
            .get(&labels)
            .is_none()
        );

        r.record_event(&ev);
        {
            let lock = r.metrics.lock()
                .expect("lock");
            let scope = lock
                .responses
                .get(&labels)
                .expect("scope should be some after event");

            assert_eq!(scope.total(), 1);

            scope.latency().assert_bucket_exactly(200, 1);
            scope.latency().assert_lt_exactly(200, 0);
            scope.latency().assert_gt_exactly(200, 0);
        }

    }

    fn test_record_one_conn_request_outbound(client_tls: TlsStatus, server_tls: TlsStatus) {
        use self::Event::*;
        use self::labels::*;
        use std::sync::Arc;

        let proxy = ctx::Proxy::Outbound;
        let server = server(proxy, server_tls);

        let client = client(proxy, indexmap![
            "service".into() => "draymond".into(),
            "deployment".into() => "durant".into(),
            "pod".into() => "klay".into(),
        ], client_tls);

        let (req, rsp) = request("http://buoyant.io", &server, &client);
        let server_transport =
            Arc::new(ctx::transport::Ctx::Server(server.clone()));
        let client_transport =
             Arc::new(ctx::transport::Ctx::Client(client.clone()));
        let transport_close = event::TransportClose {
            clean: true,
            errno: None,
            duration: Duration::from_secs(30_000),
            rx_bytes: 4321,
            tx_bytes: 4321,
        };

        let request_open_at = Instant::now();
        let request_end_at = request_open_at + Duration::from_millis(10);
        let response_open_at = request_open_at + Duration::from_millis(100);
        let response_first_frame_at = response_open_at + Duration::from_millis(100);
        let response_end_at = response_first_frame_at + Duration::from_millis(100);
        let events = vec![
            TransportOpen(server_transport.clone()),
            TransportOpen(client_transport.clone()),
            StreamRequestOpen(req.clone()),
            StreamRequestEnd(req.clone(), event::StreamRequestEnd {
                request_open_at,
                request_end_at,
            }),

            StreamResponseOpen(rsp.clone(), event::StreamResponseOpen {
                request_open_at,
                response_open_at,
            }),
            StreamResponseEnd(rsp.clone(), event::StreamResponseEnd {
                grpc_status: None,
                request_open_at,
                response_open_at,
                response_first_frame_at,
                response_end_at,
                bytes_sent: 0,
                frames_sent: 0,
            }),
            TransportClose(
                server_transport.clone(),
                transport_close.clone(),
            ),
            TransportClose(
                client_transport.clone(),
                transport_close.clone(),
            ),
        ];

        let (mut r, _) = metrics::new(Duration::from_secs(1000), Default::default(), Default::default());

        let req_labels = RequestLabels::new(&req);
        let rsp_labels = ResponseLabels::new(&rsp, None);

        assert_eq!(client_tls, req_labels.tls_status());
        assert_eq!(client_tls, rsp_labels.tls_status());

        {
            let lock = r.metrics.lock().expect("lock");
            assert!(lock.requests.get(&req_labels).is_none());
            assert!(lock.responses.get(&rsp_labels).is_none());
            assert_eq!(r.transports.open_total(&server_transport), 0);
            assert_eq!(r.transports.open_total(&client_transport), 0);
        }

        for e in &events {
            r.record_event(e);
        }

        {
            let lock = r.metrics.lock().expect("lock");

            // === request scope ====================================
            assert_eq!(
                lock.requests
                    .get(&req_labels)
                    .map(|scope| scope.total()),
                Some(1)
            );

            // === response scope ===================================
            {
                let response_scope = lock
                    .responses
                    .get(&rsp_labels)
                    .expect("response scope missing");
                assert_eq!(response_scope.total(), 1);

                response_scope.latency()
                    .assert_bucket_exactly(200, 1)
                    .assert_gt_exactly(200, 0)
                    .assert_lt_exactly(200, 0);
            }

            use super::transport::Eos;
            let transport_duration: u64 = 30_000 * 1_000;
            let t = r.transports;

            assert_eq!(t.open_total(&server_transport), 1);
            assert_eq!(t.rx_tx_bytes_total(&server_transport), (4321, 4321));
            assert_eq!(t.close_total(&server_transport, Eos::Clean), 1);
            t.connection_durations(&server_transport, Eos::Clean)
                .assert_bucket_exactly(transport_duration, 1)
                .assert_gt_exactly(transport_duration, 0)
                .assert_lt_exactly(transport_duration, 0);

            assert_eq!(t.open_total(&client_transport), 1);
            assert_eq!(t.rx_tx_bytes_total(&client_transport), (4321, 4321));
            assert_eq!(t.close_total(&server_transport, Eos::Clean), 1);
            t.connection_durations(&server_transport, Eos::Clean)
                .assert_bucket_exactly(transport_duration, 1)
                .assert_gt_exactly(transport_duration, 0)
                .assert_lt_exactly(transport_duration, 0);
        }
    }

    #[test]
    fn record_one_conn_request_outbound_client_tls() {
        test_record_one_conn_request_outbound(TLS_ENABLED, TLS_DISABLED)
    }

    #[test]
    fn record_one_conn_request_outbound_server_tls() {
        test_record_one_conn_request_outbound(TLS_DISABLED, TLS_ENABLED)
    }

    #[test]
    fn record_one_conn_request_outbound_both_tls() {
        test_record_one_conn_request_outbound(TLS_ENABLED, TLS_ENABLED)
    }

    #[test]
    fn record_one_conn_request_outbound_no_tls() {
        test_record_one_conn_request_outbound(TLS_DISABLED, TLS_DISABLED)
    }

    #[test]
    fn record_response_end_outbound_client_tls() {
        test_record_response_end_outbound(TLS_ENABLED, TLS_DISABLED)
    }

    #[test]
    fn record_response_end_outbound_server_tls() {
        test_record_response_end_outbound(TLS_DISABLED, TLS_ENABLED)
    }

    #[test]
    fn record_response_end_outbound_both_tls() {
        test_record_response_end_outbound(TLS_ENABLED, TLS_ENABLED)
    }

    #[test]
    fn record_response_end_outbound_no_tls() {
        test_record_response_end_outbound(TLS_DISABLED, TLS_DISABLED)
    }
}
