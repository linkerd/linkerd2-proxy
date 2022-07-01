pub mod inject_failure;
pub mod modify_header;
pub mod redirect;

pub use self::{
    inject_failure::{Distribution, FailureResponse, InjectFailure},
    modify_header::ModifyHeader,
    redirect::{InvalidRedirect, RedirectRequest, Redirection},
};

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum ModifyPath {
    ReplaceFullPath(String),
    ReplacePrefixMatch(String),
}
