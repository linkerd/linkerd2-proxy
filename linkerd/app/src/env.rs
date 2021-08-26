use crate::core::{
    addr,
    config::*,
    control::{Config as ControlConfig, ControlAddr},
    proxy::http::{h1, h2},
    tls,
    transport::{Keepalive, ListenAddr},
    Addr, AddrMatch, Conditional, IpNet,
};
use crate::{dns, gateway, identity, inbound, oc_collector, outbound};
use inbound::policy;
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

const ENV_INBOUND_DISPATCH_TIMEOUT: &str = "LINKERD2_PROXY_INBOUND_DISPATCH_TIMEOUT";
const ENV_OUTBOUND_DISPATCH_TIMEOUT: &str = "LINKERD2_PROXY_OUTBOUND_DISPATCH_TIMEOUT";

pub const ENV_INBOUND_DETECT_TIMEOUT: &str = "LINKERD2_PROXY_INBOUND_DETECT_TIMEOUT";
const ENV_OUTBOUND_DETECT_TIMEOUT: &str = "LINKERD2_PROXY_OUTBOUND_DETECT_TIMEOUT";

const ENV_INBOUND_CONNECT_TIMEOUT: &str = "LINKERD2_PROXY_INBOUND_CONNECT_TIMEOUT";
const ENV_OUTBOUND_CONNECT_TIMEOUT: &str = "LINKERD2_PROXY_OUTBOUND_CONNECT_TIMEOUT";

const ENV_INBOUND_ACCEPT_KEEPALIVE: &str = "LINKERD2_PROXY_INBOUND_ACCEPT_KEEPALIVE";
const ENV_OUTBOUND_ACCEPT_KEEPALIVE: &str = "LINKERD2_PROXY_OUTBOUND_ACCEPT_KEEPALIVE";

const ENV_INBOUND_CONNECT_KEEPALIVE: &str = "LINKERD2_PROXY_INBOUND_CONNECT_KEEPALIVE";
const ENV_OUTBOUND_CONNECT_KEEPALIVE: &str = "LINKERD2_PROXY_OUTBOUND_CONNECT_KEEPALIVE";

pub const ENV_BUFFER_CAPACITY: &str = "LINKERD2_PROXY_BUFFER_CAPACITY";

pub const ENV_INBOUND_ROUTER_MAX_IDLE_AGE: &str = "LINKERD2_PROXY_INBOUND_ROUTER_MAX_IDLE_AGE";
pub const ENV_OUTBOUND_ROUTER_MAX_IDLE_AGE: &str = "LINKERD2_PROXY_OUTBOUND_ROUTER_MAX_IDLE_AGE";

const ENV_INBOUND_MAX_IDLE_CONNS_PER_ENDPOINT: &str = "LINKERD2_PROXY_MAX_IDLE_CONNS_PER_ENDPOINT";
const ENV_OUTBOUND_MAX_IDLE_CONNS_PER_ENDPOINT: &str =
    "LINKERD2_PROXY_OUTBOUND_MAX_IDLE_CONNS_PER_ENDPOINT";

pub const ENV_INBOUND_MAX_IN_FLIGHT: &str = "LINKERD2_PROXY_INBOUND_MAX_IN_FLIGHT";
pub const ENV_OUTBOUND_MAX_IN_FLIGHT: &str = "LINKERD2_PROXY_OUTBOUND_MAX_IN_FLIGHT";

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
pub const ENV_IDENTITY_IDENTITY_LOCAL_NAME: &str = "LINKERD2_PROXY_IDENTITY_LOCAL_NAME";
pub const ENV_IDENTITY_TOKEN_FILE: &str = "LINKERD2_PROXY_IDENTITY_TOKEN_FILE";
pub const ENV_IDENTITY_MIN_REFRESH: &str = "LINKERD2_PROXY_IDENTITY_MIN_REFRESH";
pub const ENV_IDENTITY_MAX_REFRESH: &str = "LINKERD2_PROXY_IDENTITY_MAX_REFRESH";

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

