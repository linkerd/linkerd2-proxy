use super::*;
use bytes::BytesMut;
use http_body::Body;
use linkerd2_proxy_api::tap as pb;
use std::io::Cursor;

pub fn client(addr: SocketAddr) -> Client {
    let api = pb::client::Tap::new(SyncSvc(client::http2(addr, "localhost")));
    Client { api }
}

pub fn client_with_auth<T: Into<String>>(addr: SocketAddr, auth: T) -> Client {
    let api = pb::client::Tap::new(SyncSvc(client::http2(addr, auth)));
    Client { api }
}

pub struct Client {
    api: pb::client::Tap<SyncSvc>,
}

impl Client {
    pub fn observe(
        &mut self,
        req: ObserveBuilder,
    ) -> impl Stream01<Item = pb::TapEvent, Error = tower_grpc::Status> {
        let req = tower_grpc::Request::new(req.0);
        self.api
            .observe(req)
            .wait()
            .expect("tap observe wait")
            .into_inner()
    }

    pub fn observe_with_require_id(
        &mut self,
        req: ObserveBuilder,
        require_id: &str,
    ) -> impl Stream01<Item = pb::TapEvent, Error = tower_grpc::Status> {
        let mut req = tower_grpc::Request::new(req.0);

        let require_id = tower_grpc::metadata::MetadataValue::from_str(require_id).unwrap();
        req.metadata_mut().insert("l5d-require-id", require_id);

        self.api
            .observe(req)
            .wait()
            .expect("tap observe wait")
            .into_inner()
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

    fn request_init_method(&self) -> String;
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

    fn request_init_method(&self) -> String {
        match self.event() {
            pb::tap_event::http::Event::RequestInit(_ev) => {
                //TODO: ugh
                unimplemented!("method");
            }
            _ => panic!("not RequestInit event"),
        }
    }

    fn request_init_authority(&self) -> &str {
        match self.event() {
            pb::tap_event::http::Event::RequestInit(ev) => &ev.authority,
            _ => panic!("not RequestInit event"),
        }
    }

    fn request_init_path(&self) -> &str {
        match self.event() {
            pb::tap_event::http::Event::RequestInit(ev) => &ev.path,
            _ => panic!("not RequestInit event"),
        }
    }

    fn response_init_status(&self) -> u16 {
        match self.event() {
            pb::tap_event::http::Event::ResponseInit(ev) => ev.http_status as u16,
            _ => panic!("not ResponseInit event"),
        }
    }

    fn response_end_bytes(&self) -> u64 {
        match self.event() {
            pb::tap_event::http::Event::ResponseEnd(ev) => ev.response_bytes,
            _ => panic!("not ResponseEnd event"),
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
            _ => panic!("not ResponseEnd event"),
        }
    }
}

struct SyncSvc(client::Client);

impl<B> tower_01::Service<http_01::Request<B>> for SyncSvc
where
    B: grpc::Body,
{
    type Response = http_01::Response<CompatBody>;
    type Error = String;
    type Future = Box<dyn Future01<Item = Self::Response, Error = Self::Error> + Send>;

    fn poll_ready(&mut self) -> Poll01<(), Self::Error> {
        unreachable!("tap SyncSvc poll_ready");
    }

    fn call(&mut self, req: http_01::Request<B>) -> Self::Future {
        use bytes::BufMut;
        use bytes_04::{Buf, IntoBuf};
        use std::convert::TryInto;
        // XXX: this is all terrible, but hopefully all this code can leave soon.
        let (parts, mut body) = req.into_parts();
        let mut buf = BytesMut::new();
        while let Some(bytes) = futures_01::future::poll_fn(|| body.poll_data())
            .wait()
            .map_err(Into::into)
            .expect("req body")
        {
            buf.put(bytes.into_buf().bytes());
        }

        let body = buf.freeze();
        let mut req = http::Request::builder()
            .method(parts.method.as_str().try_into().unwrap())
            // .version(parts.version.as_str().try_into().unwrap())
            .uri((&parts.uri.to_string()[..]).try_into().unwrap());
        *req.headers_mut().unwrap() = headermap_compat_01(parts.headers);
        let req = req.body(body).unwrap();

        Box::new(
            Box::pin(self.0.send_req(req))
                .compat()
                .map_err(|err| err.to_string())
                .map(CompatBody::new),
        )
    }
}

struct CompatBody(hyper::Body);

impl http_body_01::Body for CompatBody {
    type Data = Cursor<bytes_04::Bytes>;
    type Error = hyper::Error;

    fn is_end_stream(&self) -> bool {
        self.0.is_end_stream()
    }

    fn poll_data(&mut self) -> Poll01<Option<Self::Data>, Self::Error> {
        let poll = task_compat::with_context(|cx| Pin::new(self.0).poll_data(cx));
        match poll {
            Poll::Ready(Some(Ok(data))) => {
                Ok(Async::Ready(Some(Cursor::new(Bytes::from(data.bytes())))))
            }
            Poll::Ready(Some(Err(e))) => Err(e),
            Poll::Ready(None) => Ok(Async::Ready(None)),
            Poll::Pending => Ok(Async::NotReady),
        }
    }

    fn poll_trailers(&mut self) -> Poll01<Option<http_01::HeaderMap>, Self::Error> {
        let poll = task_compat::with_context(|cx| Pin::new(&mut self.0).poll_trailers(cx));
        match poll {
            Poll::Ready(Some(Ok(headers))) => {
                let headers = headermap_compat(headers);
                Ok(Async::Ready(Some(headers)))
            }
            Poll::Ready(Some(Err(e))) => Err(e),
            Poll::Ready(None) => Ok(Async::Ready(None)),
            Poll::Pending => Ok(Async::NotReady),
        }
    }
}

fn headermap_compat(headers: http::HeaderMap) -> http_01::HeaderMap {
    // XXX: this far from efficient, but hopefully this code will go away soon.
    headers
        .drain()
        .map(|(k, v)| {
            let name = http_01::header::HeaderName::from_bytes(k.as_ref()).unwrap();
            let value = http_01::HeaderValue::from_bytes(v.as_ref()).unwrap();
            (name, value)
        })
        .collect()
}

fn headermap_compat_01(headers: http_01::HeaderMap) -> http::HeaderMap {
    // XXX: this far from efficient, but hopefully this code will go away soon.
    headers
        .drain()
        .map(|(k, v)| {
            let name = http::header::HeaderName::from_bytes(k.as_ref()).unwrap();
            let value = http1::HeaderValue::from_bytes(v.as_ref()).unwrap();
            (name, value)
        })
        .collect()
}
