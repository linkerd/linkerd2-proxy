#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Header {
    /// The target port.
    #[prost(int32, tag = "1")]
    pub port: i32,
    /// An optional hostname. Intended for gateway forwarding.
    #[prost(string, tag = "2")]
    pub name: ::prost::alloc::string::String,
    /// The session protocol, if one is known. When no protocol is specified, the
    /// connection is handled opaquely.
    #[prost(message, optional, tag = "3")]
    pub session_protocol: ::core::option::Option<SessionProtocol>,
}
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct SessionProtocol {
    #[prost(oneof = "session_protocol::Kind", tags = "1, 2")]
    pub kind: ::core::option::Option<session_protocol::Kind>,
}
/// Nested message and enum types in `SessionProtocol`.
pub mod session_protocol {
    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct Http1 {}
    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct Http2 {}
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Kind {
        #[prost(message, tag = "1")]
        Http1(Http1),
        #[prost(message, tag = "2")]
        Http2(Http2),
    }
}
