use std::time::Duration;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServerParams {
    pub flow_control: Option<FlowControl>,
    pub keep_alive: Option<KeepAlive>,
    pub max_concurrent_streams: Option<u32>,

    // Internals
    pub max_frame_size: Option<u32>,
    pub max_header_list_size: Option<u32>,
    pub max_pending_accept_reset_streams: Option<usize>,
    pub max_send_buf_size: Option<usize>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Hash)]
pub struct ClientParams {
    pub flow_control: Option<FlowControl>,
    pub keep_alive: Option<ClientKeepAlive>,

    // Internals
    pub max_concurrent_reset_streams: Option<usize>,
    pub max_frame_size: Option<u32>,
    pub max_send_buf_size: Option<usize>,
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct KeepAlive {
    pub interval: Duration,
    pub timeout: Duration,
}

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Hash)]
pub struct ClientKeepAlive {
    pub interval: Duration,
    pub timeout: Duration,
    pub while_idle: bool,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum FlowControl {
    Adaptive,
    Fixed {
        initial_stream_window_size: u32,
        initial_connection_window_size: u32,
    },
}
