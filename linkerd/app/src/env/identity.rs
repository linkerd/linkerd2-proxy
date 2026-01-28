use super::*;

pub fn parse_identity_config<S: Strings>(
    strings: &S,
    inbound: &inbound::Config,
    outbound: &outbound::Config,
) -> Result<crate::identity::Config, EnvError> {
    let (id, server_name, trust_anchors_pem) = parse_tls_params(strings)?;

    match parse_deprecated(
        strings,
        ENV_IDENTITY_SPIRE_WORKLOAD_API_ADDRESS,
        ENV_IDENTITY_SPIRE_SOCKET,
        |s| Ok(s.to_string()),
    )? {
        Some(workload_api_addr) => match &id {
            // TODO: perform stricter SPIFFE ID validation following:
            // https://github.com/spiffe/spiffe/blob/27b59b81ba8c56885ac5d4be73b35b9b3305fd7a/standards/SPIFFE-ID.md
            crate::identity::Id::Uri(uri)
                if uri.scheme().eq_ignore_ascii_case(SPIFFE_ID_URI_SCHEME) =>
            {
                Ok(crate::identity::Config::Spire {
                    id,
                    server_name,
                    trust_anchors_pem,
                    client: spire::Config {
                        workload_api_addr: std::sync::Arc::new(workload_api_addr),
                        backoff: parse_backoff(
                            strings,
                            IDENTITY_SPIRE_BASE,
                            DEFAULT_SPIRE_BACKOFF,
                        )?,
                    },
                })
            }
            _ => {
                error!("Spire support requires a SPIFFE TLS Id");
                return Err(EnvError::InvalidEnvVar);
            }
        },
        None => {
            match (&id, &server_name) {
                (linkerd_app_core::identity::Id::Dns(id), sni) if id == sni => {}
                (_id, _sni) => {
                    return Err(EnvError::TlsIdAndServerNameNotMatching.into());
                }
            };

            let (addr, certify) = self::identity::parse_linkerd_identity_config(strings)?;

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

            Ok(crate::identity::Config::Linkerd {
                certify,
                id,
                server_name,
                trust_anchors_pem,
                client: ControlConfig {
                    addr,
                    connect,
                    buffer: QueueConfig {
                        capacity: DEFAULT_CONTROL_QUEUE_CAPACITY,
                        failfast_timeout,
                    },
                },
            })
        }
    }
}

fn parse_linkerd_identity_config<S: Strings>(
    strings: &S,
) -> Result<(ControlAddr, crate::identity::client::linkerd::Config), EnvError> {
    let control = parse_control_addr(strings, ENV_IDENTITY_SVC_BASE);
    let dir = parse(strings, ENV_IDENTITY_DIR, |ref s| Ok(PathBuf::from(s)));
    let tok = parse(strings, ENV_IDENTITY_TOKEN_FILE, |ref s| {
        crate::identity::client::linkerd::TokenSource::if_nonempty_file(s.to_string()).map_err(
            |e| {
                error!("Could not read {ENV_IDENTITY_TOKEN_FILE}: {e}");
                ParseError::InvalidTokenSource
            },
        )
    });

    let min_refresh = parse(strings, ENV_IDENTITY_MIN_REFRESH, parse_duration);
    let max_refresh = parse(strings, ENV_IDENTITY_MAX_REFRESH, parse_duration);

    match (control?, dir?, tok?, min_refresh?, max_refresh?) {
        (Some(control), Some(dir), Some(token), min_refresh, max_refresh) => {
            let certify = crate::identity::client::linkerd::Config {
                token,
                min_refresh: min_refresh.unwrap_or(DEFAULT_IDENTITY_MIN_REFRESH),
                max_refresh: max_refresh.unwrap_or(DEFAULT_IDENTITY_MAX_REFRESH),
                documents: crate::identity::client::linkerd::certify::Documents::load(dir)
                    .map_err(|error| {
                        error!(%error, "Failed to read identity documents");
                        EnvError::InvalidEnvVar
                    })?,
            };

            Ok((control, certify))
        }
        (addr, end_entity_dir, token, _minr, _maxr) => {
            let s = format!("{ENV_IDENTITY_SVC_BASE}_ADDR and {ENV_IDENTITY_SVC_BASE}_NAME");
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

fn parse_tls_params<S: Strings>(strings: &S) -> Result<crate::identity::TlsParams, EnvError> {
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
            Ok((server_id, server_name, trust_anchors_pem))
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
