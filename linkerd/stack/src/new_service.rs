/// Immediately and infalliby creates (usually) a Service.
pub trait NewService<T> {
    type Service;

    fn new_service(&self, target: T) -> Self::Service;
}

impl<F, T, S> NewService<T> for F
where
    F: Fn(T) -> S,
{
    type Service = S;

    fn new_service(&self, target: T) -> Self::Service {
        (self)(target)
    }
}
