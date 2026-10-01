//! Tiny helper for logging URLs without leaking query-string secrets.
//!
//! Jellyfin stream URLs embed the access token as an `ApiKey` query
//! parameter (see `jellyfin_api::JellyfinClient::stream_url`'s doc comment:
//! "no Authorization header involved") -- logging one of those URLs whole at
//! INFO level puts a live bearer credential into the log file. Every log
//! call that includes a URL should route it through [`redact_url`] first.

/// Returns the URL with its query string (everything from the first `?`
/// onward, e.g. `?ApiKey=...&static=true`) stripped, so the path stays
/// useful for debugging without the secret. A URL with no `?` is returned
/// unchanged.
pub(crate) fn redact_url(url: &str) -> &str {
    match url.find('?') {
        Some(idx) => &url[..idx],
        None => url,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_query_string() {
        assert_eq!(
            redact_url("https://server/Videos/abc/stream?ApiKey=SECRET&static=true"),
            "https://server/Videos/abc/stream"
        );
    }

    #[test]
    fn leaves_url_without_query_string_unchanged() {
        assert_eq!(
            redact_url("https://server/Videos/abc/stream"),
            "https://server/Videos/abc/stream"
        );
    }

    #[test]
    fn handles_empty_query_string() {
        assert_eq!(redact_url("https://server/path?"), "https://server/path");
    }
}
