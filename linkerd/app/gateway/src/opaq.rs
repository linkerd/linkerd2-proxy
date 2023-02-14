use super::{Opaque, RefusedNotResolved};
use linkerd_app_core::{
    io,
    proxy::{api_resolve::Metadata, core::Resolve},
    svc, tls,
    transport::{ClientAddr, Local},
    Error,
};
use linkerd_app_outbound::{self as outbound, Outbound};
use std::fmt;

/// Builds an outbound opaque stack.
///
/// Requires that the connection targets either a logical service or a known
/// endpoint.
pub(super) fn stack<I, O, R>(
    outbound: Outbound<O>,
    resolve: R,
) -> svc::ArcNewService<
    Opaque,
    impl svc::Service<I, Response = (), Error = Error, Future = impl Send>,
>
where
    I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + fmt::Debug + Send + Sync + Unpin + 'static,
    O: Clone + Send + Sync + Unpin + 'static,
    O: svc::MakeConnection<outbound::tcp::Connect, Metadata = Local<ClientAddr>, Error = io::Error>,
    O::Connection: Send + Unpin,
    O::Future: Send + Unpin + 'static,
    R: Resolve<outbound::tcp::Concrete, Endpoint = Metadata, Error = Error>,
{
    let logical = outbound
        .clone()
        .push_tcp_endpoint()
        .push_opaq_concrete(resolve)
        .push_opaq_logical();
    let endpoint = outbound
        .clone()
        .push_tcp_endpoint()
        .push_opaq_forward()
        .into_stack();
    let inbound_ips = outbound.config().inbound_ips.clone();
    endpoint
        .push_switch(
            move |opaque: Opaque| -> Result<_, Error> {
                if let Some((addr, metadata)) = opaque.profile.endpoint() {
                    return Ok(svc::Either::A(outbound::tcp::Endpoint::from_metadata(
                        addr,
                        metadata,
                        tls::NoClientTls::NotProvidedByServiceDiscovery,
                        opaque.profile.is_opaque_protocol(),
                        &inbound_ips,
                    )));
                }

                let logical_addr = opaque
                    .profile
                    .logical_addr()
                    .ok_or(RefusedNotResolved(opaque.target))?;
                Ok(svc::Either::B(outbound::tcp::Logical {
                    profile: opaque.profile,
                    logical_addr,
                    protocol: (),
                }))
            },
            logical.into_inner(),
        )
        .push(svc::ArcNewService::layer())
        .check_new_service::<Opaque, I>()
        .into_inner()
}
