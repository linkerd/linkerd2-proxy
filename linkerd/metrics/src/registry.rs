use crate::LastUpdate;
use std::{
    borrow::Borrow,
    collections::HashMap,
    hash::Hash,
    sync::{Arc, Mutex},
    time::Duration,
};
pub struct Registry<K, M>
where
    K: Hash + Eq,
{
    inner: HashMap<K, Arc<Mutex<M>>>,
    retain_idle: Duration,
}

impl<K, M> Registry<K, M>
where
    K: Hash + Eq,
    M: LastUpdate,
{
    pub fn retain_active(&mut self) {
        let idle = self.retain_idle;
        self.inner
            .retain(|_, metric| metric.lock().unwrap().last_update().elapsed() <= idle)
    }
}

impl<K, M> Registry<K, M>
where
    K: Hash + Eq,
{
    pub fn get<Q>(&self, q: &Q) -> Option<&Arc<Mutex<M>>>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        self.inner.get(q)
    }

    pub fn get_mut<Q>(&mut self, q: &Q) -> Option<&mut Arc<Mutex<M>>>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        self.inner.get_mut(q)
    }
}
