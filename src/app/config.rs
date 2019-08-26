use super::control::ControlAddr;
use super::identity;
use crate::addr::{self, Addr};
use crate::proxy::reconnect::Backoff;
use crate::transport::tls;
use crate::{dns, Conditional};
use indexmap::IndexSet;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::iter::FromIterator;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;
use std::{fmt, fs};
use tracing::{error, warn};

const INBOUND_CONNECT_BASE: &str = "INBOUND_CONNECT";
const OUTBOUND_CONNECT_BASE: &str = "OUTBOUND_CONNECT";
pub const CONTROL_BASE: &str = "CONTROL";

/// Tracks all configuration settings for the process.
#[derive(Debug)]
pub struct Config {
    /// Where to listen for connections that are initiated on the host.
    pub outbound_listener: Listener,

    /// Where to listen for connections initiated by external sources.
    pub inbound_listener: Listener,

    /// Where to listen for connections initiated by the tap service.
    pub control_listener: Option<TapSettings>,

    /// Where to serve admin HTTP.
    pub admin_listener: Listener,

    /// Where to forward externally received connections.
    pub inbound_forward: Option<SocketAddr>,

    /// The maximum amount of time that an inbound request can spend buffered in the inbound proxy.
    pub inbound_dispatch_timeout: Duration,

    /// The maximum amount of time that an inbound request can spend buffered in the outbound proxy.
    pub outbound_dispatch_timeout: Duration,

    /// The maximum amount of time that an inbound request can be buffered.
    pub control_dispatch_timeout: Duration,

    /// The maximum amount of time to wait for a connection to a local peer.
    pub inbound_connect_timeout: Duration,

    /// The maximum amount of time to wait for a connection to a remote peer.
    pub outbound_connect_timeout: Duration,

    /// Settings for the back-off used to determine the amount of time to wait
    /// between inbound connection attempts.
    pub inbound_connect_backoff: Backoff,

    /// Settings for the back-off used to determine the amount of time to wait
    /// between outbound connection attempts.
    pub outbound_connect_backoff: Backoff,

    // TCP Keepalive set on accepted inbound connections.
    pub inbound_accept_keepalive: Option<Duration>,

    // TCP Keepalive set on accepted outbound connections.
    pub outbound_accept_keepalive: Option<Duration>,

    // TCP Keepalive set on inbound connections to the local application.
    pub inbound_connect_keepalive: Option<Duration>,

    // TCP Keepalive set on outbound connections to the remote peers.
    pub outbound_connect_keepalive: Option<Duration>,

    pub inbound_ports_disable_protocol_detection: IndexSet<u16>,

    pub outbound_ports_disable_protocol_detection: IndexSet<u16>,

    pub inbound_router_capacity: usize,

    pub outbound_router_capacity: usize,

    pub inbound_router_max_idle_age: Duration,

    pub outbound_router_max_idle_age: Duration,

    pub inbound_max_requests_in_flight: usize,

    pub outbound_max_requests_in_flight: usize,

    /// Age after which metrics may be dropped.
    pub metrics_retain_idle: Duration,

    /// Settings for the back-off used to determine the amount of time to wait
    /// between when encountering errors talking to control plane before
    /// a new connection is attempted.
    pub control_backoff: Backoff,

    /// The maximum amount of time to wait for a connection to the controller.
    pub control_connect_timeout: Duration,

    pub identity_config: tls::Conditional<identity::Config>,

    //
    // Destination Config
    //
    /// Where to talk to the control plane.
    pub destination_addr: ControlAddr,

    /// The maximum number of queries to the Destination service which may be
    /// active concurrently.
    pub destination_buffer_capacity: usize,

    /// Configured by `ENV_DESTINATION_GET_SUFFIXES`.
    pub destination_get_suffixes: Vec<dns::Suffix>,

    /// Configured by `ENV_DESTINATION_PROFILE_SUFFIXES`.
    pub destination_profile_suffixes: Vec<dns::Suffix>,

    /// This token is passed to the Destination service so that it can return
    /// different results depending on the identity of the proxy making the
    /// call.
    pub destination_context: String,

    //
    // DNS Config
    //
    /// The path to "/etc/resolv.conf"
    pub resolv_conf_path: PathBuf,

    /// Optional minimum TTL for DNS lookups.
    pub dns_min_ttl: Option<Duration>,

    /// Optional maximum TTL for DNS lookups.
    pub dns_max_ttl: Option<Duration>,

    pub dns_canonicalize_timeout: Duration,

    pub h2_settings: H2Settings,
}

#[derive(Copy, Clone, Debug, Default)]
pub struct H2Settings {
    pub initial_stream_window_size: Option<u32>,
    pub initial_connection_window_size: Option<u32>,
}

/// Configuration settings for the tap server
#[derive(Debug)]
pub struct TapSettings {
    pub listener: Listener,
    pub tap_svc_name: identity::Name,
}

/// Configuration settings for binding a listener.
///
/// TODO: Rename this to be more inline with the actual types.
#[derive(Clone, Debug)]
pub struct Listener {
    /// The address to which the listener should bind.
    pub addr: SocketAddr,
}

/// Errors produced when loading a `Config` struct.
#[derive(Clone, Debug)]
pub enum Error {
    InvalidEnvVar,
    NoDestinationAddress,
}

#[derive(Debug, Eq, PartialEq)]
pub enum ParseError {
    EnvironmentUnsupported,
    NotADuration,
    NotADomainSuffix,
    NotANumber,
    HostIsNotAnIpAddress,
    NotUnicode,
    AddrError(addr::Error),
    NameError,
    InvalidTokenSource,
    InvalidTrustAnchors,
}

/// The strings used to build a configuration.
pub trait Strings {
    /// Retrieves the value for the key `key`.
    ///
    /// `key` must be one of the `ENV_` values below.
    fn get(&self, key: &str) -> Result<Option<String>, Error>;
}

/// An implementation of `Strings` that reads the values from environment variables.
pub struct Env;

