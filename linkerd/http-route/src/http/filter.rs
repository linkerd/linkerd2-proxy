pub mod error_respond;
pub mod modify_header;
pub mod redirect;

pub use self::{
    error_respond::RespondWithError,
    modify_header::ModifyHeader,
    redirect::{InvalidRedirect, RedirectRequest, Redirection},
};

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum ModifyPath {
    ReplaceFullPath(String),
    ReplacePrefixMatch(String),
}
