use mpedb_host::*;
use std::collections::BTreeMap;
fn setup(root: &std::path::Path) -> (Environment, BTreeMap<String, Grants>) {
    let environment = Environment {
        directories: BTreeMap::from([(
            "files".into(),
            Directory {
                root: root.into(),
                read: true,
                write: true,
            },
        )]),
        allow_acl_changes: true,
        ..Default::default()
    };
    let grants = BTreeMap::from([
        (
            "admin".into(),
            Grants {
                manage_access: true,
                ..Default::default()
            },
        ),
        ("worker".into(), Grants::default()),
    ]);
    (environment, grants)
}
fn permissions() -> Grants {
    Grants {
        read_directories: ["files".into()].into(),
        write_directories: ["files".into()].into(),
        ..Default::default()
    }
}
fn read(path: &str) -> Request {
    Request::ReadFile {
        directory: "files".into(),
        path: path.into(),
    }
}
fn write(path: &str, bytes: &[u8]) -> Request {
    Request::WriteFile {
        directory: "files".into(),
        path: path.into(),
        bytes: bytes.into(),
    }
}
#[test]
fn inner_acl_can_expand_and_revoke_itself_without_expanding_the_environment() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a"), b"ok").unwrap();
    let (mut env, acl) = setup(dir.path());
    env.directories.get_mut("files").unwrap().write = false;
    let runtime = Runtime::native(env, acl).unwrap();
    assert!(matches!(
        runtime.execute("worker", &read("a")),
        Err(Error::Denied(_))
    ));
    assert!(runtime
        .set_grants("worker", "worker", permissions())
        .is_err());
    let mut expanded = permissions();
    expanded.manage_access = true;
    runtime.set_grants("admin", "worker", expanded).unwrap();
    assert!(
        matches!(runtime.execute("worker",&read("a")).unwrap(),Response::File {bytes} if bytes==b"ok")
    );
    // A database administrator may grant more; the environment still wins.
    assert!(matches!(
        runtime.execute("worker", &write("b", b"no")),
        Err(Error::Denied(_))
    ));
    assert!(!dir.path().join("b").exists());
    runtime
        .set_grants("worker", "worker", Grants::default())
        .unwrap();
    assert!(runtime.execute("worker", &read("a")).is_err());
    assert!(runtime
        .set_grants("worker", "worker", permissions())
        .is_err());
}
#[test]
fn environment_can_disable_acl_administration_even_for_inner_admins() {
    let dir = tempfile::tempdir().unwrap();
    let (mut env, acl) = setup(dir.path());
    env.allow_acl_changes = false;
    let runtime = Runtime::native(env, acl).unwrap();
    assert!(matches!(
        runtime.set_grants("admin", "admin", permissions()),
        Err(Error::Denied(_))
    ));
}
#[test]
fn file_io_is_real_atomic_bounded_and_principal_scoped() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    let (mut env, mut acl) = setup(dir.path());
    env.limits.read_bytes = 4;
    env.limits.write_bytes = 8;
    let mut rights = permissions();
    rights.limits.read_bytes = 20;
    rights.limits.write_bytes = 6;
    acl.insert("worker".into(), rights);
    let runtime = Runtime::native(env, acl).unwrap();
    assert!(matches!(
        runtime
            .execute("worker", &write("sub/a", b"123456"))
            .unwrap(),
        Response::Written { bytes: 6 }
    ));
    assert!(matches!(
        runtime.execute("worker", &read("sub/a")),
        Err(Error::Limit(_))
    ));
    assert!(matches!(
        runtime.execute("worker", &write("sub/a", b"1234567")),
        Err(Error::Limit(_))
    ));
    assert_eq!(std::fs::read(dir.path().join("sub/a")).unwrap(), b"123456");
    runtime
        .execute("worker", &write("sub/a", "ø🙂".as_bytes()))
        .unwrap();
    runtime
        .execute("worker", &write("sub/a", b"\0\xffab"))
        .unwrap();
    assert!(
        matches!(runtime.execute("worker",&read("sub/a")).unwrap(),Response::File {bytes} if bytes==b"\0\xffab")
    );
    assert!(runtime.execute("admin", &read("sub/a")).is_err());
    assert!(runtime.execute("other-database", &read("sub/a")).is_err());
    assert_eq!(
        std::fs::read_dir(dir.path().join("sub")).unwrap().count(),
        1
    );
}
#[test]
fn traversal_and_unknown_directories_never_reach_the_filesystem() {
    let dir = tempfile::tempdir().unwrap();
    let (env, mut acl) = setup(dir.path());
    acl.insert("worker".into(), permissions());
    let runtime = Runtime::native(env, acl).unwrap();
    for path in ["../outside", "/etc/passwd", "a/../../b", "a\\b", "x\0y", ""] {
        assert!(
            matches!(
                runtime.execute("worker", &read(path)),
                Err(Error::Denied(_))
            ),
            "{path:?}"
        );
        assert!(
            matches!(
                runtime.execute("worker", &write(path, b"no")),
                Err(Error::Denied(_))
            ),
            "{path:?}"
        );
    }
    runtime
        .set_grants(
            "admin",
            "worker",
            Grants {
                read_directories: ["missing".into()].into(),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(matches!(
        runtime.execute(
            "worker",
            &Request::ReadFile {
                directory: "missing".into(),
                path: "x".into()
            }
        ),
        Err(Error::Denied(_))
    ));
}
#[cfg(unix)]
#[test]
fn symlink_escapes_and_special_files_fail_and_writes_do_not_follow_hardlinks() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret"), b"original").unwrap();
    symlink(outside.path(), root.path().join("escape")).unwrap();
    symlink(outside.path().join("secret"), root.path().join("link")).unwrap();
    std::fs::hard_link(outside.path().join("secret"), root.path().join("hard")).unwrap();
    let (env, mut acl) = setup(root.path());
    acl.insert("worker".into(), permissions());
    let runtime = Runtime::native(env, acl).unwrap();
    assert!(runtime.execute("worker", &read("escape/secret")).is_err());
    assert!(runtime.execute("worker", &read("link")).is_err());
    assert!(runtime
        .execute("worker", &write("escape/secret", b"bad"))
        .is_err());
    runtime
        .execute("worker", &write("link", b"replacement"))
        .unwrap();
    runtime
        .execute("worker", &write("hard", b"replacement"))
        .unwrap();
    assert_eq!(
        std::fs::read(outside.path().join("secret")).unwrap(),
        b"original"
    );
    let fifo = std::ffi::CString::new(root.path().join("pipe").to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    assert!(matches!(
        runtime.execute("worker", &read("pipe")),
        Err(Error::Denied(_))
    ));
}
#[test]
fn configuration_is_strict_and_empty_configuration_grants_nothing() {
    assert!(Environment::from_toml("allow_all = true").is_err());
    assert!(Environment::from_toml("[limits]\ntimeout_ms = 0").is_err());
    assert!(Environment::from_toml("[directories.files]\nroot = 'relative'\nread = true").is_err());
    let runtime = Runtime::native(Environment::default(), BTreeMap::new()).unwrap();
    assert!(runtime.execute("admin", &read("x")).is_err());
}

#[test]
fn concurrent_services_have_bounded_in_flight_work_and_acl_changes_do_not_wait_for_io() {
    use std::sync::{mpsc, Arc, Mutex};
    struct Blocking {
        entered: mpsc::SyncSender<()>,
        release: Mutex<mpsc::Receiver<()>>,
    }
    impl Backend for Blocking {
        fn execute(&self, _: Authorized<'_>) -> Result<Response> {
            self.entered.send(()).unwrap();
            self.release.lock().unwrap().recv().unwrap();
            Ok(Response::File { bytes: vec![] })
        }
    }
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let backend = Blocking {
        entered: entered_tx,
        release: Mutex::new(release_rx),
    };
    let (mut env, mut acl) = setup(std::path::Path::new("/trusted-provider-resource"));
    env.max_in_flight_per_runtime = 1;
    acl.insert("worker".into(), permissions());
    let runtime = Arc::new(Runtime::new(env, acl, backend).unwrap());
    let worker = {
        let runtime = runtime.clone();
        std::thread::spawn(move || runtime.execute("worker", &read("x")))
    };
    entered_rx
        .recv_timeout(std::time::Duration::from_secs(3))
        .unwrap();
    assert!(matches!(
        runtime.execute("worker", &read("x")),
        Err(Error::Limit(_))
    ));
    runtime
        .set_grants("admin", "worker", Grants::default())
        .unwrap();
    assert!(matches!(
        runtime.execute("worker", &read("x")),
        Err(Error::Denied(_))
    ));
    release_tx.send(()).unwrap();
    assert!(worker.join().unwrap().is_ok());
    runtime
        .set_grants("admin", "worker", permissions())
        .unwrap();
    release_tx.send(()).unwrap();
    assert!(runtime.execute("worker", &read("x")).is_ok());
}
