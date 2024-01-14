use crate::{dns, gateway, identity, inbound, oc_collector, outbound, policy, spire};
use linkerd_app_core::{
    addr,
    config::*,
    control::{Config as ControlConfig, ControlAddr},
    proxy::http::{h1, h2},
    tls,
    transport::{Keepalive, ListenAddr},
    Addr, AddrMatch, Conditional, IpNet,
};
use linkerd_tonic_stream::ReceiveLimits;
use rangemap::RangeInclusiveSet;
use std::{
    collections::{HashMap, HashSet},
    fs,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    str::FromStr,
    time::Duration,
};
use thiserror::Error;
use tracing::{debug, error, info, warn};

/// The strings used to build a configuration.
pub trait Strings {
    /// Retrieves the value for the key `key`.
    ///
    /// `key` must be one of the `ENV_` values below.
    fn get(&self, key: &str) -> Result<Option<String>, EnvError>;
}

/// An implementation of `Strings` that reads the values from environment variables.
pub struct Env;

/// Errors produced when loading a `Config` struct.
#[derive(Clone, Debug, Error)]
pub enum EnvError {
    #[error("invalid environment variable")]
    InvalidEnvVar,
    #[error("no destination service configured")]
    NoDestinationAddress,
    #[error("no policy service configured")]
    NoPolicyAddress,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum ParseError {
    #[error("not a valid duration")]
    NotADuration,
    #[error("not a valid DNS domain suffix")]
    NotADomainSuffix,
    #[error("not a boolean value: {0}")]
    NotABool(
        #[from]
        #[source]
        std::str::ParseBoolError,
    ),
    #[error("not an integer: {0}")]
    NotAnInteger(
        #[from]
        #[source]
        std::num::ParseIntError,
    ),
    #[error("not a floating-point number: {0}")]
    NotAFloat(
        #[from]
        #[source]
        std::num::ParseFloatError,
    ),
    #[error("not a valid subnet mask")]
    NotANetwork,
    #[error("host is not an IP address")]
    HostIsNotAnIpAddress,
    #[error("not a valid IP address: {0}")]
    NotAnIp(
        #[from]
        #[source]
        std::net::AddrParseError,
    ),
    #[error("not a valid port range")]
    NotAPortRange,
    #[error(transparent)]
    AddrError(addr::Error),
    #[error("not a valid identity name")]
    NameError,
    #[error("could not read token source")]
    InvalidTokenSource,
    #[error("invalid trust anchors")]
    InvalidTrustAnchors,
    #[error("not a valid port policy: {0}")]
    InvalidPortPolicy(String),
}

// Environment variables to look at when loading the configuration
pub const ENV_OUTBOUND_LISTEN_ADDR: &str = "LINKERD2_PROXY_OUTBOUND_LISTEN_ADDR";
pub const ENV_INBOUND_LISTEN_ADDR: &str = "LINKERD2_PROXY_INBOUND_LISTEN_ADDR";
pub const ENV_CONTROL_LISTEN_ADDR: &str = "LINKERD2_PROXY_CONTROL_LISTEN_ADDR";
pub const ENV_ADMIN_LISTEN_ADDR: &str = "LINKERD2_PROXY_ADMIN_LISTEN_ADDR";

pub const ENV_METRICS_RETAIN_IDLE: &str = "LINKERD2_PROXY_METRICS_RETAIN_IDLE";

const ENV_INGRESS_MODE: &str = "LINKERD2_PROXY_INGRESS_MODE";

const ENV_INBOUND_HTTP_QUEUE_CAPACITY: &str = "LINKERD2_PROXY_INBOUND_HTTP_QUEUE_CAPACITY";
const ENV_INBOUND_HTTP_FAILFAST_TIMEOUT: &str = "LINKERD2_PROXY_INBOUND_HTTP_FAILFAST_TIMEOUT";

const ENV_OUTBOUND_TCP_QUEUE_CAPACITY: &str = "LINKERD2_PROXY_OUTBOUND_TCP_QUEUE_CAPACITY";
const ENV_OUTBOUND_TCP_FAILFAST_TIMEOUT: &str = "LINKERD2_PROXY_OUTBOUND_TCP_FAILFAST_TIMEOUT";
const ENV_OUTBOUND_HTTP_QUEUE_CAPACITY: &str = "LINKERD2_PROXY_OUTBOUND_HTTP_QUEUE_CAPACITY";
const ENV_OUTBOUND_HTTP_FAILFAST_TIMEOUT: &str = "LINKERD2_PROXY_OUTBOUND_HTTP_FAILFAST_TIMEOUT";

pub const ENV_INBOUND_DETECT_TIMEOUT: &str = "LINKERD2_PROXY_INBOUND_DETECT_TIMEOUT";
const ENV_OUTBOUND_DETECT_TIMEOUT: &str = "LINKERD2_PROXY_OUTBOUND_DETECT_TIMEOUT";

const ENV_INBOUND_CONNECT_TIMEOUT: &str = "LINKERD2_PROXY_INBOUND_CONNECT_TIMEOUT";
const ENV_OUTBOUND_CONNECT_TIMEOUT: &str = "LINKERD2_PROXY_OUTBOUND_CONNECT_TIMEOUT";

const ENV_INBOUND_ACCEPT_KEEPALIVE: &str = "LINKERD2_PROXY_INBOUND_ACCEPT_KEEPALIVE";
const ENV_OUTBOUND_ACCEPT_KEEPALIVE: &str = "LINKERD2_PROXY_OUTBOUND_ACCEPT_KEEPALIVE";

const ENV_INBOUND_CONNECT_KEEPALIVE: &str = "LINKERD2_PROXY_INBOUND_CONNECT_KEEPALIVE";
const ENV_OUTBOUND_CONNECT_KEEPALIVE: &str = "LINKERD2_PROXY_OUTBOUND_CONNECT_KEEPALIVE";

const ENV_INBOUND_MAX_IDLE_CONNS_PER_ENDPOINT: &str = "LINKERD2_PROXY_MAX_IDLE_CONNS_PER_ENDPOINT";
const ENV_OUTBOUND_MAX_IDLE_CONNS_PER_ENDPOINT: &str =
    "LINKERD2_PROXY_OUTBOUND_MAX_IDLE_CONNS_PER_ENDPOINT";

pub const ENV_INBOUND_MAX_IN_FLIGHT: &str = "LINKERD2_PROXY_INBOUND_MAX_IN_FLIGHT";
pub const ENV_OUTBOUND_MAX_IN_FLIGHT: &str = "LINKERD2_PROXY_OUTBOUND_MAX_IN_FLIGHT";

const ENV_OUTBOUND_DISABLE_INFORMATIONAL_HEADERS: &str =
    "LINKERD2_PROXY_OUTBOUND_DISABLE_INFORMATIONAL_HEADERS";

pub const ENV_TRACE_ATTRIBUTES_PATH: &str = "LINKERD2_PROXY_TRACE_ATTRIBUTES_PATH";

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

/// Constrains which destination addresses may be used for profile/route discovery.
///
/// The value is a comma-separated list of networks that may be
/// resolved via the destination service.
///
/// If specified and empty, the destination service is not used for route discovery.
///
/// If unspecified, a default value is used.
pub const ENV_DESTINATION_PROFILE_NETWORKS: &str = "LINKERD2_PROXY_DESTINATION_PROFILE_NETWORKS";

/// Constrains which destination names are permitted.
///
/// If unspecified or empty, no inbound gateway is configured.
pub const ENV_INBOUND_GATEWAY_SUFFIXES: &str = "LINKERD2_PROXY_INBOUND_GATEWAY_SUFFIXES";

// These *disable* our protocol detection for connections whose SO_ORIGINAL_DST
// has a port in the provided list.
pub const ENV_INBOUND_PORTS_DISABLE_PROTOCOL_DETECTION: &str =
    "LINKERD2_PROXY_INBOUND_PORTS_DISABLE_PROTOCOL_DETECTION";

pub const ENV_INBOUND_PORTS_REQUIRE_IDENTITY: &str =
    "LINKERD2_PROXY_INBOUND_PORTS_REQUIRE_IDENTITY";

pub const ENV_INBOUND_PORTS_REQUIRE_TLS: &str = "LINKERD2_PROXY_INBOUND_PORTS_REQUIRE_TLS";

/// Configures the default port policy for inbound connections.
///
/// This must parse to a valid port policy (one of: `deny`, `authenticated`,
/// `unauthenticated`, or `tls-unauthenticated` ).
///
/// By default, this is `unauthenticated`.
pub const ENV_INBOUND_DEFAULT_POLICY: &str = "LINKERD2_PROXY_INBOUND_DEFAULT_POLICY";

pub const ENV_INBOUND_PORTS: &str = "LINKERD2_PROXY_INBOUND_PORTS";
pub const ENV_POLICY_SVC_BASE: &str = "LINKERD2_PROXY_POLICY_SVC";
pub const ENV_POLICY_WORKLOAD: &str = "LINKERD2_PROXY_POLICY_WORKLOAD";
pub const ENV_POLICY_CLUSTER_NETWORKS: &str = "LINKERD2_PROXY_POLICY_CLUSTER_NETWORKS";

pub const ENV_INBOUND_IPS: &str = "LINKERD2_PROXY_INBOUND_IPS";

pub const ENV_IDENTITY_DISABLED: &str = "LINKERD2_PROXY_IDENTITY_DISABLED";
pub const ENV_IDENTITY_DIR: &str = "LINKERD2_PROXY_IDENTITY_DIR";
pub const ENV_IDENTITY_TRUST_ANCHORS: &str = "LINKERD2_PROXY_IDENTITY_TRUST_ANCHORS";
pub const ENV_IDENTITY_TOKEN_FILE: &str = "LINKERD2_PROXY_IDENTITY_TOKEN_FILE";
pub const ENV_IDENTITY_MIN_REFRESH: &str = "LINKERD2_PROXY_IDENTITY_MIN_REFRESH";
pub const ENV_IDENTITY_MAX_REFRESH: &str = "LINKERD2_PROXY_IDENTITY_MAX_REFRESH";

/// This config is here for backwards compatibility. If set, both the tls id and the server
/// name will be set to the value specified in this config. The values needs to be a DNS
/// name
pub const ENV_IDENTITY_IDENTITY_LOCAL_NAME: &str = "LINKERD2_PROXY_IDENTITY_LOCAL_NAME";
/// Configures the TLS Id of the proxy inbound server. The value is expected to match the
/// DNS or URI SAN of the leaf certificate that will be provisioned to this proxy.
pub const ENV_IDENTITY_IDENTITY_SERVER_ID: &str = "LINKERD2_PROXY_IDENTITY_SERVER_ID";
/// Configures the server name of this proxy. This value is expected to match the value
/// that clients include in the SNI extension of the ClientHello, whenever they try to
/// establish a TLS connection that shall be terminated by this proxy
pub const ENV_IDENTITY_IDENTITY_SERVER_NAME: &str = "LINKERD2_PROXY_IDENTITY_SERVER_NAME";

// If this config is set, then the proxy will be configured to use Spire as identity
// provider
pub const ENV_IDENTITY_SPIRE_SOCKET: &str = "LINKERD2_PROXY_IDENTITY_SPIRE_SOCKET";
pub const IDENTITY_SPIRE_BASE: &str = "LINKERD2_PROXY_IDENTITY_SPIRE";
const DEFAULT_SPIRE_BACKOFF: ExponentialBackoff =
    ExponentialBackoff::new_unchecked(Duration::from_millis(100), Duration::from_secs(1), 0.1);
const SPIFFE_ID_URI_SCHEME: &str = "spiffe";

pub const ENV_IDENTITY_SVC_BASE: &str = "LINKERD2_PROXY_IDENTITY_SVC";

pub const ENV_DESTINATION_SVC_BASE: &str = "LINKERD2_PROXY_DESTINATION_SVC";

pub const ENV_HOSTNAME: &str = "HOSTNAME";

pub const ENV_TRACE_COLLECTOR_SVC_BASE: &str = "LINKERD2_PROXY_TRACE_COLLECTOR_SVC";

pub const ENV_DESTINATION_CONTEXT: &str = "LINKERD2_PROXY_DESTINATION_CONTEXT";
pub const ENV_DESTINATION_PROFILE_INITIAL_TIMEOUT: &str =
    "LINKERD2_PROXY_DESTINATION_PROFILE_INITIAL_TIMEOUT";

pub const ENV_TAP_SVC_NAME: &str = "LINKERD2_PROXY_TAP_SVC_NAME";
const ENV_RESOLV_CONF: &str = "LINKERD2_PROXY_RESOLV_CONF";

/// Configures a minimum value for the TTL of DNS lookups.
///
/// Lookups with TTLs below this value will use this value instead.
const ENV_DNS_MIN_TTL: &str = "LINKERD2_PROXY_DNS_MIN_TTL";
/// Configures a maximum value for the TTL of DNS lookups.
///
/// Lookups with TTLs above this value will use this value instead.
const ENV_DNS_MAX_TTL: &str = "LINKERD2_PROXY_DNS_MAX_TTL";

/// Configure the stream or connection level flow control setting for HTTP2.
///
/// If unspecified, the default value of 65,535 is used.
const ENV_INITIAL_STREAM_WINDOW_SIZE: &str = "LINKERD2_PROXY_HTTP2_INITIAL_STREAM_WINDOW_SIZE";
const ENV_INITIAL_CONNECTION_WINDOW_SIZE: &str =
    "LINKERD2_PROXY_HTTP2_INITIAL_CONNECTION_WINDOW_SIZE";

const ENV_INBOUND_HTTP1_CONNECTION_POOL_IDLE_TIMEOUT: &str =
    "LINKERD2_PROXY_INBOUND_HTTP1_CONNECTION_POOL_IDLE_TIMEOUT";
const ENV_OUTBOUND_HTTP1_CONNECTION_POOL_IDLE_TIMEOUT: &str =
    "LINKERD2_PROXY_OUTBOUND_HTTP1_CONNECTION_POOL_IDLE_TIMEOUT";

const ENV_SHUTDOWN_GRACE_PERIOD: &str = "LINKERD2_PROXY_SHUTDOWN_GRACE_PERIOD";

// Default values for various configuration fields
const DEFAULT_OUTBOUND_LISTEN_ADDR: &str = "127.0.0.1:4140";
pub const DEFAULT_INBOUND_LISTEN_ADDR: &str = "0.0.0.0:4143";
pub const DEFAULT_CONTROL_LISTEN_ADDR: &str = "0.0.0.0:4190";

const DEFAULT_ADMIN_LISTEN_ADDR: &str = "127.0.0.1:4191";
const DEFAULT_METRICS_RETAIN_IDLE: Duration = Duration::from_secs(10 * 60);

const DEFAULT_INBOUND_HTTP_QUEUE_CAPACITY: usize = 10_000;
const DEFAULT_INBOUND_HTTP_FAILFAST_TIMEOUT: Duration = Duration::from_secs(1);
const DEFAULT_INBOUND_DETECT_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_INBOUND_CONNECT_TIMEOUT: Duration = Duration::from_millis(300);
const DEFAULT_INBOUND_CONNECT_BACKOFF: ExponentialBackoff =
    ExponentialBackoff::new_unchecked(Duration::from_millis(100), Duration::from_millis(500), 0.1);

const DEFAULT_OUTBOUND_TCP_QUEUE_CAPACITY: usize = 10_000;
const DEFAULT_OUTBOUND_TCP_FAILFAST_TIMEOUT: Duration = Duration::from_secs(3);
const DEFAULT_OUTBOUND_HTTP_QUEUE_CAPACITY: usize = 10_000;
const DEFAULT_OUTBOUND_HTTP_FAILFAST_TIMEOUT: Duration = Duration::from_secs(3);
const DEFAULT_OUTBOUND_DETECT_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_OUTBOUND_CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
const DEFAULT_OUTBOUND_CONNECT_BACKOFF: ExponentialBackoff =
    ExponentialBackoff::new_unchecked(Duration::from_millis(100), Duration::from_millis(500), 0.1);

const DEFAULT_CONTROL_QUEUE_CAPACITY: usize = 100;
const DEFAULT_CONTROL_FAILFAST_TIMEOUT: Duration = Duration::from_secs(10);

const DEFAULT_RESOLV_CONF: &str = "/etc/resolv.conf";

const DEFAULT_INITIAL_STREAM_WINDOW_SIZE: u32 = 65_535; // Protocol default
const DEFAULT_INITIAL_CONNECTION_WINDOW_SIZE: u32 = 1048576; // 1MB ~ 16 streams at capacity

// 2 minutes seems like a reasonable amount of time to wait for connections to close...
const DEFAULT_SHUTDOWN_GRACE_PERIOD: Duration = Duration::from_secs(2 * 60);

// This configuration limits the amount of time Linkerd retains cached clients &
// connections for a given destination ip:port, as referenced by the application
// client.
//
// After this timeout expires, the proxy will need to re-resolve destination
// metadata. The outbound default of 5s matches Kubernetes' default DNS TTL. On
// the outbound side, especially, we want to use a limited idle timeout since
// stale clients/connections can have a severe memory impact, especially when
// the application communicates with many destinations.
const ENV_OUTBOUND_DISCOVERY_IDLE_TIMEOUT: &str = "LINKERD2_PROXY_OUTBOUND_DISCOVERY_IDLE_TIMEOUT";
const DEFAULT_OUTBOUND_DISCOVERY_IDLE_TIMEOUT: Duration = Duration::from_secs(5);

// On the inbound side, we may lookup per-port policy or per-service profile
// configuration. We are more permissive in retaining inbound configuration,
// because we expect this to be a generally lower-cardinality set of
// configurations to discover.
const ENV_INBOUND_DISCOVERY_IDLE_TIMEOUT: &str = "LINKERD2_PROXY_INBOUND_DISCOVERY_IDLE_TIMEOUT";
const DEFAULT_INBOUND_DISCOVERY_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

// XXX This default inbound connection idle timeout should be less than or equal
// to the server's idle timeout so that we don't try to reuse a connection as it
// is being timed out of the server.
//
// TODO(ver) this should be made configurable per-server from the proxy API.
const DEFAULT_INBOUND_HTTP1_CONNECTION_POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(3);

// TODO(ver) This should be configurable at the load balancer level.
const DEFAULT_OUTBOUND_HTTP1_CONNECTION_POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(3);

// By default, we don't limit the number of connections a connection pol may
// use, as doing so can severely impact CPU utilization for applications with
// many concurrent requests. It's generally preferable to use the MAX_IDLE_AGE
// limitations to quickly drop idle connections.
const DEFAULT_INBOUND_MAX_IDLE_CONNS_PER_ENDPOINT: usize = usize::MAX;
const DEFAULT_OUTBOUND_MAX_IDLE_CONNS_PER_ENDPOINT: usize = usize::MAX;

// These settings limit the number of requests that have not received responses,
// including those buffered in the proxy and dispatched to the destination
// service.
const DEFAULT_INBOUND_MAX_IN_FLIGHT: usize = 100_000;
const DEFAULT_OUTBOUND_MAX_IN_FLIGHT: usize = 100_000;

const DEFAULT_DESTINATION_PROFILE_SUFFIXES: &str = "svc.cluster.local.";
const DEFAULT_DESTINATION_PROFILE_SKIP_TIMEOUT: Duration = Duration::from_millis(500);

const DEFAULT_IDENTITY_MIN_REFRESH: Duration = Duration::from_secs(10);
const DEFAULT_IDENTITY_MAX_REFRESH: Duration = Duration::from_secs(60 * 60 * 24);

const INBOUND_CONNECT_BASE: &str = "INBOUND_CONNECT";
const OUTBOUND_CONNECT_BASE: &str = "OUTBOUND_CONNECT";

/// Load a `App` by reading ENV variables.
pub fn parse_config<S: Strings>(strings: &S) -> Result<super::Config, EnvError> {
    // Parse all the environment variables. `parse` will log any errors so
    // defer returning any errors until all of them have been parsed.
    let outbound_listener_addr = parse(strings, ENV_OUTBOUND_LISTEN_ADDR, parse_socket_addr);
    let inbound_listener_addr = parse(strings, ENV_INBOUND_LISTEN_ADDR, parse_socket_addr);
    let admin_listener_addr = parse(strings, ENV_ADMIN_LISTEN_ADDR, parse_socket_addr);

    let inbound_detect_timeout = parse(strings, ENV_INBOUND_DETECT_TIMEOUT, parse_duration);
    let inbound_connect_timeout = parse(strings, ENV_INBOUND_CONNECT_TIMEOUT, parse_duration);
    let inbound_http_queue_capacity = parse(strings, ENV_INBOUND_HTTP_QUEUE_CAPACITY, parse_number);
    let inbound_http_failfast_timeout =
        parse(strings, ENV_INBOUND_HTTP_FAILFAST_TIMEOUT, parse_duration);

    let outbound_detect_timeout = parse(strings, ENV_OUTBOUND_DETECT_TIMEOUT, parse_duration);
    let outbound_tcp_queue_capacity = parse(strings, ENV_OUTBOUND_TCP_QUEUE_CAPACITY, parse_number);
    let outbound_tcp_failfast_timeout =
        parse(strings, ENV_OUTBOUND_TCP_FAILFAST_TIMEOUT, parse_duration);
    let outbound_http_queue_capacity =
        parse(strings, ENV_OUTBOUND_HTTP_QUEUE_CAPACITY, parse_number);
    let outbound_http_failfast_timeout =
        parse(strings, ENV_OUTBOUND_HTTP_FAILFAST_TIMEOUT, parse_duration);
    let outbound_connect_timeout = parse(strings, ENV_OUTBOUND_CONNECT_TIMEOUT, parse_duration);

    let inbound_accept_keepalive = parse(strings, ENV_INBOUND_ACCEPT_KEEPALIVE, parse_duration);
    let outbound_accept_keepalive = parse(strings, ENV_OUTBOUND_ACCEPT_KEEPALIVE, parse_duration);

    let inbound_connect_keepalive = parse(strings, ENV_INBOUND_CONNECT_KEEPALIVE, parse_duration);
    let outbound_connect_keepalive = parse(strings, ENV_OUTBOUND_CONNECT_KEEPALIVE, parse_duration);

    let shutdown_grace_period = parse(strings, ENV_SHUTDOWN_GRACE_PERIOD, parse_duration);

    let inbound_discovery_idle_timeout =
        parse(strings, ENV_INBOUND_DISCOVERY_IDLE_TIMEOUT, parse_duration);
    let outbound_discovery_idle_timeout =
        parse(strings, ENV_OUTBOUND_DISCOVERY_IDLE_TIMEOUT, parse_duration);

    let inbound_max_idle_per_endpoint = parse(
        strings,
        ENV_INBOUND_MAX_IDLE_CONNS_PER_ENDPOINT,
        parse_number,
    );
    let outbound_max_idle_per_endpoint = parse(
        strings,
        ENV_OUTBOUND_MAX_IDLE_CONNS_PER_ENDPOINT,
        parse_number,
    );

    let inbound_max_in_flight = parse(strings, ENV_INBOUND_MAX_IN_FLIGHT, parse_number);
    let outbound_max_in_flight = parse(strings, ENV_OUTBOUND_MAX_IN_FLIGHT, parse_number);

    let metrics_retain_idle = parse(strings, ENV_METRICS_RETAIN_IDLE, parse_duration);

    let control_receive_limits = mk_control_receive_limits(strings)?;

    // DNS

    let resolv_conf_path = strings.get(ENV_RESOLV_CONF);

    let dns_min_ttl = parse(strings, ENV_DNS_MIN_TTL, parse_duration);
    let dns_max_ttl = parse(strings, ENV_DNS_MAX_TTL, parse_duration);

    let tls = parse_tls_params(strings);

    let hostname = strings.get(ENV_HOSTNAME);

    let oc_attributes_file_path = strings.get(ENV_TRACE_ATTRIBUTES_PATH);

    let trace_collector_addr = parse_control_addr(strings, ENV_TRACE_COLLECTOR_SVC_BASE);

    let gateway_suffixes = parse(strings, ENV_INBOUND_GATEWAY_SUFFIXES, parse_dns_suffixes);

    let dst_addr = parse_control_addr(strings, ENV_DESTINATION_SVC_BASE);
    let dst_token = strings.get(ENV_DESTINATION_CONTEXT);
    let dst_profile_skip_timeout = parse(
        strings,
        ENV_DESTINATION_PROFILE_INITIAL_TIMEOUT,
        parse_duration,
    );
    let dst_profile_suffixes = parse(
        strings,
        ENV_DESTINATION_PROFILE_SUFFIXES,
        parse_dns_suffixes,
    );
    let dst_profile_networks = parse(strings, ENV_DESTINATION_PROFILE_NETWORKS, parse_networks);

    let initial_stream_window_size = parse(strings, ENV_INITIAL_STREAM_WINDOW_SIZE, parse_number);
    let initial_connection_window_size =
        parse(strings, ENV_INITIAL_CONNECTION_WINDOW_SIZE, parse_number);

    let tap = parse_tap_config(strings);

    let h2_settings = h2::Settings {
        initial_stream_window_size: Some(
            initial_stream_window_size?.unwrap_or(DEFAULT_INITIAL_STREAM_WINDOW_SIZE),
        ),
        initial_connection_window_size: Some(
            initial_connection_window_size?.unwrap_or(DEFAULT_INITIAL_CONNECTION_WINDOW_SIZE),
        ),
        ..Default::default()
    };

    let dst_profile_suffixes = dst_profile_suffixes?
        .unwrap_or_else(|| parse_dns_suffixes(DEFAULT_DESTINATION_PROFILE_SUFFIXES).unwrap());
    let dst_profile_networks = dst_profile_networks?.unwrap_or_default();

    let inbound_ips = {
        let ips = parse(strings, ENV_INBOUND_IPS, parse_ip_set)?.unwrap_or_default();
        if ips.is_empty() {
            info!("`{ENV_INBOUND_IPS}` allowlist not configured, allowing all target addresses",);
        } else {
            debug!(allowed = ?ips, "Only allowing connections targeting `{ENV_INBOUND_IPS}`");
        }
        std::sync::Arc::new(ips)
    };

    let outbound = {
        let ingress_mode = parse(strings, ENV_INGRESS_MODE, parse_bool)?.unwrap_or(false);

        // Instances can opt out of receiving informational headers by setting this configuration.
        // These headers are also omitted by default if ingress-mode is enabled.
        let disable_headers = parse(
            strings,
            ENV_OUTBOUND_DISABLE_INFORMATIONAL_HEADERS,
            parse_bool,
        )?
        .unwrap_or(ingress_mode);

        let addr = ListenAddr(
            outbound_listener_addr?
                .unwrap_or_else(|| parse_socket_addr(DEFAULT_OUTBOUND_LISTEN_ADDR).unwrap()),
        );
        let keepalive = Keepalive(outbound_accept_keepalive?);
        let server = ServerConfig {
            addr,
            keepalive,
            h2_settings,
        };
        let discovery_idle_timeout =
            outbound_discovery_idle_timeout?.unwrap_or(DEFAULT_OUTBOUND_DISCOVERY_IDLE_TIMEOUT);
        let max_idle =
            outbound_max_idle_per_endpoint?.unwrap_or(DEFAULT_OUTBOUND_MAX_IDLE_CONNS_PER_ENDPOINT);
        let keepalive = Keepalive(outbound_connect_keepalive?);
        let connection_pool_timeout = parse(
            strings,
            ENV_OUTBOUND_HTTP1_CONNECTION_POOL_IDLE_TIMEOUT,
            parse_duration,
        )?;

        let connect = ConnectConfig {
            keepalive,
            timeout: outbound_connect_timeout?.unwrap_or(DEFAULT_OUTBOUND_CONNECT_TIMEOUT),
            backoff: parse_backoff(
                strings,
                OUTBOUND_CONNECT_BASE,
                DEFAULT_OUTBOUND_CONNECT_BACKOFF,
            )?,
            h2_settings,
            h1_settings: h1::PoolSettings {
                max_idle,
                idle_timeout: connection_pool_timeout
                    .unwrap_or(DEFAULT_OUTBOUND_HTTP1_CONNECTION_POOL_IDLE_TIMEOUT),
            },
        };

        let detect_protocol_timeout =
            outbound_detect_timeout?.unwrap_or(DEFAULT_OUTBOUND_DETECT_TIMEOUT);

        let tcp_queue_capacity =
            outbound_tcp_queue_capacity?.unwrap_or(DEFAULT_OUTBOUND_TCP_QUEUE_CAPACITY);
        let tcp_failfast_timeout =
            outbound_tcp_failfast_timeout?.unwrap_or(DEFAULT_OUTBOUND_TCP_FAILFAST_TIMEOUT);
        let http_queue_capacity =
            outbound_http_queue_capacity?.unwrap_or(DEFAULT_OUTBOUND_HTTP_QUEUE_CAPACITY);
        let http_failfast_timeout =
            outbound_http_failfast_timeout?.unwrap_or(DEFAULT_OUTBOUND_HTTP_FAILFAST_TIMEOUT);

        outbound::Config {
            ingress_mode,
            emit_headers: !disable_headers,
            allow_discovery: AddrMatch::new(dst_profile_suffixes.clone(), dst_profile_networks),
            proxy: ProxyConfig {
                server,
                connect,
                max_in_flight_requests: outbound_max_in_flight?
                    .unwrap_or(DEFAULT_OUTBOUND_MAX_IN_FLIGHT),
                detect_protocol_timeout,
            },
            inbound_ips: inbound_ips.clone(),
            discovery_idle_timeout,
            tcp_connection_queue: QueueConfig {
                capacity: tcp_queue_capacity,
                failfast_timeout: tcp_failfast_timeout,
            },
            http_request_queue: QueueConfig {
                capacity: http_queue_capacity,
                failfast_timeout: http_failfast_timeout,
            },
        }
    };

    let gateway = gateway::Config {
        allow_discovery: gateway_suffixes?.into_iter().flatten().collect(),
    };

    let admin_listener_addr = admin_listener_addr?
        .unwrap_or_else(|| parse_socket_addr(DEFAULT_ADMIN_LISTEN_ADDR).unwrap());

    let inbound = {
        let addr = ListenAddr(
            inbound_listener_addr?
                .unwrap_or_else(|| parse_socket_addr(DEFAULT_INBOUND_LISTEN_ADDR).unwrap()),
        );
        let keepalive = Keepalive(inbound_accept_keepalive?);
        let server = ServerConfig {
            addr,
            keepalive,
            h2_settings,
        };
        let discovery_idle_timeout =
            inbound_discovery_idle_timeout?.unwrap_or(DEFAULT_INBOUND_DISCOVERY_IDLE_TIMEOUT);
        let max_idle =
            inbound_max_idle_per_endpoint?.unwrap_or(DEFAULT_INBOUND_MAX_IDLE_CONNS_PER_ENDPOINT);
        let connection_pool_timeout = parse(
            strings,
            ENV_INBOUND_HTTP1_CONNECTION_POOL_IDLE_TIMEOUT,
            parse_duration,
        )?
        .unwrap_or(DEFAULT_INBOUND_HTTP1_CONNECTION_POOL_IDLE_TIMEOUT);
        let keepalive = Keepalive(inbound_connect_keepalive?);
        let connect = ConnectConfig {
            keepalive,
            timeout: inbound_connect_timeout?.unwrap_or(DEFAULT_INBOUND_CONNECT_TIMEOUT),
            backoff: parse_backoff(
                strings,
                INBOUND_CONNECT_BASE,
                DEFAULT_INBOUND_CONNECT_BACKOFF,
            )?,
            h2_settings,
            h1_settings: h1::PoolSettings {
                max_idle,
                idle_timeout: connection_pool_timeout,
            },
        };

        let detect_protocol_timeout =
            inbound_detect_timeout?.unwrap_or(DEFAULT_INBOUND_DETECT_TIMEOUT);

        // Ensure that connections that directly target the inbound port are secured (unless
        // identity is disabled).
        let policy = {
            let inbound_port = server.addr.port();

            let cluster_nets = parse(strings, ENV_POLICY_CLUSTER_NETWORKS, parse_networks)?
                .unwrap_or_else(|| {
                    info!(
                        "{ENV_POLICY_CLUSTER_NETWORKS} not set; cluster-scoped modes are unsupported",
                    );
                    Default::default()
                });

            // We always configure a default policy. This policy applies when no other policy is
            // configured, especially when the port is not documented in via `ENV_INBOUND_PORTS`.
            let default = parse(strings, ENV_INBOUND_DEFAULT_POLICY, |s| {
                parse_default_policy(s, cluster_nets, detect_protocol_timeout)
            })?
            .unwrap_or_else(|| {
                warn!(
                    "{} was not set; using `all-unauthenticated`",
                    ENV_INBOUND_DEFAULT_POLICY
                );
                inbound::policy::defaults::all_unauthenticated(detect_protocol_timeout).into()
            });

            // Load the the set of all known inbound ports to be discovered
            // eagerly during initialization.
            let mut ports = match parse(strings, ENV_INBOUND_PORTS, parse_port_range_set)? {
                Some(ports) => ports.into_iter().flatten().collect::<HashSet<_>>(),
                None => {
                    debug!("No inbound ports specified via {ENV_INBOUND_PORTS}",);
                    Default::default()
                }
            };
            if !gateway.allow_discovery.is_empty() {
                // Add the inbound port to the set of ports to be discovered if the proxy is
                // configured as a gateway. If there are no suffixes configured in the
                // gateway, it's not worth maintaining the extra policy watch (which will be
                // the more common case).
                ports.insert(inbound_port);
            }

            // Ensure that the admin server port is included in policy discovery.
            ports.insert(admin_listener_addr.port());

            // Determine any pre-configured opaque ports.
            let opaque_ports = parse(
                strings,
                ENV_INBOUND_PORTS_DISABLE_PROTOCOL_DETECTION,
                parse_port_range_set,
            )?
            // If the `INBOUND_PORTS_DISABLE_PROTOCOL_DETECTION` environment
            // variable is not set, then there are no default opaque ports,
            // and that's fine.
            .unwrap_or_default();

            inbound::policy::Config::Discover {
                default,
                ports,
                cache_max_idle_age: discovery_idle_timeout,
                opaque_ports,
            }
        };

        inbound::Config {
            allow_discovery: dst_profile_suffixes.into_iter().collect(),
            proxy: ProxyConfig {
                server,
                connect,
                max_in_flight_requests: inbound_max_in_flight?
                    .unwrap_or(DEFAULT_INBOUND_MAX_IN_FLIGHT),
                detect_protocol_timeout,
            },
            policy,
            profile_skip_timeout: dst_profile_skip_timeout?
                .unwrap_or(DEFAULT_DESTINATION_PROFILE_SKIP_TIMEOUT),
            allowed_ips: inbound_ips.into(),

            discovery_idle_timeout,
            http_request_queue: QueueConfig {
                capacity: inbound_http_queue_capacity?
                    .unwrap_or(DEFAULT_INBOUND_HTTP_QUEUE_CAPACITY),
                failfast_timeout: inbound_http_failfast_timeout?
                    .unwrap_or(DEFAULT_INBOUND_HTTP_FAILFAST_TIMEOUT),
            },
        }
    };

    let dst = {
        let addr = dst_addr?.ok_or(EnvError::NoDestinationAddress)?;
        let connect = if addr.addr.is_loopback() {
            inbound.proxy.connect.clone()
        } else {
            outbound.proxy.connect.clone()
        };
        let failfast_timeout = if addr.addr.is_loopback() {
            inbound.http_request_queue.failfast_timeout
        } else {
            outbound.http_request_queue.failfast_timeout
        };
        let limits = addr
            .addr
            .is_loopback()
            .then(ReceiveLimits::default)
            .unwrap_or(control_receive_limits);
        super::dst::Config {
            context: dst_token?.unwrap_or_default(),
            control: ControlConfig {
                addr,
                connect,
                buffer: QueueConfig {
                    capacity: DEFAULT_CONTROL_QUEUE_CAPACITY,
                    failfast_timeout,
                },
            },
            limits,
        }
    };

    let policy = {
        let addr =
            parse_control_addr(strings, ENV_POLICY_SVC_BASE)?.ok_or(EnvError::NoPolicyAddress)?;
        // The workload, which is opaque from the proxy's point-of-view, is sent to the
        // policy controller to support policy discovery.
        let workload = strings.get(ENV_POLICY_WORKLOAD)?.ok_or_else(|| {
            error!("{ENV_POLICY_WORKLOAD} must be set with {ENV_POLICY_SVC_BASE}_ADDR",);
            EnvError::InvalidEnvVar
        })?;

        let limits = addr
            .addr
            .is_loopback()
            .then(ReceiveLimits::default)
            .unwrap_or(control_receive_limits);

        let control = {
            let connect = if addr.addr.is_loopback() {
                inbound.proxy.connect.clone()
            } else {
                outbound.proxy.connect.clone()
            };
            ControlConfig {
                addr,
                connect,
                buffer: QueueConfig {
                    capacity: DEFAULT_CONTROL_QUEUE_CAPACITY,
                    failfast_timeout: DEFAULT_CONTROL_FAILFAST_TIMEOUT,
                },
            }
        };

        policy::Config {
            control,
            workload,
            limits,
        }
    };

    let admin = super::admin::Config {
        metrics_retain_idle: metrics_retain_idle?.unwrap_or(DEFAULT_METRICS_RETAIN_IDLE),
        server: ServerConfig {
            addr: ListenAddr(admin_listener_addr),
            keepalive: inbound.proxy.server.keepalive,
            h2_settings,
        },

        // TODO(ver) Currently we always enable profiling when the pprof feature
        // is enabled. In the future, this should be driven by runtime
        // configuration.
        #[cfg(feature = "pprof")]
        enable_profiling: true,
    };

    let dns = dns::Config {
        min_ttl: dns_min_ttl?,
        max_ttl: dns_max_ttl?,
        resolv_conf_path: resolv_conf_path?
            .unwrap_or_else(|| DEFAULT_RESOLV_CONF.into())
            .into(),
    };

    let oc_collector = match trace_collector_addr? {
        None => oc_collector::Config::Disabled,
        Some(addr) => {
            let connect = if addr.addr.is_loopback() {
                inbound.proxy.connect.clone()
            } else {
                outbound.proxy.connect.clone()
            };
            let failfast_timeout = if addr.addr.is_loopback() {
                inbound.http_request_queue.failfast_timeout
            } else {
                outbound.http_request_queue.failfast_timeout
            };
            let attributes = oc_attributes_file_path
                .map(|path| match path {
                    Some(path) => oc_trace_attributes(path),
                    None => HashMap::new(),
                })
                .unwrap_or_default();

            oc_collector::Config::Enabled(Box::new(oc_collector::EnabledConfig {
                attributes,
                hostname: hostname?,
                control: ControlConfig {
                    addr,
                    connect,
                    buffer: QueueConfig {
                        capacity: DEFAULT_CONTROL_QUEUE_CAPACITY,
                        failfast_timeout,
                    },
                },
            }))
        }
    };

    let tap = tap?
        .map(|(addr, ids)| super::tap::Config::Enabled {
            permitted_client_ids: ids,
            config: ServerConfig {
                addr: ListenAddr(addr),
                keepalive: inbound.proxy.server.keepalive,
                h2_settings,
            },
        })
        .unwrap_or(super::tap::Config::Disabled);

    let identity = {
        let tls = tls?;

        match strings.get(ENV_IDENTITY_SPIRE_SOCKET)? {
            Some(socket) => match &tls.id {
                // TODO: perform stricter SPIFFE ID validation following:
                // https://github.com/spiffe/spiffe/blob/27b59b81ba8c56885ac5d4be73b35b9b3305fd7a/standards/SPIFFE-ID.md
                identity::Id::Uri(uri)
                    if uri.scheme().eq_ignore_ascii_case(SPIFFE_ID_URI_SCHEME) =>
                {
                    identity::Config::Spire {
                        tls,
                        client: spire::Config {
                            socket_addr: std::sync::Arc::new(socket),
                            backoff: parse_backoff(
                                strings,
                                IDENTITY_SPIRE_BASE,
                                DEFAULT_SPIRE_BACKOFF,
                            )?,
                        },
                    }
                }
                _ => {
                    error!("Spire support requires a SPIFFE TLS Id");
                    return Err(EnvError::InvalidEnvVar);
                }
            },
            None => {
                let (addr, certify) = parse_linkerd_identity_config(strings)?;

                // If the address doesn't have a server identity, then we're on localhost.
                let connect = if addr.addr.is_loopback() {
                    inbound.proxy.connect.clone()
                } else {
                    outbound.proxy.connect.clone()
                };
                let failfast_timeout = if addr.addr.is_loopback() {
                    inbound.http_request_queue.failfast_timeout
                } else {
                    outbound.http_request_queue.failfast_timeout
                };

                identity::Config::Linkerd {
                    certify,
                    tls,
                    client: ControlConfig {
                        addr,
                        connect,
                        buffer: QueueConfig {
                            capacity: DEFAULT_CONTROL_QUEUE_CAPACITY,
                            failfast_timeout,
                        },
                    },
                }
            }
        }
    };

    Ok(super::Config {
        admin,
        dns,
        dst,
        tap,
        oc_collector,
        policy,
        identity,
        outbound,
        gateway,
        inbound,
        shutdown_grace_period: shutdown_grace_period?.unwrap_or(DEFAULT_SHUTDOWN_GRACE_PERIOD),
    })
}

fn oc_trace_attributes(oc_attributes_file_path: String) -> HashMap<String, String> {
    match fs::read_to_string(oc_attributes_file_path.clone()) {
        Ok(attributes_string) => convert_attributes_string_to_map(attributes_string),
        Err(err) => {
            warn!(
                "could not read OC trace attributes file at {}: {}",
                oc_attributes_file_path, err
            );
            HashMap::new()
        }
    }
}

fn convert_attributes_string_to_map(attributes: String) -> HashMap<String, String> {
    attributes
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(2, '=');
            parts.next().and_then(move |key| {
                parts.next().map(move |val|
                // Trim double quotes in value, present by default when attached through k8s downwardAPI
                (key.to_string(), val.trim_matches('"').to_string()))
            })
        })
        .collect()
}

