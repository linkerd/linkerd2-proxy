//! gRPC bindings for OpenCensus.
//!
//! Vendored from <https://github.com/census-instrumentation/opencensus-proto/>.

#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![allow(clippy::derive_partial_eq_without_eq)]
#![forbid(unsafe_code)]

pub mod agent {
    pub mod trace {
        pub mod v1 {
            include!("gen/opencensus.proto.agent.trace.v1.rs");
        }
    }

    pub mod common {
        pub mod v1 {
            include!("gen/opencensus.proto.agent.common.v1.rs");
        }
    }
}

pub mod trace {
    pub mod v1 {
        include!("gen/opencensus.proto.trace.v1.rs");
    }
}

pub mod resource {
    pub mod v1 {
        include!("gen/opencensus.proto.resource.v1.rs");
    }
}
