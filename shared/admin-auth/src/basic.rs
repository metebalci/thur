// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! HTTP Basic `Authorization` header parsing.

use axum::http::{Request, header};
use base64::Engine;

/// Parse an `Authorization: Basic <base64(user:pass)>` header into
/// `(user, pass)`. Returns `None` when the header is absent, isn't the
/// Basic scheme (case-insensitive per RFC 7617), isn't valid base64,
/// isn't UTF-8, or has no `:` separator. A password may itself contain
/// `:`; only the first separator splits.
pub fn parse_basic<B>(req: &Request<B>) -> Option<(String, String)> {
    let value = req.headers().get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, b64) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .ok()?;
    let text = String::from_utf8(decoded).ok()?;
    let (user, pass) = text.split_once(':')?;
    Some((user.to_string(), pass.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use base64::Engine;

    fn req_with_auth(value: &str) -> Request<()> {
        Request::builder()
            .header(header::AUTHORIZATION, value)
            .body(())
            .unwrap()
    }

    fn basic_header(user: &str, pass: &str) -> String {
        let raw = format!("{user}:{pass}");
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(raw)
        )
    }

    #[test]
    fn parses_a_well_formed_basic_header() {
        let req = req_with_auth(&basic_header("webadmin", "s3cret-pass"));
        let (u, p) = parse_basic(&req).expect("some");
        assert_eq!(u, "webadmin");
        assert_eq!(p, "s3cret-pass");
    }

    #[test]
    fn scheme_match_is_case_insensitive() {
        let header = basic_header("webadmin", "pw").replacen("Basic", "basic", 1);
        let req = req_with_auth(&header);
        assert_eq!(parse_basic(&req).expect("some").0, "webadmin");
    }

    #[test]
    fn password_may_contain_a_colon() {
        let req = req_with_auth(&basic_header("webadmin", "a:b:c"));
        assert_eq!(parse_basic(&req).expect("some").1, "a:b:c");
    }

    #[test]
    fn missing_header_is_none() {
        let req = Request::builder().body(()).unwrap();
        assert!(parse_basic(&req).is_none());
    }

    #[test]
    fn non_basic_scheme_is_none() {
        let req = req_with_auth("Bearer sometoken");
        assert!(parse_basic(&req).is_none());
    }

    #[test]
    fn invalid_base64_is_none() {
        let req = req_with_auth("Basic !!!not-base64!!!");
        assert!(parse_basic(&req).is_none());
    }

    #[test]
    fn no_colon_separator_is_none() {
        let encoded = base64::engine::general_purpose::STANDARD.encode("nocolon");
        let req = req_with_auth(&format!("Basic {encoded}"));
        assert!(parse_basic(&req).is_none());
    }
}
