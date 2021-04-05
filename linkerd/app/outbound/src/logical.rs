use crate::{tcp, Outbound};
use linkerd_app_core::{profiles, svc, Error};

impl<L> Outbound<L> {
    /// Pushes a layer that unwraps the [`Logical`] address of a given target
    /// from its profile resolution, or else falls back to the provided
    /// per-endpoint service if there was no profile resolution for that target.
    pub fn push_unwrap_logical<T, I, E, ESvc, LSvc>(
        self,
        endpoint: E,
    ) -> Outbound<
        impl svc::NewService<
                (Option<profiles::Receiver>, T),
                Service = impl svc::Service<I, Response = (), Error = Error, Future = impl Send>,
            > + Clone,
    >
    where
        tcp::Logical: From<(profiles::Receiver, T)>,
        L: svc::NewService<tcp::Logical, Service = LSvc> + Clone,
        LSvc: svc::Service<I, Response = (), Error = Error>,
        LSvc::Future: Send,
        E: svc::NewService<T, Service = ESvc> + Clone,
        ESvc: svc::Service<I, Response = (), Error = Error>,
        ESvc::Future: Send,
    {
        let Self {
            config,
            runtime,
            stack: logical,
        } = self;
        let stack = logical
            .push_map_target(tcp::Logical::from)
            .push(svc::UnwrapOr::layer(endpoint))
            .check_new_service::<(Option<profiles::Receiver>, T), _>();
        Outbound {
            config,
            runtime,
            stack,
        }
    }
}
