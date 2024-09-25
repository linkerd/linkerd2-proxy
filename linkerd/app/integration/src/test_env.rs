use linkerd_app::env::{EnvError, Strings};
use std::collections::HashMap;

/// An implementation of [`Strings`] that wraps for use in tests.
#[derive(Clone, Default)]
pub struct TestEnv {
    values: HashMap<&'static str, String>,
}

// === impl TestEnv ===

impl TestEnv {
    /// Puts a new key-value pair in the test environment.
    pub fn put(&mut self, key: &'static str, value: String) {
        self.values.insert(key, value);
    }

    /// Returns true if this environment contains the given key.
    pub fn contains_key(&self, key: &'static str) -> bool {
        self.values.contains_key(key)
    }

    /// Removes a new key-value pair from the test environment.
    pub fn remove(&mut self, key: &'static str) {
        self.values.remove(key);
    }

    /// Extends this test environment using the other given [`TestEnv`].
    pub fn extend(&mut self, other: TestEnv) {
        self.values.extend(other.values);
    }
}

impl Strings for TestEnv {
    fn get(&self, key: &str) -> Result<Option<String>, EnvError> {
        Ok(self.values.get(key).cloned())
    }
}
