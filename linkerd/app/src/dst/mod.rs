mod resolve;

use indexmap::IndexSet;
use linkerd2_app_core::{
    config::{ControlAddr, ControlConfig},
    dns, profiles, Error,
};
use std::time::Duration;
use tower_grpc::{generic::client::GrpcService, Body, BoxBody};

#[derive(Clone, Debug)]
pub struct Config {
    pub control: ControlConfig,
    pub context: String,
    pub get_suffixes: IndexSet<dns::Suffix>,
    pub get_networks: IndexSet<ipnet::IpNet>,
    pub profile_suffixes: IndexSet<dns::Suffix>,
    pub initial_profile_timeout: Duration,
}

/// Handles to destination service clients.
///
/// The addr is preserved for logging.
pub struct Dst<S> {
    pub addr: ControlAddr,
    pub profiles: profiles::Client<S, resolve::BackoffUnlessInvalidArgument>,
    pub resolve: resolve::Resolve<S>,
}

impl Config {
    // XXX This is unfortunate -- the service should be built here, but it's annoying to name.
    pub fn build<S>(self, svc: S) -> Result<Dst<S>, Error>
    where
        S: GrpcService<BoxBody> + Clone + Send + 'static,
        S::ResponseBody: Send,
        <S::ResponseBody as Body>::Data: Send,
        S::Future: Send,
    {
        let resolve = resolve::new(
            svc.clone(),
            self.get_suffixes,
            self.get_networks,
            &self.context,
            self.control.connect.backoff,
        );

        let profiles = profiles::Client::new(
            svc,
            resolve::BackoffUnlessInvalidArgument::from(self.control.connect.backoff),
            self.initial_profile_timeout,
            self.context,
            self.profile_suffixes,
        );

        Ok(Dst {
            addr: self.control.addr,
            resolve,
            profiles,
        })
    }
}