#[derive(Clone)]
pub struct TestEnv {
    values: HashMap<&'static str, String>,
}

// Environment variables to look at when loading the configuration
pub const ENV_OUTBOUND_LISTEN_ADDR: &str = "LINKERD2_PROXY_OUTBOUND_LISTEN_ADDR";
pub const ENV_INBOUND_FORWARD: &str = "LINKERD2_PROXY_INBOUND_FORWARD";
pub const ENV_INBOUND_LISTEN_ADDR: &str = "LINKERD2_PROXY_INBOUND_LISTEN_ADDR";
pub const ENV_CONTROL_LISTEN_ADDR: &str = "LINKERD2_PROXY_CONTROL_LISTEN_ADDR";
pub const ENV_ADMIN_LISTEN_ADDR: &str = "LINKERD2_PROXY_ADMIN_LISTEN_ADDR";
pub const ENV_METRICS_RETAIN_IDLE: &str = "LINKERD2_PROXY_METRICS_RETAIN_IDLE";
const ENV_INBOUND_DISPATCH_TIMEOUT: &str = "LINKERD2_PROXY_INBOUND_DISPATCH_TIMEOUT";
const ENV_OUTBOUND_DISPATCH_TIMEOUT: &str = "LINKERD2_PROXY_OUTBOUND_DISPATCH_TIMEOUT";
const ENV_INBOUND_CONNECT_TIMEOUT: &str = "LINKERD2_PROXY_INBOUND_CONNECT_TIMEOUT";
const ENV_OUTBOUND_CONNECT_TIMEOUT: &str = "LINKERD2_PROXY_OUTBOUND_CONNECT_TIMEOUT";
const ENV_INBOUND_ACCEPT_KEEPALIVE: &str = "LINKERD2_PROXY_INBOUND_ACCEPT_KEEPALIVE";
const ENV_OUTBOUND_ACCEPT_KEEPALIVE: &str = "LINKERD2_PROXY_OUTBOUND_ACCEPT_KEEPALIVE";

const ENV_INBOUND_CONNECT_KEEPALIVE: &str = "LINKERD2_PROXY_INBOUND_CONNECT_KEEPALIVE";
const ENV_OUTBOUND_CONNECT_KEEPALIVE: &str = "LINKERD2_PROXY_OUTBOUND_CONNECT_KEEPALIVE";

pub const DEPRECATED_ENV_PRIVATE_LISTEN_ADDR: &str = "LINKERD2_PROXY_PRIVATE_LISTEN_ADDR";
pub const DEPRECATED_ENV_PRIVATE_FORWARD: &str = "LINKERD2_PROXY_PRIVATE_FORWARD";

// Limits the number of HTTP routes that may be active in the proxy at any time. There is
// an inbound route for each local port that receives connections. There is an outbound
// route for each protocol and authority.
pub const ENV_INBOUND_ROUTER_CAPACITY: &str = "LINKERD2_PROXY_INBOUND_ROUTER_CAPACITY";
pub const ENV_OUTBOUND_ROUTER_CAPACITY: &str = "LINKERD2_PROXY_OUTBOUND_ROUTER_CAPACITY";

pub const ENV_INBOUND_ROUTER_MAX_IDLE_AGE: &str = "LINKERD2_PROXY_INBOUND_ROUTER_MAX_IDLE_AGE";
pub const ENV_OUTBOUND_ROUTER_MAX_IDLE_AGE: &str = "LINKERD2_PROXY_OUTBOUND_ROUTER_MAX_IDLE_AGE";

pub const ENV_INBOUND_MAX_IN_FLIGHT: &str = "LINKERD2_PROXY_INBOUND_MAX_IN_FLIGHT";
pub const ENV_OUTBOUND_MAX_IN_FLIGHT: &str = "LINKERD2_PROXY_OUTBOUND_MAX_IN_FLIGHT";

/// Constrains which destination names are resolved through the destination
/// service.
///
/// The value is a comma-separated list of domain name suffixes that may be
/// resolved via the destination service. A value of `.` indicates that all
/// domains should be resolved via the service.
///
/// If specified and empty, the destination service is not used for resolution.
///
/// If unspecified, a default value is used.
pub const ENV_DESTINATION_GET_SUFFIXES: &str = "LINKERD2_PROXY_DESTINATION_GET_SUFFIXES";

/// Constrains which destination names may be used for profile/route discovery.
///
/// The value is a comma-separated list of domain name suffixes that may be
/// resolved via the destination service. A value of `.` indicates that all
/// domains should be discovered via the service.
///
/// If specified and empty, the destination service is not used for route discovery.
///
/// If unspecified, a default value is used.
pub const ENV_DESTINATION_PROFILE_SUFFIXES: &str = "LINKERD2_PROXY_DESTINATION_PROFILE_SUFFIXES";

// These *disable* our protocol detection for connections whose SO_ORIGINAL_DST
// has a port in the provided list.
pub const ENV_INBOUND_PORTS_DISABLE_PROTOCOL_DETECTION: &str =
    "LINKERD2_PROXY_INBOUND_PORTS_DISABLE_PROTOCOL_DETECTION";
pub const ENV_OUTBOUND_PORTS_DISABLE_PROTOCOL_DETECTION: &str =
    "LINKERD2_PROXY_OUTBOUND_PORTS_DISABLE_PROTOCOL_DETECTION";

pub const ENV_IDENTITY_DISABLED: &str = "LINKERD2_PROXY_IDENTITY_DISABLED";
pub const ENV_IDENTITY_DIR: &str = "LINKERD2_PROXY_IDENTITY_DIR";
pub const ENV_IDENTITY_TRUST_ANCHORS: &str = "LINKERD2_PROXY_IDENTITY_TRUST_ANCHORS";
pub const ENV_IDENTITY_IDENTITY_LOCAL_NAME: &str = "LINKERD2_PROXY_IDENTITY_LOCAL_NAME";
pub const ENV_IDENTITY_TOKEN_FILE: &str = "LINKERD2_PROXY_IDENTITY_TOKEN_FILE";
pub const ENV_IDENTITY_MIN_REFRESH: &str = "LINKERD2_PROXY_IDENTITY_MIN_REFRESH";
pub const ENV_IDENTITY_MAX_REFRESH: &str = "LINKERD2_PROXY_IDENTITY_MAX_REFRESH";

