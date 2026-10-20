//! The bearer check every gated route shares: `/mcp` (`MCP_TOKEN`,
//! `MCP_ADMIN_TOKEN`) and the admin listener's `/admin/api/*`
//! (`ADMIN_TOKEN`).

use axum::http::{HeaderMap, header};

/// The token of an `Authorization: Bearer <token>` header, trimmed; `None`
/// for any other scheme or no header. The scheme is case-insensitive (RFC
/// 7235 §2.1), so `bearer` and `BEARER` present a token too.
pub(crate) fn presented_bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.trim_start().split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim())
        .filter(|t| !t.is_empty())
}

/// Equal without an early exit on the first differing byte (`subtle`, so the
/// compiler cannot vectorise an exit back in). The length is not hidden,
/// which is fine: it is fixed by the operator's token.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    use subtle::ConstantTimeEq as _;
    a.len() == b.len() && bool::from(a.ct_eq(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_is_plain_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn only_a_bearer_header_presents_a_token() -> Result<(), Box<dyn std::error::Error>> {
        let mut h = HeaderMap::new();
        assert_eq!(presented_bearer(&h), None);
        h.insert(header::AUTHORIZATION, "Basic abc".parse()?);
        assert_eq!(presented_bearer(&h), None);
        h.insert(header::AUTHORIZATION, "Bearer  abc ".parse()?);
        assert_eq!(presented_bearer(&h), Some("abc"));
        for scheme in ["bearer", "BEARER", "BeArEr"] {
            h.insert(header::AUTHORIZATION, format!("{scheme} abc").parse()?);
            assert_eq!(presented_bearer(&h), Some("abc"), "{scheme}");
        }
        for bad in ["Bearer", "Bearer ", "Bearerabc", "Bearers abc"] {
            h.insert(header::AUTHORIZATION, bad.parse()?);
            assert_eq!(presented_bearer(&h), None, "{bad:?}");
        }
        Ok(())
    }
}
