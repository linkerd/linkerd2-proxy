use super::*;

#[tokio::test]
async fn returns_env_var_json() {
    let _trace = trace_init();
    let Fixture {
        metrics,
        proxy: _proxy,
        _profile,
        dst_tx: _dst_tx,
        ..
    } = Fixture::outbound().await;

    let json = get_env_json(&metrics, "/env.json").await;
    let actual = std::env::vars()
        .map(|(name, val)| (name, Some(val)))
        .collect::<HashMap<_, Option<String>>>();

    assert_eq!(actual, json);
}

#[tokio::test]
async fn filters_on_query_params() {
    let _trace = trace_init();
    let Fixture {
        metrics,
        proxy: _proxy,
        _profile,
        dst_tx: _dst_tx,
        ..
    } = Fixture::outbound().await;

    // we can be relatively confident this env var is set by `cargo test`
    const REAL_VAR: &str = "CARGO_PKG_NAME";

    // and, we can _hope_ nothing ever sets this one... :)
    const FAKE_VAR: &str = "ELIZAS_OBVIOUSLY_FAKE_ENV_VAR_THAT_SHOULD_NEVER_BE_SET";

    let expected = [
        (REAL_VAR.to_string(), std::env::var(REAL_VAR).ok()),
        (FAKE_VAR.to_string(), None),
    ]
    .into_iter()
    .collect::<HashMap<_, _>>();

    let json = get_env_json(&metrics, &format!("/env.json?{REAL_VAR}&{FAKE_VAR}")).await;
    assert_eq!(expected, json);

    // now flip it
    let json = get_env_json(&metrics, &format!("/env.json?{FAKE_VAR}&{REAL_VAR}")).await;
    assert_eq!(expected, json);
}

#[tracing::instrument(level = "info", skip(client))]
async fn get_env_json(client: &client::Client, path: &str) -> HashMap<String, Option<String>> {
    let json = client.get(path).await;
    tracing::info!(json);

    let deserialized = serde_json::from_str(json.as_str()).expect("response should be valid JSON");
    tracing::info!(deserialized = ?format_args!("{deserialized:#?}"));
    deserialized
}
