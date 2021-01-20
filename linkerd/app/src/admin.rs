use crate::identity::LocalCrtKey;
use linkerd_app_core::{
    admin, config::ServerConfig, drain, metrics::FmtMetrics, serve, tls, trace, Error,
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
        let accept = tls::NewDetectTls::new(
            identity,
            admin.into_accept(),
            std::time::Duration::from_secs(1),
        );
        let serve = Box::pin(serve::serve(listen, accept, drain.signaled()));
        Ok(Admin {
            listen_addr,
            latch,
            serve,
        })
    }
}