// Default values for various configuration fields
const DEFAULT_OUTBOUND_LISTEN_ADDR: &str = "127.0.0.1:4140";
pub const DEFAULT_INBOUND_LISTEN_ADDR: &str = "0.0.0.0:4143";
pub const DEFAULT_CONTROL_LISTEN_ADDR: &str = "0.0.0.0:4190";
const DEFAULT_ADMIN_LISTEN_ADDR: &str = "127.0.0.1:4191";
const DEFAULT_METRICS_RETAIN_IDLE: Duration = Duration::from_secs(10 * 60);
const DEFAULT_INBOUND_DISPATCH_TIMEOUT: Duration = Duration::from_secs(1);
const DEFAULT_INBOUND_DETECT_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_INBOUND_CONNECT_TIMEOUT: Duration = Duration::from_millis(300);
const DEFAULT_INBOUND_CONNECT_BACKOFF: ExponentialBackoff = ExponentialBackoff {
    min: Duration::from_millis(100),
    max: Duration::from_millis(500),
    jitter: 0.1,
};
const DEFAULT_OUTBOUND_DISPATCH_TIMEOUT: Duration = Duration::from_secs(3);
const DEFAULT_OUTBOUND_DETECT_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_OUTBOUND_CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
const DEFAULT_OUTBOUND_CONNECT_BACKOFF: ExponentialBackoff = ExponentialBackoff {
    min: Duration::from_millis(100),
    max: Duration::from_millis(500),
    jitter: 0.1,
};
const DEFAULT_RESOLV_CONF: &str = "/etc/resolv.conf";

const DEFAULT_INITIAL_STREAM_WINDOW_SIZE: u32 = 65_535; // Protocol default
const DEFAULT_INITIAL_CONNECTION_WINDOW_SIZE: u32 = 1048576; // 1MB ~ 16 streams at capacity

// This configuration limits the amount of time Linkerd retains cached clients &
// connections.
//
// After this timeout expires, the proxy will need to re-resolve destination
// metadata. The outbound default of 5s matches Kubernetes' default DNS TTL. On
// the outbound side, especially, we want to use a limited idle timeout since
// stale clients/connections can have a severe memory impact, especially when
// the application communicates with many endpoints or at high concurrency.
//
// On the inbound side, we want to be a bit more permissive so that periodic, as
// the number of endpoints should generally be pretty constrained.
const DEFAULT_INBOUND_ROUTER_MAX_IDLE_AGE: Duration = Duration::from_secs(20);
const DEFAULT_OUTBOUND_ROUTER_MAX_IDLE_AGE: Duration = Duration::from_secs(5);

// By default, we don't limit the number of connections a connection pol may
// use, as doing so can severely impact CPU utilization for applications with
// many concurrent requests. It's generally preferable to use the MAX_IDLE_AGE
// limitations to quickly drop idle connections.
const DEFAULT_INBOUND_MAX_IDLE_CONNS_PER_ENDPOINT: usize = std::usize::MAX;
const DEFAULT_OUTBOUND_MAX_IDLE_CONNS_PER_ENDPOINT: usize = std::usize::MAX;

// These settings limit the number of requests that have not received responses,
// including those buffered in the proxy and dispatched to the destination
// service.
const DEFAULT_INBOUND_MAX_IN_FLIGHT: usize = 100_000;
const DEFAULT_OUTBOUND_MAX_IN_FLIGHT: usize = 100_000;

// This value should be large enough to admit requests without exerting
// backpressure so that requests implicitly buffer in the executor; but it
// should be small enough that callers can't force the proxy to consume an
// extreme amount of memory. Also keep in mind that there may be several buffers
// used in a given proxy, each of which assumes this capacity.
//
// The value of 10K is chosen somewhat arbitrarily, but seems high enough to
// buffer requests for high-load services.
const DEFAULT_BUFFER_CAPACITY: usize = 10_000;