pub const ENV_IDENTITY_SVC_BASE: &str = "LINKERD2_PROXY_IDENTITY_SVC";

pub const ENV_DESTINATION_SVC_BASE: &str = "LINKERD2_PROXY_DESTINATION_SVC";
pub const ENV_DESTINATION_SVC_ADDR: &str = "LINKERD2_PROXY_DESTINATION_SVC_ADDR";

pub const ENV_DESTINATION_CONTEXT: &str = "LINKERD2_PROXY_DESTINATION_CONTEXT";

pub const ENV_CONTROL_EXP_BACKOFF_MIN: &str = "LINKERD2_PROXY_CONTROL_EXP_BACKOFF_MIN";
pub const ENV_CONTROL_EXP_BACKOFF_MAX: &str = "LINKERD2_PROXY_CONTROL_EXP_BACKOFF_MAX";
pub const ENV_CONTROL_EXP_BACKOFF_JITTER: &str = "LINKERD2_PROXY_CONTROL_EXP_BACKOFF_JITTER";
pub const ENV_TAP_DISABLED: &str = "LINKERD2_PROXY_TAP_DISABLED";
pub const ENV_TAP_SVC_NAME: &str = "LINKERD2_PROXY_TAP_SVC_NAME";
const ENV_CONTROL_CONNECT_TIMEOUT: &str = "LINKERD2_PROXY_CONTROL_CONNECT_TIMEOUT";
const ENV_CONTROL_DISPATCH_TIMEOUT: &str = "LINKERD2_PROXY_CONTROL_DISPATCH_TIMEOUT";
const ENV_RESOLV_CONF: &str = "LINKERD2_PROXY_RESOLV_CONF";

/// Configures a minimum value for the TTL of DNS lookups.
///
/// Lookups with TTLs below this value will use this value instead.
const ENV_DNS_MIN_TTL: &str = "LINKERD2_PROXY_DNS_MIN_TTL";
/// Configures a maximum value for the TTL of DNS lookups.
///
/// Lookups with TTLs above this value will use this value instead.
const ENV_DNS_MAX_TTL: &str = "LINKERD2_PROXY_DNS_MAX_TTL";

/// The amount of time to wait for a DNS query to succeed before falling back to
/// an uncanonicalized address.
const ENV_DNS_CANONICALIZE_TIMEOUT: &str = "LINKERD2_PROXY_DNS_CANONICALIZE_TIMEOUT";

/// Configure the stream or connection level flow control setting for HTTP2.
///
/// If unspecified, the default value of 65,535 is used.
const ENV_INITIAL_STREAM_WINDOW_SIZE: &str = "LINKERD2_PROXY_HTTP2_INITIAL_STREAM_WINDOW_SIZE";
const ENV_INITIAL_CONNECTION_WINDOW_SIZE: &str =
    "LINKERD2_PROXY_HTTP2_INITIAL_CONNECTION_WINDOW_SIZE";

// Default values for various configuration fields
const DEFAULT_OUTBOUND_LISTEN_ADDR: &str = "127.0.0.1:4140";
const DEFAULT_INBOUND_LISTEN_ADDR: &str = "0.0.0.0:4143";
const DEFAULT_CONTROL_LISTEN_ADDR: &str = "0.0.0.0:4190";
const DEFAULT_ADMIN_LISTEN_ADDR: &str = "127.0.0.1:4191";
const DEFAULT_METRICS_RETAIN_IDLE: Duration = Duration::from_secs(10 * 60);
const DEFAULT_INBOUND_DISPATCH_TIMEOUT: Duration = Duration::from_secs(1);
const DEFAULT_INBOUND_CONNECT_TIMEOUT: Duration = Duration::from_millis(100);
const DEFAULT_INBOUND_CONNECT_BACKOFF: Backoff = Backoff::Exponential {
    min: Duration::from_millis(100),
    max: Duration::from_millis(500),
    jitter: 0.1,
};
const DEFAULT_OUTBOUND_DISPATCH_TIMEOUT: Duration = Duration::from_secs(3);
const DEFAULT_OUTBOUND_CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
const DEFAULT_OUTBOUND_CONNECT_BACKOFF: Backoff = Backoff::Exponential {
    min: Duration::from_millis(100),
    max: Duration::from_millis(500),
    jitter: 0.1,
};
const DEFAULT_CONTROL_DISPATCH_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_CONTROL_CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
const DEFAULT_CONTROL_BACKOFF: Backoff = Backoff::Exponential {
    min: Duration::from_secs(1),
    max: Duration::from_secs(5),
    jitter: 0.1,
};
const DEFAULT_DNS_CANONICALIZE_TIMEOUT: Duration = Duration::from_millis(100);
const DEFAULT_RESOLV_CONF: &str = "/etc/resolv.conf";

/// It's assumed that a typical proxy can serve inbound traffic for up to 100 pod-local
/// HTTP services and may communicate with up to 10K external HTTP domains.
const DEFAULT_INBOUND_ROUTER_CAPACITY: usize = 100;
const DEFAULT_OUTBOUND_ROUTER_CAPACITY: usize = 10_000;

const DEFAULT_INBOUND_ROUTER_MAX_IDLE_AGE: Duration = Duration::from_secs(60);
const DEFAULT_OUTBOUND_ROUTER_MAX_IDLE_AGE: Duration = Duration::from_secs(60);

// 10_000 is arbitrarily chosen for now...
const DEFAULT_INBOUND_MAX_IN_FLIGHT: usize = 10_000;
const DEFAULT_OUTBOUND_MAX_IN_FLIGHT: usize = 10_000;

const DEFAULT_DESTINATION_BUFFER_CAPACITY: usize = 100;

