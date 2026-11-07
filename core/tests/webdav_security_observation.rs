//! Echte HTTPS-Gegenläufe für die ausschließlich ergänzende DAV-Beobachtung.
use doppeldecker_core::*;
use http_body_util::{BodyExt, Full};
use hyper::{server::conn::http1, service::service_fn, Request, Response};
use hyper_util::rt::TokioIo;
use std::io;
use std::path::Path;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::Duration;
use tokio_rustls::TlsAcceptor;

struct Secrets;
impl SecretResolver for Secrets {
    fn resolve(&self, _: &KeychainSecretReference) -> io::Result<SecretString> {
        SecretString::new("synthetic-fixture".into())
    }
}
#[derive(Clone)]
struct Behavior {
    query_status: u16,
    property_status: u16,
    wrong_href: bool,
    malformed: bool,
    change_after: bool,
    disappear_after: bool,
    read: bool,
    calls: Vec<(String, String, String)>,
}
struct Server {
    config: WebDavConnectionConfig,
    behavior: Arc<Mutex<Behavior>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Server {
    fn new() -> Self {
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let key =
            rustls::pki_types::PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der());
        let tls = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![certified.cert.der().clone()], key.into())
        .unwrap();
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let origin = format!(
            "https://localhost:{}",
            listener.local_addr().unwrap().port()
        );
        listener.set_nonblocking(true).unwrap();
        let behavior = Arc::new(Mutex::new(Behavior {
            query_status: 207,
            property_status: 404,
            wrong_href: false,
            malformed: false,
            change_after: false,
            disappear_after: false,
            read: false,
            calls: vec![],
        }));
        let state = behavior.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let ended = stop.clone();
        let thread = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                let acceptor = TlsAcceptor::from(Arc::new(tls));
                while !ended.load(Ordering::SeqCst) {
                    let accepted = tokio::time::timeout(Duration::from_millis(25), listener.accept()).await;
                    let Ok(Ok((stream, _))) = accepted else { continue };
                    let acceptor = acceptor.clone(); let state = state.clone();
                    tokio::spawn(async move {
                        let Ok(tls) = acceptor.accept(stream).await else { return };
                        let service = service_fn(move |request: Request<hyper::body::Incoming>| {
                            let state = state.clone();
                            async move {
                                let (parts, body) = request.into_parts();
                                let bytes = body.collect().await.unwrap().to_bytes();
                                let body = String::from_utf8(bytes.to_vec()).unwrap();
                                let mut state = state.lock().unwrap();
                                state.calls.push((parts.method.to_string(), parts.uri.path().to_string(), body.clone()));
                                let mut status = 207;
                                let mut result = String::new();
                                if parts.method == "OPTIONS" { status = 200; }
                                else if parts.method == "GET" { status = 200; result = "data".into(); }
                                else if parts.method != "PROPFIND" { status = 405; }
                                else if body.contains("<D:acl/>") {
                                    assert_eq!(parts.headers.get("depth").unwrap(), "0");
                                    assert!(!body.contains("current-user"));
                                    state.read = true; status = state.query_status;
                                    if state.malformed { result = "<broken>".into(); }
                                    else {
                                        let href = if state.wrong_href { "/outside" } else { parts.uri.path() };
                                        let properties = if state.property_status == 200 { "<D:acl><D:ace><D:principal><D:href>/principals/synthetic</D:href></D:principal></D:ace></D:acl><D:owner/>" } else { "<D:acl/><D:owner/>" };
                                        result = format!("<D:multistatus xmlns:D='DAV:'><D:response><D:href>{href}</D:href><D:propstat><D:prop>{properties}</D:prop><D:status>HTTP/1.1 {} Property</D:status></D:propstat></D:response></D:multistatus>",state.property_status);
                                    }
                                } else if state.read && state.disappear_after { status=404; }
                                else {
                                    let changed = state.read && state.change_after;
                                    let etag = if changed { "changed" } else { "stable" };
                                    let kind = if parts.uri.path().trim_end_matches('/') == "/dav" { "<D:collection/>" } else { "" };
                                    result = format!("<D:multistatus xmlns:D='DAV:'><D:response><D:href>{}</D:href><D:propstat><D:prop><D:resourcetype>{kind}</D:resourcetype><D:getcontentlength>4</D:getcontentlength><D:getlastmodified>Wed, 21 Oct 2015 07:28:00 GMT</D:getlastmodified><D:getetag>\"{etag}\"</D:getetag></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>",parts.uri.path());
                                }
                                if parts.method == "PROPFIND" && parts.headers.get("depth").is_some_and(|v| v == "1") && parts.uri.path().trim_end_matches('/') == "/dav" {
                                    let child = result.replace("/dav", "/dav/file.txt").replace("<D:collection/>", "");
                                    let child = child.strip_prefix("<D:multistatus xmlns:D='DAV:'>").unwrap().strip_suffix("</D:multistatus>").unwrap();
                                    result.insert_str(result.len()-"</D:multistatus>".len(), child);
                                }
                                Ok::<_, io::Error>(Response::builder().status(status).header("dav","1").header("etag", "\"stable\"").header("server","doppeldecker-security-fixture").body(Full::new(hyper::body::Bytes::from(result))).unwrap())
                            }
                        });
                        let _ = http1::Builder::new().serve_connection(TokioIo::new(tls), service).await;
                    });
                }
            });
        });
        let certificate = discover_webdav_certificate(&origin).unwrap();
        Self {
            config: WebDavConnectionConfig {
                origin,
                username: "synthetic".into(),
                requested_root: "/dav".into(),
                certificate,
                certificate_policy: FtpsCertificatePolicy::PinnedCertificate,
                password_reference: KeychainSecretReference::new("security-fixture", "synthetic")
                    .unwrap(),
            },
            behavior,
            stop,
            thread: Some(thread),
        }
    }
    fn vfs(&self) -> WebDavVfs {
        WebDavVfs::connect(self.config.clone(), Arc::new(Secrets)).unwrap()
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.thread.take().unwrap().join().unwrap();
    }
}
#[test]
fn https_manifest_beobachtet_ohne_list_oder_stat_zu_erweitern() {
    let server = Server::new();
    let vfs = server.vfs();
    vfs.stat(Path::new("/file.txt")).unwrap();
    assert!(!server
        .behavior
        .lock()
        .unwrap()
        .calls
        .iter()
        .any(|(_, _, body)| body.contains("<D:acl/>")));
    let manifest = vfs.metadata_manifest(Path::new("/file.txt")).unwrap();
    let observed = manifest.webdav_security.unwrap();
    assert_eq!(observed.query_status, 207);
    assert_eq!(
        observed.acl,
        WebDavPropertyObservation::Unavailable { status: 404 }
    );
    assert_eq!(
        observed.owner,
        WebDavPropertyObservation::Unavailable { status: 404 }
    );
    let calls = &server.behavior.lock().unwrap().calls;
    assert!(calls
        .iter()
        .all(|(method, _, _)| method == "PROPFIND" || method == "OPTIONS"));
    assert_eq!(
        calls
            .iter()
            .filter(|(_, _, body)| body.contains("<D:acl/>"))
            .count(),
        1
    );
}
#[test]
fn https_query_absagen_werden_nur_mit_unveraenderter_ressource_beobachtet() {
    for status in [403, 404, 405, 501] {
        let server = Server::new();
        let vfs = server.vfs();
        server.behavior.lock().unwrap().query_status = status;
        let observed = vfs
            .metadata_manifest(Path::new("/file.txt"))
            .unwrap()
            .webdav_security
            .unwrap();
        assert_eq!(observed.query_status, status);
        assert_eq!(observed.acl, WebDavPropertyObservation::Missing);
        server.behavior.lock().unwrap().disappear_after = true;
        assert!(vfs.metadata_manifest(Path::new("/file.txt")).is_err());
    }
}
#[test]
fn https_opaque_200_und_leerer_owner_sind_beobachtungen() {
    let server = Server::new();
    let vfs = server.vfs();
    server.behavior.lock().unwrap().property_status = 200;
    let manifest = vfs.metadata_manifest(Path::new("/file.txt")).unwrap();
    let observed = manifest.webdav_security.unwrap();
    assert!(matches!(
        observed.acl,
        WebDavPropertyObservation::Observed { empty: false, .. }
    ));
    assert!(matches!(
        observed.owner,
        WebDavPropertyObservation::Observed { empty: true, .. }
    ));
    assert!(!serde_json::to_string(&observed)
        .unwrap()
        .contains("principals"));
}
#[test]
fn https_fehler_und_drift_werden_nie_leere_acl() {
    for case in 0..5 {
        let server = Server::new();
        let vfs = server.vfs();
        {
            let mut b = server.behavior.lock().unwrap();
            match case {
                0 => b.query_status = 401,
                1 => b.wrong_href = true,
                2 => b.malformed = true,
                3 => b.change_after = true,
                _ => b.disappear_after = true,
            }
        }
        assert!(
            vfs.metadata_manifest(Path::new("/file.txt")).is_err(),
            "case {case}"
        );
    }
}
#[test]
fn namespace_bleibt_beim_reconnect_stabil_und_unterscheidet_konten_und_roots() {
    let server = Server::new();
    let vfs = server.vfs();
    let first = vfs
        .metadata_manifest(Path::new("/file.txt"))
        .unwrap()
        .webdav_security
        .unwrap()
        .namespace;
    vfs.reconnect().unwrap();
    assert_eq!(
        first,
        vfs.metadata_manifest(Path::new("/file.txt"))
            .unwrap()
            .webdav_security
            .unwrap()
            .namespace
    );
    let mut other = server.config.clone();
    other.username = "another-synthetic".into();
    let other = WebDavVfs::connect(other, Arc::new(Secrets)).unwrap();
    assert_ne!(
        first,
        other
            .metadata_manifest(Path::new("/file.txt"))
            .unwrap()
            .webdav_security
            .unwrap()
            .namespace
    );
}

