use crate::{http, identity::LocalCrtKey, inbound, svc};
use linkerd_app_core::{
    admin, config::ServerConfig, detect, drain, metrics::FmtMetrics, serve, tls, trace,
    transport::listen, Error,
};
use std::{net::SocketAddr, pin::Pin, time::Duration};
use tokio::sync::mpsc;

#[derive(Clone, Debug)]
pub struct Config {
    pub server: ServerConfig,
    pub metrics_retain_idle: Duration,
}

pub struct Admin {
    pub listen_addr: SocketAddr,
    pub latch: admin::Latch,
    pub serve: Pin<Box<dyn std::future::Future<Output = Result<(), Error>> + Send + 'static>>,
}

impl Config {
    pub fn build<R>(
        self,
        identity: Option<LocalCrtKey>,
        report: R,
        trace: trace::Handle,
        drain: drain::Watch,
        shutdown: mpsc::UnboundedSender<()>,
    ) -> Result<Admin, Error>
    where
        R: FmtMetrics + Clone + Send + 'static,
    {
        let (listen_addr, listen) = self.server.bind.bind()?;

        let (ready, latch) = admin::Readiness::new();
        let admin = admin::Admin::new(report, ready, shutdown, trace);
        let admin = svc::stack(admin)
            .push_map_target(|(_, accept): (_, inbound::TcpAccept)| accept)
            .push(http::NewServeHttp::layer(Default::default(), drain.clone()))
            .push(detect::NewDetectService::timeout(
                Duration::from_secs(1),
                http::DetectHttp::default(),
            ))
            .push_map_target(inbound::TcpAccept::from)
            .check_new_service::<listen::Addrs, _>()
            .push_map_target(|(_, addrs): tls::server::Meta<listen::Addrs>| addrs)
            .push(tls::NewDetectTls::layer(
                identity.map(|crt_key| crt_key.id().clone()),
                Duration::from_secs(1),
            ))
            .check_new_service::<listen::Addrs, _>()
            .into_inner();
        let serve = Box::pin(serve::serve(listen, admin, drain.signaled()));
        Ok(Admin {
            listen_addr,
            latch,
            serve,
        })
    }
}