const DEFAULT_DESTINATION_GET_SUFFIXES: &str = "svc.cluster.local.";
const DEFAULT_DESTINATION_PROFILE_SUFFIXES: &str = "svc.cluster.local.";

const DEFAULT_IDENTITY_MIN_REFRESH: Duration = Duration::from_secs(10);
const DEFAULT_IDENTITY_MAX_REFRESH: Duration = Duration::from_secs(60 * 60 * 24);

// By default, we keep a list of known assigned ports of server-first protocols.
//
// https://www.iana.org/assignments/service-names-port-numbers/service-names-port-numbers.txt
const DEFAULT_PORTS_DISABLE_PROTOCOL_DETECTION: &[u16] = &[
    25,   // SMTP
    3306, // MySQL
];

// ===== impl Config =====

impl dns::ConfigureResolver for Config {
    /// Modify a `trust-dns-resolver::config::ResolverOpts` to reflect
    /// the configured minimum and maximum DNS TTL values.
    fn configure_resolver(&self, opts: &mut dns::ResolverOpts) {
        opts.positive_min_ttl = self.dns_min_ttl;
        opts.positive_max_ttl = self.dns_max_ttl;
        // TODO: Do we want to allow the positive and negative TTLs to be
        //       configured separately?
        opts.negative_min_ttl = self.dns_min_ttl;
        opts.negative_max_ttl = self.dns_max_ttl;
    }
}

