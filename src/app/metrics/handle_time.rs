use proxy::http::metrics::handle_time;

#[derive(Clone, Debug)]
pub struct Scopes {
    inbound: handle_time::Scope,
    outbound: handle_time::Scope,
}
