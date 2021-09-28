use std::collections::HashMap;
use std::fmt;
use std::string::ToString;

#[derive(Debug, Clone)]
pub struct MetricMatch {
    name: String,
    labels: HashMap<String, String>,
    value: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Labels(HashMap<String, String>);

pub fn metric(name: impl Into<String>) -> MetricMatch {
    MetricMatch::new(name)
}

pub fn labels() -> Labels {
    Labels::default()
}

#[derive(Eq, PartialEq)]
pub struct MatchErr(String);

macro_rules! match_err {
    ($($arg:tt)+) => {
        return Err(MatchErr(format!($($arg)+)));
    }
}

impl MetricMatch {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            labels: HashMap::new(),
            value: None,
        }
    }

    pub fn label(mut self, key: impl AsRef<str>, val: impl fmt::Display) -> Self {
        self.labels.insert(key.as_ref().to_owned(), val.to_string());
        self
    }

    pub fn value(self, value: impl ToString) -> Self {
        Self {
            value: Some(value.to_string()),
            ..self
        }
    }

    pub fn set_value(&mut self, value: impl ToString) -> &mut Self {
        self.value = Some(value.to_string());
        self
    }

    pub fn is_not_in(&self, scrape: impl AsRef<str>) -> bool {
        self.is_in(scrape).is_err()
    }

    pub fn is_in(&self, scrape: impl AsRef<str>) -> Result<(), MatchErr> {
        let scrape = scrape.as_ref();
        let metrics = scrape
            .lines()
            .filter(|&line| line.starts_with(&self.name))
            .collect::<Vec<&str>>();
        let mut candidates = Vec::new();

        let num_expected_labels = self.labels.len();

        for &metric in &metrics {
            let span = tracing::debug_span!("checking", ?metric);
            let _e = span.enter();
            // Count the number of expected labels matched in this metric, so
            // that we can ensure all expected labels were found.
            let mut matched_labels = 0;

            if let Some(labels) = metric
                .split('{')
                .nth(1)
                .and_then(|line| line.split('}').next())
            {
                let mut matched = true;
                for label in labels.split(',') {
                    tracing::trace!(label, "checking");

                    if label.is_empty() {
                        match_err!("malformed metric (empty label)\n  metric: \"{}\"", metric);
                    }

                    if !label.contains('=') {
                        match_err!(
                            "malformed metric (missing '=')\n in label: {:?}\n  metric: \"{}\"",
                            label,
                            metric,
                        );
                    }

                    let mut split = label.split('=');
                    let key = split.next().expect("label contains an '='");
                    let value = split.next().expect("label contains an '='");

                    if split.next().is_some() {
                        match_err!(
                            "malformed metric (multiple '=' in label)\n in label: {:?}\n   metric: \"{}\"",
                            label,
                            metric,
                        );
                    }

                    if key.contains('"') {
                        match_err!(
                            "malformed metric (quotation mark before '=' in label)\n in label: {:?}\n   metric: \"{}\"",
                            label,
                            metric,
                        );
                    }

                    if !(value.starts_with('"') && value.ends_with('"')) {
                        match_err!(
                            "malformed metric (label value not quoted)\n in label: {:?}\n    value: {:?}\n   metric: \"{}\"",
                            label,
                            value,
                            metric,
                        );
                    }

                    // Ignore the quotation marks on either side of the value
                    // when comparing it against the expected value. We just
                    // checked that there are quotation marks, on both sides, so
                    // we know the first and last chars are quotes.
                    let value = &value[1..value.len() - 1];
                    let expected_value = self.labels.get(key);
                    tracing::trace!(key, value, ?expected_value);

                    if let Some(expected_value) = expected_value {
                        if expected_value != value {
                            tracing::debug!(key, value, ?expected_value, "label is not a match!");
                            // The label isn't a match, but check all the labels
                            // in this metric so we can error on invalid metrics
                            // as well.
                            matched = false;
                        } else {
                            matched_labels += 1;
                        }
                    }
                }

                tracing::debug!(
                    matched,
                    matched_labels,
                    num_expected_labels,
                    "checked all labels"
                );

                if matched_labels != num_expected_labels {
                    tracing::debug!("did not find all expected labels");
                    matched = false;
                }

                if matched {
                    candidates.push(metric);
                }
            }
        }

        if candidates.is_empty() {
            match_err!(
                "did not find a `{}` metric\n  with labels: {:?}\nfound metrics: {:#?}",
                self.name,
                self.labels,
                metrics
            );
        }

        if let Some(ref expected_value) = self.value {
            for &line in &candidates {
                if let Some(value) = line.split('}').nth(1) {
                    let value = value.trim();
                    if value == expected_value {
                        return Ok(());
                    }
                }
            }
            match_err!(
                "did not find a `{}` metric\n  with labels: {:?}\n    and value: {}\nfound metrics: {:#?}",
                self.name, self.labels, expected_value, candidates
            );
        }

        Ok(())
    }

    #[track_caller]
    pub async fn assert_in(&self, client: &crate::client::Client) {
        use std::str::FromStr;
        use std::time::{Duration, Instant};
        use std::{env, u64};
        use tracing::Instrument as _;
        const MAX_RETRIES: usize = 5;
        // TODO: don't do this *every* time eventually is called (lazy_static?)
        let patience = env::var(crate::ENV_TEST_PATIENCE_MS)
            .ok()
            .map(|s| {
                let millis = u64::from_str(&s).expect(
                    "Could not parse RUST_TEST_PATIENCE_MS environment \
                         variable.",
                );
                Duration::from_millis(millis)
            })
            .unwrap_or(crate::DEFAULT_TEST_PATIENCE);
        async {
            let start_t = Instant::now();
            for i in 1..=MAX_RETRIES {
                tracing::info!(retries_remaining = MAX_RETRIES - i);
                let scrape = client.get("/metrics").await;
                match self.is_in(scrape) {
                    Ok(()) => break,
                    Err(e) if i == MAX_RETRIES => panic!(
                        "{}\n  retried {} times ({:?})",
                        e,
                        MAX_RETRIES,
                        start_t.elapsed()
                    ),
                    Err(_) => {
                        tracing::trace!("waiting...");
                        tokio::time::sleep(patience).await;
                        std::thread::yield_now();
                        tracing::trace!("done")
                    }
                }
            }
        }
        .instrument(tracing::trace_span!(
            "assert_eventually",
            patience  = ?patience,
            max_retries = MAX_RETRIES,
        ))
        .await
    }
}

impl Labels {
    pub fn label(mut self, key: impl AsRef<str>, val: impl fmt::Display) -> Self {
        self.0.insert(key.as_ref().to_owned(), val.to_string());
        self
    }

    /// Return a new `Labels` consisting of all the labels in `self` plus all
    /// the labels in `other`.
    ///
    /// If `other` defines values for labels that are also defined in `self`,
    /// the values from `other` overwrite the values in `self`.
    pub fn and(&self, other: Labels) -> Labels {
        let mut new_labels = self.0.clone();
        new_labels.extend(other.0.into_iter());
        Labels(new_labels)
    }

    pub fn metric(&self, name: impl Into<String>) -> MetricMatch {
        MetricMatch {
            labels: self.0.clone(),
            name: name.into(),
            value: None,
        }
    }
}

// === impl MatchErr ===

impl fmt::Debug for MatchErr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl fmt::Display for MatchErr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}
