use http::{self, header::HOST};

/// Settings portion of the `Recognize` key for a request.
///
/// This marks whether to use HTTP/2 or HTTP/1.x for a request. In
/// the case of HTTP/1.x requests, it also stores a "host" key to ensure
/// that each host receives its own connection.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Settings {
    Http1 {
        /// Indicates whether a new service must be created for each request.
        stack_per_request: bool,
        /// Whether or not the request URI was in absolute form.
        ///
        /// This is used to configure Hyper's behaviour at the connection
        /// level, so it's necessary that requests with and without
        /// absolute URIs be bound to separate service stacks. It is also
        /// used to determine what URI normalization will be necessary.
        was_absolute_form: bool,
    },
    Http2,
}

// ===== impl Settings =====

impl Settings {
    // The router need only have enough capacity for each `Settings` variant.
    const ROUTER_CAPACITY: usize = 5;

    pub fn from_request<B>(req: &http::Request<B>) -> Self {
        if req.version() == http::Version::HTTP_2 {
            return Settings::Http2;
        }

        let is_missing_authority = req
            .uri()
            .authority_part()
            .map(|_| false)
            .or_else(|| {
                req.headers()
                    .get(HOST)
                    .and_then(|h| h.to_str().ok())
                    .map(|h| h.is_empty())
            })
            .unwrap_or(true);

        Settings::Http1 {
            stack_per_request: is_missing_authority,
            was_absolute_form: super::h1::is_absolute_form(req.uri()),
        }
    }

    /// Returns true if the request was originally received in absolute form.
    pub fn was_absolute_form(&self) -> bool {
        match self {
            Settings::Http1 {
                was_absolute_form, ..
            } => *was_absolute_form,
            Settings::Http2 => false,
        }
    }

    pub fn can_reuse_clients(&self) -> bool {
        match self {
            Settings::Http1 {
                stack_per_request, ..
            } => !stack_per_request,
            Settings::Http2 => true,
        }
    }

    pub fn is_http2(&self) -> bool {
        match self {
            Settings::Http1 { .. } => false,
            Settings::Http2 => true,
        }
    }
}

pub mod router {
    extern crate linkerd2_router as rt;

    use futures::{Future, Poll};
    use http;
    use std::{error, fmt};
    use std::marker::PhantomData;

    use super::Settings;
    use proxy::http::client::Config;
    use proxy::http::HasH2Reason;
    use svc;
    use transport::connect;

    pub trait HasConnect {
        fn connect(&self) -> connect::Target;
    }

    #[derive(Debug)]
    pub struct Layer<T, B>(PhantomData<(T, fn(B))>);

    #[derive(Debug)]
    pub struct Stack<B, M>(M, PhantomData<fn(B)>);

    pub struct Service<B, M>
    where
        M: svc::Stack<Config>,
        M::Value: svc::Service<http::Request<B>>,
    {
        router: Router<B, M>,
    }

    pub struct ResponseFuture<B, M>
    where
        M: svc::Stack<Config>,
        M::Value: svc::Service<http::Request<B>>,
    {
        inner: <Router<B, M> as svc::Service<http::Request<B>>>::Future
    }

    #[derive(Debug)]
    pub enum Error<E, M> {
        Service(E),
        Stack(M),
    }

    pub struct Recognize(connect::Target);

    type Router<B, M> = rt::Router<http::Request<B>, Recognize, M>;

    pub fn layer<T: HasConnect, B>() -> Layer<T, B> {
        Layer(PhantomData)
    }

    impl<T, B> Clone for Layer<T, B> {
        fn clone(&self) -> Self {
            Layer(PhantomData)
        }
    }