fn mk_control_receive_limits(env: &dyn Strings) -> Result<ReceiveLimits, EnvError> {
    const ENV_INIT: &str = "LINKERD2_PROXY_CONTROL_STREAM_INITIAL_TIMEOUT";
    const ENV_IDLE: &str = "LINKERD2_PROXY_CONTROL_STREAM_IDLE_TIMEOUT";
    const ENV_LIFE: &str = "LINKERD2_PROXY_CONTROL_STREAM_LIFETIME";

    let initial = parse(env, ENV_INIT, parse_duration_opt)?.flatten();
    let idle = parse(env, ENV_IDLE, parse_duration_opt)?.flatten();
    let lifetime = parse(env, ENV_LIFE, parse_duration_opt)?.flatten();

    if initial.unwrap_or(Duration::ZERO) > idle.unwrap_or(Duration::MAX) {
        error!("{ENV_INIT} must be less than {ENV_IDLE}");
        return Err(EnvError::InvalidEnvVar);
    }
    if initial.unwrap_or(Duration::ZERO) > lifetime.unwrap_or(Duration::MAX) {
        error!("{ENV_INIT} must be less than {ENV_LIFE}");
        return Err(EnvError::InvalidEnvVar);
    }
    if idle.unwrap_or(Duration::ZERO) > lifetime.unwrap_or(Duration::MAX) {
        error!("{ENV_IDLE} must be less than {ENV_LIFE}");
        return Err(EnvError::InvalidEnvVar);
    }

    Ok(ReceiveLimits {
        initial,
        idle,
        lifetime,
    })
}

