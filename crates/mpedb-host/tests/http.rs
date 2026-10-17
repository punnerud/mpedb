#![cfg(feature = "http")]
use mpedb_host::*;
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::{IpAddr, TcpListener, TcpStream},
    thread,
    time::{Duration, Instant},
};
struct Message {
    line: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}
fn message(stream: &mut TcpStream) -> Message {
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut bytes = Vec::new();
    let end = loop {
        if let Some(end) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
            break end + 4;
        }
        assert!(bytes.len() < 65536);
        let mut part = [0; 1024];
        let n = stream.read(&mut part).unwrap();
        assert!(n > 0);
        bytes.extend_from_slice(&part[..n]);
    };
    let text = std::str::from_utf8(&bytes[..end]).unwrap();
    let mut lines = text.split("\r\n");
    let line = lines.next().unwrap().to_string();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string()))
        .collect::<BTreeMap<_, _>>();
    let length = headers
        .get("content-length")
        .map_or(0, |n| n.parse::<usize>().unwrap());
    assert!(length < 2 * 1024 * 1024);
    let mut body = bytes[end..].to_vec();
    while body.len() < length {
        let mut part = [0; 4096];
        let n = stream.read(&mut part).unwrap();
        assert!(n > 0);
        body.extend_from_slice(&part[..n]);
    }
    assert_eq!(body.len(), length);
    Message {
        line,
        headers,
        body,
    }
}
fn server(f: impl FnOnce(Message) -> Vec<u8> + Send + 'static) -> (u16, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    let worker = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline =>
                {
                    thread::sleep(Duration::from_millis(5))
                }
                Err(e) => panic!("test server accept: {e}"),
            }
        };
        let request = message(&mut stream);
        let response = f(request);
        let _ = stream.write_all(&response);
    });
    (port, worker)
}
fn reply(status: u16, headers: &str, body: &[u8]) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
}
fn endpoint(port: u16) -> Endpoint {
    Endpoint {
        base_url: format!("http://host.invalid:{port}/api/"),
        connect_ips: vec![IpAddr::from([127, 0, 0, 1])],
        methods: ["GET".into(), "POST".into()].into(),
        headers: ["x-id".into()].into(),
        allow_http: true,
    }
}
fn setup(target: Endpoint) -> (Environment, BTreeMap<String, Grants>) {
    let env = Environment {
        endpoints: BTreeMap::from([("service".into(), target)]),
        ..Default::default()
    };
    let acl = BTreeMap::from([(
        "worker".into(),
        Grants {
            endpoints: BTreeMap::from([("service".into(), ["GET".into()].into())]),
            ..Default::default()
        },
    )]);
    (env, acl)
}
fn request(path: &str) -> Request {
    Request::Http {
        endpoint: "service".into(),
        method: "GET".into(),
        path: path.into(),
        headers: BTreeMap::new(),
        body: vec![],
    }
}
#[test]
fn native_http_pins_dns_and_preserves_the_endpoint_host_and_path() {
    let (port, worker) = server(|m| {
        assert!(m.line.starts_with("GET /api/items?x=1 HTTP/1.1"));
        assert!(m.headers["host"].starts_with("host.invalid:"));
        assert_eq!(m.headers["x-id"], "allowed");
        reply(200, "Content-Type: text/plain\r\n", b"real response")
    });
    let (env, acl) = setup(endpoint(port));
    let runtime = Runtime::native(env, acl).unwrap();
    let call = Request::Http {
        endpoint: "service".into(),
        method: "GET".into(),
        path: "items?x=1".into(),
        headers: BTreeMap::from([("X-ID".into(), "allowed".into())]),
        body: vec![],
    };
    assert!(
        matches!(runtime.execute("worker",&call).unwrap(),Response::Http {status:200,body,..} if body==b"real response")
    );
    worker.join().unwrap();
}
#[test]
fn environment_and_inner_network_grants_cannot_be_bypassed_by_url_or_headers() {
    let (env, acl) = setup(endpoint(9));
    let runtime = Runtime::native(env, acl).unwrap();
    for path in [
        "../outside",
        "https://elsewhere.invalid/",
        "//elsewhere.invalid/",
        "%2e%2e/outside",
        "x%2fy",
        "x%252fy",
        "x\\y",
        "x#fragment",
    ] {
        assert!(
            matches!(
                runtime.execute("worker", &request(path)),
                Err(Error::Denied(_))
            ),
            "{path}"
        );
    }
    for header in [
        "Host",
        "Authorization",
        "Transfer-Encoding",
        "Content-Length",
        "Connection",
    ] {
        let call = Request::Http {
            endpoint: "service".into(),
            method: "GET".into(),
            path: "items".into(),
            headers: BTreeMap::from([(header.into(), "forbidden".into())]),
            body: vec![],
        };
        assert!(matches!(
            runtime.execute("worker", &call),
            Err(Error::Denied(_))
        ));
    }
    let call = Request::Http {
        endpoint: "service".into(),
        method: "POST".into(),
        path: "items".into(),
        headers: BTreeMap::new(),
        body: vec![],
    };
    assert!(matches!(
        runtime.execute("worker", &call),
        Err(Error::Denied(_))
    ));
    assert!(runtime.execute("other-db", &request("items")).is_err());
}
#[test]
fn redirects_are_returned_without_following_and_body_limits_are_enforced() {
    let (port, worker) =
        server(|_| reply(302, "Location: http://elsewhere.invalid/escape\r\n", b""));
    let (env, acl) = setup(endpoint(port));
    let runtime = Runtime::native(env, acl).unwrap();
    assert!(matches!(
        runtime.execute("worker", &request("redirect")).unwrap(),
        Response::Http { status: 302, .. }
    ));
    worker.join().unwrap();
    let (port, worker) = server(|_| reply(200, "", b"oversized"));
    let (mut env, acl) = setup(endpoint(port));
    env.limits.read_bytes = 3;
    let runtime = Runtime::native(env, acl).unwrap();
    assert!(matches!(
        runtime.execute("worker", &request("big")),
        Err(Error::Limit(_))
    ));
    worker.join().unwrap();
}
#[test]
fn network_deadlines_and_literal_ip_pins_are_enforced() {
    let (port, worker) = server(|_| {
        thread::sleep(Duration::from_millis(250));
        reply(200, "", b"ok")
    });
    let (mut env, acl) = setup(endpoint(port));
    env.limits.timeout_ms = 50;
    let runtime = Runtime::native(env, acl).unwrap();
    assert!(matches!(
        runtime.execute("worker", &request("slow")),
        Err(Error::Transport(_))
    ));
    worker.join().unwrap();
    let mut target = endpoint(9);
    target.base_url = "http://127.0.0.2:9/api/".into();
    let (env, acl) = setup(target);
    assert!(Runtime::native(env, acl).is_err());
}
#[test]
fn gateway_executes_the_same_request_under_its_own_environment_and_acl() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("file"), b"remote file").unwrap();
    let env = Environment {
        directories: BTreeMap::from([(
            "files".into(),
            Directory {
                root: dir.path().into(),
                read: true,
                write: false,
            },
        )]),
        ..Default::default()
    };
    let acl = BTreeMap::from([(
        "worker".into(),
        Grants {
            read_directories: ["files".into()].into(),
            ..Default::default()
        },
    )]);
    let service = Runtime::native(env, acl.clone()).unwrap();
    let (port, worker) = server(move |m| {
        assert_eq!(m.line, "POST /api/dispatch HTTP/1.1");
        assert_eq!(m.headers["authorization"], "Bearer test-credential");
        let envelope: GatewayEnvelope = serde_json::from_slice(&m.body).unwrap();
        assert_eq!(envelope.version, 1);
        assert_eq!(envelope.principal, "worker");
        // The real gateway authenticates/binds this subject, independently of
        // the JSON assertion. It never blindly trusts envelope.principal.
        let response = service.execute("worker", &envelope.request).unwrap();
        reply(
            200,
            "Content-Type: application/json\r\n",
            &serde_json::to_vec(&response).unwrap(),
        )
    });
    let mut target = endpoint(port);
    target.headers = ["authorization".into(), "content-type".into()].into();
    let gateway =
        GatewayBackend::new(target, "dispatch".into(), Some("test-credential".into())).unwrap();
    let env = Environment {
        directories: BTreeMap::from([(
            "files".into(),
            Directory {
                root: "/not-opened-on-client".into(),
                read: true,
                write: false,
            },
        )]),
        ..Default::default()
    };
    let runtime = Runtime::new(env, acl, gateway).unwrap();
    let request = Request::ReadFile {
        directory: "files".into(),
        path: "file".into(),
    };
    assert!(
        matches!(runtime.execute("worker",&request).unwrap(),Response::File {bytes} if bytes==b"remote file")
    );
    worker.join().unwrap();
}
#[test]
fn gateway_cannot_return_a_different_operation_or_expand_result_limits() {
    for response in [
        Response::Written { bytes: 1 },
        Response::File { bytes: vec![0; 10] },
    ] {
        let (port, worker) = server(move |_| {
            reply(
                200,
                "Content-Type: application/json\r\n",
                &serde_json::to_vec(&response).unwrap(),
            )
        });
        let mut target = endpoint(port);
        target.headers = ["content-type".into()].into();
        let gateway = GatewayBackend::new(target, "dispatch".into(), None).unwrap();
        let env = Environment {
            directories: BTreeMap::from([(
                "files".into(),
                Directory {
                    root: "/remote".into(),
                    read: true,
                    write: false,
                },
            )]),
            limits: Limits {
                read_bytes: 2,
                ..Default::default()
            },
            ..Default::default()
        };
        let acl = BTreeMap::from([(
            "worker".into(),
            Grants {
                read_directories: ["files".into()].into(),
                ..Default::default()
            },
        )]);
        let runtime = Runtime::new(env, acl, gateway).unwrap();
        assert!(matches!(
            runtime.execute(
                "worker",
                &Request::ReadFile {
                    directory: "files".into(),
                    path: "x".into()
                }
            ),
            Err(Error::Transport(_))
        ));
        worker.join().unwrap();
    }
}

#[test]
fn pooled_transport_reuses_connections_and_preserves_repeated_response_headers() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    let worker = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline =>
                {
                    thread::sleep(Duration::from_millis(5))
                }
                Err(e) => panic!("pooled server accept: {e}"),
            }
        };
        for _ in 0..2 {
            let request = message(&mut stream);
            assert!(request.line.starts_with("GET /api/reuse "));
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: keep-alive\r\nSet-Cookie: a=1\r\nSet-Cookie: b=2\r\n\r\nok").unwrap();
        }
    });
    let (env, acl) = setup(endpoint(port));
    let runtime = Runtime::native(env, acl).unwrap();
    for _ in 0..2 {
        let Response::Http {
            status,
            headers,
            body,
        } = runtime.execute("worker", &request("reuse")).unwrap()
        else {
            panic!("expected HTTP response")
        };
        assert_eq!(status, 200);
        assert_eq!(body, b"ok");
        assert_eq!(
            headers
                .iter()
                .filter(|(name, _)| name == "set-cookie")
                .map(|(_, value)| value.as_str())
                .collect::<Vec<_>>(),
            vec!["a=1", "b=2"]
        );
    }
    worker.join().unwrap();
}
