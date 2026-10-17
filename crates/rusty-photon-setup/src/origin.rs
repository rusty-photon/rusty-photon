//! The cross-site check every `POST` passes before it is acted on.
//!
//! Browsers resend cached Basic credentials on any request to the origin,
//! so a form `POST` behind Basic Auth could be submitted by a page on another
//! site. A `POST` is accepted only when the request says it came from the
//! page itself: `Sec-Fetch-Site` is `same-origin` or `none`, or, where a
//! browser sends none, the `Origin` header's host and port are the request's
//! own `Host`. No token, no cookie, no state.

use http::header::{HOST, ORIGIN};
use http::HeaderMap;

/// Why a `POST` was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused(pub String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Accept a `POST` that came from the page itself; refuse any other.
///
/// # Errors
///
/// Returns the reason when the request names another site, or names no
/// site at all.
pub fn check(headers: &HeaderMap) -> Result<(), Refused> {
    if let Some(site) = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) {
        return match site.trim().to_ascii_lowercase().as_str() {
            "same-origin" | "none" => Ok(()),
            other => Err(Refused(format!(
                "This form was submitted from another site (Sec-Fetch-Site: {other}); only the page itself may submit it."
            ))),
        };
    }
    let Some(origin) = headers.get(ORIGIN).and_then(|v| v.to_str().ok()) else {
        return Err(Refused(
            "This form was submitted without saying where from (no Sec-Fetch-Site or Origin header); only the page itself may submit it."
                .to_string(),
        ));
    };
    let Some(host) = headers.get(HOST).and_then(|v| v.to_str().ok()) else {
        return Err(Refused(
            "This request names no Host, so its origin cannot be checked.".to_string(),
        ));
    };
    if same_authority(origin, host) {
        Ok(())
    } else {
        Err(Refused(format!(
            "This form was submitted from {origin}, not from this server ({host}); only the page itself may submit it."
        )))
    }
}

/// Whether `origin` (`scheme://host[:port]`) names the authority `host`
/// (`host[:port]`), a default port spelled or not on either side.
fn same_authority(origin: &str, host: &str) -> bool {
    let Some((scheme, authority)) = origin.trim().split_once("://") else {
        return false;
    };
    let default_port = match scheme.to_ascii_lowercase().as_str() {
        "https" => "443",
        "http" => "80",
        _ => return false,
    };
    let normalize = |authority: &str| -> String {
        let authority = authority.trim().trim_end_matches('/').to_ascii_lowercase();
        let (name, port) = split_port(&authority);
        format!("{name}:{}", port.unwrap_or(default_port))
    };
    normalize(authority) == normalize(host)
}

/// Split `host[:port]`, bracketed IPv6 literals included.
fn split_port(authority: &str) -> (String, Option<&str>) {
    if let Some(rest) = authority.strip_prefix('[') {
        if let Some((name, after)) = rest.split_once(']') {
            let port = after.strip_prefix(':');
            return (format!("[{name}]"), port);
        }
    }
    match authority.rsplit_once(':') {
        Some((name, port)) if !name.contains(':') => (name.to_string(), Some(port)),
        _ => (authority.to_string(), None),
    }
}

/// The host part of a `Host` header, without its port: what a link the page
/// renders for its own server carries.
#[must_use]
pub fn host_without_port(host: &str) -> String {
    split_port(host.trim()).0
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use http::HeaderValue;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            _ = map.append(*name, HeaderValue::from_str(value).unwrap());
        }
        map
    }

    #[test]
    fn same_origin_and_none_are_accepted() {
        check(&headers(&[("sec-fetch-site", "same-origin")])).unwrap();
        check(&headers(&[("sec-fetch-site", "none")])).unwrap();
        check(&headers(&[("sec-fetch-site", "Same-Origin")])).unwrap();
    }

    #[test]
    fn same_site_and_cross_site_are_refused() {
        // Another port on the same name is another origin — a page on rp's
        // origin may not submit a driver's form.
        let err = check(&headers(&[
            ("sec-fetch-site", "same-site"),
            ("origin", "http://driver:11119"),
            ("host", "driver:11119"),
        ]))
        .unwrap_err();
        assert!(err.to_string().contains("same-site"), "{err}");
        check(&headers(&[("sec-fetch-site", "cross-site")])).unwrap_err();
    }

    #[test]
    fn without_sec_fetch_site_the_origin_must_match_the_host() {
        check(&headers(&[
            ("origin", "http://driver:11119"),
            ("host", "driver:11119"),
        ]))
        .unwrap();
        check(&headers(&[
            ("origin", "HTTP://Driver:11119/"),
            ("host", "driver:11119"),
        ]))
        .unwrap();
        let err = check(&headers(&[
            ("origin", "https://elsewhere.example"),
            ("host", "driver:11119"),
        ]))
        .unwrap_err();
        assert!(err.to_string().contains("elsewhere.example"), "{err}");
        check(&headers(&[("origin", "null"), ("host", "driver:11119")])).unwrap_err();
    }

    #[test]
    fn a_default_port_matches_spelled_or_not() {
        check(&headers(&[
            ("origin", "https://driver"),
            ("host", "driver:443"),
        ]))
        .unwrap();
        check(&headers(&[
            ("origin", "http://driver:80"),
            ("host", "driver"),
        ]))
        .unwrap();
        check(&headers(&[
            ("origin", "https://driver"),
            ("host", "driver:80"),
        ]))
        .unwrap_err();
    }

    #[test]
    fn ipv6_literals_compare_with_their_brackets() {
        check(&headers(&[
            ("origin", "http://[::1]:11119"),
            ("host", "[::1]:11119"),
        ]))
        .unwrap();
        assert_eq!(host_without_port("[::1]:11119"), "[::1]");
        assert_eq!(host_without_port("driver:11119"), "driver");
        assert_eq!(host_without_port("driver"), "driver");
    }

    #[test]
    fn neither_header_is_refused() {
        let err = check(&headers(&[("host", "driver:11119")])).unwrap_err();
        assert!(
            err.to_string().contains("without saying where from"),
            "{err}"
        );
        check(&headers(&[("origin", "http://driver:11119")])).unwrap_err();
    }
}
