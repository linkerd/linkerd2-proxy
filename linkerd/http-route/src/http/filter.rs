pub mod inject_failure;
pub mod modify_header;
pub mod redirect;
pub mod forwarded_for;

pub use self::{
    inject_failure::{Distribution, FailureResponse, InjectFailure},
    modify_header::ModifyHeader,
    redirect::{InvalidRedirect, RedirectRequest, Redirection},
    forwarded_for::ForwardedFor,
};

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum ModifyPath {
    ReplaceFullPath(String),
    ReplacePrefixMatch(String),
}
