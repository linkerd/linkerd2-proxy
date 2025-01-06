use std::collections::HashMap;

pub(super) fn read_trace_attributes(path: &std::path::Path) -> HashMap<String, String> {
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

pub(super) fn parse_env_trace_attributes(attrs: &str) -> HashMap<String, String> {
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
