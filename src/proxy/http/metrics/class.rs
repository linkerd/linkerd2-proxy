use http;
use std::fmt;

use linkerd2_metrics::FmtLabels;

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct Class {
    status_code: StatusCode,
    grpc_status: Option<GrpcStatus>,
}

enum Classification {
    Success,
    Failure,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
struct StatusCode(http::StatusCode);

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
struct GrpcStatus(u32);

impl FmtLabels for Class {
    fn fmt_labels(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let status = (&self.status_code, self.grpc_status.as_ref());
        (self.classification(), status).fmt_labels(f)
    }
}

impl FmtLabels for StatusCode {
    fn fmt_labels(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "status_code=\"{}\"", self.0)
    }
}

impl FmtLabels for GrpcStatus {
    fn fmt_labels(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "grpc_status_code=\"{}\"", self.0)
    }
}

impl Class {
    fn classification(&self) -> Classification {
        if self.status_code.0.is_server_error() {
            return Classification::Failure;
        }

        if let Some(GrpcStatus(code)) = self.grpc_status {
            if code != 0 {
                return Classification::Failure;
            }
        }

        Classification::Success
    }
}

impl FmtLabels for Classification {
    fn fmt_labels(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            &Classification::Success => f.pad("classification=\"success\""),
            &Classification::Failure => f.pad("classification=\"failure\""),
        }
    }
}