// === impl Env ===

impl Strings for Env {
    fn get(&self, key: &str) -> Result<Option<String>, EnvError> {
        use std::env;

        match env::var(key) {
            Ok(value) => Ok(Some(value)),
            Err(env::VarError::NotPresent) => Ok(None),
            Err(env::VarError::NotUnicode(_)) => {
                error!("{key} is not encoded in Unicode");
                Err(EnvError::InvalidEnvVar)
            }
        }
    }
}

impl Env {
    pub fn try_config(&self) -> Result<super::Config, EnvError> {
        parse_config(self)
    }
}

// === Parsing ===

/// There is a dependency on identity being enabled for tap to work. The
/// status of tap is determined by the ENV_TAP_SVC_NAME env variable being set
/// or not set.
///
/// - If identity is disabled, tap is disabled, but a warning is issued if
///   ENV_TAP_SVC_NAME is set.
/// - If identity is enabled, the status of tap is determined by
///   ENV_TAP_SVC_NAME.
fn parse_tap_config(
    strings: &dyn Strings,
) -> Result<Option<(SocketAddr, HashSet<tls::server::ClientId>)>, EnvError> {
    let tap_identity = parse(strings, ENV_TAP_SVC_NAME, parse_identity)?;
    let addr = parse(strings, ENV_CONTROL_LISTEN_ADDR, parse_socket_addr)?
        .unwrap_or_else(|| parse_socket_addr(DEFAULT_CONTROL_LISTEN_ADDR).unwrap());
    if let Some(id) = tap_identity {
        return Ok(Some((
            addr,
            vec![id].into_iter().map(tls::ClientId).collect(),
        )));
    }
    Ok(None)
}

