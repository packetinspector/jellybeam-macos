//! Pure, network-free URL logic for `seerr_connect`
//! (URL validation): expands a user-entered
//! host/URL into ordered candidate base URLs to probe with a real login, and
//! normalizes a chosen base URL to end in exactly one `/api/v1`.
//! `SeerrClient::connect` in `client.rs` is the only caller that dials these.
//!
//! Each candidate uses its own scheme's default port (443/80) rather than
//! carrying over the other scheme's port, so flipping scheme never yields a
//! wrong candidate like `https://host:80`.

use url::Url;

/// Seerr/Jellyseerr's documented default port, used when the user didn't type one.
const DEFAULT_SEERR_PORT: u16 = 5055;

/// Expand `input` into ordered, de-duplicated candidate base URLs (bare
/// `scheme://host[:port][/path]`, no `/api/v1` yet -- see
/// [`normalize_api_base`]); the caller tries each with a real login and
/// stops at the first success. Every probe sends real credentials
/// (password or API key), so for a bare host -- where we're guessing the
/// scheme -- every guessed-https candidate is ordered before any
/// guessed-http one; an explicit scheme the user typed is honored exactly
/// as given and never reordered.
///
/// - Full URL: as given, plus the same scheme at `:5055` if no explicit
///   non-default port was given.
/// - Bare host, no explicit port: `https` at its default port, `https` at
///   `:5055`, `http` at its default port, `http` at `:5055`, in that order
///   -- every https candidate precedes every http one.
/// - Bare host, explicit port: `https` then `http`, both at that port.
pub fn candidate_urls(input: &str) -> Vec<String> {
    let input = input.trim();
    if input.is_empty() {
        return Vec::new();
    }

    let mut candidates: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut push = |url: Url| {
        let rendered = url.to_string();
        let rendered = rendered.trim_end_matches('/').to_string();
        if seen.insert(rendered.clone()) {
            candidates.push(rendered);
        }
    };

    if input.starts_with("http://") || input.starts_with("https://") {
        let Ok(parsed) = Url::parse(input) else {
            return Vec::new();
        };
        let had_explicit_port = parsed.port().is_some();
        push(parsed.clone());
        if !had_explicit_port {
            let mut with_port = parsed.clone();
            if with_port.set_port(Some(DEFAULT_SEERR_PORT)).is_ok() {
                push(with_port);
            }
        }
    } else {
        // https first: credentials ride every probe, so a guessed-https
        // candidate must be tried before any guessed-http one.
        let Ok(https_url) = Url::parse(&format!("https://{input}")) else {
            return Vec::new();
        };
        let had_explicit_port = https_url.port().is_some();

        push(https_url.clone());
        if !had_explicit_port {
            let mut https_port = https_url.clone();
            if https_port.set_port(Some(DEFAULT_SEERR_PORT)).is_ok() {
                push(https_port);
            }
        }

        let mut http_url = https_url;
        if http_url.set_scheme("http").is_ok() {
            if had_explicit_port {
                push(http_url);
            } else {
                // Let http use its own default port (80) rather than carrying over https's 443.
                let _ = http_url.set_port(None);
                push(http_url.clone());
                let mut http_port = http_url;
                if http_port.set_port(Some(DEFAULT_SEERR_PORT)).is_ok() {
                    push(http_port);
                }
            }
        }
    }

    candidates
}

/// Normalize `base_url` to end in exactly one `/api/v1`, no trailing slash.
/// Idempotent, which guards against double-normalizing into `/api/v1/api/v1`.
pub fn normalize_api_base(base_url: &str) -> String {
    let trimmed = base_url.trim().trim_end_matches('/');
    if trimmed.ends_with("/api/v1") {
        return trimmed.to_string();
    }
    format!("{trimmed}/api/v1")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_host_expands_to_four_scheme_port_combinations_https_first() {
        let candidates = candidate_urls("seerr.test");
        assert_eq!(
            candidates,
            vec![
                "https://seerr.test",
                "https://seerr.test:5055",
                "http://seerr.test",
                "http://seerr.test:5055",
            ]
        );
    }

    #[test]
    fn bare_host_with_explicit_port_does_not_guess_5055() {
        let candidates = candidate_urls("seerr.test:9999");
        assert_eq!(
            candidates,
            vec!["https://seerr.test:9999", "http://seerr.test:9999"]
        );
    }

    #[test]
    fn bare_host_never_yields_an_http_candidate_before_every_https_candidate() {
        // Regression pin: a bare-host entry for an https-only server must
        // never send real credentials over cleartext port 80 first.
        for input in ["seerr.test", "seerr.test:9999"] {
            let candidates = candidate_urls(input);
            let last_https = candidates.iter().rposition(|c| c.starts_with("https://"));
            let first_http = candidates.iter().position(|c| c.starts_with("http://"));
            if let (Some(last_https), Some(first_http)) = (last_https, first_http) {
                assert!(
                    last_https < first_http,
                    "expected every https candidate before any http candidate, got {candidates:?}"
                );
            }
        }
    }

    #[test]
    fn full_https_url_with_default_port_adds_5055_variant() {
        let candidates = candidate_urls("https://seerr.test");
        assert_eq!(
            candidates,
            vec!["https://seerr.test", "https://seerr.test:5055"]
        );
    }

    #[test]
    fn full_url_with_explicit_non_default_port_has_no_5055_variant() {
        let candidates = candidate_urls("http://seerr.test:8080");
        assert_eq!(candidates, vec!["http://seerr.test:8080"]);
    }

    #[test]
    fn full_url_already_ending_in_api_v1_keeps_the_path_on_every_candidate() {
        // Candidate expansion must not touch an already-normalized URL's path.
        let candidates = candidate_urls("https://seerr.test/api/v1");
        assert_eq!(
            candidates,
            vec![
                "https://seerr.test/api/v1",
                "https://seerr.test:5055/api/v1",
            ]
        );
    }

    #[test]
    fn empty_input_produces_no_candidates() {
        assert!(candidate_urls("").is_empty());
        assert!(candidate_urls("   ").is_empty());
    }

    #[test]
    fn normalize_appends_api_v1_to_a_bare_base() {
        assert_eq!(
            normalize_api_base("http://seerr.test"),
            "http://seerr.test/api/v1"
        );
        assert_eq!(
            normalize_api_base("http://seerr.test/"),
            "http://seerr.test/api/v1"
        );
    }

    #[test]
    fn normalize_leaves_an_already_normalized_base_untouched() {
        assert_eq!(
            normalize_api_base("http://seerr.test/api/v1"),
            "http://seerr.test/api/v1"
        );
        assert_eq!(
            normalize_api_base("http://seerr.test/api/v1/"),
            "http://seerr.test/api/v1"
        );
    }

    #[test]
    fn normalize_is_idempotent_guarding_the_double_api_v1_pathology() {
        let once = normalize_api_base("http://seerr.test");
        let twice = normalize_api_base(&once);
        assert_eq!(once, twice);
        assert!(!twice.contains("/api/v1/api/v1"));
    }
}
