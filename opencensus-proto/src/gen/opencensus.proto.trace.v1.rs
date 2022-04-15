/// A span represents a single operation within a trace. Spans can be
/// nested to form a trace tree. Spans may also be linked to other spans
/// from the same or different trace. And form graphs. Often, a trace
/// contains a root span that describes the end-to-end latency, and one
/// or more subspans for its sub-operations. A trace can also contain
/// multiple root spans, or none at all. Spans do not need to be
/// contiguous - there may be gaps or overlaps between spans in a trace.
///
/// The next id is 17.
/// TODO(bdrutu): Add an example.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Span {
    /// A unique identifier for a trace. All spans from the same trace share
    /// the same `trace_id`. The ID is a 16-byte array. An ID with all zeroes
    /// is considered invalid.
    ///
    /// This field is semantically required. Receiver should generate new
    /// random trace_id if empty or invalid trace_id was received.
    ///
    /// This field is required.
    #[prost(bytes="vec", tag="1")]
    pub trace_id: ::prost::alloc::vec::Vec<u8>,
    /// A unique identifier for a span within a trace, assigned when the span
    /// is created. The ID is an 8-byte array. An ID with all zeroes is considered
    /// invalid.
    ///
    /// This field is semantically required. Receiver should generate new
    /// random span_id if empty or invalid span_id was received.
    ///
    /// This field is required.
    #[prost(bytes="vec", tag="2")]
    pub span_id: ::prost::alloc::vec::Vec<u8>,
    /// The Tracestate on the span.
    #[prost(message, optional, tag="15")]
    pub tracestate: ::core::option::Option<span::Tracestate>,
    /// The `span_id` of this span's parent span. If this is a root span, then this
    /// field must be empty. The ID is an 8-byte array.
    #[prost(bytes="vec", tag="3")]
    pub parent_span_id: ::prost::alloc::vec::Vec<u8>,
    /// A description of the span's operation.
    ///
    /// For example, the name can be a qualified method name or a file name
    /// and a line number where the operation is called. A best practice is to use
    /// the same display name at the same call point in an application.
    /// This makes it easier to correlate spans in different traces.
    ///
    /// This field is semantically required to be set to non-empty string.
    /// When null or empty string received - receiver may use string "name"
    /// as a replacement. There might be smarted algorithms implemented by
    /// receiver to fix the empty span name.
    ///
    /// This field is required.
    #[prost(message, optional, tag="4")]
    pub name: ::core::option::Option<TruncatableString>,
    /// Distinguishes between spans generated in a particular context. For example,
    /// two spans with the same name may be distinguished using `CLIENT` (caller)
    /// and `SERVER` (callee) to identify queueing latency associated with the span.
    #[prost(enumeration="span::SpanKind", tag="14")]
    pub kind: i32,
    /// The start time of the span. On the client side, this is the time kept by
    /// the local machine where the span execution starts. On the server side, this
    /// is the time when the server's application handler starts running.
    ///
    /// This field is semantically required. When not set on receive -
    /// receiver should set it to the value of end_time field if it was
    /// set. Or to the current time if neither was set. It is important to
    /// keep end_time > start_time for consistency.
    ///
    /// This field is required.
    #[prost(message, optional, tag="5")]
    pub start_time: ::core::option::Option<::prost_types::Timestamp>,
    /// The end time of the span. On the client side, this is the time kept by
    /// the local machine where the span execution ends. On the server side, this
    /// is the time when the server application handler stops running.
    ///
    /// This field is semantically required. When not set on receive -
    /// receiver should set it to start_time value. It is important to
    /// keep end_time > start_time for consistency.
    ///
    /// This field is required.
    #[prost(message, optional, tag="6")]
    pub end_time: ::core::option::Option<::prost_types::Timestamp>,
    /// A set of attributes on the span.
    #[prost(message, optional, tag="7")]
    pub attributes: ::core::option::Option<span::Attributes>,
    /// A stack trace captured at the start of the span.
    #[prost(message, optional, tag="8")]
    pub stack_trace: ::core::option::Option<StackTrace>,
    /// The included time events.
    #[prost(message, optional, tag="9")]
    pub time_events: ::core::option::Option<span::TimeEvents>,
    /// The included links.
    #[prost(message, optional, tag="10")]
    pub links: ::core::option::Option<span::Links>,
    /// An optional final status for this span. Semantically when Status
    /// wasn't set it is means span ended without errors and assume
    /// Status.Ok (code = 0).
    #[prost(message, optional, tag="11")]
    pub status: ::core::option::Option<Status>,
    /// An optional resource that is associated with this span. If not set, this span 
    /// should be part of a batch that does include the resource information, unless resource 
    /// information is unknown.
    #[prost(message, optional, tag="16")]
    pub resource: ::core::option::Option<super::super::resource::v1::Resource>,
    /// A highly recommended but not required flag that identifies when a
    /// trace crosses a process boundary. True when the parent_span belongs
    /// to the same process as the current span. This flag is most commonly
    /// used to indicate the need to adjust time as clocks in different
    /// processes may not be synchronized.
    #[prost(message, optional, tag="12")]
    pub same_process_as_parent_span: ::core::option::Option<bool>,
    /// An optional number of child spans that were generated while this span
    /// was active. If set, allows an implementation to detect missing child spans.
    #[prost(message, optional, tag="13")]
    pub child_span_count: ::core::option::Option<u32>,
}
/// Nested message and enum types in `Span`.
pub mod span {
    /// This field conveys information about request position in multiple distributed tracing graphs.
    /// It is a list of Tracestate.Entry with a maximum of 32 members in the list.
    ///
    /// See the <https://github.com/w3c/distributed-tracing> for more details about this field.
    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct Tracestate {
        /// A list of entries that represent the Tracestate.
        #[prost(message, repeated, tag="1")]
        pub entries: ::prost::alloc::vec::Vec<tracestate::Entry>,
    }
    /// Nested message and enum types in `Tracestate`.
    pub mod tracestate {
        #[derive(Clone, PartialEq, ::prost::Message)]
        pub struct Entry {
            /// The key must begin with a lowercase letter, and can only contain
            /// lowercase letters 'a'-'z', digits '0'-'9', underscores '_', dashes
            /// '-', asterisks '*', and forward slashes '/'.
            #[prost(string, tag="1")]
            pub key: ::prost::alloc::string::String,
            /// The value is opaque string up to 256 characters printable ASCII
            /// RFC0020 characters (i.e., the range 0x20 to 0x7E) except ',' and '='.
            /// Note that this also excludes tabs, newlines, carriage returns, etc.
            #[prost(string, tag="2")]
            pub value: ::prost::alloc::string::String,
        }
    }
    /// A set of attributes, each with a key and a value.
    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct Attributes {
        /// The set of attributes. The value can be a string, an integer, a double
        /// or the Boolean values `true` or `false`. Note, global attributes like 
        /// server name can be set as tags using resource API. Examples of attributes:
        ///
        ///     "/http/user_agent": "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_14_2) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/71.0.3578.98 Safari/537.36"
        ///     "/http/server_latency": 300
        ///     "abc.com/myattribute": true
        ///     "abc.com/score": 10.239
        #[prost(map="string, message", tag="1")]
        pub attribute_map: ::std::collections::HashMap<::prost::alloc::string::String, super::AttributeValue>,
        /// The number of attributes that were discarded. Attributes can be discarded
        /// because their keys are too long or because there are too many attributes.
        /// If this value is 0, then no attributes were dropped.
        #[prost(int32, tag="2")]
        pub dropped_attributes_count: i32,
    }
    /// A time-stamped annotation or message event in the Span.
    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct TimeEvent {
        /// The time the event occurred.
        #[prost(message, optional, tag="1")]
        pub time: ::core::option::Option<::prost_types::Timestamp>,
        /// A `TimeEvent` can contain either an `Annotation` object or a
        /// `MessageEvent` object, but not both.
        #[prost(oneof="time_event::Value", tags="2, 3")]
        pub value: ::core::option::Option<time_event::Value>,
    }
    /// Nested message and enum types in `TimeEvent`.
    pub mod time_event {
        /// A text annotation with a set of attributes.
        #[derive(Clone, PartialEq, ::prost::Message)]
        pub struct Annotation {
            /// A user-supplied message describing the event.
            #[prost(message, optional, tag="1")]
            pub description: ::core::option::Option<super::super::TruncatableString>,
            /// A set of attributes on the annotation.
            #[prost(message, optional, tag="2")]
            pub attributes: ::core::option::Option<super::Attributes>,
        }
        /// An event describing a message sent/received between Spans.
        #[derive(Clone, PartialEq, ::prost::Message)]
        pub struct MessageEvent {
            /// The type of MessageEvent. Indicates whether the message was sent or
            /// received.
            #[prost(enumeration="message_event::Type", tag="1")]
            pub r#type: i32,
            /// An identifier for the MessageEvent's message that can be used to match
            /// SENT and RECEIVED MessageEvents. For example, this field could
            /// represent a sequence ID for a streaming RPC. It is recommended to be
            /// unique within a Span.
            #[prost(uint64, tag="2")]
            pub id: u64,
            /// The number of uncompressed bytes sent or received.
            #[prost(uint64, tag="3")]
            pub uncompressed_size: u64,
            /// The number of compressed bytes sent or received. If zero, assumed to
            /// be the same size as uncompressed.
            #[prost(uint64, tag="4")]
            pub compressed_size: u64,
        }
        /// Nested message and enum types in `MessageEvent`.
        pub mod message_event {
            /// Indicates whether the message was sent or received.
            #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, ::prost::Enumeration)]
            #[repr(i32)]
            pub enum Type {
                /// Unknown event type.
                Unspecified = 0,
                /// Indicates a sent message.
                Sent = 1,
                /// Indicates a received message.
                Received = 2,
            }
        }
        /// A `TimeEvent` can contain either an `Annotation` object or a
        /// `MessageEvent` object, but not both.
        #[derive(Clone, PartialEq, ::prost::Oneof)]
        pub enum Value {
            /// A text annotation with a set of attributes.
            #[prost(message, tag="2")]
            Annotation(Annotation),
            /// An event describing a message sent/received between Spans.
            #[prost(message, tag="3")]
            MessageEvent(MessageEvent),
        }
    }
    /// A collection of `TimeEvent`s. A `TimeEvent` is a time-stamped annotation
    /// on the span, consisting of either user-supplied key-value pairs, or
    /// details of a message sent/received between Spans.
    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct TimeEvents {
        /// A collection of `TimeEvent`s.
        #[prost(message, repeated, tag="1")]
        pub time_event: ::prost::alloc::vec::Vec<TimeEvent>,
        /// The number of dropped annotations in all the included time events.
        /// If the value is 0, then no annotations were dropped.
        #[prost(int32, tag="2")]
        pub dropped_annotations_count: i32,
        /// The number of dropped message events in all the included time events.
        /// If the value is 0, then no message events were dropped.
        #[prost(int32, tag="3")]
        pub dropped_message_events_count: i32,
    }
    /// A pointer from the current span to another span in the same trace or in a
    /// different trace. For example, this can be used in batching operations,
    /// where a single batch handler processes multiple requests from different
    /// traces or when the handler receives a request from a different project.
    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct Link {
        /// A unique identifier of a trace that this linked span is part of. The ID is a 
        /// 16-byte array.
        #[prost(bytes="vec", tag="1")]
        pub trace_id: ::prost::alloc::vec::Vec<u8>,
        /// A unique identifier for the linked span. The ID is an 8-byte array.
        #[prost(bytes="vec", tag="2")]
        pub span_id: ::prost::alloc::vec::Vec<u8>,
        /// The relationship of the current span relative to the linked span.
        #[prost(enumeration="link::Type", tag="3")]
        pub r#type: i32,
        /// A set of attributes on the link.
        #[prost(message, optional, tag="4")]
        pub attributes: ::core::option::Option<Attributes>,
        /// The Tracestate associated with the link.
        #[prost(message, optional, tag="5")]
        pub tracestate: ::core::option::Option<Tracestate>,
    }
    /// Nested message and enum types in `Link`.
    pub mod link {
        /// The relationship of the current span relative to the linked span: child,
        /// parent, or unspecified.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, ::prost::Enumeration)]
        #[repr(i32)]
        pub enum Type {
            /// The relationship of the two spans is unknown, or known but other
            /// than parent-child.
            Unspecified = 0,
            /// The linked span is a child of the current span.
            ChildLinkedSpan = 1,
            /// The linked span is a parent of the current span.
            ParentLinkedSpan = 2,
        }
    }
    /// A collection of links, which are references from this span to a span
    /// in the same or different trace.
    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct Links {
        /// A collection of links.
        #[prost(message, repeated, tag="1")]
        pub link: ::prost::alloc::vec::Vec<Link>,
        /// The number of dropped links after the maximum size was enforced. If
        /// this value is 0, then no links were dropped.
        #[prost(int32, tag="2")]
        pub dropped_links_count: i32,
    }
    /// Type of span. Can be used to specify additional relationships between spans
    /// in addition to a parent/child relationship.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, ::prost::Enumeration)]
    #[repr(i32)]
    pub enum SpanKind {
        /// Unspecified.
        Unspecified = 0,
        /// Indicates that the span covers server-side handling of an RPC or other
        /// remote network request.
        Server = 1,
        /// Indicates that the span covers the client-side wrapper around an RPC or
        /// other remote request.
        Client = 2,
    }
}
/// The `Status` type defines a logical error model that is suitable for different
/// programming environments, including REST APIs and RPC APIs. This proto's fields
/// are a subset of those of
/// \[google.rpc.Status\](<https://github.com/googleapis/googleapis/blob/master/google/rpc/status.proto>),
/// which is used by \[gRPC\](<https://github.com/grpc>).
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Status {
    /// The status code. This is optional field. It is safe to assume 0 (OK)
    /// when not set.
    #[prost(int32, tag="1")]
    pub code: i32,
    /// A developer-facing error message, which should be in English.
    #[prost(string, tag="2")]
    pub message: ::prost::alloc::string::String,
}
/// The value of an Attribute.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct AttributeValue {
    /// The type of the value.
    #[prost(oneof="attribute_value::Value", tags="1, 2, 3, 4")]
    pub value: ::core::option::Option<attribute_value::Value>,
}
/// Nested message and enum types in `AttributeValue`.
pub mod attribute_value {
    /// The type of the value.
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Value {
        /// A string up to 256 bytes long.
        #[prost(message, tag="1")]
        StringValue(super::TruncatableString),
        /// A 64-bit signed integer.
        #[prost(int64, tag="2")]
        IntValue(i64),
        /// A Boolean value represented by `true` or `false`.
        #[prost(bool, tag="3")]
        BoolValue(bool),
        /// A double value.
        #[prost(double, tag="4")]
        DoubleValue(f64),
    }
}
/// The call stack which originated this span.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct StackTrace {
    /// Stack frames in this stack trace.
    #[prost(message, optional, tag="1")]
    pub stack_frames: ::core::option::Option<stack_trace::StackFrames>,
    /// The hash ID is used to conserve network bandwidth for duplicate
    /// stack traces within a single trace.
    ///
    /// Often multiple spans will have identical stack traces.
    /// The first occurrence of a stack trace should contain both
    /// `stack_frames` and a value in `stack_trace_hash_id`.
    ///
    /// Subsequent spans within the same request can refer
    /// to that stack trace by setting only `stack_trace_hash_id`.
    ///
    /// TODO: describe how to deal with the case where stack_trace_hash_id is
    /// zero because it was not set.
    #[prost(uint64, tag="2")]
    pub stack_trace_hash_id: u64,
}
/// Nested message and enum types in `StackTrace`.
pub mod stack_trace {
    /// A single stack frame in a stack trace.
    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct StackFrame {
        /// The fully-qualified name that uniquely identifies the function or
        /// method that is active in this frame.
        #[prost(message, optional, tag="1")]
        pub function_name: ::core::option::Option<super::TruncatableString>,
        /// An un-mangled function name, if `function_name` is
        /// \[mangled\](<http://www.avabodh.com/cxxin/namemangling.html>). The name can
        /// be fully qualified.
        #[prost(message, optional, tag="2")]
        pub original_function_name: ::core::option::Option<super::TruncatableString>,
        /// The name of the source file where the function call appears.
        #[prost(message, optional, tag="3")]
        pub file_name: ::core::option::Option<super::TruncatableString>,
        /// The line number in `file_name` where the function call appears.
        #[prost(int64, tag="4")]
        pub line_number: i64,
        /// The column number where the function call appears, if available.
        /// This is important in JavaScript because of its anonymous functions.
        #[prost(int64, tag="5")]
        pub column_number: i64,
        /// The binary module from where the code was loaded.
        #[prost(message, optional, tag="6")]
        pub load_module: ::core::option::Option<super::Module>,
        /// The version of the deployed source code.
        #[prost(message, optional, tag="7")]
        pub source_version: ::core::option::Option<super::TruncatableString>,
    }
    /// A collection of stack frames, which can be truncated.
    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct StackFrames {
        /// Stack frames in this call stack.
        #[prost(message, repeated, tag="1")]
        pub frame: ::prost::alloc::vec::Vec<StackFrame>,
        /// The number of stack frames that were dropped because there
        /// were too many stack frames.
        /// If this value is 0, then no stack frames were dropped.
        #[prost(int32, tag="2")]
        pub dropped_frames_count: i32,
    }
}
/// A description of a binary module.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Module {
    /// TODO: document the meaning of this field.
    /// For example: main binary, kernel modules, and dynamic libraries
    /// such as libc.so, sharedlib.so.
    #[prost(message, optional, tag="1")]
    pub module: ::core::option::Option<TruncatableString>,
    /// A unique identifier for the module, usually a hash of its
    /// contents.
    #[prost(message, optional, tag="2")]
    pub build_id: ::core::option::Option<TruncatableString>,
}
/// A string that might be shortened to a specified length.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct TruncatableString {
    /// The shortened string. For example, if the original string was 500 bytes long and
    /// the limit of the string was 128 bytes, then this value contains the first 128
    /// bytes of the 500-byte string. Note that truncation always happens on a
    /// character boundary, to ensure that a truncated string is still valid UTF-8.
    /// Because it may contain multi-byte characters, the size of the truncated string
    /// may be less than the truncation limit.
    #[prost(string, tag="1")]
    pub value: ::prost::alloc::string::String,
    /// The number of bytes removed from the original string. If this
    /// value is 0, then the string was not shortened.
    #[prost(int32, tag="2")]
    pub truncated_byte_count: i32,
}
/// Global configuration of the trace service. All fields must be specified, or
/// the default (zero) values will be used for each type.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct TraceConfig {
    /// The global default max number of attributes per span.
    #[prost(int64, tag="4")]
    pub max_number_of_attributes: i64,
    /// The global default max number of annotation events per span.
    #[prost(int64, tag="5")]
    pub max_number_of_annotations: i64,
    /// The global default max number of message events per span.
    #[prost(int64, tag="6")]
    pub max_number_of_message_events: i64,
    /// The global default max number of link entries per span.
    #[prost(int64, tag="7")]
    pub max_number_of_links: i64,
    /// The global default sampler used to make decisions on span sampling.
    #[prost(oneof="trace_config::Sampler", tags="1, 2, 3")]
    pub sampler: ::core::option::Option<trace_config::Sampler>,
}
/// Nested message and enum types in `TraceConfig`.
pub mod trace_config {
    /// The global default sampler used to make decisions on span sampling.
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Sampler {
        #[prost(message, tag="1")]
        ProbabilitySampler(super::ProbabilitySampler),
        #[prost(message, tag="2")]
        ConstantSampler(super::ConstantSampler),
        #[prost(message, tag="3")]
        RateLimitingSampler(super::RateLimitingSampler),
    }
}
/// Sampler that tries to uniformly sample traces with a given probability.
/// The probability of sampling a trace is equal to that of the specified probability.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ProbabilitySampler {
    /// The desired probability of sampling. Must be within [0.0, 1.0].
    #[prost(double, tag="1")]
    pub sampling_probability: f64,
}
/// Sampler that always makes a constant decision on span sampling.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ConstantSampler {
    #[prost(enumeration="constant_sampler::ConstantDecision", tag="1")]
    pub decision: i32,
}
/// Nested message and enum types in `ConstantSampler`.
pub mod constant_sampler {
    /// How spans should be sampled:
    /// - Always off
    /// - Always on
    /// - Always follow the parent Span's decision (off if no parent).
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, ::prost::Enumeration)]
    #[repr(i32)]
    pub enum ConstantDecision {
        AlwaysOff = 0,
        AlwaysOn = 1,
        AlwaysParent = 2,
    }
}
/// Sampler that tries to sample with a rate per time window.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RateLimitingSampler {
    /// Rate per second.
    #[prost(int64, tag="1")]
    pub qps: i64,
}
