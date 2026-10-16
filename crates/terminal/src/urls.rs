//! Spotting dev-server URLs in terminal output, so the IDE can offer to open
//! them in the embedded browser.

use std::sync::OnceLock;

use regex::Regex;

fn local_url_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)\bhttps?://(?:localhost|127\.0\.0\.1|0\.0\.0\.0|\[::1?\])(?::\d{1,5})?(?:/[^\s'\x22<>)\]]*)?",
        )
        .expect("valid regex")
    })
}

/// Local URLs in `text`, deduplicated in order of appearance. `0.0.0.0` and
/// `[::]` (bind-all addresses servers like to print) become `localhost`.
pub fn detect_local_urls(text: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for m in local_url_regex().find_iter(text) {
        // `http://localhost.evil.com` matches up to `localhost`; a hostname
        // character right after the match means the host was something else.
        let rest = &text[m.end()..];
        if rest.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '-')
            || rest.starts_with('.') && rest[1..].starts_with(|c: char| c.is_ascii_alphanumeric())
        {
            continue;
        }
        let url = m
            .as_str()
            .trim_end_matches(['.', ',', ';', ':'])
            .replacen("0.0.0.0", "localhost", 1)
            .replacen("[::]", "localhost", 1);
        if !found.contains(&url) {
            found.push(url);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_common_dev_server_banners() {
        let output = "\
  VITE v5.0.0  ready in 300 ms
  ➜  Local:   http://localhost:5173/
  ➜  Network: use --host to expose
Server listening on http://0.0.0.0:8080.
next dev: ready - started server on http://127.0.0.1:3000, url: http://localhost:3000";
        assert_eq!(
            detect_local_urls(output),
            vec![
                "http://localhost:5173/",
                "http://localhost:8080",
                "http://127.0.0.1:3000",
                "http://localhost:3000",
            ]
        );
    }

    #[test]
    fn ignores_remote_hosts() {
        assert!(detect_local_urls("see https://example.com:3000/").is_empty());
        assert!(detect_local_urls("http://localhost.evil.com/").is_empty());
        assert!(detect_local_urls("http://localhostile:80/").is_empty());
        // A sentence-ending period is not part of the host.
        assert_eq!(detect_local_urls("open http://localhost."), vec!["http://localhost"]);
    }
}