impl Config {
    /// Load a `Config` by reading ENV variables.
    pub fn parse<S: Strings>(strings: &S) -> Result<Self, Error> {
        // Parse all the environment variables. `parse` will log any errors so
        // defer returning any errors until all of them have been parsed.
        let outbound_listener_addr = parse(strings, ENV_OUTBOUND_LISTEN_ADDR, parse_socket_addr);
        let inbound_listener_addr = parse(strings, ENV_INBOUND_LISTEN_ADDR, parse_socket_addr);
        let admin_listener_addr = parse(strings, ENV_ADMIN_LISTEN_ADDR, parse_socket_addr);
        let inbound_forward = parse(strings, ENV_INBOUND_FORWARD, parse_socket_addr);

        let inbound_dispatch_timeout = parse(strings, ENV_INBOUND_DISPATCH_TIMEOUT, parse_duration);
        let inbound_connect_timeout = parse(strings, ENV_INBOUND_CONNECT_TIMEOUT, parse_duration);

        let outbound_dispatch_timeout =
            parse(strings, ENV_OUTBOUND_DISPATCH_TIMEOUT, parse_duration);
        let outbound_connect_timeout = parse(strings, ENV_OUTBOUND_CONNECT_TIMEOUT, parse_duration);

        let control_dispatch_timeout = parse(strings, ENV_CONTROL_DISPATCH_TIMEOUT, parse_duration);
        let control_connect_timeout = parse(strings, ENV_CONTROL_CONNECT_TIMEOUT, parse_duration);

        let inbound_accept_keepalive = parse(strings, ENV_INBOUND_ACCEPT_KEEPALIVE, parse_duration);
        let outbound_accept_keepalive =
            parse(strings, ENV_OUTBOUND_ACCEPT_KEEPALIVE, parse_duration);

        let inbound_connect_keepalive =
            parse(strings, ENV_INBOUND_CONNECT_KEEPALIVE, parse_duration);
        let outbound_connect_keepalive =
            parse(strings, ENV_OUTBOUND_CONNECT_KEEPALIVE, parse_duration);

        let inbound_disable_ports = parse(
            strings,
            ENV_INBOUND_PORTS_DISABLE_PROTOCOL_DETECTION,
            parse_port_set,
        );
        let outbound_disable_ports = parse(
            strings,
            ENV_OUTBOUND_PORTS_DISABLE_PROTOCOL_DETECTION,
            parse_port_set,
        );

        let inbound_router_capacity = parse(strings, ENV_INBOUND_ROUTER_CAPACITY, parse_number);
        let outbound_router_capacity = parse(strings, ENV_OUTBOUND_ROUTER_CAPACITY, parse_number);

        let inbound_router_max_idle_age =
            parse(strings, ENV_INBOUND_ROUTER_MAX_IDLE_AGE, parse_duration);
        let outbound_router_max_idle_age =
            parse(strings, ENV_OUTBOUND_ROUTER_MAX_IDLE_AGE, parse_duration);

        let inbound_max_in_flight = parse(strings, ENV_INBOUND_MAX_IN_FLIGHT, parse_number);
        let outbound_max_in_flight = parse(strings, ENV_OUTBOUND_MAX_IN_FLIGHT, parse_number);

        let metrics_retain_idle = parse(strings, ENV_METRICS_RETAIN_IDLE, parse_duration);

        // DNS

        let resolv_conf_path = strings.get(ENV_RESOLV_CONF);

        let dns_min_ttl = parse(strings, ENV_DNS_MIN_TTL, parse_duration);
        let dns_max_ttl = parse(strings, ENV_DNS_MAX_TTL, parse_duration);

        let dns_canonicalize_timeout = parse(strings, ENV_DNS_CANONICALIZE_TIMEOUT, parse_duration);

        let identity_config = parse_identity_config(strings);

        let id_disabled = identity_config
            .as_ref()
            .map(|c| c.is_none())
            .unwrap_or(false);

        let dst_addr = if id_disabled {
            parse_control_addr_disable_identity(strings, ENV_DESTINATION_SVC_BASE)
        } else {
            parse_control_addr(strings, ENV_DESTINATION_SVC_BASE)
        };

        let dst_token = strings.get(ENV_DESTINATION_CONTEXT);

        let dst_get_suffixes = parse(strings, ENV_DESTINATION_GET_SUFFIXES, parse_dns_suffixes);
        let dst_profile_suffixes = parse(
            strings,
            ENV_DESTINATION_PROFILE_SUFFIXES,
            parse_dns_suffixes,
        );

        let initial_stream_window_size =
            parse(strings, ENV_INITIAL_STREAM_WINDOW_SIZE, parse_number);
        let initial_connection_window_size =
            parse(strings, ENV_INITIAL_CONNECTION_WINDOW_SIZE, parse_number);

        let tap_disabled = strings
            .get(ENV_TAP_DISABLED)?
            .map(|d| !d.is_empty())
            .unwrap_or(false);
        let control_listener = parse_control_listener(strings, id_disabled, tap_disabled);

        Ok(Config {
            outbound_listener: Listener {
                addr: outbound_listener_addr?
                    .unwrap_or_else(|| parse_socket_addr(DEFAULT_OUTBOUND_LISTEN_ADDR).unwrap()),
            },
            inbound_listener: Listener {
                addr: inbound_listener_addr?
                    .unwrap_or_else(|| parse_socket_addr(DEFAULT_INBOUND_LISTEN_ADDR).unwrap()),
            },
            control_listener: control_listener?,
            admin_listener: Listener {
                addr: admin_listener_addr?
                    .unwrap_or_else(|| parse_socket_addr(DEFAULT_ADMIN_LISTEN_ADDR).unwrap()),
            },
            inbound_forward: inbound_forward?,

            inbound_connect_timeout: inbound_connect_timeout?
                .unwrap_or(DEFAULT_INBOUND_CONNECT_TIMEOUT),
            outbound_connect_timeout: outbound_connect_timeout?
                .unwrap_or(DEFAULT_OUTBOUND_CONNECT_TIMEOUT),
            control_connect_timeout: control_connect_timeout?
                .unwrap_or(DEFAULT_CONTROL_CONNECT_TIMEOUT),

            inbound_dispatch_timeout: inbound_dispatch_timeout?
                .unwrap_or(DEFAULT_INBOUND_DISPATCH_TIMEOUT),
            outbound_dispatch_timeout: outbound_dispatch_timeout?
                .unwrap_or(DEFAULT_OUTBOUND_DISPATCH_TIMEOUT),
            control_dispatch_timeout: control_dispatch_timeout?
                .unwrap_or(DEFAULT_CONTROL_DISPATCH_TIMEOUT),

            inbound_connect_backoff: parse_backoff(
                strings,
                INBOUND_CONNECT_BASE,
                DEFAULT_INBOUND_CONNECT_BACKOFF,
            )?,
            outbound_connect_backoff: parse_backoff(
                strings,
                OUTBOUND_CONNECT_BASE,
                DEFAULT_OUTBOUND_CONNECT_BACKOFF,
            )?,
            control_backoff: parse_backoff(strings, CONTROL_BASE, DEFAULT_CONTROL_BACKOFF)?,

            inbound_accept_keepalive: inbound_accept_keepalive?,
            outbound_accept_keepalive: outbound_accept_keepalive?,

            inbound_connect_keepalive: inbound_connect_keepalive?,
            outbound_connect_keepalive: outbound_connect_keepalive?,

            inbound_ports_disable_protocol_detection: inbound_disable_ports?
                .unwrap_or_else(|| default_disable_ports_protocol_detection()),
            outbound_ports_disable_protocol_detection: outbound_disable_ports?
                .unwrap_or_else(|| default_disable_ports_protocol_detection()),

            inbound_router_capacity: inbound_router_capacity?
                .unwrap_or(DEFAULT_INBOUND_ROUTER_CAPACITY),
            outbound_router_capacity: outbound_router_capacity?
                .unwrap_or(DEFAULT_OUTBOUND_ROUTER_CAPACITY),

            inbound_router_max_idle_age: inbound_router_max_idle_age?
                .unwrap_or(DEFAULT_INBOUND_ROUTER_MAX_IDLE_AGE),
            outbound_router_max_idle_age: outbound_router_max_idle_age?
                .unwrap_or(DEFAULT_OUTBOUND_ROUTER_MAX_IDLE_AGE),

            inbound_max_requests_in_flight: inbound_max_in_flight?
                .unwrap_or(DEFAULT_INBOUND_MAX_IN_FLIGHT),
            outbound_max_requests_in_flight: outbound_max_in_flight?
                .unwrap_or(DEFAULT_OUTBOUND_MAX_IN_FLIGHT),

            destination_buffer_capacity: DEFAULT_DESTINATION_BUFFER_CAPACITY,

            destination_get_suffixes: dst_get_suffixes?
                .unwrap_or(parse_dns_suffixes(DEFAULT_DESTINATION_GET_SUFFIXES).unwrap()),

            destination_profile_suffixes: dst_profile_suffixes?
                .unwrap_or(parse_dns_suffixes(DEFAULT_DESTINATION_PROFILE_SUFFIXES).unwrap()),

            destination_addr: dst_addr?.ok_or(Error::NoDestinationAddress)?,
            destination_context: dst_token?.unwrap_or_default(),

            identity_config: identity_config?
                .map(Conditional::Some)
                .unwrap_or_else(|| Conditional::None(tls::ReasonForNoIdentity::Disabled)),

            resolv_conf_path: resolv_conf_path?
                .unwrap_or(DEFAULT_RESOLV_CONF.into())
                .into(),

            metrics_retain_idle: metrics_retain_idle?.unwrap_or(DEFAULT_METRICS_RETAIN_IDLE),

            dns_min_ttl: dns_min_ttl?,

            dns_max_ttl: dns_max_ttl?,

            dns_canonicalize_timeout: dns_canonicalize_timeout?
                .unwrap_or(DEFAULT_DNS_CANONICALIZE_TIMEOUT),

            h2_settings: H2Settings {
                initial_stream_window_size: initial_stream_window_size?,
                initial_connection_window_size: initial_connection_window_size?,
            },
        })
    }
}

fn default_disable_ports_protocol_detection() -> IndexSet<u16> {
    IndexSet::from_iter(DEFAULT_PORTS_DISABLE_PROTOCOL_DETECTION.iter().cloned())
}

// ===== impl Env =====

