use http::{self, header::HOST};

/// Settings portion of the `Recognize` key for a request.
///
/// This marks whether to use HTTP/2 or HTTP/1.x for a request. In
/// the case of HTTP/1.x requests, it also stores a "host" key to ensure
/// that each host receives its own connection.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Settings {
    Http1 {
        /// Indicates whether connections can be reused for each request.
        keep_alive: bool,
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
            keep_alive: !is_missing_authority,
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

    pub fn is_http2(&self) -> bool {
        match self {
            Settings::Http1 { .. } => false,
            Settings::Http2 => true,
        }
    }
}

pub mod router {
    extern crate linkerd2_router as rt;

    use futures::Poll;
    use http;
    use std::fmt;
    use std::hash::Hash;
    use std::marker::PhantomData;

    use super::Settings;
    use proxy::http::client::Config;
    use svc;

    type Error = Box<dyn std::error::Error + Send + Sync>;

    #[derive(Debug)]
    pub struct Layer<B, T>(PhantomData<(T, fn(B))>);

    #[derive(Debug)]
    pub struct MakeSvc<B, T, M>(M, PhantomData<fn(B, T)>);

    pub struct Service<B, T, M>
    where
        T: fmt::Debug + Clone + Hash + Eq,
        M: rt::Make<Config<T>>,
        M::Value: svc::Service<http::Request<B>>,
    {
        router: Router<B, T, M>,
    }

    pub struct Recognize<T>(T);

    type Router<B, T, M> = rt::Router<http::Request<B>, Recognize<T>, M>;

    pub fn layer<B, T>() -> Layer<B, T>
    where
        T: fmt::Debug + Clone + Hash + Eq,
    {
        Layer(PhantomData)
    }

    impl<B, T> Clone for Layer<B, T> {
        fn clone(&self) -> Self {
            Layer(PhantomData)
        }
    }

    impl<B, T, M, Svc> svc::Layer<M> for Layer<B, T>
    where
        MakeSvc<B, T, M>: svc::Service<T>,
        T: fmt::Debug + Clone + Hash + Eq,
        M: rt::Make<Config<T>, Value = Svc> + Clone,
        Svc: svc::Service<http::Request<B>> + Clone,
        Svc::Error: Into<Error>,
    {
        type Service = MakeSvc<B, T, M>;

        fn layer(&self, inner: M) -> Self::Service {
            MakeSvc(inner, PhantomData)
        }
    }

    impl<B, T, M: Clone> Clone for MakeSvc<B, T, M>
    where
        T: fmt::Debug + Clone + Hash + Eq,
    {
        fn clone(&self) -> Self {
            MakeSvc(self.0.clone(), PhantomData)
        }
    }

    impl<B, T, M, Svc> svc::Service<T> for MakeSvc<B, T, M>
    where
        T: fmt::Debug + Clone + Hash + Eq,
        M: rt::Make<Config<T>, Value = Svc> + Clone,
        Svc: svc::Service<http::Request<B>> + Clone,
        Svc::Error: Into<Error>,
    {
        type Response = Service<B, T, M>;
        type Error = never::Never;
        type Future = futures::future::FutureResult<Self::Response, Self::Error>;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            Ok(().into()) // always ready to make a Router
        }

        fn call(&mut self, target: T) -> Self::Future {
            use std::time::Duration;

            let router = Router::new(
                Recognize(target),
                self.0.clone(),
                Settings::ROUTER_CAPACITY,
                // Doesn't matter, since we are guaranteed to have enough capacity.
                Duration::from_secs(0),
            );

            futures::future::ok(Service { router })
        }
    }

    impl<B, T> rt::Recognize<http::Request<B>> for Recognize<T>
    where
        T: fmt::Debug + Clone + Hash + Eq,
    {
        type Target = Config<T>;

        fn recognize(&self, req: &http::Request<B>) -> Option<Self::Target> {
            let settings = Settings::from_request(req);
            Some(Config::new(self.0.clone(), settings))
        }
    }

    impl<B, T, M> Clone for Service<B, T, M>
    where
        T: fmt::Debug + Clone + Hash + Eq,
        M: rt::Make<Config<T>>,
        M::Value: svc::Service<http::Request<B>>,
    {
        fn clone(&self) -> Self {
            Self {
                router: self.router.clone(),
            }
        }
    }

    impl<B, T, M, Svc> svc::Service<http::Request<B>> for Service<B, T, M>
    where
        T: fmt::Debug + Clone + Hash + Eq,
        M: rt::Make<Config<T>, Value = Svc>,
        Svc: svc::Service<http::Request<B>> + Clone,
        Svc::Error: Into<Error>,
    {
        type Response = <Router<B, T, M> as svc::Service<http::Request<B>>>::Response;
        type Error = Error;
        type Future = rt::ResponseFuture<http::Request<B>, Svc>;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            self.router.poll_ready()
        }

        fn call(&mut self, req: http::Request<B>) -> Self::Future {
            self.router.call(req)
        }
    }
}
