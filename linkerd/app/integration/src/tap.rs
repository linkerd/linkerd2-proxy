use super::*;
use futures::stream;
use http_body::Body;
use linkerd2_proxy_api::tap as pb;
use linkerd_app_core::svc::http::BoxBody;

pub fn client(addr: SocketAddr) -> Client {
    let api = pb::tap_client::TapClient::new(SyncSvc(client::http2(addr, "localhost")));
    Client { api }
}

pub fn client_with_auth<T: Into<String>>(addr: SocketAddr, auth: T) -> Client {
    let api = pb::tap_client::TapClient::new(SyncSvc(client::http2(addr, auth)));
    Client { api }
}

pub struct Client {
    api: pb::tap_client::TapClient<SyncSvc>,
}

impl Client {
    pub async fn observe(
        &mut self,
        req: ObserveBuilder,
    ) -> Pin<Box<dyn Stream<Item = Result<pb::TapEvent, tonic::Status>> + Send>> {
        let req = tonic::Request::new(req.0);
        match self.api.observe(req).await {
            Ok(rsp) => Box::pin(rsp.into_inner()),
            Err(e) => Box::pin(stream::once(async move { Err(e) })),
        }
    }

    pub async fn observe_with_require_id(
        &mut self,
        req: ObserveBuilder,
        require_id: &str,
    ) -> Pin<Box<dyn Stream<Item = Result<pb::TapEvent, tonic::Status>> + Send>> {
        let mut req = tonic::Request::new(req.0);

        let require_id = tonic::metadata::MetadataValue::try_from(require_id).unwrap();
        req.metadata_mut().insert("l5d-require-id", require_id);

        match self.api.observe(req).await {
            Ok(rsp) => Box::pin(rsp.into_inner()),
            Err(e) => Box::pin(stream::once(async move { Err(e) })),
        }
    }
}

pub fn observe_request() -> ObserveBuilder {
    ObserveBuilder(pb::ObserveRequest {
        limit: 100,
        r#match: Some(pb::observe_request::Match {
            r#match: Some(pb::observe_request::r#match::Match::Http(
                pb::observe_request::r#match::Http {
                    r#match: Some(pb::observe_request::r#match::http::Match::Path(
                        pb::observe_request::r#match::http::StringMatch {
                            r#match: Some(
                                pb::observe_request::r#match::http::string_match::Match::Prefix(
                                    "/".to_string(),
                                ),
                            ),
                        },
                    )),
                },
            )),
        }),
        extract: Some(pb::observe_request::Extract {
            extract: Some(pb::observe_request::extract::Extract::Http(
                pb::observe_request::extract::Http {
                    extract: None, // no headers
                },
            )),
        }),
    })
}

#[derive(Debug)]
pub struct ObserveBuilder(pb::ObserveRequest);

impl ObserveBuilder {
    pub fn limit(mut self, limit: u32) -> Self {
        self.0.limit = limit;
        self
    }

    pub fn ports(mut self, min: u16, max: u16) -> Self {
        self.0.r#match = Some(pb::observe_request::Match {
            r#match: Some(pb::observe_request::r#match::Match::Destination(
                pb::observe_request::r#match::Tcp {
                    r#match: Some(pb::observe_request::r#match::tcp::Match::Ports(
                        pb::observe_request::r#match::tcp::PortRange {
                            min: min.into(),
                            max: max.into(),
                        },
                    )),
                },
            )),
        });
        self
    }
}

pub trait TapEventExt {
    fn is_inbound(&self) -> bool;
    fn is_outbound(&self) -> bool;
    //fn id(&self) -> (u32, u64);
    fn event(&self) -> &pb::tap_event::http::Event;

    fn request_init_authority(&self) -> &str;
    fn request_init_path(&self) -> &str;

    fn response_init_status(&self) -> u16;

    fn response_end_bytes(&self) -> u64;
    fn response_end_eos_grpc(&self) -> u32;
}

impl TapEventExt for pb::TapEvent {
    fn is_inbound(&self) -> bool {
        self.proxy_direction == pb::tap_event::ProxyDirection::Inbound as i32
    }

    fn is_outbound(&self) -> bool {
        self.proxy_direction == pb::tap_event::ProxyDirection::Outbound as i32
    }

    fn event(&self) -> &pb::tap_event::http::Event {
        match self.event {
            Some(pb::tap_event::Event::Http(pb::tap_event::Http {
                event: Some(ref ev),
            })) => ev,
            _ => panic!("unknown event: {:?}", self.event),
        }
    }

    fn request_init_authority(&self) -> &str {
        match self.event() {
            pb::tap_event::http::Event::RequestInit(ev) => &ev.authority,
            e => panic!("not RequestInit event: {e:?}"),
        }
    }

    fn request_init_path(&self) -> &str {
        match self.event() {
            pb::tap_event::http::Event::RequestInit(ev) => &ev.path,
            e => panic!("not RequestInit event: {e:?}"),
        }
    }

    fn response_init_status(&self) -> u16 {
        match self.event() {
            pb::tap_event::http::Event::ResponseInit(ev) => ev.http_status as u16,
            e => panic!("not ResponseInit event: {e:?}"),
        }
    }

    fn response_end_bytes(&self) -> u64 {
        match self.event() {
            pb::tap_event::http::Event::ResponseEnd(ev) => ev.response_bytes,
            e => panic!("not ResponseEnd event: {e:?}"),
        }
    }

    fn response_end_eos_grpc(&self) -> u32 {
        match self.event() {
            pb::tap_event::http::Event::ResponseEnd(ev) => match ev.eos {
                Some(pb::Eos {
                    end: Some(pb::eos::End::GrpcStatusCode(code)),
                }) => code,
                _ => panic!("not Eos GrpcStatusCode: {:?}", ev.eos),
            },
            ev => panic!("not ResponseEnd event: {ev:?}"),
        }
    }
}

struct SyncSvc(client::Client);

type ResponseFuture =
    Pin<Box<dyn Future<Output = Result<http::Response<hyper::body::Incoming>, String>> + Send>>;

impl<B> tower::Service<http::Request<B>> for SyncSvc
where
    B: Body,
    B::Error: std::fmt::Debug,
{
    type Response = http::Response<hyper::body::Incoming>;
    type Error = String;
    type Future = ResponseFuture;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        use http_body_util::Full;
        let Self(client) = self;
        let req = req.map(Self::collect_body).map(Full::new).map(BoxBody::new);
        let fut = client.send_req(req).map_err(|err| err.to_string());
        Box::pin(fut)
    }
}

impl SyncSvc {
    /// Collects the given [`Body`], returning a [`Bytes`].
    ///
    /// NB: This blocks the current thread until the provided body has been collected. This is
    /// an acceptable practice in test code for the sake of simplicitly, because we will always
    /// provide [`SyncSvc`] with bodies that are complete.
    fn collect_body<B>(body: B) -> Bytes
    where
        B: Body,
        B::Error: std::fmt::Debug,
    {
        futures::executor::block_on(async move {
            use http_body_util::BodyExt;
            body.collect()
                .await
                .expect("body should not fail")
                .to_bytes()
        })
    }
}
