/// Like `std::option::Option<C>` but `None` carries a reason why the value
/// isn't available.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum Conditional<C, R> {
    Some(C),
    None(R),
}

impl<C, R> Conditional<C, R>
where
    C: Clone,
    R: Copy + Clone,
{
    pub fn and_then<CR, RR, F>(self, f: F) -> Conditional<CR, RR>
    where
        CR: Clone,
        R: Into<RR>,
        RR: Clone,
        F: FnOnce(C) -> Conditional<CR, RR>,
    {
        match self {
            Conditional::Some(c) => f(c),
            Conditional::None(r) => Conditional::None(r.into()),
        }
    }

    pub fn as_ref<'a>(&'a self) -> Conditional<&'a C, R> {
        match self {
            Conditional::Some(c) => Conditional::Some(&c),
            Conditional::None(r) => Conditional::None(*r),
        }
    }

    pub fn map<CR, RR, F>(self, f: F) -> Conditional<CR, RR>
    where
        CR: Clone,
        R: Into<RR>,
        RR: Clone,
        F: FnOnce(C) -> CR,
    {
        self.and_then(|c| Conditional::Some(f(c)))
    }
}
