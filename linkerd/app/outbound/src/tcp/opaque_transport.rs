use crate::target::Endpoint;
use linkerd2_app_core::{
    connection_header::Header,
    dns::Name,
    svc::{self, layer},
    transport::io,
    Error,
};
use std::{
    future::Future,
    pin::Pin,
    str::FromStr,
    task::{Context, Poll},
};
use tracing::{debug, warn};

pub struct OpaqueTransport<S> {
    inner: S,
}

impl<S> OpaqueTransport<S> {
    pub fn layer() -> impl layer::Layer<S, Service = Self> {
        layer::mk(|inner| OpaqueTransport { inner })
    }
}

impl<S, P> svc::Service<Endpoint<P>> for OpaqueTransport<S>
where
    P: Send,
    S: svc::Service<Endpoint<P>> + Send + 'static,
    S::Error: Into<Error>,
    S::Response: io::AsyncWrite + Unpin + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<S::Response, Error>> + Send + 'static>>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(futures::ready!(self.inner.poll_ready(cx)).map_err(Into::into))
    }

    fn call(&mut self, mut ep: Endpoint<P>) -> Self::Future {
        // Determine whether an opaque header should written on the socket.
        let mut opaque_header = None;
        if let Some(override_port) = ep.metadata.opaque_transport_port() {
            // If there's a destination override, encode that in the opaque
            // transport (i.e. for multicluster gateways). Otherwise, simply
            // encode the original target port.
            let orig_port = ep.addr.port();
            let header = ep
                .metadata
                .authority_override()
                .and_then(|auth| {
                    let port = auth.port_u16().unwrap_or(orig_port);
                    Name::from_str(auth.host())
                        .map_err(|error| warn!(%error, "Invalid name"))
                        .ok()
                        .map(|n| Header {
                            port,
                            name: Some(n),
                        })
                })
                .unwrap_or_else(|| Header {
                    port: orig_port,
                    name: None,
                });

            debug!(?header, override_port, "Using opaque transport");
            opaque_header = Some(header);

            // Update the endpoint to target the discovery-provided control
            // plane port.
            ep.addr = (ep.addr.ip(), override_port).into();
        }

        // Connect to the endpoint.
        let connect = self.inner.call(ep);
        Box::pin(async move {
            let mut socket = connect.await.map_err(Into::into)?;

            // Once connected, write the opaque header on the socket before
            // returning it.
            if let Some(hdr) = opaque_header {
                hdr.write(&mut socket).await?;
                println!("Wrote");
            }

            Ok(socket)
        })
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::target::{Concrete, Endpoint, Logical};
    use futures::future;
    use linkerd2_app_core::{
        connection_header::Header,
        proxy::api_resolve::{Metadata, ProtocolHint},
        transport::{
            io::{self, AsyncWriteExt},
            tls,
        },
    };
    use tower::util::{service_fn, ServiceExt};

    fn ep(metadata: Metadata) -> Endpoint<()> {
        Endpoint {
            addr: ([127, 0, 0, 2], 4321).into(),
            identity: tls::PeerIdentity::None(
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
                let hdr = Header {
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
                let hdr = Header {
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
                let hdr = Header {
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