    impl<B, T, M> svc::Layer<T, Config, M> for Layer<T, B>
    where
        T: HasConnect,
        M: svc::Stack<Config> + Clone,
        M::Value: svc::Service<http::Request<B>>,
    {
        type Value = <Stack<B, M> as svc::Stack<T>>::Value;
        type Error = <Stack<B, M> as svc::Stack<T>>::Error;
        type Stack = Stack<B, M>;

        fn bind(&self, inner: M) -> Self::Stack {
            Stack(inner, PhantomData)
        }
    }

    impl<B, M: Clone> Clone for Stack<B, M> {
        fn clone(&self) -> Self {
            Stack(self.0.clone(), PhantomData)
        }
    }

    impl<B, T, M> svc::Stack<T> for Stack<B, M>
    where
        T: HasConnect,
        M: svc::Stack<Config> + Clone,
        M::Value: svc::Service<http::Request<B>>,
    {
        type Value = Service<B, M>;
        type Error = M::Error;

        fn make(&self, target: &T) -> Result<Self::Value, Self::Error> {
            use std::time::Duration;

            let router = Router::new(
                Recognize(target.connect()),
                self.0.clone(),
                Settings::ROUTER_CAPACITY,
                // Doesn't matter, since we are guaranteed to have enough capacity.
                Duration::from_secs(0),
            );

            Ok(Service { router })
        }
    }

    impl<B> rt::Recognize<http::Request<B>> for Recognize {
        type Target = Config;

        fn recognize(&self, req: &http::Request<B>) -> Option<Self::Target> {
            let settings = Settings::from_request(req);
            Some(Config::new(self.0.clone(), settings))
        }
    }

    impl<B, M> svc::Service<http::Request<B>> for Service<B, M>
    where
        M: svc::Stack<Config>,
        M::Value: svc::Service<http::Request<B>>,
    {
        type Response = <Router<B, M> as svc::Service<http::Request<B>>>::Response;
        type Error = Error<<M::Value as svc::Service<http::Request<B>>>::Error, M::Error>;
        type Future = ResponseFuture<B, M>;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            match self.router.poll_ready() {
                Ok(async) => Ok(async),
                Err(rt::Error::Inner(e)) => Err(Error::Service(e)),
                Err(rt::Error::Route(e)) => Err(Error::Stack(e)),
                Err(rt::Error::NoCapacity(_)) | Err(rt::Error::NotRecognized) => {
                    unreachable!("router must reliably dispatch");
                }
            }
        }

        fn call(&mut self, req: http::Request<B>) -> Self::Future {
            ResponseFuture { inner: self.router.call(req) }
        }
    }

    impl<B, M> Future for ResponseFuture<B, M>
    where
        M: svc::Stack<Config>,
        M::Value: svc::Service<http::Request<B>>,
    {
        type Item = <Router<B, M> as svc::Service<http::Request<B>>>::Response;
        type Error = Error<<M::Value as svc::Service<http::Request<B>>>::Error, M::Error>;

        fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
            match self.inner.poll() {
                Ok(async) => Ok(async),
                Err(rt::Error::Inner(e)) => Err(Error::Service(e)),
                Err(rt::Error::Route(e)) => Err(Error::Stack(e)),
                Err(rt::Error::NoCapacity(_)) | Err(rt::Error::NotRecognized) => {
                    unreachable!("router must reliably dispatch");
                }
            }
        }
    }

    impl<E: fmt::Display, M: fmt::Display> fmt::Display for Error<E, M> {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            match self {
                Error::Service(e) => e.fmt(f),
                Error::Stack(e) => e.fmt(f),
            }
        }
    }

    impl<E: error::Error, M: error::Error> error::Error for Error<E, M> {
        fn cause(&self) -> Option<&error::Error> {
            match self {
                Error::Service(e) => e.cause(),
                Error::Stack(e) => e.cause(),
            }
        }
    }

    impl<E: HasH2Reason, M> HasH2Reason for Error<E, M> {
        fn h2_reason(&self) -> Option<::h2::Reason> {
            match self {
                Error::Service(e) => e.h2_reason(),
                Error::Stack(_) => None,
            }
        }
    }
}
