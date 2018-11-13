use std::collections::HashMap;
use std::env;
use std::iter::FromIterator;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use http;
use indexmap::IndexSet;
use trust_dns_resolver::config::ResolverOpts;

use addr;
use dns;
use convert::TryFrom;
use transport::tls;
use {Conditional, Addr};

// TODO:
//
// * Make struct fields private.

/// Tracks all configuration settings for the process.
#[derive(Debug)]
pub struct Config {
    /// Where to listen for connections that are initiated on the host.
    pub outbound_listener: Listener,

    /// Where to listen for connections initiated by external sources.
    pub inbound_listener: Listener,

    /// Where to listen for connections initiated by the control plane.
    pub control_listener: Listener,

    /// Where to serve Prometheus metrics.
    pub metrics_listener: Listener,

    /// Where to forward externally received connections.
    pub inbound_forward: Option<SocketAddr>,

    /// The maximum amount of time to wait for a connection to a local peer.
    pub inbound_connect_timeout: Duration,

    /// The maximum amount of time to wait for a connection to a remote peer.
    pub outbound_connect_timeout: Duration,

    pub inbound_ports_disable_protocol_detection: IndexSet<u16>,

    pub outbound_ports_disable_protocol_detection: IndexSet<u16>,

    pub inbound_router_capacity: usize,

    pub outbound_router_capacity: usize,

    pub inbound_router_max_idle_age: Duration,

    pub outbound_router_max_idle_age: Duration,

    /// The maximum number of queries to the Destination service which may be
    /// active concurrently.
    pub destination_concurrency_limit: usize,

    /// Configured by `ENV_DESTINATION_GET_SUFFIXES`.
    pub destination_get_suffixes: Vec<dns::Suffix>,

    /// Configured by `ENV_DESTINATION_PROFILE_SUFFIXES`.
    pub destination_profile_suffixes: Vec<dns::Suffix>,

    pub tls_settings: Conditional<tls::CommonSettings, tls::ReasonForNoTls>,

    /// The path to "/etc/resolv.conf"
    pub resolv_conf_path: PathBuf,

    /// Where to talk to the control plane.
    ///
    /// When set, DNS is only after the Destination service says that there is
    /// no service with the given name. When not set, the Destination service
    /// is completely bypassed for service discovery and DNS is always used.
    ///
    /// This is optional to allow the proxy to work without the controller for
    /// experimental & testing purposes.
    pub control_host_and_port: Option<Addr>,

    /// Time to wait when encountering errors talking to control plane before
    /// a new connection.
    pub control_backoff_delay: Duration,

    /// The maximum amount of time to wait for a connection to the controller.
    pub control_connect_timeout: Duration,

    /// Age after which metrics may be dropped.
    pub metrics_retain_idle: Duration,

    /// Timeout after which to cancel binding a request.
    pub bind_timeout: Duration,

    pub namespaces: Namespaces,

    /// Optional minimum TTL for DNS lookups.
    pub dns_min_ttl: Option<Duration>,

    /// Optional maximum TTL for DNS lookups.
    pub dns_max_ttl: Option<Duration>,
}

#[derive(Clone, Debug)]
pub struct Namespaces {
    pub pod: String,

    /// `None` if TLS is disabled; otherwise must be `Some`.
    pub tls_controller: Option<String>,
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
    InvalidEnvVar
}

