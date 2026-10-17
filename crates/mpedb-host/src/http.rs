use super::*;
use std::{io::Read, net::SocketAddr, time::Duration};
use url::Url;
const RESERVED_HEADERS: &[&str] = &[
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "upgrade",
    "expect",
];
pub(crate) fn validate_endpoint(target: &Endpoint) -> Result<Url> {
    let base = Url::parse(&target.base_url)
        .map_err(|_| Error::Config("invalid host endpoint URL".into()))?;
    if !matches!(base.scheme(), "https" | "http")
        || (base.scheme() == "http" && !target.allow_http)
        || base.host().is_none()
        || !base.username().is_empty()
        || base.password().is_some()
        || base.query().is_some()
        || base.fragment().is_some()
        || !base.path().ends_with('/')
        || target.connect_ips.is_empty()
    {
        return Err(Error::Config(
            "endpoint requires an explicit origin, directory path, pinned IPs and permitted scheme"
                .into(),
        ));
    }
    if let Some(url::Host::Ipv4(ip)) = base.host() {
        if !target.connect_ips.contains(&ip.into()) {
            return Err(Error::Config(
                "literal endpoint address must match pinned IPs".into(),
            ));
        }
    }
    if let Some(url::Host::Ipv6(ip)) = base.host() {
        if !target.connect_ips.contains(&ip.into()) {
            return Err(Error::Config(
                "literal endpoint address must match pinned IPs".into(),
            ));
        }
    }
    Ok(base)
}
fn request_url(target: &Endpoint, path: &str) -> Result<Url> {
    let base = validate_endpoint(target)?;
    if path.starts_with('/')
        || path.contains(['\\', '\0', '#'])
        || path.chars().any(char::is_control)
    {
        return Err(Error::Denied(
            "network path must stay below its named endpoint",
        ));
    }
    // Prevent server-dependent decoding of escaped separators/dot segments.
    let route = path.split('?').next().unwrap_or("").as_bytes();
    for i in 0..route.len() {
        if route[i] == b'%' {
            let escape = route
                .get(i + 1..i + 3)
                .and_then(|b| std::str::from_utf8(b).ok())
                .and_then(|s| u8::from_str_radix(s, 16).ok())
                .ok_or(Error::Denied("invalid URL path escape"))?;
            if matches!(escape, 0 | b'/' | b'\\' | b'.' | b'%') {
                return Err(Error::Denied(
                    "escaped path separators/dot segments are not permitted",
                ));
            }
        }
    }
    let url = base
        .join(path)
        .map_err(|_| Error::Denied("invalid endpoint-relative path"))?;
    if url.origin() != base.origin()
        || !url.path().starts_with(base.path())
        || url.path() == base.path().trim_end_matches('/')
    {
        return Err(Error::Denied("network URL leaves its endpoint scope"));
    }
    Ok(url)
}
pub(crate) fn validate_request(
    target: &Endpoint,
    path: &str,
    headers: &BTreeMap<String, String>,
) -> Result<()> {
    request_url(target, path)?;
    for (name, value) in headers {
        let lower = name.to_ascii_lowercase();
        if RESERVED_HEADERS.contains(&lower.as_str())
            || !target.headers.contains(&lower)
            || reqwest::header::HeaderName::from_bytes(name.as_bytes()).is_err()
            || reqwest::header::HeaderValue::from_str(value).is_err()
        {
            return Err(Error::Denied("network header is not permitted"));
        }
    }
    Ok(())
}
pub(crate) struct Transport {
    pub(crate) target: Endpoint,
    client: reqwest::blocking::Client,
}
impl Transport {
    pub(crate) fn new(target: Endpoint) -> Result<Self> {
        let base = validate_endpoint(&target)?;
        let host = base
            .host_str()
            .ok_or(Error::Denied("missing endpoint host"))?;
        let addresses = target
            .connect_ips
            .iter()
            .map(|ip| SocketAddr::new(*ip, base.port_or_known_default().unwrap()))
            .collect::<Vec<_>>();
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .retry(reqwest::retry::never())
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .redirect(reqwest::redirect::Policy::none())
            .referer(false)
            .timeout(Duration::from_millis(120_000))
            .connect_timeout(Duration::from_millis(120_000))
            .resolve_to_addrs(host, &addresses)
            .build()
            .map_err(|_| Error::Transport("HTTP client initialization failed"))?;
        Ok(Self { target, client })
    }
    pub(crate) fn request(
        &self,
        method: &str,
        path: &str,
        headers: &BTreeMap<String, String>,
        body: &[u8],
        limits: Limits,
    ) -> Result<Response> {
        let target = &self.target;
        validate_request(target, path, headers)?;
        if !target.methods.contains(method) || body.len() as u64 > limits.write_bytes {
            return Err(Error::Denied("network method/body exceeds grant"));
        }
        let url = request_url(target, path)?;
        let method = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|_| Error::Denied("invalid HTTP method"))?;
        let mut call = self
            .client
            .request(method, url)
            .timeout(Duration::from_millis(limits.timeout_ms));
        for (name, value) in headers {
            call = call.header(name, value);
        }
        let response = call.body(body.to_vec()).send().map_err(|e| {
            if e.is_timeout() {
                Error::Transport("HTTP request timed out")
            } else {
                Error::Transport("HTTP request failed")
            }
        })?;
        let status = response.status().as_u16();
        let mut headers = Vec::new();
        let mut size = 0usize;
        for (name, value) in response.headers() {
            size = size
                .saturating_add(name.as_str().len())
                .saturating_add(value.as_bytes().len());
            if size > 65536 || headers.len() >= 128 {
                return Err(Error::Limit("HTTP response header budget exceeded"));
            }
            headers.push((
                name.to_string(),
                value
                    .to_str()
                    .map_err(|_| Error::Transport("HTTP response contains non-text headers"))?
                    .into(),
            ));
        }
        let mut body = Vec::new();
        response
            .take(limits.read_bytes + 1)
            .read_to_end(&mut body)
            .map_err(|_| Error::Transport("HTTP response read failed"))?;
        if body.len() as u64 > limits.read_bytes {
            return Err(Error::Limit("HTTP response byte budget exceeded"));
        }
        Ok(Response::Http {
            status,
            headers,
            body,
        })
    }
}
