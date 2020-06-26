use crate::identity::LocalIdentity;
use linkerd2_app_core::{
    admin, config::ServerConfig, drain, metrics::FmtMetrics, serve, trace::LevelHandle,
    transport::tls, Error,
};
use std::net::SocketAddr;
use std::pin::Pin;
use std::time::Duration;

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
        identity: LocalIdentity,
        report: R,
        log_level: LevelHandle,
        drain: drain::Watch,
    ) -> Result<Admin, Error>
    where
        R: FmtMetrics + Clone + Send + 'static,
    {
        let (listen_addr, listen) = self.server.bind.bind(drain.signal()).map_err(Error::from)?;

        let (ready, latch) = admin::Readiness::new();
        let admin = admin::Admin::new(report, ready, log_level);
        let accept = tls::AcceptTls::new(identity, admin.into_accept());
        let serve = Box::pin(serve::serve(listen, accept));
        Ok(Admin {
            listen_addr,
            latch,
            serve,
        })
    }
}
