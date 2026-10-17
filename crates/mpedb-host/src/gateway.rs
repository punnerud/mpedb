//! JSON transport for an authenticated host service behind Nginx or another
//! gateway. The service MUST bind/authenticate this principal before dispatch;
//! a JSON principal alone is never authentication or authorization.
use super::*;
const WIRE_LIMIT: u64 = 64 * 1024 * 1024;
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayEnvelope {
    pub version: u32,
    pub principal: String,
    pub request: Request,
}
pub struct GatewayBackend {
    transport: crate::http::Transport,
    path: String,
    bearer: Option<String>,
}
impl GatewayBackend {
    /// Target/IPs/credential come only from the host environment, not SQL.
    /// No automatic retries: a lost response may follow an external side effect.
    pub fn new(target: Endpoint, path: String, bearer: Option<String>) -> Result<Self> {
        crate::http::validate_endpoint(&target)?;
        if !target.methods.contains("POST") {
            return Err(Error::Config("gateway endpoint must permit POST".into()));
        }
        let mut headers = BTreeMap::from([("content-type".into(), "application/json".into())]);
        if let Some(token) = &bearer {
            headers.insert("authorization".into(), format!("Bearer {token}"));
        }
        crate::http::validate_request(&target, &path, &headers)?;
        Ok(Self {
            transport: crate::http::Transport::new(target)?,
            path,
            bearer,
        })
    }
}
impl Backend for GatewayBackend {
    fn execute(&self, authorized: Authorized<'_>) -> Result<Response> {
        #[derive(Serialize)]
        struct Envelope<'a> {
            version: u32,
            principal: &'a str,
            request: &'a Request,
        }
        let bytes = serde_json::to_vec(&Envelope {
            version: 1,
            principal: authorized.principal,
            request: authorized.request,
        })
        .map_err(|_| Error::Transport("gateway request encoding failed"))?;
        if bytes.len() as u64 > WIRE_LIMIT {
            return Err(Error::Limit("gateway request frame exceeds budget"));
        }
        let mut headers = BTreeMap::from([("content-type".into(), "application/json".into())]);
        if let Some(token) = &self.bearer {
            headers.insert("authorization".into(), format!("Bearer {token}"));
        }
        let limits = Limits {
            read_bytes: WIRE_LIMIT,
            write_bytes: WIRE_LIMIT,
            timeout_ms: authorized.limits.timeout_ms,
        };
        let Response::Http { status, body, .. } = self
            .transport
            .request("POST", &self.path, &headers, &bytes, limits)?
        else {
            unreachable!()
        };
        if status != 200 {
            return Err(Error::Transport(
                "gateway refused or failed the host request",
            ));
        }
        let response: Response = serde_json::from_slice(&body)
            .map_err(|_| Error::Transport("invalid gateway response frame"))?;
        // A remote provider cannot enlarge the local result budget or change
        // the response shape; the service enforces its OWN environment too.
        match (&authorized.request, &response) {
            (Request::ReadFile { .. }, Response::File { bytes })
                if bytes.len() as u64 <= authorized.limits.read_bytes => {}
            (Request::WriteFile { bytes, .. }, Response::Written { bytes: written })
                if *written == bytes.len() as u64 => {}
            (Request::Http { .. }, Response::Http { body, headers, .. })
                if body.len() as u64 <= authorized.limits.read_bytes
                    && headers.len() <= 128
                    && headers
                        .iter()
                        .map(|(k, v)| k.len().saturating_add(v.len()))
                        .sum::<usize>()
                        <= 65536 => {}
            _ => {
                return Err(Error::Transport(
                    "gateway response shape/size violates the authorized request",
                ))
            }
        }
        Ok(response)
    }
}
