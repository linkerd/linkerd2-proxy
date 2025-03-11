use super::*;
use crate::direct::ClientInfo;
use futures::future;
use linkerd_app_core::{
    io,
    metrics::prom,
    svc, tls,
    transport::addrs::{ClientAddr, OrigDstAddr, Remote},
    transport_header::{SessionProtocol, TransportHeader},
    Error,
};
use std::str::FromStr;

fn new_ok<T>() -> svc::ArcNewTcp<T, io::BoxedIo> {
    svc::ArcNewService::new(|_| svc::BoxService::new(svc::mk(|_| future::ok::<(), Error>(()))))
}

macro_rules! assert_counted {
    ($registry:expr, $proto:expr, $port:expr, $name:expr, $value:expr) => {{
        let mut buf = String::new();
        prom::encoding::text::encode_registry(&mut buf, $registry).expect("encode registry failed");
        let metric = format!("connections_total{{session_protocol=\"{}\",target_port=\"{}\",target_name=\"{}\",client_id=\"test.client\"}}", $proto, $port, $name);
        assert_eq!(
            buf.split_terminator('\n')
                .find(|l| l.starts_with(&*metric)),
            Some(&*format!("{metric} {}", $value)),
            "metric '{metric}' not found in:\n{buf}"
        );
    }};
}

// Added helper to setup and run the test
fn run_metric_test(header: TransportHeader) -> prom::Registry {
    let mut registry = prom::Registry::default();
    let families = MetricsFamilies::register(&mut registry);
    let new_record = svc::layer::Layer::layer(&NewRecord::layer(families.clone()), new_ok());
    // common client info
    let client_id = tls::ClientId::from_str("test.client").unwrap();
    let client_addr = Remote(ClientAddr(([127, 0, 0, 1], 40000).into()));
    let local_addr = OrigDstAddr(([127, 0, 0, 1], 4143).into());
    let client_info = ClientInfo {
        client_id: client_id.clone(),
        alpn: Some(tls::NegotiatedProtocol("transport.l5d.io/v1".into())),
        client_addr,
        local_addr,
    };
    let _svc = svc::NewService::new_service(&new_record, (header.clone(), client_info.clone()));
    registry
}

#[test]
fn records_metrics_http1_local() {
    let header = TransportHeader {
        port: 8080,
        name: None,
        protocol: Some(SessionProtocol::Http1),
    };
    let registry = run_metric_test(header);
    assert_counted!(&registry, "http/1", 8080, "", 1);
}

#[test]
fn records_metrics_http2_local() {
    let header = TransportHeader {
        port: 8081,
        name: None,
        protocol: Some(SessionProtocol::Http2),
    };
    let registry = run_metric_test(header);
    assert_counted!(&registry, "http/2", 8081, "", 1);
}

#[test]
fn records_metrics_opaq_local() {
    let header = TransportHeader {
        port: 8082,
        name: None,
        protocol: None,
    };
    let registry = run_metric_test(header);
    assert_counted!(&registry, "", 8082, "", 1);
}

#[test]
fn records_metrics_http1_gateway() {
    let header = TransportHeader {
        port: 8080,
        name: Some("mysvc.myns.svc.cluster.local".parse().unwrap()),
        protocol: Some(SessionProtocol::Http1),
    };
    let registry = run_metric_test(header);
    assert_counted!(&registry, "http/1", 8080, "mysvc.myns.svc.cluster.local", 1);
}

#[test]
fn records_metrics_http2_gateway() {
    let header = TransportHeader {
        port: 8081,
        name: Some("mysvc.myns.svc.cluster.local".parse().unwrap()),
        protocol: Some(SessionProtocol::Http2),
    };
    let registry = run_metric_test(header);
    assert_counted!(&registry, "http/2", 8081, "mysvc.myns.svc.cluster.local", 1);
}

#[test]
fn records_metrics_opaq_gateway() {
    let header = TransportHeader {
        port: 8082,
        name: Some("mysvc.myns.svc.cluster.local".parse().unwrap()),
        protocol: None,
    };
    let registry = run_metric_test(header);
    assert_counted!(&registry, "", 8082, "mysvc.myns.svc.cluster.local", 1);
}
