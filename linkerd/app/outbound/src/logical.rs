use crate::{tcp, Outbound};
use linkerd_app_core::{profiles, svc};

impl<L> Outbound<L> {
    /// Pushes a layer that unwraps the [`Logical`] address of a given target
    /// from its profile resolution, or else falls back to the provided
    /// per-endpoint service if there was no profile resolution for that target.
    pub fn push_unwrap_logical<T, E>(
        self,
        endpoint: E,
    ) -> Outbound<
        impl svc::NewService<
                (Option<profiles::Receiver>, T),
                Service = svc::stack::ResultService<svc::Either<L::Service, E::Service>>,
            > + Clone,
    >
    where
        tcp::Logical: From<(profiles::Receiver, T)>,
        L: svc::NewService<tcp::Logical> + Clone,
        E: svc::NewService<T> + Clone,
    {
        let Self {
            config,
            runtime,
            stack: logical,
        } = self;
        let stack = logical
            .push_map_target(tcp::Logical::from)
            .push(svc::UnwrapOr::layer(endpoint));
        Outbound {
            config,
            runtime,
            stack,
        }
    }
}
