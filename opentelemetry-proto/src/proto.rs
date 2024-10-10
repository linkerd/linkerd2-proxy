pub mod collector {
    pub mod trace {
        pub mod v1 {
            include!("gen/opentelemetry.proto.collector.trace.v1.rs");
        }
    }
}

pub mod common {
    pub mod v1 {
        include!("gen/opentelemetry.proto.common.v1.rs");
    }
}

pub mod trace {
    pub mod v1 {
        include!("gen/opentelemetry.proto.trace.v1.rs");
    }
}

pub mod resource {
    pub mod v1 {
        include!("gen/opentelemetry.proto.resource.v1.rs");
    }
}