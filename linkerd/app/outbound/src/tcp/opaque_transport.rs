use crate::target::Endpoint;
use linkerd_app_core::{
    dns::Name,
    io,
    svc::{self, layer},
    transport_header::TransportHeader,
    Error,
};
use std::{
    future::Future,
    pin::Pin,
    str::FromStr,
    task::{Context, Poll},
};
use tracing::{debug, trace, warn};

#[derive(Clone, Debug)]
pub struct OpaqueTransport<S> {
    inner: S,
}

impl<S> OpaqueTransport<S> {
    pub fn layer() -> impl layer::Layer<S, Service = Self> + Copy {
        layer::mk(|inner| OpaqueTransport { inner })
    }
}

impl<S, P> svc::Service<Endpoint<P>> for OpaqueTransport<S>
where
    S: svc::Service<Endpoint<P>> + Send + 'static,
    S::Error: Into<Error>,
    S::Response: io::AsyncWrite + Send + Unpin,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<S::Response, Error>> + Send + 'static>>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, mut ep: Endpoint<P>) -> Self::Future {
        // Determine whether an opaque header should written on the socket.
        let header = match ep.metadata.opaque_transport_port() {
            None => {
                trace!("No opaque transport configured");
                None
            }
            Some(override_port) => {
                // Update the endpoint to target the discovery-provided control
                // plane port.
                let orig_port = ep.addr.port();
                ep.addr = (ep.addr.ip(), override_port).into();

                // If there's a destination override, encode that in the opaque
                // transport (i.e. for multicluster gateways). Otherwise, simply
                // encode the original target port. Note that we prefer any port
                // specified in the override to the original destination port.
                let header = ep
                    .metadata
                    .authority_override()
                    .and_then(|auth| {
                        let port = auth.port_u16().unwrap_or(orig_port);
                        Name::from_str(auth.host())
                            .map_err(|error| warn!(%error, "Invalid name"))
                            .ok()
                            .map(|n| TransportHeader {
                                port,
                                name: Some(n),
                            })
                    })
                    .unwrap_or(TransportHeader {
                        port: orig_port,
                        name: None,
                    });
                debug!(?header, override_port, "Using opaque transport");
                Some(header)
            }
        };

        // Connect to the endpoint.
        let connect = self.inner.call(ep);
        Box::pin(async move {
            let mut io = connect.await.map_err(Into::into)?;

            // Once connected, write the opaque header on the socket before
            // returning it.
            if let Some(h) = header {
                let sz = h.write(&mut io).await?;
                debug!(sz, "Wrote header to transport");
            }

            Ok(io)
        })
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::target::{Concrete, Endpoint, Logical};
    use futures::future;
    use linkerd_app_core::{
        io::{self, AsyncWriteExt},
        proxy::api_resolve::{Metadata, ProtocolHint},
        tls,
        transport_header::TransportHeader,
    };
    use tower::util::{service_fn, ServiceExt};

    fn ep(metadata: Metadata) -> Endpoint<()> {
        Endpoint {
            addr: ([127, 0, 0, 2], 4321).into(),
            identity: tls::Conditional::None(
                tls::ReasonForNoPeerName::NotProvidedByServiceDiscovery,
            ),
            metadata,
            concrete: Concrete {
                resolve: None,
                logical: Logical {
                    orig_dst: ([127, 0, 0, 2], 4321).into(),
                    profile: None,
                    protocol: (),
                },
            },
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn plain() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();

        let svc = OpaqueTransport {
            inner: service_fn(|ep: Endpoint<()>| {
                assert_eq!(ep.addr.port(), 4321);
                future::ready(Ok::<_, io::Error>(
                    tokio_test::io::Builder::new().write(b"hello").build(),
                ))
            }),
        };
        let mut io = svc
            .oneshot(ep(Metadata::default()))
            .await
            .expect("Connect must not fail");
        io.write_all(b"hello").await.expect("Write must succeed");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn opaque_no_name() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();

        let svc = OpaqueTransport {
            inner: service_fn(|ep: Endpoint<()>| {
                assert_eq!(ep.addr.port(), 4143);
                let hdr = TransportHeader {
                    port: ep.concrete.logical.orig_dst.port(),
                    name: None,
                };
                let buf = hdr.encode_prefaced_buf().expect("Must encode");
                future::ready(Ok::<_, io::Error>(
                    tokio_test::io::Builder::new()
                        .write(&buf[..])
                        .write(b"hello")
                        .build(),
                ))
            }),
        };

        let e = ep(Metadata::new(
            Default::default(),
            ProtocolHint::Unknown,
            Some(4143),
            None,
            None,
        ));
        let mut io = svc.oneshot(e).await.expect("Connect must not fail");
        io.write_all(b"hello").await.expect("Write must succeed");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn opaque_named_with_port() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();

        let svc = OpaqueTransport {
            inner: service_fn(|ep: Endpoint<()>| {
                assert_eq!(ep.addr.port(), 4143);
                let hdr = TransportHeader {
                    port: 5555,
                    name: Some(Name::from_str("foo.bar.example.com").unwrap()),
                };
                let buf = hdr.encode_prefaced_buf().expect("Must encode");
                future::ready(Ok::<_, io::Error>(
                    tokio_test::io::Builder::new()
                        .write(&buf[..])
                        .write(b"hello")
                        .build(),
                ))
            }),
        };

        let e = ep(Metadata::new(
            Default::default(),
            ProtocolHint::Unknown,
            Some(4143),
            None,
            Some(http::uri::Authority::from_str("foo.bar.example.com:5555").unwrap()),
        ));
        let mut io = svc.oneshot(e).await.expect("Connect must not fail");
        io.write_all(b"hello").await.expect("Write must succeed");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn opaque_named_no_port() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();

        let svc = OpaqueTransport {
            inner: service_fn(|ep: Endpoint<()>| {
                assert_eq!(ep.addr.port(), 4143);
                let hdr = TransportHeader {
                    port: ep.concrete.logical.orig_dst.port(),
                    name: None,
                };
                let buf = hdr.encode_prefaced_buf().expect("Must encode");
                future::ready(Ok::<_, io::Error>(
                    tokio_test::io::Builder::new()
                        .write(&buf[..])
                        .write(b"hello")
                        .build(),
                ))
            }),
        };

        let e = ep(Metadata::new(
            Default::default(),
            ProtocolHint::Unknown,
            Some(4143),
            None,
            None,
        ));
        let mut io = svc.oneshot(e).await.expect("Connect must not fail");
        io.write_all(b"hello").await.expect("Write must succeed");
    }
}
