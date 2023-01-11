use crate::Meta;
use std::{sync::Arc, time};

pub type Protocol = crate::Protocol<()>;

pub type Policy = crate::Policy<()>;

impl Policy {
    pub fn invalid_http(timeout: time::Duration) -> Self {
        let meta = Arc::new(Meta::Default {
            name: "invalid".into(),
        });

        Self {
            meta: meta.clone(),
            protocol: Protocol::Detect {
                timeout,
                http: Arc::new([crate::http::Route {
                    hosts: vec![],
                    rules: vec![crate::http::Rule {
                        matches: vec![crate::http::r#match::MatchRequest::default()],
                        policy: crate::http::Policy {
                            distribution: (),
                            meta,
                            authorizations: Arc::new([]),
                            filters: vec![crate::http::Filter::InternalError(
                                "invalid server configuration",
                            )],
                        },
                    }],
                }]),
                opaque: Arc::new([]),
            },
        }
    }
}
