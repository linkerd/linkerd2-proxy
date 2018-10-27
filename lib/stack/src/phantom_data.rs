use std::marker::PhantomData;

pub fn layer<T, M>() -> Layer<T, M>
where
    M: super::Stack<T>,
{
    Layer(PhantomData)
}

#[derive(Clone, Debug)]
pub struct Layer<T, M>(PhantomData<fn() -> (T, M)>);

#[derive(Clone, Debug)]
pub struct Stack<T, M> {
    inner: M,
    _p: PhantomData<fn() -> (T, M)>,
}

impl<T, M: super::Stack<T>> super::Layer<T, T, M> for Layer<T, M> {
    type Value = <Stack<T, M> as super::Stack<T>>::Value;
    type Error = <Stack<T, M> as super::Stack<T>>::Error;
    type Stack = Stack<T, M>;

    fn bind(&self, inner: M) -> Self::Stack {
        Stack {
            inner,
            _p: PhantomData
        }
    }
}

impl<T, M: super::Stack<T>> super::Stack<T> for Stack<T, M> {
    type Value = M::Value;
    type Error = M::Error;

    fn make(&self, target: &T) -> Result<Self::Value, Self::Error> {
        self.inner.make(target)
    }
}
