use std::collections::HashMap;
use std::path::Path;

pub(super) struct TraceLabelConfig {
    pub labels_path: Option<String>,
    pub extra_attrs: Option<String>,
    pub otel_attrs: Option<String>,
    pub service_name: Option<String>,
}

pub(super) fn generate_trace_service_labels(config: TraceLabelConfig) -> HashMap<String, String> {
    let mut trace_labels = HashMap::new();

    // This defines the precedence order for trace labels. Checks later in this chain take higher
    // precedence than ones before. The current precedence is:
    // - Values from OTEL_RESOURCE_ATTRIBUTES
    // - Values from LINKERD2_PROXY_TRACE_EXTRA_ATTRIBUTES
    // - Pod labels in the downward API
    // - The service name from LINKERD2_PROXY_TRACE_SERVICE_NAME

    if let Some(service_name) = config.service_name {
        trace_labels.insert("service.name".to_string(), service_name);
    }

    if let Some(labels_path) = config.labels_path {
        trace_labels.extend(read_trace_attributes(&labels_path));
    }

    if let Some(extra_attrs) = config.extra_attrs {
        trace_labels.extend(parse_env_trace_attributes(&extra_attrs));
    }

    if let Some(otel_attrs) = config.otel_attrs {
        trace_labels.extend(parse_env_trace_attributes(&otel_attrs));
    }

    trace_labels
}

fn read_trace_attributes(path: &str) -> HashMap<String, String> {
    let path = Path::new(path);
    match std::fs::read_to_string(path) {
        Ok(attrs) => parse_attrs(&attrs),
        Err(error) => {
            tracing::warn!(
                %error,
                path = %path.display(),
                "Failed to read trace attributes",
            );
            HashMap::new()
        }
    }
}

fn parse_env_trace_attributes(attrs: &str) -> HashMap<String, String> {
    parse_attrs(attrs)
}

fn parse_attrs(attrs: &str) -> HashMap<String, String> {
    attrs
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_attrs_different_values() {
        let attrs = "\
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
        ";

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

        assert_eq!(parse_attrs(attrs), expected);
    }

    #[test]
    fn generate_traces() -> linkerd_error::Result<()> {
        let labels = r#"app="web-svc"
linkerd.io/control-plane-ns="linkerd"
linkerd.io/proxy-deployment="web"
linkerd.io/workload-ns="emojivoto"
pod-template-hash="66bdd4d96d"
version="v11""#;
        let labels_file = tempfile::NamedTempFile::new()?;
        std::fs::write(labels_file.path(), labels)?;

        let extra = r#"k8s.pod.ip="0.0.0.0"
k8s.pod.uid="00000000-0000-0000-0000-000000000000"
k8s.container.name="linkerd-proxy"
service.name="web-linkerd-proxy"
"#;
        let service_name = "linkerd-proxy";

        let mut expected = [
            ("app".to_string(), "web-svc".to_string()),
            (
                "linkerd.io/control-plane-ns".to_string(),
                "linkerd".to_string(),
            ),
            ("linkerd.io/proxy-deployment".to_string(), "web".to_string()),
            (
                "linkerd.io/workload-ns".to_string(),
                "emojivoto".to_string(),
            ),
            ("pod-template-hash".to_string(), "66bdd4d96d".to_string()),
            ("version".to_string(), "v11".to_string()),
            ("k8s.pod.ip".to_string(), "0.0.0.0".to_string()),
            (
                "k8s.pod.uid".to_string(),
                "00000000-0000-0000-0000-000000000000".to_string(),
            ),
            (
                "k8s.container.name".to_string(),
                "linkerd-proxy".to_string(),
            ),
            ("service.name".to_string(), "web-linkerd-proxy".to_string()),
        ]
        .to_vec();
        expected.sort_unstable();

        let mut actual = generate_trace_service_labels(TraceLabelConfig {
            labels_path: Some(labels_file.path().to_string_lossy().to_string()),
            extra_attrs: Some(extra.to_string()),
            otel_attrs: None,
            service_name: Some(service_name.to_string()),
        })
        .into_iter()
        .collect::<Vec<_>>();
        actual.sort_unstable();

        assert_eq!(expected, actual);

        Ok(())
    }
}
