use crate::{target::Endpoint, Outbound};
use linkerd_app_core::{svc, tls};

impl<E> Outbound<E> {
    pub fn push_into_endpoint<P, T>(
        self,
    ) -> Outbound<impl svc::NewService<T, Service = E::Service> + Clone>
    where
        Endpoint<P>: From<(tls::NoClientTls, T)>,
        E: svc::NewService<Endpoint<P>> + Clone,
    {
        let Self {
            config,
            runtime,
            stack: endpoint,
        } = self;
        let identity_disabled = runtime.identity.is_none();
        let no_tls_reason = if identity_disabled {
            tls::NoClientTls::Disabled
        } else {
            tls::NoClientTls::NotProvidedByServiceDiscovery
        };
        let stack =
            svc::stack(endpoint).push_map_target(move |t| Endpoint::<P>::from((no_tls_reason, t)));
        Outbound {
            config,
            runtime,
            stack,
        }
    }
}
