use linkerd_app::env::{EnvError, Strings};
use std::collections::HashMap;

#[derive(Clone, Default)]
pub struct TestEnv {
    values: HashMap<&'static str, String>,
}

// ===== impl TestEnv =====

impl TestEnv {
    pub fn put(&mut self, key: &'static str, value: String) {
        self.values.insert(key, value);
    }

    pub fn contains_key(&self, key: &'static str) -> bool {
        self.values.contains_key(key)
    }

    pub fn remove(&mut self, key: &'static str) {
        self.values.remove(key);
    }

    pub fn extend(&mut self, other: TestEnv) {
        self.values.extend(other.values);
    }
}

impl Strings for TestEnv {
    fn get(&self, key: &str) -> Result<Option<String>, EnvError> {
        Ok(self.values.get(key).cloned())
    }
}
