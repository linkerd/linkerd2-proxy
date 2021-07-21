#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]
#![allow(clippy::inconsistent_struct_constructor)]

mod layer;
mod service;

pub use self::layer::RecordErrorLayer;
pub use self::service::RecordError;
pub use linkerd_metrics::FmtLabels;
use linkerd_metrics::{self as metrics, Counter, FmtMetrics};
use parking_lot::Mutex;
use std::{collections::HashMap, error::Error, fmt, hash::Hash, marker::PhantomData, sync::Arc};

pub trait LabelError {
    type Labels: FmtLabels + Hash + Eq;

    fn label_error(&self, error: &(dyn Error + 'static)) -> Option<Self::Labels>;

    fn or_else<B>(self, other: B) -> OrElse<Self, B>
    where
        Self: Sized,
    {
        OrElse { a: self, b: other }
    }
}

pub type Metric = metrics::Metric<'static, &'static str, Counter>;

/// Produces layers and reports results.
#[derive(Debug)]
pub struct Registry<K>
where
    K: Hash + Eq,
{
    errors: Arc<Mutex<HashMap<K, Counter>>>,
    metric: Metric,
}

#[derive(Debug, Copy, Clone, Default)]
pub struct OrElse<A, B> {
    a: A,
    b: B,
}

#[derive(Clone)]
pub struct LabelFn<L, F = fn(&(dyn Error + 'static)) -> Option<L>> {
    f: F,
    _p: PhantomData<fn(L)>,
}

impl<K: Hash + Eq> Registry<K> {
    pub fn new(metric: Metric) -> Self {
        Self {
            errors: Default::default(),
            metric,
        }
    }

    pub fn layer<L>(&self, label: L) -> RecordErrorLayer<L, K> {
        RecordErrorLayer::new(label, self.errors.clone())
    }
}

impl<K: Hash + Eq> Clone for Registry<K> {
    fn clone(&self) -> Self {
        Self {
            errors: self.errors.clone(),
            metric: self.metric,
        }
    }
}

impl<K: FmtLabels + Hash + Eq> FmtMetrics for Registry<K> {
    fn fmt_metrics(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let errors = self.errors.lock();
        if errors.is_empty() {
            return Ok(());
        }

        self.metric.fmt_help(f)?;
        self.metric.fmt_scopes(f, errors.iter(), |c| &c)?;

        Ok(())
    }
}

// === impl OrElse ===

impl<A, B> LabelError for OrElse<A, B>
where
    A: LabelError,
    B: LabelError<Labels = A::Labels>,
{
    type Labels = A::Labels;

    #[inline]
    fn label_error(&self, error: &(dyn Error + 'static)) -> Option<Self::Labels> {
        self.a
            .label_error(error)
            .or_else(|| self.b.label_error(error))
    }
}

// === impl LabelFn ===

pub fn label_fn<L, F>(f: F) -> LabelFn<L, F>
where
    F: Fn(&(dyn Error + 'static)) -> Option<L>,
    L: FmtLabels + Hash + Eq,
{
    LabelFn { f, _p: PhantomData }
}

impl<L, F> LabelError for LabelFn<L, F>
where
    F: Fn(&(dyn Error + 'static)) -> Option<L>,
    L: FmtLabels + Hash + Eq,
{
    type Labels = L;

    #[inline]
    fn label_error(&self, error: &(dyn Error + 'static)) -> Option<Self::Labels> {
        (self.f)(error)
    }
}

impl<L, F> fmt::Debug for LabelFn<L, F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use std::any::type_name;
        f.debug_struct(type_name::<Self>())
            .field("label_type", &format_args!("{}", type_name::<L>()))
            .field("f", &format_args!("{}", type_name::<F>()))
            .finish()
    }
}
