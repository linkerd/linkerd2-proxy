use crate::ServerId;
use linkerd_identity as identity;
use std::convert::TryFrom;
use tracing::trace;

pub struct Incomplete;

/// Determintes whether the given `input` looks like the start of a TLS
/// connection that the proxy should terminate.
///
/// The determination is made based on whether the input looks like (the start
/// of) a valid ClientHello that a reasonable TLS client might send, and the
/// SNI matches the given identity.
///
/// XXX: Once the TLS record header is matched, the determination won't be
/// made until the entire TLS record including the entire ClientHello handshake
/// message is available.
///
/// TODO: Reject non-matching inputs earlier.
///
/// This assumes that the ClientHello is small and is sent in a single TLS
/// record, which is what all reasonable implementations do. (If they were not
/// to, they wouldn't interoperate with picky servers.)
pub fn parse_sni(input: &[u8]) -> Result<Option<ServerId>, Incomplete> {
    let r = untrusted::Input::from(input).read_all(untrusted::EndOfInput, |input| {
        let r = extract_sni(input);
        input.skip_to_end(); // Ignore anything after what we parsed.
        r
    });
    match r {
        Ok(Some(sni)) => {
            let sni = identity::Name::try_from(sni.as_slice_less_safe())
                .ok()
                .map(ServerId);
            trace!(?sni, "parse_sni: parsed correctly up to SNI");
            Ok(sni)
        }
        Ok(None) => {
            trace!("parse_sni: failed to parse up to SNI");
            Ok(None)
        }
        Err(untrusted::EndOfInput) => {
            trace!("parse_sni: needs more input");
            Err(Incomplete)
        }
    }
}

/// The result is `Ok(Some(hostname))` if the SNI extension was found, `Ok(None)`
/// if we affirmatively rejected the input before we found the SNI extension, or
/// `Err(EndOfInput)` if we don't have enough input to continue.
fn extract_sni<'a>(
    input: &mut untrusted::Reader<'a>,
) -> Result<Option<untrusted::Input<'a>>, untrusted::EndOfInput> {
    // TLS ciphertext record header.

    if input.read_byte()? != 22 {
        // ContentType::handshake
        return Ok(None);
    }
    if input.read_byte()? != 0x03 {
        // legacy_record_version.major is always 0x03.
        return Ok(None);
    }
    {
        // legacy_record_version.minor may be 0x01 or 0x03 according to
        // https://tools.ietf.org/html/draft-ietf-tls-tls13-28#section-5.1
        let minor = input.read_byte()?;
        if minor != 0x01 && minor != 0x03 {
            return Ok(None);
        }
    }

    // Treat the record length and its body as a vector<u16>.
    let r = read_vector(input, |input| {
        if input.read_byte()? != 1 {
            // HandshakeType::client_hello
            return Ok(None);
        }
        // The length is a 24-bit big-endian value. Nobody (good) will never
        // send a value larger than 0xffff so treat it as a 0x00 followed
        // by vector<u16>
        if input.read_byte()? != 0 {
            // Most significant byte of the length
            return Ok(None);
        }
        read_vector(input, |input| {
            // version{.major,.minor} == {0x3, 0x3} for TLS 1.2 and later.
            if input.read_byte()? != 0x03 || input.read_byte()? != 0x03 {
                return Ok(None);
            }

            input.skip(32)?; // random
            skip_vector_u8(input)?; // session_id
            if !skip_vector(input)? {
                // cipher_suites
                return Ok(None);
            }
            skip_vector_u8(input)?; // compression_methods

            // Look for the SNI extension as specified in
            // https://tools.ietf.org/html/rfc6066#section-1.1
            read_vector(input, |input| {
                while !input.at_end() {
                    let extension_type = read_u16(input)?;
                    if extension_type != 0 {
                        // ExtensionType::server_name
                        skip_vector(input)?;
                        continue;
                    }

                    // Treat extension_length followed by extension_value as a
                    // vector<u16>.
                    let r = read_vector(input, |input| {
                        // server_name_list
                        read_vector(input, |input| {
                            // Nobody sends an SNI extension with anything
                            // other than a single `host_name` value.
                            if input.read_byte()? != 0 {
                                // NameType::host_name
                                return Ok(None);
                            }
                            // Return the value of the `HostName`.
                            read_vector(input, |input| Ok(Some(input.read_bytes_to_end())))
                        })
                    });

                    input.skip_to_end(); // Ignore stuff after SNI
                    return r;
                }

                Ok(None) // No SNI extension.
            })
        })
    });

    // Ignore anything after the first handshake record.
    input.skip_to_end();

    r
}

