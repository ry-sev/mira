//! Hosts, sites, and the parts of a URL.

use url::Url;

/// Suffixes under which every label belongs to someone else, so a site is
/// three labels rather than two: `example.co.uk`, `myapp.vercel.app`.
const SHARED_SUFFIXES: &[&str] = &[
    "co.uk",
    "org.uk",
    "ac.uk",
    "gov.uk",
    "me.uk",
    "ltd.uk",
    "plc.uk",
    "com.au",
    "net.au",
    "org.au",
    "edu.au",
    "gov.au",
    "co.nz",
    "org.nz",
    "co.jp",
    "ne.jp",
    "or.jp",
    "co.kr",
    "or.kr",
    "com.br",
    "net.br",
    "org.br",
    "com.cn",
    "net.cn",
    "org.cn",
    "com.hk",
    "com.tw",
    "co.in",
    "net.in",
    "org.in",
    "co.za",
    "co.il",
    "com.mx",
    "com.ar",
    "com.tr",
    "com.sg",
    "com.my",
    "co.id",
    "com.ph",
    "com.vn",
    "com.ua",
    "com.pl",
    "com.ru",
    "cloudfront.net",
    "amazonaws.com",
    "azurewebsites.net",
    "azureedge.net",
    "appspot.com",
    "herokuapp.com",
    "github.io",
    "gitlab.io",
    "blogspot.com",
    "workers.dev",
    "pages.dev",
    "vercel.app",
    "netlify.app",
    "web.app",
    "firebaseapp.com",
    "googleusercontent.com",
    "r2.dev",
    "b-cdn.net",
    "akamaized.net",
    "edgekey.net",
    "fastly.net",
    "onrender.com",
    "glitch.me",
];

/// The site a host belongs to: `cdn.ads.example.co.uk` → `example.co.uk`.
///
/// An IP address is its own site.
pub fn site_of(host: &str) -> String {
    if host.is_empty() || host.starts_with('[') || host.parse::<std::net::IpAddr>().is_ok() {
        return host.to_owned();
    }
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() <= 2 {
        return host;
    }
    let keep = if SHARED_SUFFIXES.contains(&labels[labels.len() - 2..].join(".").as_str()) {
        3
    } else {
        2
    };
    labels[labels.len() - keep..].join(".")
}

/// A URL split for drawing: the scheme dims, the host takes its site's
/// color, the path reads plainly, and the query dims again.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Parts {
    pub scheme: String,
    pub host: String,
    pub path: String,
    pub query: String,
}

/// The parts of `url`.
pub fn parts(url: &Url) -> Parts {
    let host = match (url.host_str(), url.port()) {
        (Some(host), Some(port)) => format!("{host}:{port}"),
        (Some(host), None) => host.to_owned(),
        (None, _) => String::new(),
    };
    Parts {
        scheme: format!("{}://", url.scheme()),
        host,
        path: url.path().to_owned(),
        query: url.query().map(|q| format!("?{q}")).unwrap_or_default(),
    }
}

/// A URL's query parameters, decoded, in order.
pub fn params(url: &Url) -> Vec<(String, String)> {
    url.query_pairs()
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_site_of() {
        assert_eq!(site_of("cdn.ads.example.co.uk"), "example.co.uk");
        assert_eq!(site_of("www.example.com"), "example.com");
        assert_eq!(site_of("Example.COM."), "example.com");
        assert_eq!(site_of("myapp.vercel.app"), "myapp.vercel.app");
        assert_eq!(site_of("10.0.0.1"), "10.0.0.1");
        assert_eq!(site_of("localhost"), "localhost");
    }

    #[test]
    fn test_parts() {
        let url = Url::parse("https://api.example.com:8443/v1/items?page=2").expect("url");
        let p = parts(&url);
        assert_eq!(p.scheme, "https://");
        assert_eq!(p.host, "api.example.com:8443");
        assert_eq!(p.path, "/v1/items");
        assert_eq!(p.query, "?page=2");
    }

    #[test]
    fn test_params_are_decoded() {
        let url = Url::parse("http://x/?q=a%20b&empty=").expect("url");
        assert_eq!(
            params(&url),
            vec![
                ("q".to_owned(), "a b".to_owned()),
                ("empty".to_owned(), String::new())
            ]
        );
    }
}