impl Strings for Env {
    fn get(&self, key: &str) -> Result<Option<String>, Error> {
        use std::env;

        match env::var(key) {
            Ok(value) => Ok(Some(value)),
            Err(env::VarError::NotPresent) => Ok(None),
            Err(env::VarError::NotUnicode(_)) => {
                error!("{} is not encoded in Unicode", key);
                Err(Error::InvalidEnvVar)
            }
        }
    }
}

// ===== impl TestEnv =====

impl TestEnv {
    pub fn new() -> Self {
        Self {
            values: Default::default(),
        }
    }

    pub fn put(&mut self, key: &'static str, value: String) {
        self.values.insert(key, value);
    }

    pub fn contains_key(&self, key: &'static str) -> bool {
        self.values.contains_key(key)
    }

    pub fn remove(&mut self, key: &'static str) {
        self.values.remove(key);
    }
}

impl Strings for TestEnv {
    fn get(&self, key: &str) -> Result<Option<String>, Error> {
        Ok(self.values.get(key).cloned())
    }
}

// ===== impl TapSettings =====

impl TapSettings {
    fn new(addr: SocketAddr, tap_svc_name: identity::Name) -> Self {
        Self {
            listener: Listener { addr },
            tap_svc_name,
        }
    }
}

// ===== Parsing =====

/// There is a dependency on identity being enabled for tap to properly work.
/// Depending on the setting of both, a user could end up in an improperly
/// configured proxy environment.
///
/// ```plain
///     E = Enabled; D = Disabled
///     +----------+-----+--------------+
///     | Identity | Tap | Result       |
///     +----------+-----+--------------+
///     | E        | E   | Ok(Some(..)) |
///     +----------+-----+--------------+
///     | E        | D   | Ok(None)     |
///     +----------+-----+--------------+
///     | D        | E   | Err(..)      |
///     +----------+-----+--------------+
///     | D        | D   | Ok(None)     |
///     +----------+-----+--------------+
/// ```
fn parse_control_listener(
    strings: &dyn Strings,
    id_disabled: bool,
    tap_disabled: bool,
) -> Result<Option<TapSettings>, Error> {
    match (id_disabled, tap_disabled) {
        (_, true) => Ok(None),
        (true, false) => {
            error!("{} must be set if identity is disabled", ENV_TAP_DISABLED);
            Err(Error::InvalidEnvVar)
        }
        (false, false) => {
            let addr = parse(strings, ENV_CONTROL_LISTEN_ADDR, parse_socket_addr)?
                .unwrap_or_else(|| parse_socket_addr(DEFAULT_CONTROL_LISTEN_ADDR).unwrap());
            let tap_svc_name = parse(strings, ENV_TAP_SVC_NAME, parse_identity);

            match tap_svc_name? {
                Some(tap_svc_name) => Ok(Some(TapSettings::new(addr, tap_svc_name))),
                None => {
                    error!("{} must be set or tap must be disabled", ENV_TAP_SVC_NAME);
                    Err(Error::InvalidEnvVar)
                }
            }
        }
    }
}

fn parse_number<T>(s: &str) -> Result<T, ParseError>
where
    T: FromStr,
{
    s.parse().map_err(|_| ParseError::NotANumber)
}

fn parse_duration(s: &str) -> Result<Duration, ParseError> {
    use regex::Regex;

    let re = Regex::new(r"^\s*(\d+)(ms|s|m|h|d)?\s*$").expect("duration regex");

    let cap = re.captures(s).ok_or(ParseError::NotADuration)?;

    let magnitude = parse_number(&cap[1])?;
    match cap.get(2).map(|m| m.as_str()) {
        None if magnitude == 0 => Ok(Duration::from_secs(0)),
        Some("ms") => Ok(Duration::from_millis(magnitude)),
        Some("s") => Ok(Duration::from_secs(magnitude)),
        Some("m") => Ok(Duration::from_secs(magnitude * 60)),
        Some("h") => Ok(Duration::from_secs(magnitude * 60 * 60)),
        Some("d") => Ok(Duration::from_secs(magnitude * 60 * 60 * 24)),
        _ => Err(ParseError::NotADuration),
    }
}

fn parse_socket_addr(s: &str) -> Result<SocketAddr, ParseError> {
    match parse_addr(s)? {
        Addr::Socket(a) => Ok(a),
        _ => {
            error!("Expected IP:PORT; found: {}", s);
            Err(ParseError::HostIsNotAnIpAddress)
        }
    }
}

fn parse_addr(s: &str) -> Result<Addr, ParseError> {
    addr::Addr::from_str(s).map_err(|e| {
        error!("Not a valid address: {}", s);
        ParseError::AddrError(e)
    })
}

fn parse_port_set(s: &str) -> Result<IndexSet<u16>, ParseError> {
    let mut set = IndexSet::new();
    for num in s.split(',') {
        set.insert(parse_number::<u16>(num)?);
    }
    Ok(set)
}

pub(super) fn parse_identity(s: &str) -> Result<identity::Name, ParseError> {
    identity::Name::from_hostname(s.as_bytes()).map_err(|identity::InvalidName| {
        error!("Not a valid identity name: {}", s);
        ParseError::NameError
    })
}

pub(super) fn parse<T, Parse>(
    strings: &dyn Strings,
    name: &str,
    parse: Parse,
) -> Result<Option<T>, Error>
where
    Parse: FnOnce(&str) -> Result<T, ParseError>,
{
    match strings.get(name)? {
        Some(ref s) => {
            let r = parse(s).map_err(|parse_error| {
                error!("{}={:?} is not valid: {:?}", name, s, parse_error);
                Error::InvalidEnvVar
            })?;
            Ok(Some(r))
        }
        None => Ok(None),
    }
}

#[allow(dead_code)]
fn parse_deprecated<T, Parse>(
    strings: &dyn Strings,
    name: &str,
    deprecated_name: &str,
    f: Parse,
) -> Result<Option<T>, Error>
where
    Parse: Copy,
    Parse: Fn(&str) -> Result<T, ParseError>,
{
    match parse(strings, name, f)? {
        Some(v) => Ok(Some(v)),
        None => {
            let v = parse(strings, deprecated_name, f)?;
            if v.is_some() {
                warn!("{} has been deprecated; use {}", deprecated_name, name);
            }
            Ok(v)
        }
    }
}