fn parse_bool(s: &str) -> Result<bool, ParseError> {
    s.parse().map_err(Into::into)
}

fn parse_number<T>(s: &str) -> Result<T, ParseError>
where
    T: FromStr,
    ParseError: From<T::Err>,
{
    s.parse().map_err(Into::into)
}

fn parse_duration_opt(s: &str) -> Result<Option<Duration>, ParseError> {
    if s.is_empty() {
        return Ok(None);
    }
    parse_duration(s).map(Some)
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
            error!("Expected IP:PORT; found: {s}");
            Err(ParseError::HostIsNotAnIpAddress)
        }
    }
}

fn parse_ip_set(s: &str) -> Result<HashSet<IpAddr>, ParseError> {
    s.split(',')
        .map(|s| s.parse::<IpAddr>().map_err(Into::into))
        .collect()
}

fn parse_addr(s: &str) -> Result<Addr, ParseError> {
    Addr::from_str(s).map_err(|e| {
        error!("Not a valid address: {s}");
        ParseError::AddrError(e)
    })
}

fn parse_port_range_set(s: &str) -> Result<RangeInclusiveSet<u16>, ParseError> {
    let mut set = RangeInclusiveSet::new();
    if !s.is_empty() {
        for part in s.split(',') {
            let part = part.trim();
            // Ignore empty list entries
            if part.is_empty() {
                continue;
            }

            let mut parts = part.splitn(2, '-');
            let low = parts
                .next()
                .ok_or_else(|| {
                    error!("Not a valid port range: {part}");
                    ParseError::NotAPortRange
                })?
                .trim();
            let low = parse_number::<u16>(low)?;
            if let Some(high) = parts.next() {
                let high = high.trim();
                let high = parse_number::<u16>(high).map_err(|e| {
                    error!("Not a valid port range: {part}");
                    e
                })?;
                if high < low {
                    error!("Not a valid port range: {part}; {high} is greater than {low}");
                    return Err(ParseError::NotAPortRange);
                }
                set.insert(low..=high);
            } else {
                set.insert(low..=low);
            }
        }
    }
    Ok(set)
}

