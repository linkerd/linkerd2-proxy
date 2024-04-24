use super::{parse, parse_duration_opt, EnvError, Strings};
use std::time::Duration;
use tracing::error;

pub use linkerd_tonic_stream::ReceiveLimits;

pub(super) fn mk_receive_limits(env: &dyn Strings) -> Result<ReceiveLimits, EnvError> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn control_stream_limits() {
        let mut env = HashMap::default();

        env.insert("LINKERD2_PROXY_CONTROL_STREAM_INITIAL_TIMEOUT", "1s");
        env.insert("LINKERD2_PROXY_CONTROL_STREAM_IDLE_TIMEOUT", "2s");
        env.insert("LINKERD2_PROXY_CONTROL_STREAM_LIFETIME", "3s");
        let limits = mk_receive_limits(&env).unwrap();
        assert_eq!(limits.initial, Some(Duration::from_secs(1)));
        assert_eq!(limits.idle, Some(Duration::from_secs(2)));
        assert_eq!(limits.lifetime, Some(Duration::from_secs(3)));

        env.insert("LINKERD2_PROXY_CONTROL_STREAM_INITIAL_TIMEOUT", "");
        env.insert("LINKERD2_PROXY_CONTROL_STREAM_IDLE_TIMEOUT", "");
        env.insert("LINKERD2_PROXY_CONTROL_STREAM_LIFETIME", "");
        let limits = mk_receive_limits(&env).unwrap();
        assert_eq!(limits.initial, None);
        assert_eq!(limits.idle, None);
        assert_eq!(limits.lifetime, None);

        env.insert("LINKERD2_PROXY_CONTROL_STREAM_INITIAL_TIMEOUT", "3s");
        env.insert("LINKERD2_PROXY_CONTROL_STREAM_IDLE_TIMEOUT", "1s");
        env.insert("LINKERD2_PROXY_CONTROL_STREAM_LIFETIME", "");
        assert!(mk_receive_limits(&env).is_err());

        env.insert("LINKERD2_PROXY_CONTROL_STREAM_INITIAL_TIMEOUT", "3s");
        env.insert("LINKERD2_PROXY_CONTROL_STREAM_IDLE_TIMEOUT", "");
        env.insert("LINKERD2_PROXY_CONTROL_STREAM_LIFETIME", "1s");
        assert!(mk_receive_limits(&env).is_err());

        env.insert("LINKERD2_PROXY_CONTROL_STREAM_INITIAL_TIMEOUT", "");
        env.insert("LINKERD2_PROXY_CONTROL_STREAM_IDLE_TIMEOUT", "3s");
        env.insert("LINKERD2_PROXY_CONTROL_STREAM_LIFETIME", "1s");
        assert!(mk_receive_limits(&env).is_err());
    }
}
