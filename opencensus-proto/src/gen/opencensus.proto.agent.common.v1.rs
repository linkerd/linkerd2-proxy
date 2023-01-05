/// Identifier metadata of the Node that produces the span or tracing data.
/// Note, this is not the metadata about the Node or service that is described by associated spans.
/// In the future we plan to extend the identifier proto definition to support
/// additional information (e.g cloud id, etc.)
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Node {
    /// Identifier that uniquely identifies a process within a VM/container.
    #[prost(message, optional, tag = "1")]
    pub identifier: ::core::option::Option<ProcessIdentifier>,
    /// Information on the OpenCensus Library that initiates the stream.
    #[prost(message, optional, tag = "2")]
    pub library_info: ::core::option::Option<LibraryInfo>,
    /// Additional information on service.
    #[prost(message, optional, tag = "3")]
    pub service_info: ::core::option::Option<ServiceInfo>,
    /// Additional attributes.
    #[prost(map = "string, string", tag = "4")]
    pub attributes: ::std::collections::HashMap<
        ::prost::alloc::string::String,
        ::prost::alloc::string::String,
    >,
}
/// Identifier that uniquely identifies a process within a VM/container.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ProcessIdentifier {
    /// The host name. Usually refers to the machine/container name.
    /// For example: os.Hostname() in Go, socket.gethostname() in Python.
    #[prost(string, tag = "1")]
    pub host_name: ::prost::alloc::string::String,
    /// Process id.
    #[prost(uint32, tag = "2")]
    pub pid: u32,
    /// Start time of this ProcessIdentifier. Represented in epoch time.
    #[prost(message, optional, tag = "3")]
    pub start_timestamp: ::core::option::Option<::prost_types::Timestamp>,
}
/// Information on OpenCensus Library.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct LibraryInfo {
    /// Language of OpenCensus Library.
    #[prost(enumeration = "library_info::Language", tag = "1")]
    pub language: i32,
    /// Version of Agent exporter of Library.
    #[prost(string, tag = "2")]
    pub exporter_version: ::prost::alloc::string::String,
    /// Version of OpenCensus Library.
    #[prost(string, tag = "3")]
    pub core_library_version: ::prost::alloc::string::String,
}
/// Nested message and enum types in `LibraryInfo`.
pub mod library_info {
    #[derive(
        Clone,
        Copy,
        Debug,
        PartialEq,
        Eq,
        Hash,
        PartialOrd,
        Ord,
        ::prost::Enumeration
    )]
    #[repr(i32)]
    pub enum Language {
        Unspecified = 0,
        Cpp = 1,
        CSharp = 2,
        Erlang = 3,
        GoLang = 4,
        Java = 5,
        NodeJs = 6,
        Php = 7,
        Python = 8,
        Ruby = 9,
        WebJs = 10,
    }
    impl Language {
        /// String value of the enum field names used in the ProtoBuf definition.
        ///
        /// The values are not transformed in any way and thus are considered stable
        /// (if the ProtoBuf definition does not change) and safe for programmatic use.
        pub fn as_str_name(&self) -> &'static str {
            match self {
                Language::Unspecified => "LANGUAGE_UNSPECIFIED",
                Language::Cpp => "CPP",
                Language::CSharp => "C_SHARP",
                Language::Erlang => "ERLANG",
                Language::GoLang => "GO_LANG",
                Language::Java => "JAVA",
                Language::NodeJs => "NODE_JS",
                Language::Php => "PHP",
                Language::Python => "PYTHON",
                Language::Ruby => "RUBY",
                Language::WebJs => "WEB_JS",
            }
        }
        /// Creates an enum from field names used in the ProtoBuf definition.
        pub fn from_str_name(value: &str) -> ::core::option::Option<Self> {
            match value {
                "LANGUAGE_UNSPECIFIED" => Some(Self::Unspecified),
                "CPP" => Some(Self::Cpp),
                "C_SHARP" => Some(Self::CSharp),
                "ERLANG" => Some(Self::Erlang),
                "GO_LANG" => Some(Self::GoLang),
                "JAVA" => Some(Self::Java),
                "NODE_JS" => Some(Self::NodeJs),
                "PHP" => Some(Self::Php),
                "PYTHON" => Some(Self::Python),
                "RUBY" => Some(Self::Ruby),
                "WEB_JS" => Some(Self::WebJs),
                _ => None,
            }
        }
    }
}
/// Additional service information.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ServiceInfo {
    /// Name of the service.
    #[prost(string, tag = "1")]
    pub name: ::prost::alloc::string::String,
}
