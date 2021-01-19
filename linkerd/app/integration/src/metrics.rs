use std::fmt;
use std::string::ToString;

#[derive(Debug, Clone)]
pub struct MetricMatch {
    name: String,
    labels: Vec<String>,
    value: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Labels(Vec<String>);

pub fn metric(name: impl Into<String>) -> MetricMatch {
    MetricMatch::new(name)
}

pub fn labels() -> Labels {
    Labels::default()
}

#[derive(Eq, PartialEq)]
pub struct MatchErr(String);

impl MetricMatch {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            labels: Vec::new(),
            value: None,
        }
    }

    pub fn label(mut self, key: impl AsRef<str>, val: impl fmt::Display) -> Self {
        self.labels.push(format!("{}=\"{}\"", key.as_ref(), val));
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
        let lines = scrape
            .lines()
            .filter(|&line| line.starts_with(&self.name))
            .collect::<Vec<&str>>();
        let mut candidates = Vec::new();
        'lines: for &line in &lines {
            if let Some(labels) = line
                .split('{')
                .nth(1)
                .and_then(|line| line.split('}').next())
            {
                for label in &self.labels {
                    if !labels.contains(&label[..]) {
                        continue 'lines;
                    }
                }
                candidates.push(line);
            }
        }

        if candidates.is_empty() {
            return Err(MatchErr(format!(
                "did not find a `{}` metric\n  with labels: {:?}\nfound metrics: {:#?}",
                self.name, self.labels, lines
            )));
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
            return Err(MatchErr(format!(
                "did not find a `{}` metric\n  with labels: {:?}\n    and value: {}\nfound metrics: {:#?}",
                self.name, self.labels, expected_value, candidates
            )));
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
        self.0.push(format!("{}=\"{}\"", key.as_ref(), val));
        self
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