/// Reads a `u16` vector, which is formatted as a big-endian `u16` length
/// followed by that many bytes.
fn read_vector<'a, F, T>(
    input: &mut untrusted::Reader<'a>,
    f: F,
) -> Result<Option<T>, untrusted::EndOfInput>
where
    F: Fn(&mut untrusted::Reader<'a>) -> Result<Option<T>, untrusted::EndOfInput>,
    T: 'a,
{
    let length = read_u16(input)?;

    // ClientHello has to be small for compatibility with many deployed
    // implementations, so if it is (relatively) huge, we might not be looking
    // at TLS traffic, and we're definitely not looking at proxy-terminated
    // traffic, so bail out early.
    if length > 8192 {
        return Ok(None);
    }
    let r = input.read_bytes(usize::from(length))?;
    r.read_all(untrusted::EndOfInput, f)
}

/// Like `read_vector` except the contents are ignored.
fn skip_vector(input: &mut untrusted::Reader<'_>) -> Result<bool, untrusted::EndOfInput> {
    let r = read_vector(input, |input| {
        input.skip_to_end();
        Ok(Some(()))
    });
    r.map(|r| r.is_some())
}

/// Like `skip_vector` for vectors with `u8` lengths.
fn skip_vector_u8(input: &mut untrusted::Reader<'_>) -> Result<(), untrusted::EndOfInput> {
    let length = input.read_byte()?;
    input.skip(usize::from(length))
}

/// Read a big-endian-encoded `u16`.
fn read_u16(input: &mut untrusted::Reader<'_>) -> Result<u16, untrusted::EndOfInput> {
    let hi = input.read_byte()?;
    let lo = input.read_byte()?;
    Ok(u16::from(hi) << 8 | u16::from(lo))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    /// From `cargo run --example tlsclient -- --http example.com`
    static VALID_EXAMPLE_COM: &[u8] = include_bytes!("testdata/example-com-client-hello.bin");

    #[test]
    fn matches() {
        check_all_prefixes(Match::Matched, "example.com", VALID_EXAMPLE_COM);
    }

    #[test]
    fn mismatch_different_sni() {
        check_all_prefixes(Match::NotMatched, "example.org", VALID_EXAMPLE_COM);
    }

    #[test]
    fn mismatch_truncated_sni() {
        check_all_prefixes(Match::NotMatched, "example.coma", VALID_EXAMPLE_COM);
    }

    #[test]
    fn mismatch_appended_sni() {
        check_all_prefixes(Match::NotMatched, "example.co", VALID_EXAMPLE_COM);
    }

    #[test]
    fn mismatch_prepended_sni() {
        check_all_prefixes(Match::NotMatched, "aexample.com", VALID_EXAMPLE_COM);
    }

    #[test]
    fn mismatch_http_1_0_request() {
        check_all_prefixes(
            Match::NotMatched,
            "example.com",
            b"GET /TheProject.html HTTP/1.0\r\n\r\n",
        );
    }

    fn check_all_prefixes(expected_match: Match, identity: &str, input: &[u8]) {
        assert!(expected_match == Match::Matched || expected_match == Match::NotMatched);

        let identity = identity::Name::from_str(identity).unwrap();

        let mut i = 0;

        // `Async::NotReady` will be returned for some number of prefixes.
        loop {
            let m = match_client_hello(&input[..i], &identity);
            if m != Match::Incomplete {
                assert_eq!(m, expected_match);
                break;
            }
            i += 1;
        }

        // The same result will be returned for all longer prefixes.
        for i in (i + 1)..input.len() {
            assert_eq!(expected_match, match_client_hello(&input[..i], &identity))
        }
    }
}
