use super::*;
use linkerd_app_core::{
    proxy::http::{self, StatusCode},
    svc, trace, NameAddr,
};
use linkerd_proxy_client_policy as client_policy;
use std::{net::SocketAddr, sync::Arc};
use tokio::sync::watch;

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn routes() {
    let _trace = trace::test::trace_init();

    const AUTHORITY: &str = "logical.test.svc.cluster.local";
    const PORT: u16 = 666;
    let addr = SocketAddr::new([192, 0, 2, 41].into(), PORT);
    let dest: NameAddr = format!("{AUTHORITY}:{PORT}")
        .parse::<NameAddr>()
        .expect("dest addr is valid");
    let (svc, mut handle) = tower_test::mock::pair();
    let connect = HttpConnect::default().service(addr, svc);
    let resolve = support::resolver().endpoint_exists(dest.clone(), addr, Default::default());
    let (rt, _shutdown) = runtime();
    let stack = Outbound::new(default_config(), rt, &mut Default::default())
        .with_stack(svc::ArcNewService::new(connect))
        .push_http_cached(resolve)
        .into_inner();

    let backend = default_backend(&dest);
    let (_route_tx, routes) =
        watch::channel(Routes::Policy(policy::Params::Http(policy::HttpParams {
            addr: dest.into(),
            meta: ParentRef(client_policy::Meta::new_default("parent")),
            backends: Arc::new([backend.clone()]),
            routes: Arc::new([default_route(backend)]),
            failure_accrual: client_policy::FailureAccrual::None,
        })));
    let target = Target {
        num: 1,
        version: http::Variant::H2,
        routes,
    };
    let svc = stack.new_service(target);

    handle.allow(1);
    let rsp = send_req(svc.clone(), http_get());
    serve(&mut handle, mk_rsp(StatusCode::OK, "good")).await;
    assert_eq!(
        rsp.await.expect("request must succeed").status(),
        http::StatusCode::OK
    );
}
