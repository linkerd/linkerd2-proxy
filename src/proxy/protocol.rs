use byteorder::{BigEndian, ByteOrder};
use httparse;

/// Transport protocols that can be transparently detected by `Server`.
#[derive(Debug)]
pub enum Protocol {
    Http1,
    Http2,
    Kafka,
}

const H2_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

impl Protocol {
    /// Tries to detect a known protocol in the peeked bytes.
    ///
    /// If no protocol can be determined, returns `None`.
    pub fn detect(bytes: &[u8]) -> Option<Protocol> {
        // http2 is easiest to detect
        if bytes.len() >= H2_PREFACE.len() {
            if &bytes[..H2_PREFACE.len()] == H2_PREFACE {
                println!("^^^^^ is H2");
                return Some(Protocol::Http2);
            }
        }

        // http1 can have a really long first line, but if the bytes so far
        // look like http1, we'll assume it is. a different protocol
        // should look different in the first few bytes

        let mut headers = [httparse::EMPTY_HEADER; 0];
        let mut req = httparse::Request::new(&mut headers);
        match req.parse(bytes) {
            // Ok(Complete) or Ok(Partial) both mean it looks like HTTP1!
            //
            // If we got past the first line, we'll see TooManyHeaders,
            // because we passed an array of 0 headers to parse into. That's fine!
            // We didn't want to keep parsing headers, just validate that
            // the first line is HTTP1.
            Ok(_) | Err(httparse::Error::TooManyHeaders) => {
                println!("^^^^^ is H1");
                return Some(Protocol::Http1);
            }
            _ => {}
        }

        // Detect if this is a Kafka traffic

        // Header:
        //     Request/response Size => INT32
        //     api_key => INT16
        //     api_version => INT16
        //     correlation_id => INT32
        //     client_id => NULLABLE_STRING (first two bytes as the length of the client_id string)
        const KAFKA_REQUEST_HEADER_LENGTH: usize = 4 + 2 + 2 + 4 + 2;
        println!("length {}", bytes.len());
        if bytes.len() >= KAFKA_REQUEST_HEADER_LENGTH {
            let request_size = BigEndian::read_i32(&bytes);
            let api_key = BigEndian::read_i16(&bytes[4..]);
            let api_version = BigEndian::read_i16(&bytes[6..]);
            let correlation_id = BigEndian::read_i32(&bytes[8..]);
            let len_client_id = BigEndian::read_i16(&bytes[12..]);
            let mut client_id = "null";

            if len_client_id > -1 {
                use std::str;

                client_id = match str::from_utf8(&bytes[14..(14 + len_client_id as usize)]) {
                    Ok(v) => v,
                    Err(_) => return None,
                };
            }

            println!("checking if is kafka...");
            println!("\trequest_size: {}", request_size);
            println!("\tapi_key: {}", api_key);
            println!("\tapi_version: {}", api_version);
            println!("\tcorrelation_id: {}", correlation_id);
            println!("\tlen_client_id: {}", len_client_id);
            println!("\tclient_id: {}", client_id);
            return Some(Protocol::Kafka);
        }

        println!("^^^^^ is None");
        None
    }
}
