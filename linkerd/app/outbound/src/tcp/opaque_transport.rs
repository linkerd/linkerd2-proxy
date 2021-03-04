use crate::target::Endpoint;
use linkerd_app_core::{
    dns, io,
    svc::{self, Param},
    tls,
    transport::{Remote, ServerAddr},
    transport_header::{SessionProtocol, TransportHeader, PROTOCOL},
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
    pub fn layer() -> impl svc::Layer<S, Service = Self> + Copy {
        svc::layer::mk(|inner| OpaqueTransport { inner })
    }

    /// Determines whether the connection has negotiated support for the
    /// transport header.
    #[inline]
    fn header_negotiated<I: tls::HasNegotiatedProtocol>(io: &I) -> bool {
        if let Some(tls::NegotiatedProtocolRef(protocol)) = io.negotiated_protocol() {
            protocol == PROTOCOL
        } else {
            false
        }
    }
}

impl<S, P> svc::Service<Endpoint<P>> for OpaqueTransport<S>
where
    Endpoint<P>: Param<Option<SessionProtocol>>,
    S: svc::Service<Endpoint<P>> + Send + 'static,
    S::Error: Into<Error>,
    S::Response: io::AsyncWrite + tls::HasNegotiatedProtocol + Send + Unpin,
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
        // Configure the target port from the endpoint. In opaque cases, this is
        // the application's actual port to be encoded in the header.
        let Remote(ServerAddr(addr)) = ep.addr;
        let mut target_port = addr.port();

        // If this endpoint should use opaque transport, then we update the
        // endpoint so the connection actually targets the target proxy's
        // inbound port.
        if let Some(opaque_port) = ep.metadata.opaque_transport_port() {
            debug!(target_port, opaque_port, "Using opaque transport");
            ep.addr = Remote(ServerAddr((addr.ip(), opaque_port).into()));
        }

        // If an authority override is present, we're communicating with a
        // remote gateway:
        // - The target port will already be the proxy's inbound port, so
        //   override it from the authority.
        // - Encode the name from the authority override so the gateway can
        //   route the connection appropriately.
        let mut name = None;
        if let Some(authority) = ep.metadata.authority_override() {
            if let Some(override_port) = authority.port_u16() {
                name = dns::Name::from_str(authority.host())
                    .map_err(|error| warn!(%error, "Invalid name"))
                    .ok();
                target_port = override_port;
                debug!(?name, target_port, "Using authority override");
            }
        }

        let protocol: Option<SessionProtocol> = ep.param();

        let connect = self.inner.call(ep);
        Box::pin(async move {
            let mut io = connect.await.map_err(Into::into)?;

            // If transport header support has been negotiated via ALPN, encode
            // the header and then return the socket.
            if Self::header_negotiated(&io) {
                let header = TransportHeader {
                    port: target_port,
                    name,
                    protocol,
                };
                trace!(?header, "Writing transport header");
                let sz = header.write(&mut io).await?;
                debug!(sz, "Wrote transport header");
            } else {
                trace!("Connection does not expect a transport header");
            }

            Ok(io)
        })
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::target::Endpoint;
    use futures::future;
    use linkerd_app_core::{
        io::{self, AsyncWriteExt},
        proxy::api_resolve::{Metadata, ProtocolHint},
        tls,
        transport::{Remote, ServerAddr},
        transport_header::TransportHeader,
        Addr, Conditional,
    };
    use pin_project::pin_project;
    use std::task::Context;
    use tower::util::{service_fn, ServiceExt};

    fn ep(metadata: Metadata) -> Endpoint<()> {
        Endpoint {
            addr: Remote(ServerAddr(([127, 0, 0, 2], 4321).into())),
            tls: Conditional::None(tls::NoClientTls::NotProvidedByServiceDiscovery),
            metadata,
            logical_addr: Addr::Socket(([127, 0, 0, 2], 4321).into()),
            protocol: (),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn plain() {
        #[cfg(feature = "test-subscriber")]
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();

        let svc = OpaqueTransport {
            inner: service_fn(|ep: Endpoint<()>| {
                let Remote(ServerAddr(sa)) = ep.addr;
                assert_eq!(sa.port(), 4321);
                future::ready(Ok::<_, io::Error>(Io {
                    io: tokio_test::io::Builder::new().write(b"hello").build(),
                    alpn: None,
                }))
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
        #[cfg(feature = "test-subscriber")]
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();

        let svc = OpaqueTransport {
            inner: service_fn(|ep: Endpoint<()>| {
                let Remote(ServerAddr(sa)) = ep.addr;
                assert_eq!(sa.port(), 4143);
                let hdr = TransportHeader {
                    port: 4321,
                    name: None,
                    protocol: None,
                };
                let buf = hdr.encode_prefaced_buf().expect("Must encode");
                future::ready(Ok::<_, io::Error>(Io {
                    alpn: Some(tls::NegotiatedProtocolRef(PROTOCOL)),
                    io: tokio_test::io::Builder::new()
                        .write(&buf[..])
                        .write(b"hello")
                        .build(),
                }))
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
        #[cfg(feature = "test-subscriber")]
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();

        let svc = OpaqueTransport {
            inner: service_fn(|ep: Endpoint<()>| {
                let Remote(ServerAddr(sa)) = ep.addr;
                assert_eq!(sa.port(), 4143);
                let hdr = TransportHeader {
                    port: 5555,
                    name: Some(dns::Name::from_str("foo.bar.example.com").unwrap()),
                    protocol: None,
                };
                let buf = hdr.encode_prefaced_buf().expect("Must encode");
                future::ready(Ok::<_, io::Error>(Io {
                    alpn: Some(tls::NegotiatedProtocolRef(PROTOCOL)),
                    io: tokio_test::io::Builder::new()
                        .write(&buf[..])
                        .write(b"hello")
                        .build(),
                }))
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
        #[cfg(feature = "test-subscriber")]
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();

        let svc = OpaqueTransport {
            inner: service_fn(|ep: Endpoint<()>| {
                let Remote(ServerAddr(sa)) = ep.addr;
                assert_eq!(sa.port(), 4143);
                let hdr = TransportHeader {
                    port: 4321,
                    name: None,
                    protocol: None,
                };
                let buf = hdr.encode_prefaced_buf().expect("Must encode");
                future::ready(Ok::<_, io::Error>(Io {
                    alpn: Some(tls::NegotiatedProtocolRef(PROTOCOL)),
                    io: tokio_test::io::Builder::new()
                        .write(&buf[..])
                        .write(b"hello")
                        .build(),
                }))
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

    #[pin_project]
    pub struct Io {
        #[pin]
        io: tokio_test::io::Mock,
        alpn: Option<tls::NegotiatedProtocolRef<'static>>,
    }

    impl tls::HasNegotiatedProtocol for Io {
        fn negotiated_protocol(&self) -> Option<tls::NegotiatedProtocolRef<'_>> {
            self.alpn
        }
    }

    impl io::AsyncRead for Io {
        #[inline]
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut io::ReadBuf<'_>,
        ) -> io::Poll<()> {
            self.project().io.poll_read(cx, buf)
        }
    }

    impl io::AsyncWrite for Io {
        #[inline]
        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> io::Poll<()> {
            self.project().io.poll_shutdown(cx)
        }

        #[inline]
        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> io::Poll<()> {
            self.project().io.poll_flush(cx)
        }

        #[inline]
        fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> io::Poll<usize> {
            self.project().io.poll_write(cx, buf)
        }

        #[inline]
        fn poll_write_vectored(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[io::IoSlice<'_>],
        ) -> io::Poll<usize> {
            self.project().io.poll_write_vectored(cx, buf)
        }

        #[inline]
        fn is_write_vectored(&self) -> bool {
            self.io.is_write_vectored()
        }
    }
}