pub(super) fn parse_dns_name(s: &str) -> Result<dns::Name, ParseError> {
    s.parse().map_err(|_| {
        error!("Not a valid identity name: {s}");
        ParseError::NameError
    })
}

pub(super) fn parse_identity(s: &str) -> Result<identity::Id, ParseError> {
    s.parse().map_err(|_| {
        error!("Not a valid identity name: {s}");
        ParseError::NameError
    })
}

pub(super) fn parse<T, Parse>(
    strings: &dyn Strings,
    name: &str,
    parse: Parse,
) -> Result<Option<T>, EnvError>
where
    Parse: FnOnce(&str) -> Result<T, ParseError>,
{
    match strings.get(name)? {
        Some(ref s) => {
            let r = parse(s).map_err(|parse_error| {
                error!("{name}={s:?} is not valid: {parse_error:?}");
                EnvError::InvalidEnvVar
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
) -> Result<Option<T>, EnvError>
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

fn parse_dns_suffixes(list: &str) -> Result<HashSet<dns::Suffix>, ParseError> {
    let mut suffixes = HashSet::new();
    for item in list.split(',') {
        let item = item.trim();
        if !item.is_empty() {
            let sfx = parse_dns_suffix(item)?;
            suffixes.insert(sfx);
        }
    }

    Ok(suffixes)
}

fn parse_dns_suffix(s: &str) -> Result<dns::Suffix, ParseError> {
    if s == "." {
        return Ok(dns::Suffix::Root);
    }

    dns::Suffix::from_str(s).map_err(|_| ParseError::NotADomainSuffix)
}

fn parse_networks(list: &str) -> Result<HashSet<IpNet>, ParseError> {
    let mut nets = HashSet::new();
    for input in list.split(',') {
        let input = input.trim();
        if !input.is_empty() {
            let net = IpNet::from_str(input).map_err(|error| {
                error!(%input, %error, "Invalid network");
                ParseError::NotANetwork
            })?;
            nets.insert(net);
        }
    }
    Ok(nets)
}

fn parse_default_policy(
    s: &str,
    cluster_nets: HashSet<IpNet>,
    detect_timeout: Duration,
) -> Result<inbound::policy::DefaultPolicy, ParseError> {
    match s {
        "deny" => Ok(inbound::policy::DefaultPolicy::Deny),
        "all-authenticated" => {
            Ok(inbound::policy::defaults::all_authenticated(detect_timeout).into())
        }
        "all-unauthenticated" => {
            Ok(inbound::policy::defaults::all_unauthenticated(detect_timeout).into())
        }

        // If cluster networks are configured, support cluster-scoped default policies.
        name if cluster_nets.is_empty() => Err(ParseError::InvalidPortPolicy(name.to_string())),
        "cluster-authenticated" => Ok(inbound::policy::defaults::cluster_authenticated(
            cluster_nets,
            detect_timeout,
        )
        .into()),
        "cluster-unauthenticated" => Ok(inbound::policy::defaults::cluster_unauthenticated(
            cluster_nets,
            detect_timeout,
        )
        .into()),

        name => Err(ParseError::InvalidPortPolicy(name.to_string())),
    }
}
pub fn parse_backoff<S: Strings>(
    strings: &S,
    base: &str,
    default: ExponentialBackoff,
) -> Result<ExponentialBackoff, EnvError> {
    let min_env = format!("LINKERD2_PROXY_{}_EXP_BACKOFF_MIN", base);
    let min = parse(strings, &min_env, parse_duration);
    let max_env = format!("LINKERD2_PROXY_{}_EXP_BACKOFF_MAX", base);
    let max = parse(strings, &max_env, parse_duration);
    let jitter_env = format!("LINKERD2_PROXY_{}_EXP_BACKOFF_JITTER", base);
    let jitter = parse(strings, &jitter_env, parse_number::<f64>);

    match (min?, max?, jitter?) {
        (None, None, None) => Ok(default),
        (Some(min), Some(max), jitter) => {
            ExponentialBackoff::try_new(min, max, jitter.unwrap_or_default()).map_err(|error| {
                error!(message="Invalid backoff", %error, %min_env, ?min, %max_env, ?max, %jitter_env, ?jitter);
                EnvError::InvalidEnvVar
            })
        }
        _ => {
            error!("You need to specify either all of {min_env} {max_env} {jitter_env} or none of them to use the default backoff");
            Err(EnvError::InvalidEnvVar)
        }
    }
}

pub fn parse_control_addr<S: Strings>(
    strings: &S,
    base: &str,
) -> Result<Option<ControlAddr>, EnvError> {
    let a = parse(strings, &format!("{}_ADDR", base), parse_addr)?;
    let n = parse(strings, &format!("{}_NAME", base), parse_dns_name)?;
    match (a, n) {
        (None, None) => Ok(None),
        (Some(ref addr), _) if addr.is_loopback() => Ok(Some(ControlAddr {
            addr: addr.clone(),
            identity: Conditional::None(tls::NoClientTls::Loopback),
        })),
        (Some(addr), Some(name)) => Ok(Some(ControlAddr {
            addr,
            identity: Conditional::Some(tls::ClientTls::new(
                tls::ServerId(name.clone().into()),
                tls::ServerName(name),
            )),
        })),
        _ => {
            error!("{base}_ADDR and {base}_NAME must be specified together");
            Err(EnvError::InvalidEnvVar)
        }
    }
}

pub fn parse_tls_params<S: Strings>(strings: &S) -> Result<identity::TlsParams, EnvError> {
    let ta = parse(strings, ENV_IDENTITY_TRUST_ANCHORS, |s| {
        if s.is_empty() {
            return Err(ParseError::InvalidTrustAnchors);
        }
        Ok(s.to_string())
    });

    // The assumtion here is that if `ENV_IDENTITY_IDENTITY_LOCAL_NAME` has been set
    // we will use that for both tls id and server name.
    let (server_id_env_var, server_name_env_var) =
        if strings.get(ENV_IDENTITY_IDENTITY_LOCAL_NAME)?.is_some() {
            (
                ENV_IDENTITY_IDENTITY_LOCAL_NAME,
                ENV_IDENTITY_IDENTITY_LOCAL_NAME,
            )
        } else {
            (
                ENV_IDENTITY_IDENTITY_SERVER_ID,
                ENV_IDENTITY_IDENTITY_SERVER_NAME,
            )
        };

    let server_id = parse(strings, server_id_env_var, parse_identity);
    let server_name = parse(strings, server_name_env_var, parse_dns_name);

    if strings
        .get(ENV_IDENTITY_DISABLED)?
        .map(|d| !d.is_empty())
        .unwrap_or(false)
    {
        error!("{ENV_IDENTITY_DISABLED} is no longer supported. Identity is must be enabled.");
        return Err(EnvError::InvalidEnvVar);
    }

    match (ta?, server_id?, server_name?) {
        (Some(trust_anchors_pem), Some(server_id), Some(server_name)) => {
            let params = identity::TlsParams {
                id: server_id,
                server_name,
                trust_anchors_pem,
            };
            Ok(params)
        }
        (trust_anchors_pem, server_id, server_name) => {
            for (unset, name) in &[
                (trust_anchors_pem.is_none(), ENV_IDENTITY_TRUST_ANCHORS),
                (server_id.is_none(), server_id_env_var),
                (server_name.is_none(), server_name_env_var),
            ] {
                if *unset {
                    error!("{} must be set.", name);
                }
            }
            Err(EnvError::InvalidEnvVar)
        }
    }
}

pub fn parse_linkerd_identity_config<S: Strings>(
    strings: &S,
) -> Result<(ControlAddr, identity::client::linkerd::Config), EnvError> {
    let control = parse_control_addr(strings, ENV_IDENTITY_SVC_BASE);
    let dir = parse(strings, ENV_IDENTITY_DIR, |ref s| Ok(PathBuf::from(s)));
    let tok = parse(strings, ENV_IDENTITY_TOKEN_FILE, |ref s| {
        identity::client::linkerd::TokenSource::if_nonempty_file(s.to_string()).map_err(|e| {
            error!("Could not read {ENV_IDENTITY_TOKEN_FILE}: {e}");
            ParseError::InvalidTokenSource
        })
    });

    let min_refresh = parse(strings, ENV_IDENTITY_MIN_REFRESH, parse_duration);
    let max_refresh = parse(strings, ENV_IDENTITY_MAX_REFRESH, parse_duration);

    match (control?, dir?, tok?, min_refresh?, max_refresh?) {
        (Some(control), Some(dir), Some(token), min_refresh, max_refresh) => {
            let certify = identity::client::linkerd::Config {
                token,
                min_refresh: min_refresh.unwrap_or(DEFAULT_IDENTITY_MIN_REFRESH),
                max_refresh: max_refresh.unwrap_or(DEFAULT_IDENTITY_MAX_REFRESH),
                documents: identity::client::linkerd::certify::Documents::load(dir).map_err(
                    |error| {
                        error!(%error, "Failed to read identity documents");
                        EnvError::InvalidEnvVar
                    },
                )?,
            };

            Ok((control, certify))
        }
        (addr, end_entity_dir, token, _minr, _maxr) => {
            let s = format!("{0}_ADDR and {0}_NAME", ENV_IDENTITY_SVC_BASE);
            let svc_env: &str = s.as_str();
            for (unset, name) in &[
                (addr.is_none(), svc_env),
                (end_entity_dir.is_none(), ENV_IDENTITY_DIR),
                (token.is_none(), ENV_IDENTITY_TOKEN_FILE),
            ] {
                if *unset {
                    error!("{name} must be set.");
                }
            }
            Err(EnvError::InvalidEnvVar)
        }
    }
}

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
        test_unit("ms", Duration::from_millis);
    }

    #[test]
    fn parse_duration_unit_s() {
        test_unit("s", Duration::from_secs);
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
        assert!(matches!(
            parse_duration("123456789012345678901234567890ms"),
            Err(ParseError::NotAnInteger(_))
        ));
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
    fn convert_attributes_string_to_map_different_values() {
        let attributes_string = "\
            cluster=\"test-cluster1\"\n\
            rack=\"rack-22\"\n\
            zone=us-est-coast\n\
            linkerd.io/control-plane-component=\"controller\"\n\
            linkerd.io/proxy-deployment=\"linkerd-controller\"\n\
            workload=\n\
            kind=\"\"\n\
            key1=\"=\"\n\
            key2==value2\n\
            key3\n\
            =key4\n\
            "
        .to_string();

        let expected = [
            ("cluster".to_string(), "test-cluster1".to_string()),
            ("rack".to_string(), "rack-22".to_string()),
            ("zone".to_string(), "us-est-coast".to_string()),
            (
                "linkerd.io/control-plane-component".to_string(),
                "controller".to_string(),
            ),
            (
                "linkerd.io/proxy-deployment".to_string(),
                "linkerd-controller".to_string(),
            ),
            ("workload".to_string(), "".to_string()),
            ("kind".to_string(), "".to_string()),
            ("key1".to_string(), "=".to_string()),
            ("key2".to_string(), "=value2".to_string()),
            ("".to_string(), "key4".to_string()),
        ]
        .iter()
        .cloned()
        .collect();

        assert_eq!(
            convert_attributes_string_to_map(attributes_string),
            expected
        );
    }

    #[test]
    fn dns_suffixes() {
        fn p(s: &str) -> Result<Vec<String>, ParseError> {
            let mut sfxs = parse_dns_suffixes(s)?
                .into_iter()
                .map(|s| format!("{}", s))
                .collect::<Vec<_>>();
            sfxs.sort();
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

    #[test]
    fn ip_sets() {
        let ips = &[
            IpAddr::from([127, 0, 0, 1]),
            IpAddr::from([10, 0, 2, 42]),
            IpAddr::from([192, 168, 0, 69]),
        ];
        assert_eq!(
            parse_ip_set("127.0.0.1"),
            Ok(ips[..1].iter().cloned().collect())
        );
        assert_eq!(
            parse_ip_set("127.0.0.1,10.0.2.42"),
            Ok(ips[..2].iter().cloned().collect())
        );
        assert_eq!(
            parse_ip_set("127.0.0.1,10.0.2.42,192.168.0.69"),
            Ok(ips[..3].iter().cloned().collect())
        );
        assert!(parse_ip_set("blaah").is_err());
        assert!(parse_ip_set("10.4.0.555").is_err());
        assert!(parse_ip_set("10.4.0.3,foobar,192.168.0.69").is_err());
        assert!(parse_ip_set("10.0.1.1/24").is_err());
    }

    #[test]
    fn ranges() {
        fn set(
            ranges: impl IntoIterator<Item = std::ops::RangeInclusive<u16>>,
        ) -> Result<RangeInclusiveSet<u16>, ParseError> {
            Ok(ranges.into_iter().collect())
        }

        assert_eq!(dbg!(parse_port_range_set("1-65535")), set([1..=65535]));
        assert_eq!(
            dbg!(parse_port_range_set("1-2,42-420")),
            set([1..=2, 42..=420])
        );
        assert_eq!(
            dbg!(parse_port_range_set("1-2,42,80-420")),
            set([1..=2, 42..=42, 80..=420])
        );
        assert_eq!(
            dbg!(parse_port_range_set("1,20,30,40")),
            set([1..=1, 20..=20, 30..=30, 40..=40])
        );
        assert_eq!(
            dbg!(parse_port_range_set("40,30,20,1")),
            set([1..=1, 20..=20, 30..=30, 40..=40])
        );
        // ignores empty list entries
        assert_eq!(dbg!(parse_port_range_set("1,,,,2")), set([1..=1, 2..=2]));
        // ignores rando whitespace
        assert_eq!(
            dbg!(parse_port_range_set("1, 2,\t3- 5")),
            set([1..=1, 2..=2, 3..=5])
        );
        assert_eq!(dbg!(parse_port_range_set("1, , 2,")), set([1..=1, 2..=2]));

        // non-numeric strings
        assert!(dbg!(parse_port_range_set("asdf")).is_err());
        assert!(dbg!(parse_port_range_set("80, 443, http")).is_err());
        assert!(dbg!(parse_port_range_set("80,http-443")).is_err());

        // backwards ranges
        assert!(dbg!(parse_port_range_set("80-79")).is_err());
        assert!(dbg!(parse_port_range_set("1,2,5-2")).is_err());

        // malformed (half-open) ranges
        assert!(dbg!(parse_port_range_set("-1")).is_err());
        assert!(dbg!(parse_port_range_set("1,2-,50")).is_err());

        // not a u16
        assert!(dbg!(parse_port_range_set("69420")).is_err());
        assert!(dbg!(parse_port_range_set("1-69420")).is_err());
    }

    #[test]
    fn control_stream_limits() {
        impl Strings for HashMap<&'static str, &'static str> {
            fn get(&self, key: &str) -> Result<Option<String>, EnvError> {
                Ok(self.get(key).map(ToString::to_string))
            }
        }

        let mut env = HashMap::default();
        env.insert("LINKERD2_PROXY_CONTROL_STREAM_INITIAL_TIMEOUT", "1s");
        env.insert("LINKERD2_PROXY_CONTROL_STREAM_IDLE_TIMEOUT", "2s");
        env.insert("LINKERD2_PROXY_CONTROL_STREAM_LIFETIME", "3s");
        let limits = mk_control_receive_limits(&env).unwrap();
        assert_eq!(limits.initial, Some(Duration::from_secs(1)));
        assert_eq!(limits.idle, Some(Duration::from_secs(2)));
        assert_eq!(limits.lifetime, Some(Duration::from_secs(3)));

        env.insert("LINKERD2_PROXY_CONTROL_STREAM_INITIAL_TIMEOUT", "");
        env.insert("LINKERD2_PROXY_CONTROL_STREAM_IDLE_TIMEOUT", "");
        env.insert("LINKERD2_PROXY_CONTROL_STREAM_LIFETIME", "");
        let limits = mk_control_receive_limits(&env).unwrap();
        assert_eq!(limits.initial, None);
        assert_eq!(limits.idle, None);
        assert_eq!(limits.lifetime, None);

        env.insert("LINKERD2_PROXY_CONTROL_STREAM_INITIAL_TIMEOUT", "3s");
        env.insert("LINKERD2_PROXY_CONTROL_STREAM_IDLE_TIMEOUT", "1s");
        env.insert("LINKERD2_PROXY_CONTROL_STREAM_LIFETIME", "");
        assert!(mk_control_receive_limits(&env).is_err());

        env.insert("LINKERD2_PROXY_CONTROL_STREAM_INITIAL_TIMEOUT", "3s");
        env.insert("LINKERD2_PROXY_CONTROL_STREAM_IDLE_TIMEOUT", "");
        env.insert("LINKERD2_PROXY_CONTROL_STREAM_LIFETIME", "1s");
        assert!(mk_control_receive_limits(&env).is_err());

        env.insert("LINKERD2_PROXY_CONTROL_STREAM_INITIAL_TIMEOUT", "");
        env.insert("LINKERD2_PROXY_CONTROL_STREAM_IDLE_TIMEOUT", "3s");
        env.insert("LINKERD2_PROXY_CONTROL_STREAM_LIFETIME", "1s");
        assert!(mk_control_receive_limits(&env).is_err());
    }
}
