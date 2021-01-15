use std::fmt;
use std::string::ToString;

#[derive(Debug)]
pub struct MetricMatch {
    name: String,
    labels: Vec<String>,
    value: Option<String>,
}

pub fn metric(name: impl Into<String>) -> MetricMatch {
    MetricMatch::new(name)
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

    pub fn with_label(mut self, key: impl AsRef<str>, val: impl fmt::Display) -> Self {
        self.labels.push(format!("{}=\"{}\"", key.as_ref(), val));
        self
    }

    pub fn with_value(self, value: impl ToString) -> Self {
        Self {
            value: Some(value.to_string()),
            ..self
        }
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
                .split("{")
                .skip(1)
                .next()
                .and_then(|line| line.split("}").next())
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
                if let Some(value) = line.split("}").skip(1).next() {
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
}

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
