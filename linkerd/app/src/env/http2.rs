use super::{parse, types::*, EnvError, Strings};
use linkerd_app_core::proxy::http::h2;
use linkerd_app_outbound::http::h2::ServerParams;

pub(super) fn parse_server<S: Strings>(
    strings: &S,
    base: &str,
) -> Result<h2::ServerParams, EnvError> {
    Ok(ServerParams {
        flow_control: Some(parse_flow_control(strings, base)?),
        keep_alive: parse_keep_alive(strings, &format!("{base}_KEEP_ALIVE"))?,
        max_concurrent_streams: parse(
            strings,
            &format!("{base}_MAX_CONCURRENT_STREAMS"),
            parse_number,
        )?,
        max_frame_size: parse(strings, &format!("{base}_MAX_FRAME_SIZE"), parse_number)?,
        max_header_list_size: parse(
            strings,
            &format!("{base}_MAX_HEADER_LIST_SIZE"),
            parse_number,
        )?,
        max_pending_accept_reset_streams: parse(
            strings,
            &format!("{base}_MAX_PENDING_ACCEPT_RESET_STREAMS"),
            parse_number,
        )?,
        max_send_buf_size: parse(strings, &format!("{base}_MAX_SEND_BUF_SIZE"), parse_number)?,
    })
}

fn parse_flow_control<S: Strings>(strings: &S, base: &str) -> Result<h2::FlowControl, EnvError> {
    if let Some(true) = parse(
        strings,
        &format!("{base}_ADAPTIVE_FLOW_CONTROL"),
        parse_bool,
    )? {
        return Ok(h2::FlowControl::Adaptive);
    }

    if let (Some(initial_stream_window_size), Some(initial_connection_window_size)) = (
        parse(
            strings,
            &format!("{base}_INITIAL_STREAM_WINDOW_SIZE"),
            parse_number,
        )?,
        parse(
            strings,
            &format!("{base}_INITIAL_CONNECTION_WINDOW_SIZE"),
            parse_number,
        )?,
    ) {
        return Ok(h2::FlowControl::Fixed {
            initial_stream_window_size,
            initial_connection_window_size,
        });
    }

    // The proxy's defaults are used if no flow control settings are provided.
    Ok(h2::FlowControl::Fixed {
        initial_connection_window_size: super::DEFAULT_INITIAL_CONNECTION_WINDOW_SIZE,
        initial_stream_window_size: super::DEFAULT_INITIAL_STREAM_WINDOW_SIZE,
    })
}

fn parse_keep_alive<S: Strings>(
    strings: &S,
    base: &str,
) -> Result<Option<h2::KeepAlive>, EnvError> {
    if let (Some(timeout), Some(interval)) = (
        parse(strings, &format!("{base}_TIMEOUT"), parse_duration)?,
        parse(strings, &format!("{base}_INTERVAL"), parse_duration)?,
    ) {
        return Ok(Some(h2::KeepAlive { interval, timeout }));
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashMap, time::Duration};

    #[test]
    fn server_params() {
        let mut env = HashMap::default();

        // Produces empty params if no relevant env vars are set.
        let default = h2::ServerParams {
            flow_control: Some(h2::FlowControl::Fixed {
                initial_stream_window_size: super::super::DEFAULT_INITIAL_STREAM_WINDOW_SIZE,
                initial_connection_window_size:
                    super::super::DEFAULT_INITIAL_CONNECTION_WINDOW_SIZE,
            }),
            ..Default::default()
        };
        assert_eq!(parse_server(&env, "TEST").unwrap(), default);

        // Set all the fields.
        env.insert("TEST_MAX_CONCURRENT_STREAMS", "3");
        env.insert("TEST_MAX_FRAME_SIZE", "4");
        env.insert("TEST_MAX_HEADER_LIST_SIZE", "5");
        env.insert("TEST_MAX_PENDING_ACCEPT_RESET_STREAMS", "6");
        env.insert("TEST_MAX_SEND_BUF_SIZE", "7");
        env.insert("TEST_KEEP_ALIVE_TIMEOUT", "1s");
        env.insert("TEST_KEEP_ALIVE_INTERVAL", "2s");
        env.insert("TEST_INITIAL_STREAM_WINDOW_SIZE", "1");
        env.insert("TEST_INITIAL_CONNECTION_WINDOW_SIZE", "2");
        let expected = h2::ServerParams {
            flow_control: Some(h2::FlowControl::Fixed {
                initial_stream_window_size: 1,
                initial_connection_window_size: 2,
            }),
            keep_alive: Some(h2::KeepAlive {
                interval: Duration::from_secs(2),
                timeout: Duration::from_secs(1),
            }),
            max_concurrent_streams: Some(3),
            max_frame_size: Some(4),
            max_header_list_size: Some(5),
            max_pending_accept_reset_streams: Some(6),
            max_send_buf_size: Some(7),
        };
        assert_eq!(parse_server(&env, "TEST").unwrap(), expected);

        // Enable adaptive flow control, overriding other flow control settings.
        env.insert("TEST_ADAPTIVE_FLOW_CONTROL", "true");
        assert_eq!(
            parse_server(&env, "TEST").unwrap(),
            h2::ServerParams {
                flow_control: Some(h2::FlowControl::Adaptive),
                ..expected
            }
        );

        // Clear the flow control and set adaptive to false to ensure the
        // default flow control is used.
        env.remove("TEST_INITIAL_STREAM_WINDOW_SIZE");
        env.remove("TEST_INITIAL_CONNECTION_WINDOW_SIZE");
        env.insert("TEST_ADAPTIVE_FLOW_CONTROL", "false");
        assert_eq!(
            parse_server(&env, "TEST").unwrap(),
            h2::ServerParams {
                flow_control: default.flow_control,
                ..expected
            }
        );
    }
}
