use crate::{http, Outbound};
use linkerd_app_core::{detect, io, svc, Error, Infallible};
use std::{fmt::Debug, hash::Hash};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Http<T> {
    version: http::Version,
    parent: T,
}

/// Parameter type indicating how the proxy should handle a connection.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Protocol {
    Http1,
    Http2,
    Detect,
    Opaque,
}

// === impl Outbound ===

impl<N> Outbound<N> {
    /// Builds a stack that handles protocol detection as well as routing and
    /// load balancing for a single logical destination.
    ///
    /// The inner stack is built once for each `T` target when the protocol is
    /// known. When `Protocol::Detect` is used, the inner stack is built for
    /// each connection.
    pub fn push_protocol<T, I, H, HSvc, NSvc>(self, http: H) -> Outbound<svc::ArcNewTcp<T, I>>
    where
        // Target type indicating whether detection should be skipped.
        T: svc::Param<Protocol>,
        T: Eq + Hash + Clone + Debug + Send + Sync + 'static,
        // Server-side socket.
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr,
        I: Debug + Send + Sync + Unpin + 'static,
        // HTTP request stack.
        H: svc::NewService<Http<T>, Service = HSvc>,
        H: Clone + Send + Sync + Unpin + 'static,
        HSvc: svc::Service<
            http::Request<http::BoxBody>,
            Response = http::Response<http::BoxBody>,
            Error = Error,
        >,
        HSvc: Clone + Send + Sync + Unpin + 'static,
        HSvc::Future: Send,
        // Opaque connection stack.
        N: svc::NewService<T, Service = NSvc>,
        N: Clone + Send + Sync + Unpin + 'static,
        NSvc: svc::Service<io::EitherIo<I, io::PrefixedIo<I>>, Response = (), Error = Error>,
        NSvc: Clone + Send + Sync + Unpin + 'static,
        NSvc::Future: Send,
    {
        let opaq = self.clone().into_stack();

        let http = self.with_stack(http).map_stack(|config, rt, stk| {
            stk.push_on_service(http::BoxRequest::layer())
                .unlift_new()
                .push(http::NewServeHttp::layer(
                    config.proxy.server.h2_settings,
                    rt.drain.clone(),
                ))
                .push_on_service(svc::BoxService::layer())
                .push(svc::ArcNewService::layer())
        });

        let detect = http.clone().map_stack(|config, _, http| {
            http.push_switch(
                |(result, parent): (detect::Result<http::Version>, T)| -> Result<_, Infallible> {
                    Ok(match detect::allow_timeout(result) {
                        Some(version) => svc::Either::A(Http { version, parent }),
                        None => svc::Either::B(parent),
                    })
                },
                opaq.clone().into_inner(),
            )
            // `DetectService` oneshots the inner service, so we add
            // a loadshed to prevent leaking tasks if (for some
            // unexpected reason) the inner service is not ready.
            .push_on_service(svc::LoadShed::layer())
            .push_on_service(svc::MapTargetLayer::new(io::EitherIo::Right))
            .lift_new_with_target::<(detect::Result<http::Version>, T)>()
            .push(detect::NewDetectService::layer(config.proxy.detect_http()))
            .push_on_service(svc::BoxService::layer())
            .push(svc::ArcNewService::layer())
        });

        http.map_stack(|_, _, http| {
            // First separate traffic that needs protocol detection. Then switch
            // between traffic that is known to be HTTP or opaque.
            http.push_switch(Ok::<_, Infallible>, opaq.clone().into_inner())
                .push_on_service(svc::MapTargetLayer::new(io::EitherIo::Left))
                .push_switch(
                    |parent: T| -> Result<_, Infallible> {
                        match parent.param() {
                            Protocol::Http1 => Ok(svc::Either::A(svc::Either::A(Http {
                                version: http::Version::Http1,
                                parent,
                            }))),
                            Protocol::Http2 => Ok(svc::Either::A(svc::Either::A(Http {
                                version: http::Version::H2,
                                parent,
                            }))),
                            Protocol::Opaque => Ok(svc::Either::A(svc::Either::B(parent))),
                            Protocol::Detect => Ok(svc::Either::B(parent)),
                        }
                    },
                    detect.into_inner(),
                )
                .push_on_service(svc::BoxService::layer())
                .push(svc::ArcNewService::layer())
        })
    }
}

// === impl Http ===

impl<T> From<(http::Version, T)> for Http<T> {
    fn from((version, parent): (http::Version, T)) -> Self {
        Self { version, parent }
    }
}

impl<T> svc::Param<http::Version> for Http<T> {
    fn param(&self) -> http::Version {
        self.version
    }
}

impl<T> svc::Param<http::normalize_uri::DefaultAuthority> for Http<T>
where
    T: svc::Param<http::normalize_uri::DefaultAuthority>,
{
    fn param(&self) -> http::normalize_uri::DefaultAuthority {
        self.parent.param()
    }
}

impl<T> std::ops::Deref for Http<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.parent
    }
}
