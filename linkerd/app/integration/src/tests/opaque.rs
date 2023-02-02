use crate::*;

/// Reproduces https://github.com/linkerd/linkerd2/issues/9888
#[tokio::test]
async fn opaque_transport_http() {
    let _trace = trace_init();

    // identity is always required for direct connections
    let in_svc_acct = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
    let in_identity = identity::Identity::new("foo-ns1", in_svc_acct.to_string());

    let out_svc_acct = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";
    let out_identity = identity::Identity::new("bar-ns1", out_svc_acct.to_string());

    let srv = server::http1().route("/", "hello").run().await;
    let srv_addr = srv.addr;
    let dst = format!("transparency.test.svc.cluster.local:{}", srv_addr.port());

    let (inbound, _profile_in) = {
        let ctrl = controller::new();
        let _profile = ctrl.profile_tx_default(&dst, "transparency.test.svc.cluster.local");
        let ctrl = ctrl
            .run()
            .instrument(tracing::info_span!("ctrl", "inbound"))
            .await;
        let proxy = proxy::new()
            .controller(ctrl)
            .identity(in_identity.service().run().await)
            .inbound(srv)
            .inbound_direct()
            .named("inbound")
            .run_with_test_env(in_identity.env)
            .await;
        (proxy, _profile)
    };

    let (outbound, _profile_out, _dst) = {
        let ctrl = controller::new();
        let _profile = ctrl.profile_tx_default(srv_addr, "transparency.test.svc.cluster.local");
        let dst = ctrl.destination_tx(dst);
        dst.send(
            controller::DestinationBuilder {
                hint: controller::Hint::H2,
                opaque_port: Some(inbound.inbound.port()),
                identity: Some(in_svc_acct.to_string()),
                ..Default::default()
            }
            .into_update(srv_addr),
        );
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
        (proxy, _profile, dst)
    };
    let client = client::http1(outbound.outbound, "transparency.test.svc.cluster.local");

    assert_eq!(client.get("/").await, "hello");
}