fn parse_dns_suffixes(list: &str) -> Result<Vec<dns::Suffix>, ParseError> {
    let mut suffixes = Vec::new();
    for item in list.split(',') {
        let item = item.trim();
        if !item.is_empty() {
            let sfx = parse_dns_suffix(item)?;
            suffixes.push(sfx);
        }
    }

    Ok(suffixes)
}

fn parse_dns_suffix(s: &str) -> Result<dns::Suffix, ParseError> {
    if s == "." {
        return Ok(dns::Suffix::Root);
    }

    dns::Name::try_from(s.as_bytes())
        .map(dns::Suffix::Name)
        .map_err(|_| ParseError::NotADomainSuffix)
}

pub fn parse_backoff<S: Strings>(
    strings: &S,
    base: &str,
    default: Backoff,
) -> Result<Backoff, Error> {
    let env_disabled = format!("LINKERD2_PROXY_{}_EXP_BACKOFF_DISABLED", base);
    let disabled = strings
        .get(&env_disabled)?
        .map(|d| !d.is_empty())
        .unwrap_or(false);

    if disabled {
        Ok(Backoff::None)
    } else {
        let min_env = format!("LINKERD2_PROXY_{}_EXP_BACKOFF_MIN", base);
        let min = parse(strings, &min_env, parse_duration);
        let max_env = format!("LINKERD2_PROXY_{}_EXP_BACKOFF_MAX", base);
        let max = parse(strings, &max_env, parse_duration);
        let jitter_env = format!("LINKERD2_PROXY_{}_EXP_BACKOFF_JITTER", base);
        let jitter = parse(strings, &jitter_env, parse_number::<f64>);

        match (min?, max?, jitter?) {
            (None, None, None) => Ok(default),
            (Some(min), Some(max), Some(jitter)) => {
                if jitter < 0.0 {
                    error!("{} needs to be positive", jitter_env);
                    Err(Error::InvalidEnvVar)
                } else {
                    Ok(Backoff::Exponential { min, max, jitter })
                }
            }
            _ => {
                error!("You need to specify either all of {} {} {} or none of them to use the default backoff", min_env, max_env,jitter_env );
                Err(Error::InvalidEnvVar)
            }
        }
    }
}

pub fn parse_control_addr<S: Strings>(
    strings: &S,
    base: &str,
) -> Result<Option<ControlAddr>, Error> {
    let a_env = format!("{}_ADDR", base);
    let a = parse(strings, &a_env, parse_addr);
    let n_env = format!("{}_NAME", base);
    let n = parse(strings, &n_env, parse_identity);
    match (a?, n?) {
        (None, None) => Ok(None),
        (Some(ref addr), _) if addr.is_loopback() => Ok(Some(ControlAddr {
            addr: addr.clone(),
            identity: Conditional::None(tls::ReasonForNoPeerName::Loopback.into()),
        })),
        (Some(addr), Some(name)) => Ok(Some(ControlAddr {
            addr,
            identity: Conditional::Some(name),
        })),
        (Some(_), None) => {
            error!("{} must be specified when {} is set", n_env, a_env);
            Err(Error::InvalidEnvVar)
        }
        (None, Some(_)) => {
            error!("{} must be specified when {} is set", a_env, n_env);
            Err(Error::InvalidEnvVar)
        }
    }
}

pub fn parse_control_addr_disable_identity<S: Strings>(
    strings: &S,
    base: &str,
) -> Result<Option<ControlAddr>, Error> {
    let a = parse(strings, &format!("{}_ADDR", base), parse_addr)?;
    let identity = Conditional::None(tls::ReasonForNoIdentity::Disabled);
    Ok(a.map(|addr| ControlAddr { addr, identity }))
}

