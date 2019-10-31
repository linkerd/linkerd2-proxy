use crate::identity::LocalIdentity;
use linkerd2_app_core::{
    admin, config::ServerConfig, drain, metrics::FmtMetrics, serve, trace::LevelHandle,
    transport::tls, Error,
};
use std::net::SocketAddr;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct Config {
    pub server: ServerConfig,
    pub metrics_retain_idle: Duration,
}

pub struct Admin {
    pub listen_addr: SocketAddr,
    pub latch: admin::Latch,
    pub serve: serve::Task,
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
        use linkerd2_app_core::proxy::core::listen::{Bind, Listen};

        let listen = self.server.bind.bind().map_err(Error::from)?;
        let listen_addr = listen.listen_addr();

        let (ready, latch) = admin::Readiness::new();
        let admin = admin::Admin::new(report, ready, log_level);
        let accept = tls::AcceptTls::new(identity, admin.into_accept());
        let serve = serve::serve(listen, accept, drain);
        Ok(Admin {
            listen_addr,
            latch,
            serve,
        })
    }
}
