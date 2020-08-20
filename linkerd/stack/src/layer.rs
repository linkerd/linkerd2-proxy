pub use tower::layer::Layer;

/// Make a `Layer` from a closure.
pub fn mk<F>(f: F) -> LayerFn<F> {
    LayerFn(f)
}

/// Make a `Layer` from a closure.
#[derive(Clone, Copy, Debug)]
pub struct LayerFn<F>(F);

impl<F, S, Out> Layer<S> for LayerFn<F>
where
    F: Fn(S) -> Out,
{
    type Service = Out;

    fn layer(&self, inner: S) -> Self::Service {
        (self.0)(inner)
    }
}