#[derive(Clone, Debug, PartialEq)]
pub enum ParseError {
    EnvironmentUnsupported,
    NotADuration,
    NotADomainSuffix,
    NotANumber,
    HostIsNotAnIpAddress,
    NotUnicode,
    UrlError(UrlError),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum UrlError {
    /// The URl is syntactically invalid according to general URL parsing rules.
    SyntaxError,

    /// The URL has a scheme that isn't supported.
    UnsupportedScheme,

    /// The URL is missing the authority part.
    MissingAuthority,

    /// The URL is missing the authority part.
    AuthorityError(addr::Error),

    /// The URL contains a path component that isn't "/", which isn't allowed.
    PathNotAllowed,
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

pub struct TestEnv {
    values: HashMap<&'static str, String>
}

// Environment variables to look at when loading the configuration
pub const ENV_OUTBOUND_LISTENER: &str = "LINKERD2_PROXY_OUTBOUND_LISTENER";
pub const ENV_INBOUND_FORWARD: &str = "LINKERD2_PROXY_INBOUND_FORWARD";
pub const ENV_INBOUND_LISTENER: &str = "LINKERD2_PROXY_INBOUND_LISTENER";
pub const ENV_CONTROL_LISTENER: &str = "LINKERD2_PROXY_CONTROL_LISTENER";
pub const ENV_METRICS_LISTENER: &str = "LINKERD2_PROXY_METRICS_LISTENER";
pub const ENV_METRICS_RETAIN_IDLE: &str = "LINKERD2_PROXY_METRICS_RETAIN_IDLE";
const ENV_INBOUND_CONNECT_TIMEOUT: &str = "LINKERD2_PROXY_INBOUND_CONNECT_TIMEOUT";
const ENV_OUTBOUND_CONNECT_TIMEOUT: &str = "LINKERD2_PROXY_OUTBOUND_CONNECT_TIMEOUT";
pub const ENV_BIND_TIMEOUT: &str = "LINKERD2_PROXY_BIND_TIMEOUT";

pub const DEPRECATED_ENV_PRIVATE_LISTENER: &str = "LINKERD2_PROXY_PRIVATE_LISTENER";
pub const DEPRECATED_ENV_PRIVATE_FORWARD: &str = "LINKERD2_PROXY_PRIVATE_FORWARD";
const DEPRECATED_ENV_PUBLIC_LISTENER: &str = "LINKERD2_PROXY_PUBLIC_LISTENER";
const DEPRECATED_ENV_PRIVATE_CONNECT_TIMEOUT: &str = "LINKERD2_PROXY_PRIVATE_CONNECT_TIMEOUT";
const DEPRECATED_ENV_PUBLIC_CONNECT_TIMEOUT: &str = "LINKERD2_PROXY_PUBLIC_CONNECT_TIMEOUT";

// Limits the number of HTTP routes that may be active in the proxy at any time. There is
// an inbound route for each local port that receives connections. There is an outbound
// route for each protocol and authority.
pub const ENV_INBOUND_ROUTER_CAPACITY: &str = "LINKERD2_PROXY_INBOUND_ROUTER_CAPACITY";
pub const ENV_OUTBOUND_ROUTER_CAPACITY: &str = "LINKERD2_PROXY_OUTBOUND_ROUTER_CAPACITY";

pub const ENV_INBOUND_ROUTER_MAX_IDLE_AGE: &str = "LINKERD2_PROXY_INBOUND_ROUTER_MAX_IDLE_AGE";
pub const ENV_OUTBOUND_ROUTER_MAX_IDLE_AGE: &str = "LINKERD2_PROXY_OUTBOUND_ROUTER_MAX_IDLE_AGE";

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

/// Limits the maximum number of outbound Destination service queries.
///
/// Routes which do not result in service discovery lookups will not be capped
/// by this limit. This will have no effect if it is greater than the total
/// router capacity (as configured by `ENV_OUTBOUND_ROUTER_CAPACITY`).
pub const ENV_DESTINATION_CLIENT_CONCURRENCY_LIMIT: &str =
    "LINKERD2_PROXY_DESTINATION_CLIENT_CONCURRENCY_LIMIT";

// These *disable* our protocol detection for connections whose SO_ORIGINAL_DST
// has a port in the provided list.
pub const ENV_INBOUND_PORTS_DISABLE_PROTOCOL_DETECTION: &str = "LINKERD2_PROXY_INBOUND_PORTS_DISABLE_PROTOCOL_DETECTION";
pub const ENV_OUTBOUND_PORTS_DISABLE_PROTOCOL_DETECTION: &str = "LINKERD2_PROXY_OUTBOUND_PORTS_DISABLE_PROTOCOL_DETECTION";

pub const ENV_TLS_TRUST_ANCHORS: &str = "LINKERD2_PROXY_TLS_TRUST_ANCHORS";
pub const ENV_TLS_CERT: &str = "LINKERD2_PROXY_TLS_CERT";
pub const ENV_TLS_PRIVATE_KEY: &str = "LINKERD2_PROXY_TLS_PRIVATE_KEY";
pub const ENV_TLS_POD_IDENTITY: &str = "LINKERD2_PROXY_TLS_POD_IDENTITY";
pub const ENV_TLS_CONTROLLER_IDENTITY: &str = "LINKERD2_PROXY_TLS_CONTROLLER_IDENTITY";

pub const ENV_CONTROLLER_NAMESPACE: &str = "LINKERD2_PROXY_CONTROLLER_NAMESPACE";
pub const ENV_POD_NAMESPACE: &str = "LINKERD2_PROXY_POD_NAMESPACE";
pub const VAR_POD_NAMESPACE: &str = "$LINKERD2_PROXY_POD_NAMESPACE";

pub const ENV_CONTROL_URL: &str = "LINKERD2_PROXY_CONTROL_URL";
pub const ENV_CONTROL_BACKOFF_DELAY: &str = "LINKERD2_PROXY_CONTROL_BACKOFF_DELAY";
const ENV_CONTROL_CONNECT_TIMEOUT: &str = "LINKERD2_PROXY_CONTROL_CONNECT_TIMEOUT";
const ENV_RESOLV_CONF: &str = "LINKERD2_PROXY_RESOLV_CONF";

/// Configures a minimum value for the TTL of DNS lookups.
///
/// Lookups with TTLs below this value will use this value instead.
const ENV_DNS_MIN_TTL: &str = "LINKERD2_PROXY_DNS_MIN_TTL";
/// Configures a maximum value for the TTL of DNS lookups.
///
/// Lookups with TTLs above this value will use this value instead.
const ENV_DNS_MAX_TTL: &str = "LINKERD2_PROXY_DNS_MAX_TTL";

// Default values for various configuration fields
const DEFAULT_OUTBOUND_LISTENER: &str = "tcp://127.0.0.1:4140";
const DEFAULT_INBOUND_LISTENER: &str = "tcp://0.0.0.0:4143";
const DEFAULT_CONTROL_LISTENER: &str = "tcp://0.0.0.0:4190";
const DEFAULT_METRICS_LISTENER: &str = "tcp://127.0.0.1:4191";
const DEFAULT_METRICS_RETAIN_IDLE: Duration = Duration::from_secs(10 * 60);
const DEFAULT_INBOUND_CONNECT_TIMEOUT: Duration = Duration::from_millis(20);
const DEFAULT_OUTBOUND_CONNECT_TIMEOUT: Duration = Duration::from_millis(300);
const DEFAULT_BIND_TIMEOUT: Duration = Duration::from_secs(10); // same as in Linkerd
const DEFAULT_CONTROL_BACKOFF_DELAY: Duration = Duration::from_secs(5);
const DEFAULT_CONTROL_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const DEFAULT_RESOLV_CONF: &str = "/etc/resolv.conf";

/// It's assumed that a typical proxy can serve inbound traffic for up to 100 pod-local
/// HTTP services and may communicate with up to 10K external HTTP domains.
const DEFAULT_INBOUND_ROUTER_CAPACITY:  usize = 100;
const DEFAULT_OUTBOUND_ROUTER_CAPACITY: usize = 100;

const DEFAULT_INBOUND_ROUTER_MAX_IDLE_AGE:  Duration = Duration::from_secs(60);
const DEFAULT_OUTBOUND_ROUTER_MAX_IDLE_AGE: Duration = Duration::from_secs(60);

const DEFAULT_DESTINATION_CLIENT_CONCURRENCY_LIMIT: usize = 100;

const DEFAULT_DESTINATION_GET_SUFFIXES: &str = "svc.cluster.local.";
const DEFAULT_DESTINATION_PROFILE_SUFFIXES: &str = "svc.cluster.local.";

// By default, we keep a list of known assigned ports of server-first protocols.
//
// https://www.iana.org/assignments/service-names-port-numbers/service-names-port-numbers.txt
const DEFAULT_PORTS_DISABLE_PROTOCOL_DETECTION: &[u16] = &[
    25,   // SMTP
    3306, // MySQL
];

// ===== impl Config =====

impl Config {
    /// Modify a `trust-dns-resolver::config::ResolverOpts` to reflect
    /// the configured minimum and maximum DNS TTL values.
    pub fn configure_resolver_opts(&self, mut opts: ResolverOpts) -> ResolverOpts {
        opts.positive_min_ttl = self.dns_min_ttl;
        opts.positive_max_ttl = self.dns_max_ttl;
        // TODO: Do we want to allow the positive and negative TTLs to be
        //       configured separately?
        opts.negative_min_ttl = self.dns_min_ttl;
        opts.negative_max_ttl = self.dns_max_ttl;
        opts
    }
}

impl<'a> TryFrom<&'a Strings> for Config {
    type Err = Error;
    /// Load a `Config` by reading ENV variables.
    fn try_from(strings: &Strings) -> Result<Self, Self::Err> {
        // Parse all the environment variables. `env_var` and `env_var_parse`
        // will log any errors so defer returning any errors until all of them
        // have been parsed.
        let outbound_listener_addr = parse_deprecated(
            strings, ENV_OUTBOUND_LISTENER, DEPRECATED_ENV_PRIVATE_LISTENER, parse_addr);
        let inbound_listener_addr = parse_deprecated(
            strings, ENV_INBOUND_LISTENER, DEPRECATED_ENV_PUBLIC_LISTENER, parse_addr);
        let control_listener_addr = parse(strings, ENV_CONTROL_LISTENER, parse_addr);
        let metrics_listener_addr = parse(strings, ENV_METRICS_LISTENER, parse_addr);
        let inbound_forward = parse_deprecated(
            strings, ENV_INBOUND_FORWARD, DEPRECATED_ENV_PRIVATE_FORWARD, parse_addr);
        let inbound_connect_timeout = parse_deprecated(
            strings, ENV_INBOUND_CONNECT_TIMEOUT, DEPRECATED_ENV_PRIVATE_CONNECT_TIMEOUT, parse_duration);
        let outbound_connect_timeout = parse_deprecated(
            strings, ENV_OUTBOUND_CONNECT_TIMEOUT, DEPRECATED_ENV_PUBLIC_CONNECT_TIMEOUT, parse_duration);
        let inbound_disable_ports = parse(strings, ENV_INBOUND_PORTS_DISABLE_PROTOCOL_DETECTION, parse_port_set);
        let outbound_disable_ports = parse(strings, ENV_OUTBOUND_PORTS_DISABLE_PROTOCOL_DETECTION, parse_port_set);
        let inbound_router_capacity = parse(strings, ENV_INBOUND_ROUTER_CAPACITY, parse_number);
        let outbound_router_capacity = parse(strings, ENV_OUTBOUND_ROUTER_CAPACITY, parse_number);
        let inbound_router_max_idle_age = parse(strings, ENV_INBOUND_ROUTER_MAX_IDLE_AGE, parse_duration);
        let outbound_router_max_idle_age = parse(strings, ENV_OUTBOUND_ROUTER_MAX_IDLE_AGE, parse_duration);
        let destination_concurrency_limit =
            parse(strings, ENV_DESTINATION_CLIENT_CONCURRENCY_LIMIT, parse_number);
        let destination_get_suffixes =
            parse(strings, ENV_DESTINATION_GET_SUFFIXES, parse_dns_suffixes);
        let destination_profile_suffixes =
            parse(strings, ENV_DESTINATION_PROFILE_SUFFIXES, parse_dns_suffixes);
        let tls_trust_anchors = parse(strings, ENV_TLS_TRUST_ANCHORS, parse_path);
        let tls_end_entity_cert = parse(strings, ENV_TLS_CERT, parse_path);
        let tls_private_key = parse(strings, ENV_TLS_PRIVATE_KEY, parse_path);
        let tls_pod_identity_template = strings.get(ENV_TLS_POD_IDENTITY);
        let tls_controller_identity = strings.get(ENV_TLS_CONTROLLER_IDENTITY);
        let bind_timeout = parse(strings, ENV_BIND_TIMEOUT, parse_duration);
        let resolv_conf_path = strings.get(ENV_RESOLV_CONF);
        let metrics_retain_idle = parse(strings, ENV_METRICS_RETAIN_IDLE, parse_duration);
        let dns_min_ttl = parse(strings, ENV_DNS_MIN_TTL, parse_duration);
        let dns_max_ttl = parse(strings, ENV_DNS_MAX_TTL, parse_duration);
        let pod_namespace = strings.get(ENV_POD_NAMESPACE).and_then(|maybe_value| {
            // There cannot be a default pod namespace, and the pod namespace is required.
            maybe_value.ok_or_else(|| {
                error!("{} is not set", ENV_POD_NAMESPACE);
                Error::InvalidEnvVar
            })
        });
        let controller_namespace = strings.get(ENV_CONTROLLER_NAMESPACE);

        // There is no default controller URL because a default would make it
        // too easy to connect to the wrong controller, which would be dangerous.
        let control_host_and_port = parse(strings, ENV_CONTROL_URL, parse_url);

        let control_backoff_delay = parse(strings, ENV_CONTROL_BACKOFF_DELAY, parse_duration)?
            .unwrap_or(DEFAULT_CONTROL_BACKOFF_DELAY);
        let control_connect_timeout = parse(strings, ENV_CONTROL_CONNECT_TIMEOUT, parse_duration)?
            .unwrap_or(DEFAULT_CONTROL_CONNECT_TIMEOUT);

        let namespaces = Namespaces {
            pod: pod_namespace?,
            tls_controller: controller_namespace?,
        };

        let tls_controller_identity = tls_controller_identity?;
        let control_host_and_port = control_host_and_port?;

        let tls_settings = match (tls_trust_anchors?,
                                  tls_end_entity_cert?,
                                  tls_private_key?,
                                  tls_pod_identity_template?.as_ref())
        {
            (Some(trust_anchors),
             Some(end_entity_cert),
             Some(private_key),
             Some(tls_pod_identity_template)) => {
                let pod_identity =
                    tls_pod_identity_template.replace(VAR_POD_NAMESPACE, &namespaces.pod);
                let pod_identity = tls::Identity::from_sni_hostname(pod_identity.as_bytes())
                    .map_err(|_| Error::InvalidEnvVar)?; // Already logged.

                // Avoid setting the controller identity if it is going to be
                // a loopback connection since TLS isn't needed or supported in
                // that case.
                let controller_identity = if let Some(identity) = &tls_controller_identity {
                    match &control_host_and_port {
                        Some(hp) if hp.is_loopback() =>
                            Conditional::None(tls::ReasonForNoIdentity::Loopback),
                        Some(_) => {
                            let identity = tls::Identity::from_sni_hostname(identity.as_bytes())
                                .map_err(|_| Error::InvalidEnvVar)?; // Already logged.
                            Conditional::Some(identity)
                        },
                        None => Conditional::None(tls::ReasonForNoIdentity::NotConfigured),
                    }
                } else {
                    Conditional::None(tls::ReasonForNoIdentity::NotConfigured)
                };

                Ok(Conditional::Some(tls::CommonSettings {
                    trust_anchors,
                    end_entity_cert,
                    private_key,
                    pod_identity,
                    controller_identity,
                }))
            },
            (None, None, None, _) => Ok(Conditional::None(tls::ReasonForNoTls::Disabled)),
            (trust_anchors, end_entity_cert, private_key, pod_identity) => {
                if trust_anchors.is_none() {
                    error!("{} is not set; it is required when {} and {} are set.",
                           ENV_TLS_TRUST_ANCHORS, ENV_TLS_CERT, ENV_TLS_PRIVATE_KEY);
                }
                if end_entity_cert.is_none() {
                    error!("{} is not set; it is required when {} are set.",
                           ENV_TLS_CERT, ENV_TLS_TRUST_ANCHORS);
                }
                if private_key.is_none() {
                    error!("{} is not set; it is required when {} are set.",
                           ENV_TLS_PRIVATE_KEY, ENV_TLS_TRUST_ANCHORS);
                }
                if pod_identity.is_none() {
                    error!("{} is not set; it is required when {} are set.",
                           ENV_TLS_POD_IDENTITY, ENV_TLS_CERT);
                }
                Err(Error::InvalidEnvVar)
            },
        }?;

        Ok(Config {
            outbound_listener: Listener {
                addr: outbound_listener_addr?
                    .unwrap_or_else(|| parse_addr(DEFAULT_OUTBOUND_LISTENER).unwrap()),
            },
            inbound_listener: Listener {
                addr: inbound_listener_addr?
                    .unwrap_or_else(|| parse_addr(DEFAULT_INBOUND_LISTENER).unwrap()),
            },
            control_listener: Listener {
                addr: control_listener_addr?
                    .unwrap_or_else(|| parse_addr(DEFAULT_CONTROL_LISTENER).unwrap()),
            },
            metrics_listener: Listener {
                addr: metrics_listener_addr?
                    .unwrap_or_else(|| parse_addr(DEFAULT_METRICS_LISTENER).unwrap()),
            },
            inbound_forward: inbound_forward?,

            inbound_connect_timeout: inbound_connect_timeout?
                .unwrap_or(DEFAULT_INBOUND_CONNECT_TIMEOUT),
            outbound_connect_timeout: outbound_connect_timeout?
                .unwrap_or(DEFAULT_OUTBOUND_CONNECT_TIMEOUT),

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

            destination_concurrency_limit: destination_concurrency_limit?
                .unwrap_or(DEFAULT_DESTINATION_CLIENT_CONCURRENCY_LIMIT),

            destination_get_suffixes: destination_get_suffixes?
                .unwrap_or(parse_dns_suffixes(DEFAULT_DESTINATION_GET_SUFFIXES).unwrap()),

            destination_profile_suffixes: destination_profile_suffixes?
                .unwrap_or(parse_dns_suffixes(DEFAULT_DESTINATION_PROFILE_SUFFIXES).unwrap()),

            tls_settings,

            resolv_conf_path: resolv_conf_path?
                .unwrap_or(DEFAULT_RESOLV_CONF.into())
                .into(),
            control_host_and_port,
            control_backoff_delay,
            control_connect_timeout,

            metrics_retain_idle: metrics_retain_idle?.unwrap_or(DEFAULT_METRICS_RETAIN_IDLE),

            bind_timeout: bind_timeout?.unwrap_or(DEFAULT_BIND_TIMEOUT),

            namespaces,

            dns_min_ttl: dns_min_ttl?,

            dns_max_ttl: dns_max_ttl?,
        })
    }
}

fn default_disable_ports_protocol_detection() -> IndexSet<u16> {
    IndexSet::from_iter(DEFAULT_PORTS_DISABLE_PROTOCOL_DETECTION.iter().cloned())
}

// ===== impl Addr =====

fn parse_addr(s: &str) -> Result<SocketAddr, ParseError> {
    match parse_url(s)? {
        Addr::Socket(a) => Ok(a),
        _ => Err(ParseError::HostIsNotAnIpAddress)
    }
}

// ===== impl Env =====

impl Strings for Env {
    fn get(&self, key: &str) -> Result<Option<String>, Error> {
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
}

impl Strings for TestEnv {
    fn get(&self, key: &str) -> Result<Option<String>, Error> {
        Ok(self.values.get(key).cloned())
    }
}

// ===== Parsing =====

fn parse_number<T>(s: &str) -> Result<T, ParseError> where T: FromStr {
    s.parse().map_err(|_| ParseError::NotANumber)
}

fn parse_duration(s: &str) -> Result<Duration, ParseError> {
    use regex::Regex;

    let re = Regex::new(r"^\s*(\d+)(ms|s|m|h|d)?\s*$")
        .expect("duration regex");

    let cap = re.captures(s)
        .ok_or(ParseError::NotADuration)?;

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

fn parse_path(s: &str) -> Result<PathBuf, ParseError> {
    Ok(PathBuf::from(s))
}

fn parse_url(s: &str) -> Result<Addr, ParseError> {
    let url = s.parse::<http::Uri>().map_err(|_| ParseError::UrlError(UrlError::SyntaxError))?;
    if url.scheme_part().map(|s| s.as_str()) != Some("tcp") {
        return Err(ParseError::UrlError(UrlError::UnsupportedScheme));
    }
    let authority = url.authority_part()
        .ok_or_else(|| ParseError::UrlError(UrlError::MissingAuthority))?;

    if url.path() != "/" {
        return Err(ParseError::UrlError(UrlError::PathNotAllowed));
    }
    // http::Uri doesn't provde an accessor for the fragment; See
    // https://github.com/hyperium/http/issues/127. For now just ignore any
    // fragment that is there.

    Addr::from_authority_with_port(authority)
        .map_err(|e| ParseError::UrlError(UrlError::AuthorityError(e)))
}

fn parse_port_set(s: &str) -> Result<IndexSet<u16>, ParseError> {
    let mut set = IndexSet::new();
    for num in s.split(',') {
        set.insert(parse_number::<u16>(num)?);
    }
    Ok(set)
}

fn parse<T, Parse>(strings: &Strings, name: &str, parse: Parse) -> Result<Option<T>, Error>
    where Parse: FnOnce(&str) -> Result<T, ParseError> {
    match strings.get(name)? {
        Some(ref s) => {
            let r = parse(s).map_err(|parse_error| {
                error!("{}={:?} is not valid: {:?}", name, s, parse_error);
                Error::InvalidEnvVar
            })?;
            Ok(Some(r))
        },
        None => Ok(None),
    }
}

fn parse_deprecated<T, Parse>(strings: &Strings, name: &str, deprecated_name: &str, f: Parse)
    -> Result<Option<T>, Error>
where
    Parse: Copy,
    Parse: Fn(&str) -> Result<T, ParseError>
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
        assert_eq!(parse_duration("123456789012345678901234567890ms"), Err(ParseError::NotANumber));
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
        assert_eq!(p("a.b.c"), Ok(vec!["a.b.c".to_owned()]), "a name without trailing dot");
        assert_eq!(p("a.b.c."), Ok(vec!["a.b.c.".to_owned()]), "a name with a trailing dot");
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
