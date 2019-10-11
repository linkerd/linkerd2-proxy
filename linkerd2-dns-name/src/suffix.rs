use crate::Name;
use std::convert::TryFrom;
use std::fmt;

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum Suffix {
    Root, // The `.` suffix.
    Name(Name),
}

impl fmt::Display for Suffix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Suffix::Root => write!(f, "."),
            Suffix::Name(n) => n.fmt(f),
        }
    }
}

impl From<Name> for Suffix {
    fn from(n: Name) -> Self {
        Suffix::Name(n)
    }
}

impl<'s> TryFrom<&'s str> for Suffix {
    type Error = <Name as TryFrom<&'s [u8]>>::Error;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        if s == "." {
            Ok(Suffix::Root)
        } else {
            Name::try_from(s.as_bytes()).map(|n| n.into())
        }
    }
}

impl Suffix {
    pub fn contains(&self, name: &Name) -> bool {
        match self {
            Suffix::Root => true,
            Suffix::Name(ref sfx) => {
                let name = name.without_trailing_dot();
                let sfx = sfx.without_trailing_dot();
                name.ends_with(sfx) && {
                    name.len() == sfx.len() || {
                        // foo.bar.bah (11)
                        // bar.bah (7)
                        let idx = name.len() - sfx.len();
                        let (hd, _) = name.split_at(idx);
                        hd.ends_with('.')
                    }
                }
            }
        }
    }
}
