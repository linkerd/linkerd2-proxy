use h2;
use http;
use indexmap::IndexMap;
use std::time::Instant;

use proxy::{http::client, Source};

// TODO this should be replaced with a string name.
#[derive(Copy, Clone, Debug)]
pub enum Direction { In, Out }

#[derive(Clone, Debug)]
pub struct Endpoint {
    pub direction: Direction,
    pub client: client::Config,
    pub labels: IndexMap<String, String>,
}

#[derive(Clone, Debug)]
pub struct Request {
    pub id: usize,
    pub source: Source,
    pub endpoint: Endpoint,
    pub method: http::Method,
    pub scheme: Option<http::uri::Scheme>,
    pub authority: Option<http::uri::Authority>,
    pub path: String,
}

#[derive(Clone, Debug)]
pub struct Response {
    pub request: Request,
    pub status: http::StatusCode,
}

#[derive(Clone, Debug)]
pub enum Event {
    StreamRequestOpen(Request),
    StreamRequestFail(Request, StreamRequestFail),
    StreamRequestEnd(Request, StreamRequestEnd),

    StreamResponseOpen(Response, StreamResponseOpen),
    StreamResponseFail(Response, StreamResponseFail),
    StreamResponseEnd(Response, StreamResponseEnd),
}

#[derive(Clone, Debug)]
pub struct StreamRequestFail {
    pub request_open_at: Instant,
    pub request_fail_at: Instant,
    pub error: h2::Reason,
}

#[derive(Clone, Debug)]
pub struct StreamRequestEnd {
    pub request_open_at: Instant,
    pub request_end_at: Instant,
}

#[derive(Clone, Debug)]
pub struct StreamResponseOpen {
    pub request_open_at: Instant,
    pub response_open_at: Instant,
}

#[derive(Clone, Debug)]
pub struct StreamResponseFail {
    pub request_open_at: Instant,
    pub response_open_at: Instant,
    pub response_first_frame_at: Option<Instant>,
    pub response_fail_at: Instant,
    pub error: h2::Reason,
    pub bytes_sent: u64,
}

#[derive(Clone, Debug)]
pub struct StreamResponseEnd {
    pub request_open_at: Instant,
    pub response_open_at: Instant,
    pub response_first_frame_at: Instant,
    pub response_end_at: Instant,
    pub grpc_status: Option<u32>,
    pub bytes_sent: u64,
}