pub fn parse_identity_config<S: Strings>(strings: &S) -> Result<Option<identity::Config>, Error> {
    let sa = parse_control_addr(strings, ENV_IDENTITY_SVC_BASE);
    let ta = parse(strings, ENV_IDENTITY_TRUST_ANCHORS, |ref s| {
        identity::TrustAnchors::from_pem(s).ok_or(ParseError::InvalidTrustAnchors)
    });
    let dir = parse(strings, ENV_IDENTITY_DIR, |ref s| Ok(PathBuf::from(s)));
    let tok = parse(strings, ENV_IDENTITY_TOKEN_FILE, |ref s| {
        identity::TokenSource::if_nonempty_file(s.to_string()).map_err(|e| {
            error!("Could not read {}: {}", ENV_IDENTITY_TOKEN_FILE, e);
            ParseError::InvalidTokenSource
        })
    });
    let li = parse(strings, ENV_IDENTITY_IDENTITY_LOCAL_NAME, parse_identity);
    let min_refresh = parse(strings, ENV_IDENTITY_MIN_REFRESH, parse_duration);
    let max_refresh = parse(strings, ENV_IDENTITY_MAX_REFRESH, parse_duration);

    let disabled = strings
        .get(ENV_IDENTITY_DISABLED)?
        .map(|d| !d.is_empty())
        .unwrap_or(false);

    match (
        disabled,
        sa?,
        ta?,
        dir?,
        li?,
        tok?,
        min_refresh?,
        max_refresh?,
    ) {
        (disabled, None, None, None, None, None, None, None) => {
            if !disabled {
                error!(
                    "{} must be set or identity configuration must be specified.",
                    ENV_IDENTITY_DISABLED
                );
                Err(Error::InvalidEnvVar)
            } else {
                Ok(None)
            }
        }
        (
            false,
            Some(svc),
            Some(trust_anchors),
            Some(dir),
            Some(local_name),
            Some(token),
            min_refresh,
            max_refresh,
        ) => {
            let key = {
                let mut p = dir.clone();
                p.push("key");
                p.set_extension("p8");

                fs::read(p)
                    .map_err(|e| {
                        error!("Failed to read key: {}", e);
                        Error::InvalidEnvVar
                    })
                    .and_then(|b| {
                        identity::Key::from_pkcs8(&b).map_err(|e| {
                            error!("Invalid key: {}", e);
                            Error::InvalidEnvVar
                        })
                    })
            };

            let csr = {
                let mut p = dir;
                p.push("csr");
                p.set_extension("der");

                fs::read(p)
                    .map_err(|e| {
                        error!("Failed to read Csr: {}", e);
                        Error::InvalidEnvVar
                    })
                    .and_then(|b| {
                        identity::Csr::from_der(b).ok_or_else(|| {
                            error!("No CSR found");
                            Error::InvalidEnvVar
                        })
                    })
            };

            Ok(Some(identity::Config {
                svc,
                local_name,
                token,
                trust_anchors,
                csr: csr?,
                key: key?,
                min_refresh: min_refresh.unwrap_or(DEFAULT_IDENTITY_MIN_REFRESH),
                max_refresh: max_refresh.unwrap_or(DEFAULT_IDENTITY_MAX_REFRESH),
            }))
        }
        (disabled, svc, trust_anchors, end_entity_dir, local_id, token, _minr, _maxr) => {
            if disabled {
                error!(
                    "{} must be unset when other identity variables are set.",
                    ENV_IDENTITY_DISABLED,
                );
            }
            let s = format!("{0}_ADDR and {0}_NAME", ENV_IDENTITY_SVC_BASE);
            let svc_env: &str = &s.as_str();
            for (unset, name) in &[
                (svc.is_none(), svc_env),
                (trust_anchors.is_none(), ENV_IDENTITY_TRUST_ANCHORS),
                (end_entity_dir.is_none(), ENV_IDENTITY_DIR),
                (local_id.is_none(), ENV_IDENTITY_IDENTITY_LOCAL_NAME),
                (token.is_none(), ENV_IDENTITY_TOKEN_FILE),
            ] {
                if *unset {
                    error!(
                        "{} must be set when other identity variables are set.",
                        name,
                    );
                }
            }
            Err(Error::InvalidEnvVar)
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::InvalidEnvVar => write!(f, "invalid environment variable"),
            Error::NoDestinationAddress => write!(f, "no destination service configured"),
        }
    }
}

impl ::std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_unit<F: Fn(u64) -> Duration>(unit: &str, to_duration: F) {
        for v in &[0, 1, 23, 456_789] {
            let d = to_duration(*v);
            let text = format!("{}{}", v, unit);
            assert_eq!(parse_duration(&text), Ok(d), "text=\"{}\"", text);

            let text = format!(" {}{}\t", v, unit);
            assert_eq!(parse_duration(&text), Ok(d), "text=\"{}\"", text);
        }
    }

    #[test]
    fn parse_duration_unit_ms() {
        test_unit("ms", |v| Duration::from_millis(v));
    }

    #[test]
    fn parse_duration_unit_s() {
        test_unit("s", |v| Duration::from_secs(v));
    }

    #[test]
    fn parse_duration_unit_m() {
        test_unit("m", |v| Duration::from_secs(v * 60));
    }

    #[test]
    fn parse_duration_unit_h() {
        test_unit("h", |v| Duration::from_secs(v * 60 * 60));
    }

    #[test]
    fn parse_duration_unit_d() {
        test_unit("d", |v| Duration::from_secs(v * 60 * 60 * 24));
    }

    #[test]
    fn parse_duration_floats_invalid() {
        assert_eq!(parse_duration(".123h"), Err(ParseError::NotADuration));
        assert_eq!(parse_duration("1.23h"), Err(ParseError::NotADuration));
    }

    #[test]
    fn parse_duration_space_before_unit_invalid() {
        assert_eq!(parse_duration("1 ms"), Err(ParseError::NotADuration));
    }

    #[test]
    fn parse_duration_overflows_invalid() {
        assert_eq!(
            parse_duration("123456789012345678901234567890ms"),
            Err(ParseError::NotANumber)
        );
    }

    #[test]
    fn parse_duration_invalid_unit() {
        assert_eq!(parse_duration("12moons"), Err(ParseError::NotADuration));
        assert_eq!(parse_duration("12y"), Err(ParseError::NotADuration));
    }

    #[test]
    fn parse_duration_zero_without_unit() {
        assert_eq!(parse_duration("0"), Ok(Duration::from_secs(0)));
    }

    #[test]
    fn parse_duration_number_without_unit_is_invalid() {
        assert_eq!(parse_duration("1"), Err(ParseError::NotADuration));
    }

    #[test]
    fn dns_suffixes() {
        fn p(s: &str) -> Result<Vec<String>, ParseError> {
            let sfxs = parse_dns_suffixes(s)?
                .into_iter()
                .map(|s| format!("{}", s))
                .collect();

            Ok(sfxs)
        }

        assert_eq!(p(""), Ok(vec![]), "empty string");
        assert_eq!(p(",,,"), Ok(vec![]), "empty list components are ignored");
        assert_eq!(p("."), Ok(vec![".".to_owned()]), "root is valid");
        assert_eq!(
            p("a.b.c"),
            Ok(vec!["a.b.c".to_owned()]),
            "a name without trailing dot"
        );
        assert_eq!(
            p("a.b.c."),
            Ok(vec!["a.b.c.".to_owned()]),
            "a name with a trailing dot"
        );
        assert_eq!(
            p(" a.b.c. , d.e.f. "),
            Ok(vec!["a.b.c.".to_owned(), "d.e.f.".to_owned()]),
            "whitespace is ignored"
        );
        assert_eq!(
            p("a .b.c"),
            Err(ParseError::NotADomainSuffix),
            "whitespace not allowed within a name"
        );
        assert_eq!(
            p("mUlti.CasE.nAmE"),
            Ok(vec!["multi.case.name".to_owned()]),
            "names are coerced to lowercase"
        );
    }
}
