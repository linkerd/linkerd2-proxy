use crate::*;

#[tokio::test]
async fn tagged_transport_http() {
    let _trace = trace_init();

    // identity is always required for direct connections
    let in_svc_acct = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
    let in_identity = identity::Identity::new("foo-ns1", in_svc_acct.to_string());

    let out_svc_acct = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";
    let out_identity = identity::Identity::new("bar-ns1", out_svc_acct.to_string());

    let srv = server::http1().route("/", "hello").run().await;
    let srv_addr = srv.addr;
    let dst = format!("opaque.test.svc.cluster.local:{}", srv_addr.port());

    let (inbound, _profile_in) = {
        let (proxy, profile) = mk_inbound(srv, in_identity.service(), &dst).await;
        let proxy = proxy.run_with_test_env(in_identity.env).await;
        (proxy, profile)
    };

    let (outbound, _profile_out, _dst) = {
        let ctrl = controller::new();
        let dst = ctrl.destination_tx(dst);
        dst.send(
            controller::destination_add(srv_addr)
                .hint(controller::Hint::H2)
                .opaque_port(inbound.inbound.port())
                .identity(in_svc_acct),
        );
        let (proxy, profile) = mk_outbound(srv_addr, ctrl, out_identity).await;
        (proxy, profile, dst)
    };

    let client = client::http1(outbound.outbound, "opaque.test.svc.cluster.local");

    assert_eq!(client.get("/").await, "hello");
}

async fn mk_inbound(
    srv: server::Listening,
    id: identity::Controller,
    dst: &str,
) -> (proxy::Proxy, controller::ProfileSender) {
    let ctrl = controller::new();
    let profile = ctrl.profile_tx_default(dst, "opaque.test.svc.cluster.local");
    let ctrl = ctrl
        .run()
        .instrument(tracing::info_span!("ctrl", "inbound"))
        .await;
    let proxy = proxy::new()
        .controller(ctrl)
        .identity(id.run().await)
        .inbound(srv)
        .inbound_direct()
        .named("inbound");
    (proxy, profile)
}

async fn mk_outbound(
    srv_addr: SocketAddr,
    ctrl: controller::Controller,
    out_identity: identity::Identity,
) -> (proxy::Listening, controller::ProfileSender) {
    let profile = ctrl.profile_tx_default(srv_addr, "opaque.test.svc.cluster.local");
    let ctrl = ctrl
        .run()
        .instrument(tracing::info_span!("ctrl", "outbound"))
        .await;
    let proxy = proxy::new()
        .controller(ctrl)
        .identity(out_identity.service().run().await)
        .outbound_ip(srv_addr)
        .named("outbound")
        .run_with_test_env(out_identity.env)
        .await;
    (proxy, profile)
}
