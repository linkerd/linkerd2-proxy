use super::Registry;
use super::event::Event;
use super::labels::{RequestLabels, ResponseLabels};

/// Tracks Prometheus metrics
#[derive(Clone, Debug)]
pub struct Record {
    metrics: Registry,
}

// ===== impl Record =====

impl Record {
    pub(super) fn new(metrics: Registry) -> Self {
        Self { metrics }
    }

    #[cfg(test)]
    pub fn for_test() -> Self {
        Self { metrics: Registry::for_test() }
    }

    /// Observe the given event.
    pub fn record_event(&mut self, event: &Event) {
        trace!("Root::record({:?})", event);
        match *event {

            Event::StreamRequestOpen(_) => {},

            Event::StreamRequestFail(ref req, _) => {
                self.metrics.end_request(RequestLabels::new(req));
            },

            Event::StreamRequestEnd(ref req, _) => {
                self.metrics.end_request(RequestLabels::new(req));
            },

            Event::StreamResponseOpen(_, _) => {},

            Event::StreamResponseEnd(ref res, ref end) => {
                let latency = end.response_first_frame_at - end.request_open_at;
                self.metrics.end_response(ResponseLabels::new(res, end.grpc_status), latency);
            },

            Event::StreamResponseFail(ref res, ref fail) => {
                // TODO: do we care about the failure's error code here?
                let first_frame_at = fail.response_first_frame_at.unwrap_or(fail.response_fail_at);
                let latency = first_frame_at - fail.request_open_at;
                self.metrics.end_response(ResponseLabels::fail(res), latency);
            },
        };
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use super::super::event;
    use super::super::labels::{RequestLabels, ResponseLabels};
    use ctx::{self, test_util::*, transport::TlsStatus};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};
    use conditional::Conditional;
    use tls;

    const TLS_ENABLED: Conditional<(), tls::ReasonForNoTls> = Conditional::Some(());
    const TLS_DISABLED: Conditional<(), tls::ReasonForNoTls> =
        Conditional::None(tls::ReasonForNoTls::Disabled);

    fn new_record() -> (Record, Arc<Mutex<super::super::Inner>>) {
        let inner = Arc::new(Mutex::new(super::super::Inner::default()));
        (Record::new(Registry(inner.clone())), inner)
    }

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

        let (mut r, i) = new_record();
        let ev = Event::StreamResponseEnd(rsp.clone(), end.clone());
        let labels = ResponseLabels::new(&rsp, None);

        assert_eq!(labels.tls_status(), client_tls);

        assert!(i.lock().unwrap().responses.get(&labels).is_none());

        r.record_event(&ev);
        {
            let lock = i.lock().unwrap();
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

        let proxy = ctx::Proxy::Outbound;
        let server = server(proxy, server_tls);

        let client = client(proxy, indexmap![
            "service".into() => "draymond".into(),
            "deployment".into() => "durant".into(),
            "pod".into() => "klay".into(),
        ], client_tls);

        let (req, rsp) = request("http://buoyant.io", &server, &client);

        let request_open_at = Instant::now();
        let request_end_at = request_open_at + Duration::from_millis(10);
        let response_open_at = request_open_at + Duration::from_millis(100);
        let response_first_frame_at = response_open_at + Duration::from_millis(100);
        let response_end_at = response_first_frame_at + Duration::from_millis(100);
        let events = vec![
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
       ];

        let (mut r, i) = new_record();

        let req_labels = RequestLabels::new(&req);
        let rsp_labels = ResponseLabels::new(&rsp, None);

        assert_eq!(client_tls, req_labels.tls_status());
        assert_eq!(client_tls, rsp_labels.tls_status());

        {
            let lock = i.lock().unwrap();
            assert!(lock.requests.get(&req_labels).is_none());
            assert!(lock.responses.get(&rsp_labels).is_none());
        }

        for e in &events {
            r.record_event(e);
        }

        {
            let lock = i.lock().unwrap();

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
