#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]

/// Like `std::option::Option<C>` but `None` carries a reason why the value
/// isn't available.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum Conditional<C, R> {
    Some(C),
    None(R),
}

impl<C, R> Conditional<C, R>
where
    R: Copy + Clone,
{
    pub fn as_ref(&self) -> Conditional<&'_ C, R> {
        match self {
            Conditional::Some(c) => Conditional::Some(c),
            Conditional::None(r) => Conditional::None(*r),
        }
    }

    pub fn reason(&self) -> Option<R> {
        match self {
            Conditional::Some(_) => None,
            Conditional::None(r) => Some(*r),
        }
    }
}

impl<C, R> Conditional<C, R> {
    pub fn and_then<CR, RR, F>(self, f: F) -> Conditional<CR, RR>
    where
        R: Into<RR>,
        F: FnOnce(C) -> Conditional<CR, RR>,
    {
        match self {
            Conditional::Some(c) => f(c),
            Conditional::None(r) => Conditional::None(r.into()),
        }
    }

    pub fn map<CR, RR, F>(self, f: F) -> Conditional<CR, RR>
    where
        R: Into<RR>,
        F: FnOnce(C) -> CR,
    {
        self.and_then(|c| Conditional::Some(f(c)))
    }

    pub fn or_else<CR, RR, F>(self, f: F) -> Conditional<CR, RR>
    where
        C: Into<CR>,
        F: FnOnce(R) -> Conditional<CR, RR>,
    {
        match self {
            Conditional::Some(c) => Conditional::Some(c.into()),
            Conditional::None(n) => f(n),
        }
    }

    pub fn map_reason<CR, RR, F>(self, f: F) -> Conditional<CR, RR>
    where
        C: Into<CR>,
        F: FnOnce(R) -> RR,
    {
        self.or_else(|r| Conditional::None(f(r)))
    }

    pub fn value(&self) -> Option<&C> {
        match self {
            Conditional::Some(v) => Some(v),
            Conditional::None(_) => None,
        }
    }

    pub fn is_none(&self) -> bool {
        match self {
            Conditional::None(_) => true,
            Conditional::Some(_) => false,
        }
    }

    pub fn is_some(&self) -> bool {
        !self.is_none()
    }
}

impl<'a, C, R> Conditional<&'a C, R>
where
    C: Clone,
{
    pub fn cloned(self) -> Conditional<C, R> {
        match self {
            Conditional::Some(c) => Conditional::Some(c.clone()),
            Conditional::None(r) => Conditional::None(r),
        }
    }
}
