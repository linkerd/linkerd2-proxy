use super::Tracing;
use linkerd2_stack as svc;

use span;

/// Creates a layer that handles Tracing start and finish events
///
/// The tracer layer is responsible for applying tracing
pub fn layer(name: String) -> Layer {
    Layer { name: name.clone() }
}

#[derive(Clone, Debug)]
pub struct Layer {
    name: String
}

#[derive(Clone, Debug)]
pub struct Stack<M> {
    inner: M,
    name: String
}

impl<T, M> svc::Layer<T, T, M> for Layer
where
    M: svc::Stack<T>,
{
    type Value = <Stack<M> as svc::Stack<T>>::Value;
    type Error = <Stack<M> as svc::Stack<T>>::Error;
    type Stack = Stack<M>;

    fn bind(&self, inner: M) -> Self::Stack {
        Stack {
            inner,
            name: self.name.clone()
        }
    }
}

impl<T, M> svc::Stack<T> for Stack<M>
where
    M: svc::Stack<T>,
{
    type Value = Tracing<M::Value>;
    type Error = M::Error;

    fn make(&self, target: &T) -> Result<Self::Value, Self::Error> {
        let span: Option<span::Span> = span::SpanContext::new().span_from_request(String::from("spanName"), String::from("request_place_holder"));
        let inner = self.inner.make(target)?;
        Ok(Tracing::new(inner, self.name.clone(), span))
    }
}