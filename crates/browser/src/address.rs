//! Turning what the user typed into the address bar into a URL.

use url::Url;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AddressInput {
    /// Load this URL.
    Url(String),
    /// Not a URL: search for it.
    Search(String),
}

/// Search engine used for non-URL input; `%s` is replaced by the query.
pub const DEFAULT_SEARCH_URL: &str = "https://duckduckgo.com/?q=%s";

/// Interprets address-bar input the way browsers do: explicit schemes win,
/// `localhost:3000` and bare IPs get `http://`, things that look like domains
/// get `https://`, and everything else is a search.
pub fn parse_address(input: &str) -> AddressInput {
    let input = input.trim();
    if input.is_empty() {
        return AddressInput::Url("about:blank".into());
    }

    let lower = input.to_ascii_lowercase();
    for scheme in ["http://", "https://", "file://", "about:", "data:", "view-source:", "chrome://", "devtools://"] {
        if lower.starts_with(scheme) {
            return AddressInput::Url(input.to_string());
        }
    }
    if input.contains(char::is_whitespace) {
        return AddressInput::Search(input.to_string());
    }
    if input.starts_with('/') || input.starts_with("~/") {
        let path = if let Some(rest) = input.strip_prefix("~/") {
            std::env::var("HOME").map(|home| format!("{home}/{rest}")).unwrap_or_else(|_| input.into())
        } else {
            input.to_string()
        };
        if let Ok(url) = Url::from_file_path(&path) {
            return AddressInput::Url(url.into());
        }
    }

    let host = input
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    let host_without_port = strip_port(host);
    let is_local = host_without_port.eq_ignore_ascii_case("localhost")
        || host_without_port.ends_with(".localhost")
        || host_without_port.ends_with(".local")
        || host_without_port.parse::<std::net::Ipv4Addr>().is_ok()
        || (host_without_port.starts_with('[') && host_without_port.ends_with(']'));
    if is_local {
        return AddressInput::Url(format!("http://{input}"));
    }

    let looks_like_domain = host_without_port.contains('.')
        && !host_without_port.starts_with('.')
        && !host_without_port.ends_with('.')
        && host_without_port
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '.');
    if looks_like_domain {
        let candidate = format!("https://{input}");
        if Url::parse(&candidate).is_ok() {
            return AddressInput::Url(candidate);
        }
    }
    AddressInput::Search(input.to_string())
}

fn strip_port(host: &str) -> &str {
    if host.starts_with('[') {
        // [::1]:3000
        return match host.find(']') {
            Some(end) => &host[..=end],
            None => host,
        };
    }
    match host.rsplit_once(':') {
        Some((name, port)) if port.chars().all(|c| c.is_ascii_digit()) => name,
        _ => host,
    }
}

/// The URL to load for `input`, using `search_url` (with `%s`) for searches.
pub fn resolve_address(input: &str, search_url: &str) -> String {
    match parse_address(input) {
        AddressInput::Url(url) => url,
        AddressInput::Search(query) => {
            let encoded: String = url::form_urlencoded::byte_serialize(query.as_bytes()).collect();
            search_url.replace("%s", &encoded)
        }
    }
}

/// `scheme://host[:port]` of a URL, the unit browser-tool policy works in.
pub fn origin_of(url: &str) -> Option<String> {
    let parsed = Url::parse(url).ok()?;
    match parsed.origin() {
        url::Origin::Tuple(..) => Some(parsed.origin().ascii_serialization()),
        url::Origin::Opaque(_) => None,
    }
}

/// Whether `url` points at this machine (a dev server, typically).
pub fn is_local_url(url: &str) -> bool {
    let Ok(parsed) = Url::parse(url) else {
        return false;
    };
    match parsed.host() {
        Some(url::Host::Domain(domain)) => {
            domain.eq_ignore_ascii_case("localhost") || domain.ends_with(".localhost")
        }
        Some(url::Host::Ipv4(ip)) => ip.is_loopback() || ip.is_unspecified(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback() || ip.is_unspecified(),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> AddressInput {
        AddressInput::Url(s.into())
    }

    #[test]
    fn local_development_addresses_get_http() {
        assert_eq!(parse_address("localhost:3000"), url("http://localhost:3000"));
        assert_eq!(parse_address("localhost"), url("http://localhost"));
        assert_eq!(parse_address("127.0.0.1:8080/api"), url("http://127.0.0.1:8080/api"));
        assert_eq!(parse_address("[::1]:5173"), url("http://[::1]:5173"));
        assert_eq!(parse_address("app.localhost:4000"), url("http://app.localhost:4000"));
    }

    #[test]
    fn domains_get_https_and_schemes_are_kept() {
        assert_eq!(parse_address("docs.rs/gpui"), url("https://docs.rs/gpui"));
        assert_eq!(parse_address("http://example.com"), url("http://example.com"));
        assert_eq!(parse_address("  https://zed.dev  "), url("https://zed.dev"));
        assert_eq!(parse_address("about:blank"), url("about:blank"));
    }

    #[test]
    fn everything_else_is_a_search() {
        assert_eq!(parse_address("rust gpui"), AddressInput::Search("rust gpui".into()));
        assert_eq!(parse_address("gpui"), AddressInput::Search("gpui".into()));
        assert_eq!(
            resolve_address("how to use cef", DEFAULT_SEARCH_URL),
            "https://duckduckgo.com/?q=how+to+use+cef"
        );
    }

    #[test]
    fn file_paths_become_file_urls() {
        assert_eq!(parse_address("/tmp/index.html"), url("file:///tmp/index.html"));
    }

    #[test]
    fn origins_and_locality() {
        assert_eq!(origin_of("http://localhost:3000/a?b").as_deref(), Some("http://localhost:3000"));
        assert_eq!(origin_of("https://example.com/").as_deref(), Some("https://example.com"));
        assert_eq!(origin_of("about:blank"), None);
        assert!(is_local_url("http://127.0.0.1:9000/"));
        assert!(is_local_url("http://localhost:3000"));
        assert!(!is_local_url("http://localhost.evil.com"));
        assert!(!is_local_url("https://example.com"));
    }
}
