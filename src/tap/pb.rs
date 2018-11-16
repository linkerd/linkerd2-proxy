#![cfg_attr(feature = "cargo-clippy", allow(clippy))]

use std::error::Error;
use std::fmt;

use api::*;
use convert::*;
use proxy;
use tap::event::{self, Event};

#[derive(Debug, Clone)]
pub struct UnknownEvent;

impl fmt::Display for UnknownEvent {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "unknown tap event")
    }
}

impl Error for UnknownEvent {
    #[inline]
    fn description(&self) -> &str {
        "unknown tap event"
    }
}

impl event::StreamResponseEnd {
    fn to_tap_event(&self, ctx: &event::Request) -> tap::TapEvent {
        let eos = self.grpc_status.map(tap::Eos::from_grpc_status);

        let end = tap::tap_event::http::ResponseEnd {
            id: Some(tap::tap_event::http::StreamId {
                base: 0, // TODO FIXME
                stream: ctx.id as u64,
            }),
            since_request_init: Some(pb_elapsed(self.request_open_at, self.response_end_at)),
            since_response_init: Some(pb_elapsed(self.response_open_at, self.response_end_at)),
            response_bytes: self.bytes_sent,
            eos,
        };

        tap::TapEvent {
            proxy_direction: ctx.endpoint.direction.as_pb().into(),
            source: Some((&ctx.source.remote).into()),
            source_meta: Some(ctx.source.src_meta()),
            destination: Some((&ctx.endpoint.target.addr).into()),
            destination_meta: Some(ctx.endpoint.dst_meta()),
            event: Some(tap::tap_event::Event::Http(tap::tap_event::Http {
                event: Some(tap::tap_event::http::Event::ResponseEnd(end)),
            })),
        }
    }
}

impl event::StreamResponseFail {
    fn to_tap_event(&self, ctx: &event::Request) -> tap::TapEvent {
        let end = tap::tap_event::http::ResponseEnd {
            id: Some(tap::tap_event::http::StreamId {
                base: 0, // TODO FIXME
                stream: ctx.id as u64,
            }),
            since_request_init: Some(pb_elapsed(self.request_open_at, self.response_fail_at)),
            since_response_init: Some(pb_elapsed(self.response_open_at, self.response_fail_at)),
            response_bytes: self.bytes_sent,
            eos: Some(self.error.into()),
        };

        tap::TapEvent {
            proxy_direction: ctx.endpoint.direction.as_pb().into(),
            source: Some((&ctx.source.remote).into()),
            source_meta: Some(ctx.source.src_meta()),
            destination: Some((&ctx.endpoint.target.addr).into()),
            destination_meta: Some(ctx.endpoint.dst_meta()),
            event: Some(tap::tap_event::Event::Http(tap::tap_event::Http {
                event: Some(tap::tap_event::http::Event::ResponseEnd(end)),
            })),
        }
    }
}

impl event::StreamRequestFail {
    fn to_tap_event(&self, ctx: &event::Request) -> tap::TapEvent {
        let end = tap::tap_event::http::ResponseEnd {
            id: Some(tap::tap_event::http::StreamId {
                base: 0, // TODO FIXME
                stream: ctx.id as u64,
            }),
            since_request_init: Some(pb_elapsed(self.request_open_at, self.request_fail_at)),
            since_response_init: None,
            response_bytes: 0,
            eos: Some(self.error.into()),
        };

        tap::TapEvent {
            proxy_direction: ctx.endpoint.direction.as_pb().into(),
            source: Some((&ctx.source.remote).into()),
            source_meta: Some(ctx.source.src_meta()),
            destination: Some((&ctx.endpoint.target.addr).into()),
            destination_meta: Some(ctx.endpoint.dst_meta()),
            event: Some(tap::tap_event::Event::Http(tap::tap_event::Http {
                event: Some(tap::tap_event::http::Event::ResponseEnd(end)),
            })),
        }
    }
}

impl<'a> TryFrom<&'a Event> for tap::TapEvent {
    type Err = UnknownEvent;
    fn try_from(ev: &'a Event) -> Result<Self, Self::Err> {
        let tap_ev = match *ev {
            Event::StreamRequestOpen(ref ctx) => {
                let init = tap::tap_event::http::RequestInit {
                    id: Some(tap::tap_event::http::StreamId {
                        base: 0,
                        // TODO FIXME
                        stream: ctx.id as u64,
                    }),
                    method: Some((&ctx.method).into()),
                    scheme: ctx.scheme.as_ref().map(http_types::Scheme::from),
                    authority: ctx
                        .authority
                        .as_ref()
                        .map(|a| a.as_str())
                        .unwrap_or_default()
                        .into(),
                    path: ctx.path.clone(),
                };

                tap::TapEvent {
                    proxy_direction: ctx.endpoint.direction.as_pb().into(),
                    source: Some((&ctx.source.remote).into()),
                    source_meta: Some(ctx.source.src_meta()),
                    destination: Some((&ctx.endpoint.target.addr).into()),
                    destination_meta: Some(ctx.endpoint.dst_meta()),
                    event: Some(tap::tap_event::Event::Http(tap::tap_event::Http {
                        event: Some(tap::tap_event::http::Event::RequestInit(init)),
                    })),
                }
            }

            Event::StreamResponseOpen(ref ctx, ref rsp) => {
                let init = tap::tap_event::http::ResponseInit {
                    id: Some(tap::tap_event::http::StreamId {
                        base: 0,
                        // TODO FIXME
                        stream: ctx.request.id as u64,
                    }),
                    since_request_init: Some(pb_elapsed(rsp.request_open_at, rsp.response_open_at)),
                    http_status: u32::from(ctx.status.as_u16()),
                };

                tap::TapEvent {
                    proxy_direction: ctx.request.endpoint.direction.as_pb().into(),
                    source: Some((&ctx.request.source.remote).into()),
                    source_meta: Some(ctx.request.source.src_meta()),
                    destination: Some((&ctx.request.endpoint.target.addr).into()),
                    destination_meta: Some(ctx.request.endpoint.dst_meta()),
                    event: Some(tap::tap_event::Event::Http(tap::tap_event::Http {
                        event: Some(tap::tap_event::http::Event::ResponseInit(init)),
                    })),
                }
            }

            Event::StreamRequestFail(ref ctx, ref fail) => fail.to_tap_event(&ctx),

            Event::StreamResponseEnd(ref ctx, ref end) => end.to_tap_event(&ctx.request),

            Event::StreamResponseFail(ref ctx, ref fail) => fail.to_tap_event(&ctx.request),

            _ => return Err(UnknownEvent),
        };

        Ok(tap_ev)
    }
}

impl proxy::Source {
    fn src_meta(&self) -> tap::tap_event::EndpointMeta {
        let mut meta = tap::tap_event::EndpointMeta::default();

        meta.labels
            .insert("tls".to_owned(), format!("{}", self.tls_status));

        meta
    }
}

impl event::Direction {
    fn as_pb(&self) -> tap::tap_event::ProxyDirection {
        match self {
            event::Direction::Out => tap::tap_event::ProxyDirection::Outbound,
            event::Direction::In => tap::tap_event::ProxyDirection::Inbound,
        }
    }
}

impl event::Endpoint {
    fn dst_meta(&self) -> tap::tap_event::EndpointMeta {
        let mut meta = tap::tap_event::EndpointMeta::default();
        meta.labels.extend(self.labels.clone());
        meta.labels
            .insert("tls".to_owned(), format!("{}", self.target.tls_status()));
        meta
    }
}
