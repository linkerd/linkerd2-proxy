//! Records and serves Prometheus metrics.
//!
//! # A note on label formatting
//!
//! Prometheus labels are represented as a comma-separated list of values
//! Since the proxy labels its metrics with a fixed set of labels
//! which we know in advance, we represent these labels using a number of
//! `struct`s, all of which implement `fmt::Display`. Some of the label
//! `struct`s contain other structs which represent a subset of the labels
//! which can be present on metrics in that scope. In this case, the
//! `fmt::Display` impls for those structs call the `fmt::Display` impls for
//! the structs that they own. This has the potential to complicate the
//! insertion of commas to separate label values.
//!
//! In order to ensure that commas are added correctly to separate labels,
//! we expect the `fmt::Display` implementations for label types to behave in
//! a consistent way: A label struct is *never* responsible for printing
//! leading or trailing commas before or after the label values it contains.
//! If it contains multiple labels, it *is* responsible for ensuring any
//! labels it owns are comma-separated. This way, the `fmt::Display` impl for
//! any struct that represents a subset of the labels are position-agnostic;
//! they don't need to know if there are other labels before or after them in
//! the formatted output. The owner is responsible for managing that.
//!
//! If this rule is followed consistently across all structs representing
//! labels, we can add new labels or modify the existing ones without having
//! to worry about missing commas, double commas, or trailing commas at the
//! end of the label set (all of which will make Prometheus angry).
use std::default::Default;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

mod counter;
mod gauge;
mod histogram;
mod http;
mod labels;
pub mod latency;
pub mod prom;
mod record;
mod scopes;
mod serve;
mod transport;

pub use self::counter::Counter;
pub use self::gauge::Gauge;
pub use self::histogram::Histogram;
use self::labels::{
    RequestLabels,
    ResponseLabels,
};
pub use self::prom::{FmtMetrics, FmtLabels, FmtMetric};
pub use self::record::Record;
pub use self::scopes::Scopes;
pub use self::serve::Serve;
pub use self::transport::Transports;
use super::process;
use super::tls_config_reload;

/// The root scope for all runtime metrics.
#[derive(Debug, Default)]
struct Root {
    requests: http::RequestScopes,
    responses: http::ResponseScopes,
    transports: Transports,
    tls_config_reload: tls_config_reload::Report,
    process: process::Report,
}

#[derive(Debug)]
struct Stamped<T> {
    stamp: Instant,
    inner: T,
}

/// Construct the Prometheus metrics.
///
/// Returns the `Record` and `Serve` sides. The `Serve` side
/// is a Hyper service which can be used to create the server for the
/// scrape endpoint, while the `Record` side can receive updates to the
/// metrics by calling `record_event`.
pub fn new(start_time: SystemTime, idle_retain: Duration, tls: tls_config_reload::Report)
    -> (Record, Serve)
{
    let metrics = Arc::new(Mutex::new(Root::new(start_time, tls)));
    (Record::new(&metrics), Serve::new(&metrics, idle_retain))
}

// ===== impl Root =====

impl Root {
    pub fn new(start_time: SystemTime, tls_config_reload: tls_config_reload::Report) -> Self {
        Self {
            process: process::Report::new(start_time),
            tls_config_reload,
            .. Root::default()
        }
    }

    fn request(&mut self, labels: RequestLabels) -> &mut http::RequestMetrics {
        self.requests.get_or_default(labels).stamped()
    }

    fn response(&mut self, labels: ResponseLabels) -> &mut http::ResponseMetrics {
        self.responses.get_or_default(labels).stamped()
    }

    fn transports(&mut self) -> &mut Transports {
        &mut self.transports
    }

    fn retain_since(&mut self, epoch: Instant) {
        self.requests.retain(|_, v| v.stamp >= epoch);
        self.responses.retain(|_, v| v.stamp >= epoch);
    }
}

impl FmtMetrics for Root {
    fn fmt_metrics(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.requests.fmt_metrics(f)?;
        self.responses.fmt_metrics(f)?;
        self.transports.fmt_metrics(f)?;
        self.tls_config_reload.fmt_metrics(f)?;
        self.process.fmt_metrics(f)?;

        Ok(())
    }
}

// ===== impl Stamped =====

impl<T> Stamped<T> {
    fn stamped(&mut self) -> &mut T {
        self.stamp = Instant::now();
        &mut self.inner
    }
}

impl<T: Default> Default for Stamped<T> {
    fn default() -> Self {
        T::default().into()
    }
}

impl<T> From<T> for Stamped<T> {
    fn from(inner: T) -> Self {
        Self {
            inner,
            stamp: Instant::now(),
        }
    }
}

impl<T> ::std::ops::Deref for Stamped<T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

#[cfg(test)]
mod tests {
    use ctx;
    use ctx::test_util::*;
    use super::*;
    use conditional::Conditional;
    use tls;

    const TLS_DISABLED: Conditional<(), tls::ReasonForNoTls> =
        Conditional::None(tls::ReasonForNoTls::Disabled);

    fn mock_route(
        root: &mut Root,
        proxy: ctx::Proxy,
        server: &Arc<ctx::transport::Server>,
        team: &str
    ) {
        let client = client(proxy, indexmap!["team".into() => team.into(),], TLS_DISABLED);
        let (req, rsp) = request("http://nba.com", &server, &client);

        let client_transport = Arc::new(ctx::transport::Ctx::Client(client));
        root.transports.open(&client_transport);

        root.request(RequestLabels::new(&req)).end();
        root.response(ResponseLabels::new(&rsp, None)).end(Duration::from_millis(10));

        let duration = Duration::from_millis(15);
        root.transports().close(&client_transport, transport::Eos::Clean, duration, 100, 200);
   }

    #[test]
    fn expiry() {
        let proxy = ctx::Proxy::Outbound;

        let server = server(proxy, TLS_DISABLED);
        let server_transport = Arc::new(ctx::transport::Ctx::Server(server.clone()));

        let mut root = Root::default();

        let t0 = Instant::now();
        root.transports().open(&server_transport);

        mock_route(&mut root, proxy, &server, "warriors");
        let t1 = Instant::now();

        mock_route(&mut root, proxy, &server, "sixers");
        let t2 = Instant::now();

        assert_eq!(root.requests.len(), 2);
        assert_eq!(root.responses.len(), 2);

        root.retain_since(t0);
        assert_eq!(root.requests.len(), 2);
        assert_eq!(root.responses.len(), 2);

        root.retain_since(t1);
        assert_eq!(root.requests.len(), 1);
        assert_eq!(root.responses.len(), 1);

        root.retain_since(t2);
        assert_eq!(root.requests.len(), 0);
        assert_eq!(root.responses.len(), 0);
    }
}