#[test]
fn vorbereiteter_inhaltssync_erhaelt_konkrete_dav_quellenauslassungen() {
    use std::os::unix::fs::PermissionsExt;
    let server = Server::new();
    let target = tempfile::tempdir().unwrap();
    let config = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let queue = OperationQueue::open(config.path().join("queue.json")).unwrap();
    let prepared = prepare_content_sync_for_queue(
        &queue,
        ProviderEndpoint::webdav(Arc::new(server.vfs()), Path::new("/")).unwrap(),
        ProviderEndpoint::local(target.path()).unwrap(),
        SyncOptions {
            compare: Compare::ContentHash,
            metadata: MetadataProfile::NativeBestEffort,
            ..SyncOptions::default()
        },
        &Cancel::new(),
    )
    .unwrap();
    for class in [sync::MetadataClass::Acl, sync::MetadataClass::Owner] {
        let issues = &prepared.preview().plan.metadata_report.issues;
        assert!(
            issues.iter().any(|issue| issue.class == class
                && issue.reason.starts_with("Quelle")
                && issue.reason.contains("Property-Status 404")),
            "{issues:?}"
        );
    }
    assert_eq!(std::fs::read_dir(target.path()).unwrap().count(), 0);
    assert!(server
        .behavior
        .lock()
        .unwrap()
        .calls
        .iter()
        .all(|(method, _, _)| matches!(method.as_str(), "GET" | "PROPFIND" | "OPTIONS")));
}
