use crate::{endpoint::Endpoint, logical::Logical, tcp, transport::OrigDstAddr, Outbound};
use linkerd_app_core::{io, profiles, svc, Error, Never};
use std::fmt;

impl<S> Outbound<S> {
    /// Wraps an endpoint stack to switch to an alternate logical stack when an appropriate profile
    /// is provided.
    pub fn push_switch_logical<T, I, N, NSvc, SSvc>(
        self,
        logical: N,
    ) -> Outbound<
        impl svc::NewService<
                (Option<profiles::Receiver>, T),
                Service = impl svc::Service<I, Response = (), Error = Error, Future = impl Send>,
            > + Clone,
    >
    where
        Self: Clone + 'static,
        T: svc::Param<OrigDstAddr> + Clone + Send + Sync + 'static,
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + fmt::Debug + Send + Unpin + 'static,
        N: svc::NewService<tcp::Logical, Service = NSvc> + Clone,
        NSvc: svc::Service<I, Response = (), Error = Error>,
        NSvc::Future: Send,
        S: svc::NewService<tcp::Endpoint, Service = SSvc> + Clone,
        SSvc: svc::Service<I, Response = (), Error = Error>,
        SSvc::Future: Send,
    {
        let no_tls_reason = self.no_tls_reason();
        let Self {
            config,
            runtime,
            stack: endpoint,
        } = self;

        let stack = endpoint.push_switch(
            move |(profile, target): (Option<profiles::Receiver>, T)| -> Result<_, Never> {
                if let Some(rx) = profile {
                    let profiles::Profile {
                        ref addr,
                        ref endpoint,
                        opaque_protocol,
                        ..
                    } = *rx.borrow();

                    if let Some((addr, metadata)) = endpoint.clone() {
                        return Ok(svc::Either::A(Endpoint::from_metadata(
                            addr,
                            metadata,
                            no_tls_reason,
                            opaque_protocol,
                        )));
                    }

                    if let Some(logical_addr) = addr.clone() {
                        return Ok(svc::Either::B(Logical::new(
                            logical_addr,
                            rx.clone(),
                            target.param(),
                        )));
                    }
                }

                Ok(svc::Either::A(Endpoint::forward(
                    target.param(),
                    no_tls_reason,
                )))
            },
            logical,
        );

        Outbound {
            config,
            runtime,
            stack,
        }
    }
}
