use linkerd2_addr as addr;
use linkerd2_dns_name as dns;
use linkerd2_identity as identity;
use linkerd2_never::Never;
use linkerd2_proxy_api as api;
use linkerd2_proxy_core as core;
use linkerd2_task as task;

mod destination;
mod remote_stream;

pub use self::destination::{Resolver, Metadata, ProtocolHint, Unresolvable};