const DEFAULT_DESTINATION_PROFILE_SUFFIXES: &str = "svc.cluster.local.";
const DEFAULT_DESTINATION_PROFILE_IDLE_TIMEOUT: Duration = Duration::from_millis(500);

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
    let inbound_dispatch_timeout = parse(strings, ENV_INBOUND_DISPATCH_TIMEOUT, parse_duration);
    let inbound_connect_timeout = parse(strings, ENV_INBOUND_CONNECT_TIMEOUT, parse_duration);

    let outbound_detect_timeout = parse(strings, ENV_OUTBOUND_DETECT_TIMEOUT, parse_duration);
    let outbound_dispatch_timeout = parse(strings, ENV_OUTBOUND_DISPATCH_TIMEOUT, parse_duration);
    let outbound_connect_timeout = parse(strings, ENV_OUTBOUND_CONNECT_TIMEOUT, parse_duration);

    let inbound_accept_keepalive = parse(strings, ENV_INBOUND_ACCEPT_KEEPALIVE, parse_duration);
    let outbound_accept_keepalive = parse(strings, ENV_OUTBOUND_ACCEPT_KEEPALIVE, parse_duration);

    let inbound_connect_keepalive = parse(strings, ENV_INBOUND_CONNECT_KEEPALIVE, parse_duration);
    let outbound_connect_keepalive = parse(strings, ENV_OUTBOUND_CONNECT_KEEPALIVE, parse_duration);

    let inbound_disable_ports = parse(
        strings,
        ENV_INBOUND_PORTS_DISABLE_PROTOCOL_DETECTION,
        parse_port_set,
    );

    let buffer_capacity = parse(strings, ENV_BUFFER_CAPACITY, parse_number);

    let inbound_cache_max_idle_age =
        parse(strings, ENV_INBOUND_ROUTER_MAX_IDLE_AGE, parse_duration);
    let outbound_cache_max_idle_age =
        parse(strings, ENV_OUTBOUND_ROUTER_MAX_IDLE_AGE, parse_duration);

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

    // DNS

    let resolv_conf_path = strings.get(ENV_RESOLV_CONF);

    let dns_min_ttl = parse(strings, ENV_DNS_MIN_TTL, parse_duration);
    let dns_max_ttl = parse(strings, ENV_DNS_MAX_TTL, parse_duration);

    let identity_config = parse_identity_config(strings);

    let id_disabled = identity_config
        .as_ref()
        .map(|c| c.is_none())
        .unwrap_or(false);

    let hostname = strings.get(ENV_HOSTNAME);

    let oc_attributes_file_path = strings.get(ENV_TRACE_ATTRIBUTES_PATH);

    let trace_collector_addr =
        parse_control_addr(strings, ENV_TRACE_COLLECTOR_SVC_BASE, id_disabled);

    let gateway_suffixes = parse(strings, ENV_INBOUND_GATEWAY_SUFFIXES, parse_dns_suffixes);

    let dst_addr = parse_control_addr(strings, ENV_DESTINATION_SVC_BASE, id_disabled);
    let dst_token = strings.get(ENV_DESTINATION_CONTEXT);
    let dst_profile_idle_timeout = parse(
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

    let tap = parse_tap_config(strings, id_disabled);

    let h2_settings = h2::Settings {
        initial_stream_window_size: Some(
            initial_stream_window_size?.unwrap_or(DEFAULT_INITIAL_STREAM_WINDOW_SIZE),
        ),
        initial_connection_window_size: Some(
            initial_connection_window_size?.unwrap_or(DEFAULT_INITIAL_CONNECTION_WINDOW_SIZE),
        ),
        ..Default::default()
    };

    let buffer_capacity = buffer_capacity?.unwrap_or(DEFAULT_BUFFER_CAPACITY);

    let dst_profile_suffixes = dst_profile_suffixes?
        .unwrap_or_else(|| parse_dns_suffixes(DEFAULT_DESTINATION_PROFILE_SUFFIXES).unwrap());
    let dst_profile_networks = dst_profile_networks?.unwrap_or_default();

    let inbound_ips = {
        let ips = parse(strings, ENV_INBOUND_IPS, parse_ip_set)?.unwrap_or_default();
        if ips.is_empty() {
            info!(
                "`{}` allowlist not configured, allowing all target addresses",
                ENV_INBOUND_IPS
            );
        } else {
            debug!(allowed = ?ips, "Only allowing connections targeting `{}`", ENV_INBOUND_IPS);
        }
        ips.into()
    };

    let outbound = {
        let ingress_mode = parse(strings, ENV_INGRESS_MODE, parse_bool)?.unwrap_or(false);

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
        let cache_max_idle_age =
            outbound_cache_max_idle_age?.unwrap_or(DEFAULT_OUTBOUND_ROUTER_MAX_IDLE_AGE);
        let max_idle =
            outbound_max_idle_per_endpoint?.unwrap_or(DEFAULT_OUTBOUND_MAX_IDLE_CONNS_PER_ENDPOINT);
        let keepalive = Keepalive(outbound_connect_keepalive?);
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
                idle_timeout: cache_max_idle_age,
            },
        };

        let detect_protocol_timeout =
            outbound_detect_timeout?.unwrap_or(DEFAULT_OUTBOUND_DETECT_TIMEOUT);
        let dispatch_timeout =
            outbound_dispatch_timeout?.unwrap_or(DEFAULT_OUTBOUND_DISPATCH_TIMEOUT);

        outbound::Config {
            ingress_mode,
            allow_discovery: AddrMatch::new(dst_profile_suffixes.clone(), dst_profile_networks),
            proxy: ProxyConfig {
                server,
                connect,
                cache_max_idle_age,
                buffer_capacity,
                dispatch_timeout,
                max_in_flight_requests: outbound_max_in_flight?
                    .unwrap_or(DEFAULT_OUTBOUND_MAX_IN_FLIGHT),
                detect_protocol_timeout,
            },
            inbound_ips,
        }
    };

    let gateway = gateway::Config {
        allow_discovery: gateway_suffixes?.into_iter().flatten().collect(),
    };

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
        let cache_max_idle_age =
            inbound_cache_max_idle_age?.unwrap_or(DEFAULT_INBOUND_ROUTER_MAX_IDLE_AGE);
        let max_idle =
            inbound_max_idle_per_endpoint?.unwrap_or(DEFAULT_INBOUND_MAX_IDLE_CONNS_PER_ENDPOINT);
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
                idle_timeout: cache_max_idle_age,
            },
        };

        let detect_protocol_timeout =
            inbound_detect_timeout?.unwrap_or(DEFAULT_INBOUND_DETECT_TIMEOUT);
        let dispatch_timeout =
            inbound_dispatch_timeout?.unwrap_or(DEFAULT_INBOUND_DISPATCH_TIMEOUT);

        // Ensure that connections that directly target the inbound port are secured (unless
        // identity is disabled).
        let policy = {
            let inbound_port = server.addr.as_ref().port();

            let cluster_nets = parse(strings, ENV_POLICY_CLUSTER_NETWORKS, parse_networks)?
                .unwrap_or_else(|| {
                    info!(
                        "{} not set; cluster-scoped modes are unsupported",
                        ENV_POLICY_CLUSTER_NETWORKS
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
                policy::defaults::all_unauthenticated(detect_protocol_timeout).into()
            });

            match parse_control_addr(strings, ENV_POLICY_SVC_BASE, id_disabled)? {
                Some(addr) => {
                    // If the inbound is proxy is configured to discover policies, then load the set
                    // of all known inbound ports to be discovered during initialization.
                    let mut ports = match parse(strings, ENV_INBOUND_PORTS, parse_port_set)? {
                        Some(ports) => ports,
                        None => {
                            error!("No inbound ports specified via {}", ENV_INBOUND_PORTS,);
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

                    // The workload, which is opaque from the proxy's point-of-view, is sent to the
                    // policy controller to support policy discovery.
                    let workload = strings.get(ENV_POLICY_WORKLOAD)?.ok_or_else(|| {
                        error!(
                            "{} must be set with {}_ADDR",
                            ENV_POLICY_WORKLOAD, ENV_POLICY_SVC_BASE
                        );
                        EnvError::InvalidEnvVar
                    })?;

                    let control = {
                        let connect = if addr.addr.is_loopback() {
                            connect.clone()
                        } else {
                            outbound.proxy.connect.clone()
                        };
                        ControlConfig {
                            addr,
                            connect,
                            buffer_capacity,
                        }
                    };

                    inbound::policy::Config::Discover {
                        default,
                        ports,
                        workload,
                        control,
                    }
                }

                None => {
                    let default_allow = match default.clone() {
                        policy::DefaultPolicy::Allow(a) => a,
                        policy::DefaultPolicy::Deny => {
                            warn!("The default policy `deny` may not be used with a static policy configuration");
                            return Err(EnvError::InvalidEnvVar);
                        }
                    };

                    // If the inbound is not configured to discover policies, then load basic policies from the environment:
                    // - ports that require authentication
                    // - ports that require some form of proxy-terminated TLS, though not
                    //   necessarily with a client identity.
                    // - opaque ports
                    let require_identity_ports = {
                        let ports =
                            parse(strings, ENV_INBOUND_PORTS_REQUIRE_IDENTITY, parse_port_set)?
                                .unwrap_or_default()
                                .into_iter()
                                .map(|p| {
                                    let allow = policy::defaults::all_authenticated(
                                        detect_protocol_timeout,
                                    );
                                    (p, allow)
                                })
                                .collect::<HashMap<_, _>>();
                        if id_disabled && !ports.is_empty() {
                            error!(
                                "if {} is true, {} must be empty",
                                ENV_IDENTITY_DISABLED, ENV_INBOUND_PORTS_REQUIRE_IDENTITY
                            );
                            return Err(EnvError::InvalidEnvVar);
                        }
                        ports
                    };

                    let require_tls_ports = {
                        let mut ports =
                            parse(strings, ENV_INBOUND_PORTS_REQUIRE_TLS, parse_port_set)?
                                .unwrap_or_default()
                                .into_iter()
                                .map(|p| {
                                    let allow = policy::defaults::all_mtls_unauthenticated(
                                        detect_protocol_timeout,
                                    );
                                    (p, allow)
                                })
                                .collect::<HashMap<_, _>>();
                        // `require_identity` is more restrictive than `require_tls`, so prefer it if
                        // there are duplicates.
                        for p in require_identity_ports.keys() {
                            ports.remove(p);
                        }
                        if id_disabled && !ports.is_empty() {
                            error!(
                                "if {} is true, {} must be empty",
                                ENV_IDENTITY_DISABLED, ENV_INBOUND_PORTS_REQUIRE_TLS
                            );
                            return Err(EnvError::InvalidEnvVar);
                        }
                        ports
                    };

                    let opaque_ports = {
                        let ports = inbound_disable_ports?
                            .unwrap_or_default()
                            .into_iter()
                            .map(|p| {
                                let mut sp = require_identity_ports
                                    .get(&p)
                                    .or_else(|| require_tls_ports.get(&p))
                                    .cloned()
                                    .unwrap_or_else(|| (*default_allow).clone());
                                sp.protocol = inbound::policy::Protocol::Opaque;
                                (p, sp)
                            })
                            .collect::<HashMap<_, inbound::policy::ServerPolicy>>();
                        // Ensure that the inbound port does not disable protocol detection, as
                        if ports.contains_key(&inbound_port) {
                            error!(
                                "{} must not contain {} ({})",
                                ENV_INBOUND_PORTS_DISABLE_PROTOCOL_DETECTION,
                                ENV_INBOUND_LISTEN_ADDR,
                                inbound_port
                            );
                            return Err(EnvError::InvalidEnvVar);
                        }
                        ports
                    };

                    inbound::policy::Config::Fixed {
                        default,
                        ports: require_identity_ports
                            .into_iter()
                            .chain(require_tls_ports)
                            .chain(opaque_ports)
                            .collect(),
                    }
                }
            }
        };

        inbound::Config {
            allow_discovery: dst_profile_suffixes.into_iter().collect(),
            proxy: ProxyConfig {
                server,
                connect,
                cache_max_idle_age,
                buffer_capacity,
                dispatch_timeout,
                max_in_flight_requests: inbound_max_in_flight?
                    .unwrap_or(DEFAULT_INBOUND_MAX_IN_FLIGHT),
                detect_protocol_timeout,
            },
            policy,
            profile_idle_timeout: dst_profile_idle_timeout?
                .unwrap_or(DEFAULT_DESTINATION_PROFILE_IDLE_TIMEOUT),
        }
    };

    let dst = {
        let addr = dst_addr?.ok_or(EnvError::NoDestinationAddress)?;
        let connect = if addr.addr.is_loopback() {
            inbound.proxy.connect.clone()
        } else {
            outbound.proxy.connect.clone()
        };
        super::dst::Config {
            context: dst_token?.unwrap_or_default(),
            control: ControlConfig {
                addr,
                connect,
                buffer_capacity,
            },
        }
    };

    let admin = super::admin::Config {
        metrics_retain_idle: metrics_retain_idle?.unwrap_or(DEFAULT_METRICS_RETAIN_IDLE),
        server: ServerConfig {
            addr: ListenAddr(
                admin_listener_addr?
                    .unwrap_or_else(|| parse_socket_addr(DEFAULT_ADMIN_LISTEN_ADDR).unwrap()),
            ),
            keepalive: inbound.proxy.server.keepalive,
            h2_settings,
        },
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
                    buffer_capacity: 10,
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

    let identity = identity_config?
        .map(|(addr, certify)| {
            // If the address doesn't have a server identity, then we're on localhost.
            let connect = if addr.addr.is_loopback() {
                inbound.proxy.connect.clone()
            } else {
                outbound.proxy.connect.clone()
            };
            identity::Config::Enabled {
                certify,
                control: ControlConfig {
                    addr,
                    connect,
                    buffer_capacity: 1,
                },
            }
        })
        .unwrap_or(identity::Config::Disabled);

    Ok(super::Config {
        admin,
        dns,
        dst,
        tap,
        oc_collector,
        identity,
        outbound,
        gateway,
        inbound,
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

// ===== impl Env =====

impl Strings for Env {
    fn get(&self, key: &str) -> Result<Option<String>, EnvError> {
        use std::env;

        match env::var(key) {
            Ok(value) => Ok(Some(value)),
            Err(env::VarError::NotPresent) => Ok(None),
            Err(env::VarError::NotUnicode(_)) => {
                error!("{} is not encoded in Unicode", key);
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

// ===== Parsing =====

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
    id_disabled: bool,
) -> Result<Option<(SocketAddr, HashSet<tls::server::ClientId>)>, EnvError> {
    let tap_identity = parse(strings, ENV_TAP_SVC_NAME, parse_identity)?;
    if id_disabled {
        if tap_identity.is_some() {
            warn!(
                "{} should not be set if identity is disabled; continuing with tap disabled",
                ENV_TAP_SVC_NAME
            );
        }
    } else {
        let addr = parse(strings, ENV_CONTROL_LISTEN_ADDR, parse_socket_addr)?
            .unwrap_or_else(|| parse_socket_addr(DEFAULT_CONTROL_LISTEN_ADDR).unwrap());
        if let Some(id) = tap_identity {
            return Ok(Some((
                addr,
                vec![id].into_iter().map(tls::ClientId).collect(),
            )));
        }
    };
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

fn parse_ip_set(s: &str) -> Result<HashSet<IpAddr>, ParseError> {
    s.split(',')
        .map(|s| s.parse::<IpAddr>().map_err(Into::into))
        .collect()
}

fn parse_addr(s: &str) -> Result<Addr, ParseError> {
    Addr::from_str(s).map_err(|e| {
        error!("Not a valid address: {}", s);
        ParseError::AddrError(e)
    })
}

fn parse_port_set(s: &str) -> Result<HashSet<u16>, ParseError> {
    let mut set = HashSet::new();
    for num in s.split(',') {
        set.insert(parse_number::<u16>(num)?);
    }
    Ok(set)
}

pub(super) fn parse_identity(s: &str) -> Result<identity::Name, ParseError> {
    identity::Name::from_str(s).map_err(|identity::InvalidName| {
        error!("Not a valid identity name: {}", s);
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
                error!("{}={:?} is not valid: {:?}", name, s, parse_error);
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
) -> Result<policy::DefaultPolicy, ParseError> {
    match s {
        "deny" => Ok(policy::DefaultPolicy::Deny),
        "all-authenticated" => Ok(policy::defaults::all_authenticated(detect_timeout).into()),
        "all-unauthenticated" => Ok(policy::defaults::all_unauthenticated(detect_timeout).into()),

        // If cluster networks are configured, support cluster-scoped default policies.
        name if cluster_nets.is_empty() => Err(ParseError::InvalidPortPolicy(name.to_string())),
        "cluster-authenticated" => {
            Ok(policy::defaults::cluster_authenticated(cluster_nets, detect_timeout).into())
        }
        "cluster-unauthenticated" => {
            Ok(policy::defaults::cluster_unauthenticated(cluster_nets, detect_timeout).into())
        }

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
            ExponentialBackoff::new(min, max, jitter.unwrap_or_default()).map_err(|error| {
                error!(message="Invalid backoff", %error, %min_env, ?min, %max_env, ?max, %jitter_env, ?jitter);
                EnvError::InvalidEnvVar
            })
        }
        _ => {
            error!("You need to specify either all of {} {} {} or none of them to use the default backoff", min_env, max_env,jitter_env );
            Err(EnvError::InvalidEnvVar)
        }
    }
}

pub fn parse_control_addr<S: Strings>(
    strings: &S,
    base: &str,
    id_disabled: bool,
) -> Result<Option<ControlAddr>, EnvError> {
    let a = parse(strings, &format!("{}_ADDR", base), parse_addr)?;
    let n = parse(strings, &format!("{}_NAME", base), parse_identity)?;
    match (a, n) {
        (None, None) => Ok(None),
        (Some(ref addr), _) if addr.is_loopback() => Ok(Some(ControlAddr {
            addr: addr.clone(),
            identity: Conditional::None(tls::NoClientTls::Loopback),
        })),
        (Some(addr), None) if id_disabled => Ok(Some(ControlAddr {
            addr,
            identity: Conditional::None(tls::NoClientTls::Loopback),
        })),
        (Some(addr), Some(name)) => Ok(Some(ControlAddr {
            addr,
            identity: Conditional::Some(tls::ServerId(name).into()),
        })),
        _ => {
            error!("{}_ADDR and {}_NAME must be specified together", base, base);
            Err(EnvError::InvalidEnvVar)
        }
    }
}

pub fn parse_identity_config<S: Strings>(
    strings: &S,
) -> Result<Option<(ControlAddr, identity::certify::Config)>, EnvError> {
    let control = parse_control_addr(strings, ENV_IDENTITY_SVC_BASE, false);
    let ta = parse(strings, ENV_IDENTITY_TRUST_ANCHORS, |s| {
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
        control?,
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
                Err(EnvError::InvalidEnvVar)
            } else {
                Ok(None)
            }
        }
        (
            false,
            Some(control),
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
                        EnvError::InvalidEnvVar
                    })
                    .and_then(|b| {
                        identity::Key::from_pkcs8(&b).map_err(|e| {
                            error!("Invalid key: {}", e);
                            EnvError::InvalidEnvVar
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
                        EnvError::InvalidEnvVar
                    })
                    .and_then(|b| {
                        identity::Csr::from_der(b).ok_or_else(|| {
                            error!("No CSR found");
                            EnvError::InvalidEnvVar
                        })
                    })
            };

            Ok(Some((
                control,
                identity::certify::Config {
                    local_id: tls::LocalId(local_name),
                    token,
                    trust_anchors,
                    csr: csr?,
                    key: key?,
                    min_refresh: min_refresh.unwrap_or(DEFAULT_IDENTITY_MIN_REFRESH),
                    max_refresh: max_refresh.unwrap_or(DEFAULT_IDENTITY_MAX_REFRESH),
                },
            )))
        }
        (disabled, addr, trust_anchors, end_entity_dir, local_id, token, _minr, _maxr) => {
            if disabled {
                error!(
                    "{} must be unset when other identity variables are set.",
                    ENV_IDENTITY_DISABLED,
                );
            }
            let s = format!("{0}_ADDR and {0}_NAME", ENV_IDENTITY_SVC_BASE);
            let svc_env: &str = s.as_str();
            for (unset, name) in &[
                (addr.is_none(), svc_env),
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
}
