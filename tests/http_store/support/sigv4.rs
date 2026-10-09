use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

const HEADER_TERMINATOR: &[u8] = b"\r\n\r\n";

/// Recomputes SigV4 from the wire request and checks its payload hash and signature.
pub(crate) fn verify_wire_signature(request: &str, secret: &str) {
    let (head, body) = request.split_once("\r\n\r\n").expect("HTTP headers");
    let mut lines = head.lines();
    let request_line = lines.next().expect("request line");
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next().unwrap();
    let target = request_parts.next().unwrap();
    let (path, raw_query) = target.split_once('?').unwrap_or((target, ""));
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
        .collect::<std::collections::BTreeMap<_, _>>();
    let authorization = headers.get("authorization").unwrap();
    let signed_headers = authorization
        .split("SignedHeaders=")
        .nth(1)
        .unwrap()
        .split(',')
        .next()
        .unwrap();
    let canonical_headers = signed_headers
        .split(';')
        .map(|name| format!("{name}:{}\n", headers.get(name).unwrap().trim()))
        .collect::<String>();
    let query = raw_query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (
                aws_encode(&percent_decode(key)),
                aws_encode(&percent_decode(value)),
            )
        })
        .collect::<Vec<_>>();
    let mut query = query;
    query.sort();
    let canonical_query = query
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("&");
    let payload_hash = headers.get("x-amz-content-sha256").unwrap();
    assert_eq!(payload_hash, &hex::encode(Sha256::digest(body.as_bytes())));
    let canonical_request = format!(
        "{method}\n{path}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
    );
    let credential = authorization
        .split("Credential=")
        .nth(1)
        .unwrap()
        .split(',')
        .next()
        .unwrap();
    let scope = credential.split_once('/').unwrap().1;
    let mut scope_parts = scope.split('/');
    let date = scope_parts.next().unwrap();
    let region = scope_parts.next().unwrap();
    let service = scope_parts.next().unwrap();
    assert_eq!(scope_parts.next(), Some("aws4_request"));
    assert_eq!(&headers["x-amz-date"][..8], date);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{scope}\n{}",
        headers["x-amz-date"],
        hex::encode(Sha256::digest(canonical_request.as_bytes()))
    );
    let hmac = |key: &[u8], input: &[u8]| {
        let mut mac = Hmac::<Sha256>::new_from_slice(key).unwrap();
        mac.update(input);
        mac.finalize().into_bytes().to_vec()
    };
    let mut date_key = b"AWS4".to_vec();
    date_key.extend_from_slice(secret.as_bytes());
    let date_key = hmac(&date_key, date.as_bytes());
    let region_key = hmac(&date_key, region.as_bytes());
    let service_key = hmac(&region_key, service.as_bytes());
    let signing_key = hmac(&service_key, b"aws4_request");
    let expected_signature = hex::encode(hmac(&signing_key, string_to_sign.as_bytes()));
    let actual_signature = authorization.split("Signature=").nth(1).unwrap().trim();
    assert_eq!(actual_signature, expected_signature);
}

/// Splits concatenated wire requests using their case-insensitive content lengths.
pub(crate) fn split_captured_requests(raw: &[u8]) -> Vec<(String, Vec<u8>)> {
    let mut requests = Vec::new();
    let mut offset = 0;
    while offset < raw.len() {
        let relative_end = raw[offset..]
            .windows(HEADER_TERMINATOR.len())
            .position(|window| window == HEADER_TERMINATOR)
            .expect("request header terminator");
        let header_end = offset + relative_end;
        let head = &raw[offset..header_end];
        let content_length = std::str::from_utf8(head)
            .unwrap()
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find_map(|(name, value)| {
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap_or(0);
        let body_start = header_end + HEADER_TERMINATOR.len();
        let body_end = body_start + content_length;
        requests.push((
            String::from_utf8(head.to_vec()).expect("request headers are UTF-8"),
            raw[body_start..body_end].to_vec(),
        ));
        offset = body_end;
    }
    requests
}

/// Decodes percent-encoded UTF-8 query components without treating plus as a space.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap();
            decoded.push(u8::from_str_radix(hex, 16).unwrap());
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(decoded).unwrap()
}

/// Encodes query bytes using the SigV4 unreserved set and uppercase hexadecimal escapes.
fn aws_encode(input: &str) -> String {
    input
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

/// Extracts the lowercase wire Host header for endpoint checks.
pub(crate) fn endpoint_host_from_request(request: &str) -> String {
    request
        .lines()
        .find_map(|line| line.strip_prefix("host: ").map(str::to_owned))
        .expect("wire Host header")
}
