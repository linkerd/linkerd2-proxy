use super::*;
use crate::tls::Tls;
use linkerd_app_core::{
    svc::ServiceExt,
    tls::{NewDetectRequiredSni, ServerName},
    trace, NameAddr,
};
use linkerd_proxy_client_policy as client_policy;
use std::{net::SocketAddr, str::FromStr, sync::Arc};
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
    let resolve = support::resolver().endpoint_exists(dest.clone(), addr, Default::default());
    let (rt, _shutdown) = runtime();

    let client_hello = generate_client_hello(AUTHORITY);
    let (srv, io, rsp) = MockServer::new(addr, AUTHORITY, client_hello);

    let mut connect = ConnectTcp::default();
    connect.add_server(srv);

    let stack = Outbound::new(default_config(), rt, &mut Default::default())
        .with_stack(connect)
        .push_tls_concrete(resolve)
        .push_tls_logical()
        .map_stack(|config, _rt, stk| {
            stk.push_new_idle_cached(config.discovery_idle_timeout)
                .push_map_target(|(sni, parent): (ServerName, _)| Tls { sni, parent })
                .push(NewDetectRequiredSni::layer(Duration::from_secs(1)))
                .arc_new_clone_tcp()
        })
        .into_inner();

    let correct_backend = default_backend(addr);
    let correct_route = sni_route(
        correct_backend.clone(),
        sni::MatchSni::Exact(AUTHORITY.into()),
    );

    let wrong_addr = SocketAddr::new([0, 0, 0, 0].into(), PORT);
    let wrong_backend = default_backend(wrong_addr);
    let wrong_route_1 = sni_route(
        wrong_backend.clone(),
        sni::MatchSni::from_str("foo").unwrap(),
    );
    let wrong_route_2 = sni_route(
        wrong_backend.clone(),
        sni::MatchSni::from_str("*.test.svc.cluster.local").unwrap(),
    );

    let (_route_tx, routes) = watch::channel(Routes {
        addr: addr.into(),
        backends: Arc::new([correct_backend, wrong_backend]),
        routes: Arc::new([correct_route, wrong_route_1, wrong_route_2]),
        meta: ParentRef(client_policy::Meta::new_default("parent")),
    });

    let target = Target { num: 1, routes };
    let svc = stack.new_service(target);

    svc.oneshot(io).await.unwrap();
    let msg = rsp.await.unwrap().unwrap();
    assert_eq!(msg, AUTHORITY);
}
