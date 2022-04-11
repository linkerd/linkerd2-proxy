/// Resource information.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Resource {
    /// Type identifier for the resource.
    #[prost(string, tag="1")]
    pub r#type: ::prost::alloc::string::String,
    /// Set of labels that describe the resource.
    #[prost(map="string, string", tag="2")]
    pub labels: ::std::collections::HashMap<::prost::alloc::string::String, ::prost::alloc::string::String>,
}
