//! Gemeinsamer künstlicher HTTPS-Server für gewöhnliche WebDAV-Namespaceaktionen.

use std::collections::VecDeque;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use doppeldecker_core::{
    discover_webdav_certificate, FtpsCertificatePolicy, KeychainSecretReference, ProviderEndpoint,
    SecretResolver, SecretString, WebDavConnectionConfig, WebDavVfs,
};
use http_body_util::{BodyExt, Full};
use hyper::{server::conn::http1, service::service_fn, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use rcgen::generate_simple_self_signed;
use rustls::pki_types::PrivatePkcs8KeyDer;
use tokio_rustls::TlsAcceptor;

type Body = Full<hyper::body::Bytes>;

pub(crate) struct TestSecrets;
impl SecretResolver for TestSecrets {
    fn resolve(&self, _: &KeychainSecretReference) -> io::Result<SecretString> {
        SecretString::new("wertlos".into())
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
// Die beiden Integrationstest-Binaries verwenden unterschiedliche Fehlerfälle.
#[allow(dead_code)]
pub(crate) enum Fault {
    #[default]
    None,
    NoCreatedEtag,
    LostAfterCreate,
    MissingReadback,
    WrongKind,
    WrongCreatedEtag,
    ErrorAfterCreate,
    ParentSwapAfterList,
    ParentSwapAtMkdir,
    ProfileRootSwapAtMkdir,
    MissingParentEtag,
    LateCollision,
    NonemptyCreated,
    MissingParentChild,
    LostAfterMove,
    ParentSwapAtMove,
    ProfileRootSwapAtMove,
    SourceSwapAtMove,
    LateMoveCollision,
    ErrorAfterMove,
    WrongReadbackEtag,
    LostAfterPut,
    ErrorAfterPut,
    MissingPutEtag,
    WeakPutEtag,
    WrongPutEtag,
    WrongGetBytes,
    WrongGetAfterMove,
    MissingFileEtag,
    WeakFileEtag,
    WeakParentEtag,
    MissingCreatedToken,
    PartialListing,
    ParentSwapAtPut,
    ProfileRootSwapAtPut,
    LostAfterDelete,
    DeleteLeavesFile,
}

#[derive(Default)]
pub(crate) struct State {
    pub(crate) depth_zero_delays: VecDeque<Duration>,
    pub(crate) allow_file_overwrite: bool,
    pub(crate) deny_collection_probes: bool,
    pub(crate) changing_collection_tokens: bool,
    pub(crate) mkdir_if_headers: Vec<String>,
    pub(crate) hold_after_mkdir: bool,
    pub(crate) mkdir_paused: bool,
    pub(crate) resume_mkdir: bool,
    pub(crate) fault: Fault,
    pub(crate) fired: bool,
    pub(crate) created: Option<PathBuf>,
    pub(crate) requests: Vec<(String, String)>,
    pub(crate) hold_parent_list: bool,
    pub(crate) parent_list_paused: bool,
    pub(crate) resume_parent_list: bool,
    pub(crate) move_if_headers: Vec<String>,
    pub(crate) put_if_headers: Vec<String>,
    pub(crate) delete_if_headers: Vec<String>,
    pub(crate) cancel_on_put: Option<doppeldecker_core::Cancel>,
    pub(crate) source_change_on_put: Option<PathBuf>,
    pub(crate) hold_upload_get: Option<bool>,
    pub(crate) upload_get_paused: bool,
    pub(crate) resume_upload_get: bool,
}

pub(crate) struct Server {
    #[allow(dead_code)] // Nur der unabhängige HTTP-Gegenlauf benötigt den Trustanker.
    pub(crate) trust_certificate: reqwest::Certificate,
    pub(crate) root: tempfile::TempDir,
    pub(crate) config: WebDavConnectionConfig,
    pub(crate) state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    pub(crate) fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("pane")).unwrap();
        let certified = generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let trust_certificate = reqwest::Certificate::from_der(certified.cert.der()).unwrap();
        let key = PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der());
        let tls = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![certified.cert.der().clone()], key.into())
        .unwrap();
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let state = Arc::new(Mutex::new(State::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let server_state = state.clone();
        let server_stop = stop.clone();
        let server_root = root.path().to_path_buf();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                let acceptor = TlsAcceptor::from(Arc::new(tls));
                while !server_stop.load(Ordering::SeqCst) {
                    let Ok(Ok((stream, _))) =
                        tokio::time::timeout(Duration::from_millis(20), listener.accept()).await
                    else {
                        continue;
                    };
                    let acceptor = acceptor.clone();
                    let root = server_root.clone();
                    let state = server_state.clone();
                    tokio::spawn(async move {
                        let Ok(stream) = acceptor.accept(stream).await else {
                            return;
                        };
                        let service = service_fn(move |request: Request<hyper::body::Incoming>| {
                            let root = root.clone();
                            let state = state.clone();
                            async move {
                                let delay = if request.method() == "PROPFIND"
                                    && request.headers().get("depth").is_some_and(|v| v == "0")
                                {
                                    state.lock().unwrap().depth_zero_delays.pop_front()
                                } else {
                                    None
                                };
                                if let Some(delay) = delay {
                                    tokio::time::sleep(delay).await;
                                }
                                let pause = {
                                    let mut state = state.lock().unwrap();
                                    if state.hold_parent_list
                                        && request.method() == "PROPFIND"
                                        && request.uri().path() == "/pane"
                                        && request.headers().get("depth").is_some_and(|v| v == "1")
                                    {
                                        state.hold_parent_list = false;
                                        state.parent_list_paused = true;
                                        true
                                    } else {
                                        false
                                    }
                                };
                                if pause {
                                    let _ = tokio::time::timeout(Duration::from_secs(5), async {
                                        while !state.lock().unwrap().resume_parent_list {
                                            tokio::time::sleep(Duration::from_millis(10)).await;
                                        }
                                    })
                                    .await;
                                }
                                let hold_mkdir = {
                                    let mut current = state.lock().unwrap();
                                    if current.hold_after_mkdir
                                        && current.created.as_ref().is_some_and(|p| p.is_dir())
                                        && request.method() == "PROPFIND"
                                    {
                                        current.hold_after_mkdir = false;
                                        current.mkdir_paused = true;
                                        true
                                    } else {
                                        false
                                    }
                                };
                                if hold_mkdir {
                                    let _ = tokio::time::timeout(Duration::from_secs(15), async {
                                        while !state.lock().unwrap().resume_mkdir {
                                            tokio::time::sleep(Duration::from_millis(10)).await;
                                        }
                                    })
                                    .await;
                                }
                                let hold_get = {
                                    let mut current = state.lock().unwrap();
                                    let hold = request.method() == "GET"
                                        && current.hold_upload_get.is_some_and(|after_move| {
                                            if after_move {
                                                request.uri().path().ends_with("/Datei.txt")
                                            } else {
                                                request.uri().path().contains("fileop-tmp")
                                            }
                                        });
                                    if hold {
                                        current.hold_upload_get = None;
                                        current.upload_get_paused = true;
                                    }
                                    hold
                                };
                                if hold_get {
                                    let _ = tokio::time::timeout(Duration::from_secs(15), async {
                                        while !state.lock().unwrap().resume_upload_get {
                                            tokio::time::sleep(Duration::from_millis(10)).await;
                                        }
                                    })
                                    .await;
                                }
                                let (parts, body) = request.into_parts();
                                let bytes = if parts.method == "PUT" {
                                    body.collect()
                                        .await
                                        .map_err(io::Error::other)?
                                        .to_bytes()
                                        .to_vec()
                                } else {
                                    Vec::new()
                                };
                                respond(Request::from_parts(parts, bytes), &root, &state)
                            }
                        });
                        let _ = http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), service)
                            .await;
                    });
                }
            });
        });
        let origin = format!("https://localhost:{port}");
        let certificate = discover_webdav_certificate(&origin).unwrap();
        Self {
            trust_certificate,
            root,
            config: WebDavConnectionConfig {
                origin,
                username: "tester".into(),
                requested_root: "/".into(),
                certificate,
                certificate_policy: FtpsCertificatePolicy::PinnedCertificate,
                password_reference: KeychainSecretReference::new(
                    "doppeldecker-webdav-folder-test",
                    "tester",
                )
                .unwrap(),
            },
            state,
            stop,
            thread: Some(thread),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn endpoint(&self) -> ProviderEndpoint {
        ProviderEndpoint::webdav(
            Arc::new(WebDavVfs::connect(self.config.clone(), Arc::new(TestSecrets)).unwrap()),
            Path::new("/pane"),
        )
        .unwrap()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.thread.take().unwrap().join().unwrap();
    }
}

fn response(status: StatusCode, bytes: impl Into<hyper::body::Bytes>) -> Response<Body> {
    Response::builder()
        .status(status)
        .body(Full::new(bytes.into()))
        .unwrap()
}

fn local_path(root: &Path, raw: &str) -> Option<PathBuf> {
    let decoded = percent_encoding::percent_decode_str(raw)
        .decode_utf8()
        .ok()?;
    let mut path = root.to_path_buf();
    for part in decoded.split('/').filter(|part| !part.is_empty()) {
        if matches!(part, "." | "..") || part.contains(['\\', '\0']) {
            return None;
        }
        path.push(part);
    }
    Some(path)
}

fn etag(path: &Path, state: &Mutex<State>) -> String {
    let meta = std::fs::symlink_metadata(path).unwrap();
    if meta.is_dir() {
        if state.lock().unwrap().changing_collection_tokens {
            return format!(
                "\"folder-{}-{}-{}-{}\"",
                meta.dev(),
                meta.ino(),
                meta.mtime(),
                meta.mtime_nsec()
            );
        }
        format!("\"folder-{}-{}\"", meta.dev(), meta.ino())
    } else {
        format!(
            "\"file-{}\"",
            blake3::hash(&std::fs::read(path).unwrap()).to_hex()
        )
    }
}

fn resource(path: &Path, href: &str, state: &Mutex<State>) -> String {
    let mut url = url::Url::parse("https://webdav-test.invalid/").unwrap();
    {
        let mut parts = url.path_segments_mut().unwrap();
        parts.clear();
        for part in href.split('/').filter(|part| !part.is_empty()) {
            parts.push(part);
        }
    }
    let kind = if path.is_dir() {
        "<D:resourcetype><D:collection/></D:resourcetype>".to_owned()
    } else {
        format!("<D:resourcetype/><D:getcontentlength>{}</D:getcontentlength><D:getlastmodified>Sun, 18 Oct 2026 00:00:00 GMT</D:getlastmodified>", path.metadata().unwrap().len())
    };
    format!("<D:response><D:href>{}</D:href><D:propstat><D:prop>{kind}<D:getetag>{}</D:getetag></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>", url.path(), etag(path, state))
}

fn respond(
    request: Request<Vec<u8>>,
    root: &Path,
    state: &Mutex<State>,
) -> Result<Response<Body>, io::Error> {
    let fault = state.lock().unwrap().fault;
    let method = request.method().as_str();
    let raw = request.uri().path();
    state
        .lock()
        .unwrap()
        .requests
        .push((method.into(), raw.into()));
    if request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        != Some("Basic dGVzdGVyOndlcnRsb3M=")
    {
        return Ok(response(StatusCode::UNAUTHORIZED, ""));
    }
    let Some(local) = local_path(root, raw) else {
        return Ok(response(StatusCode::BAD_REQUEST, ""));
    };
    if state.lock().unwrap().deny_collection_probes
        && ((method == "MKCOL" && raw.contains("probe")) || (method == "DELETE" && local.is_dir()))
    {
        return Ok(response(StatusCode::FORBIDDEN, ""));
    }
    if method == "MKCOL" {
        state.lock().unwrap().mkdir_if_headers.push(
            request
                .headers()
                .get("if")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .into(),
        );
    }
    if method == "PUT" {
        state.lock().unwrap().put_if_headers.push(
            request
                .headers()
                .get("if")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .into(),
        );
        if fault == Fault::ParentSwapAtPut {
            swap_parent(root);
        }
        if fault == Fault::ProfileRootSwapAtPut {
            std::fs::rename(root.join("profil"), root.join("profil-before-swap")).unwrap();
            std::fs::create_dir(root.join("profil")).unwrap();
            std::fs::rename(
                root.join("profil-before-swap/pane"),
                root.join("profil/pane"),
            )
            .unwrap();
        }
    }
    if method == "DELETE" {
        state.lock().unwrap().delete_if_headers.push(
            request
                .headers()
                .get("if")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .into(),
        );
    }
    if method == "MKCOL"
        && matches!(
            fault,
            Fault::LateCollision | Fault::ParentSwapAtMkdir | Fault::ProfileRootSwapAtMkdir
        )
        && !state.lock().unwrap().fired
    {
        state.lock().unwrap().fired = true;
        if fault == Fault::LateCollision {
            std::fs::write(&local, b"Fremd").unwrap();
        } else if fault == Fault::ProfileRootSwapAtMkdir {
            std::fs::rename(root.join("profil"), root.join("profil-before-swap")).unwrap();
            std::fs::create_dir(root.join("profil")).unwrap();
            std::fs::rename(
                root.join("profil-before-swap/pane"),
                root.join("profil/pane"),
            )
            .unwrap();
        } else {
            swap_parent(root);
        }
    }
    if method == "MOVE" {
        state.lock().unwrap().move_if_headers.push(
            request
                .headers()
                .get("if")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_owned(),
        );
        if !state.lock().unwrap().fired {
            state.lock().unwrap().fired = true;
            match fault {
                Fault::ParentSwapAtMove => swap_parent(root),
                Fault::ProfileRootSwapAtMove => {
                    std::fs::rename(root.join("profil"), root.join("profil-before-swap")).unwrap();
                    std::fs::create_dir(root.join("profil")).unwrap();
                    std::fs::rename(
                        root.join("profil-before-swap/pane"),
                        root.join("profil/pane"),
                    )
                    .unwrap();
                }
                Fault::SourceSwapAtMove => {
                    let same_size = vec![b'X'; local.metadata().unwrap().len() as usize];
                    std::fs::write(&local, same_size).unwrap();
                }
                Fault::LateMoveCollision => {
                    let url = url::Url::parse(
                        request
                            .headers()
                            .get("destination")
                            .unwrap()
                            .to_str()
                            .unwrap(),
                    )
                    .unwrap();
                    std::fs::write(local_path(root, url.path()).unwrap(), b"Fremdes Ziel").unwrap();
                }
                _ => {}
            }
        }
    }
    if let Some(value) = request.headers().get("if").and_then(|v| v.to_str().ok()) {
        let mut rest = value;
        let mut matches = Vec::new();
        while let Some(start) = rest.find('<') {
            rest = &rest[start + 1..];
            let end = rest.find('>').unwrap();
            let url = url::Url::parse(&rest[..end]).unwrap();
            rest = &rest[end + 1..];
            let first = rest.find('[').unwrap() + 1;
            let last = rest[first..].find(']').unwrap() + first;
            let path = local_path(root, url.path()).unwrap();
            matches.push(path.exists() && etag(&path, state) == rest[first..last]);
            rest = &rest[last + 1..];
        }
        // Unterstützt die vom Produkt erzeugte Teilsyntax mit einer ETag je
        // Tagged-list. RFC 4918 §10.4.3: ODER zwischen diesen Produktionen.
        let accepted = matches.iter().any(|matched| *matched);
        if !accepted {
            return Ok(response(StatusCode::PRECONDITION_FAILED, ""));
        }
    }
    if let Some(value) = request
        .headers()
        .get("if-match")
        .and_then(|v| v.to_str().ok())
    {
        if !local.exists() || etag(&local, state) != value {
            return Ok(response(StatusCode::PRECONDITION_FAILED, ""));
        }
    }
    if local.exists()
        && request
            .headers()
            .get("if-none-match")
            .is_some_and(|v| v == "*")
    {
        return Ok(response(StatusCode::PRECONDITION_FAILED, ""));
    }
    let result = match method {
        "OPTIONS" => Response::builder()
            .status(StatusCode::NO_CONTENT)
            .header("dav", "1")
            .header("server", "doppeldecker-folder-test")
            .body(Full::new(hyper::body::Bytes::new()))
            .unwrap(),
        "PROPFIND" if local.exists() => {
            let decoded = percent_encoding::percent_decode_str(raw)
                .decode_utf8()
                .unwrap();
            if fault == Fault::MissingReadback
                && state.lock().unwrap().created.as_ref() == Some(&local)
            {
                return Ok(response(StatusCode::NOT_FOUND, ""));
            }
            let mut resources = resource(&local, &decoded, state);
            if fault == Fault::MissingCreatedToken
                && local.is_dir()
                && state.lock().unwrap().created.as_ref() == Some(&local)
            {
                resources = resources.replace(
                    &format!("<D:getetag>{}</D:getetag>", etag(&local, state)),
                    "",
                );
            }
            if fault == Fault::MissingParentEtag && local == root.join("pane") {
                resources = resources.replace(
                    &format!("<D:getetag>{}</D:getetag>", etag(&local, state)),
                    "",
                );
            }
            if fault == Fault::WrongKind && state.lock().unwrap().created.as_ref() == Some(&local) {
                resources = resources.replace("<D:resourcetype><D:collection/></D:resourcetype>", "<D:resourcetype/><D:getcontentlength>0</D:getcontentlength><D:getlastmodified>Sun, 18 Oct 2026 00:00:00 GMT</D:getlastmodified>");
            }
            if fault == Fault::WrongReadbackEtag
                && state.lock().unwrap().created.as_ref() == Some(&local)
            {
                resources = resources.replace(&etag(&local, state), "\"falscher-readback\"");
            }
            if fault == Fault::WeakParentEtag && local.is_dir() {
                resources =
                    resources.replace(&etag(&local, state), &format!("W/{}", etag(&local, state)));
            }
            if local.is_file() && matches!(fault, Fault::MissingFileEtag | Fault::WeakFileEtag) {
                resources = if fault == Fault::MissingFileEtag {
                    resources.replace(
                        &format!("<D:getetag>{}</D:getetag>", etag(&local, state)),
                        "",
                    )
                } else {
                    resources.replace(&etag(&local, state), &format!("W/{}", etag(&local, state)))
                };
            }
            if fault == Fault::PartialListing
                && request.headers().get("depth").is_some_and(|v| v == "1")
            {
                resources = resources.replace("HTTP/1.1 200 OK", "HTTP/1.1 403 Forbidden");
            }
            if request.headers().get("depth").is_some_and(|v| v == "1") && local.is_dir() {
                let mut children = std::fs::read_dir(&local)
                    .unwrap()
                    .map(Result::unwrap)
                    .collect::<Vec<_>>();
                children.sort_by_key(|child| child.file_name());
                for child in children {
                    if fault == Fault::MissingParentChild
                        && state.lock().unwrap().created.as_ref() == Some(&child.path())
                    {
                        continue;
                    }
                    let mut child_resource = resource(
                        &child.path(),
                        &format!(
                            "{}/{}",
                            decoded.trim_end_matches('/'),
                            child.file_name().to_str().unwrap()
                        ),
                        state,
                    );
                    if state.lock().unwrap().created.as_ref() == Some(&child.path()) {
                        if fault == Fault::WrongReadbackEtag {
                            child_resource = child_resource
                                .replace(&etag(&child.path(), state), "\"falscher-readback\"");
                        }
                        if fault == Fault::WrongKind {
                            child_resource = child_resource.replace("<D:resourcetype><D:collection/></D:resourcetype>", "<D:resourcetype/><D:getcontentlength>0</D:getcontentlength><D:getlastmodified>Sun, 18 Oct 2026 00:00:00 GMT</D:getlastmodified>");
                        }
                    }
                    resources.push_str(&child_resource);
                }
            }
            if fault == Fault::ParentSwapAfterList
                && local == root.join("pane")
                && request
                    .headers()
                    .get("depth")
                    .is_some_and(|value| value == "1")
                && !state.lock().unwrap().fired
            {
                state.lock().unwrap().fired = true;
                swap_parent(root);
            }
            response(
                StatusCode::MULTI_STATUS,
                format!("<D:multistatus xmlns:D=\"DAV:\">{resources}</D:multistatus>"),
            )
        }
        "PROPFIND" => response(StatusCode::NOT_FOUND, ""),
        "MKCOL" if local.exists() => response(StatusCode::METHOD_NOT_ALLOWED, ""),
        "MKCOL" => {
            std::fs::create_dir(&local).unwrap();
            state.lock().unwrap().created = Some(local.clone());
            if fault == Fault::NonemptyCreated {
                std::fs::write(local.join("Fremd.txt"), b"Erhalten").unwrap();
            }
            if fault == Fault::LostAfterCreate {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionReset,
                    "synthetischer Antwortverlust nach MKCOL",
                ));
            }
            if fault == Fault::ErrorAfterCreate {
                return Ok(response(StatusCode::INTERNAL_SERVER_ERROR, ""));
            }
            let result = Response::builder().status(StatusCode::CREATED);
            let result = match fault {
                Fault::NoCreatedEtag | Fault::MissingCreatedToken => result,
                Fault::WrongCreatedEtag => result.header("etag", "\"andere-collection\""),
                _ => result.header("etag", etag(&local, state)),
            };
            result.body(Full::new(hyper::body::Bytes::new())).unwrap()
        }
        "PUT" => {
            if request
                .headers()
                .get("if-none-match")
                .is_none_or(|v| v != "*")
                || request.headers().get("if").is_none()
            {
                return Ok(response(StatusCode::PRECONDITION_FAILED, ""));
            }
            let mut file = match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&local)
            {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    return Ok(response(StatusCode::PRECONDITION_FAILED, ""))
                }
                Err(error) => return Err(error),
            };
            std::io::Write::write_all(&mut file, request.body())?;
            if let Some(cancel) = state.lock().unwrap().cancel_on_put.take() {
                cancel.cancel();
            }
            if let Some(source) = state.lock().unwrap().source_change_on_put.take() {
                let time =
                    filetime::FileTime::from_last_modification_time(&source.metadata().unwrap());
                let length = source.metadata().unwrap().len() as usize;
                std::fs::write(&source, vec![b'X'; length]).unwrap();
                filetime::set_file_mtime(&source, time).unwrap();
            }
            if fault == Fault::LostAfterPut {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionReset,
                    "synthetischer PUT-Antwortverlust",
                ));
            }
            if fault == Fault::ErrorAfterPut {
                return Ok(response(StatusCode::INTERNAL_SERVER_ERROR, ""));
            }
            let builder = Response::builder().status(StatusCode::CREATED);
            let builder = match fault {
                Fault::MissingPutEtag => builder,
                Fault::WeakPutEtag => builder.header("etag", format!("W/{}", etag(&local, state))),
                Fault::WrongPutEtag => builder.header("etag", "\"falsch\""),
                _ => builder.header("etag", etag(&local, state)),
            };
            builder.body(Full::new(hyper::body::Bytes::new())).unwrap()
        }
        "GET"
            if local.is_file()
                && (fault == Fault::WrongGetBytes
                    || (fault == Fault::WrongGetAfterMove
                        && state.lock().unwrap().created.as_ref() == Some(&local))) =>
        {
            Response::builder()
                .status(StatusCode::OK)
                .header("etag", etag(&local, state))
                .body(Full::new(
                    vec![b'X'; local.metadata().unwrap().len() as usize].into(),
                ))
                .unwrap()
        }
        "GET" if local.is_file() => Response::builder()
            .status(StatusCode::OK)
            .header("etag", etag(&local, state))
            .body(Full::new(std::fs::read(&local).unwrap().into()))
            .unwrap(),
        "MOVE" => {
            let replace = request.headers().get("overwrite").is_some_and(|v| v == "T")
                && state.lock().unwrap().allow_file_overwrite;
            if (request.headers().get("overwrite").is_none_or(|v| v != "F") && !replace)
                || request.headers().get("if-match").is_none()
                || request.headers().get("if").is_none()
            {
                return Ok(response(StatusCode::PRECONDITION_FAILED, ""));
            }
            let Some(destination) = request
                .headers()
                .get("destination")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| url::Url::parse(v).ok())
            else {
                return Ok(response(StatusCode::BAD_REQUEST, ""));
            };
            let origin = format!(
                "https://{}",
                request.headers().get("host").unwrap().to_str().unwrap()
            );
            if destination.origin().ascii_serialization() != origin {
                return Ok(response(StatusCode::BAD_REQUEST, ""));
            }
            let Some(target) = local_path(root, destination.path()) else {
                return Ok(response(StatusCode::BAD_REQUEST, ""));
            };
            let replaced = target.exists();
            if replaced && !(replace && target.is_file() && local.is_file()) {
                return Ok(response(StatusCode::PRECONDITION_FAILED, ""));
            }
            if !local.exists() {
                return Ok(response(StatusCode::NOT_FOUND, ""));
            }
            if !target.parent().unwrap().is_dir() {
                return Ok(response(StatusCode::CONFLICT, ""));
            }
            std::fs::rename(&local, &target).unwrap();
            state.lock().unwrap().created = Some(target);
            if fault == Fault::LostAfterMove {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionReset,
                    "synthetischer Antwortverlust nach MOVE",
                ));
            }
            if fault == Fault::ErrorAfterMove {
                return Ok(response(StatusCode::INTERNAL_SERVER_ERROR, ""));
            }
            response(
                if replaced {
                    StatusCode::NO_CONTENT
                } else {
                    StatusCode::CREATED
                },
                "",
            )
        }
        // RFC 4918 verlangt für Collection-DELETE die rekursive Wirkung. Ein
        // normaler MKCOL-Aufruf darf deshalb keine solche künstliche Probe brauchen.
        "DELETE"
            if request
                .headers()
                .get("depth")
                .is_some_and(|value| value != "infinity") =>
        {
            response(StatusCode::BAD_REQUEST, "")
        }
        "DELETE" if local != root && local.is_dir() => {
            std::fs::remove_dir_all(&local).unwrap();
            response(StatusCode::NO_CONTENT, "")
        }
        "DELETE"
            if local.is_file()
                && request.headers().get("if-match").is_some()
                && request.headers().get("if").is_some()
                && request.headers().get("depth").is_none() =>
        {
            if fault != Fault::DeleteLeavesFile {
                std::fs::remove_file(&local).unwrap();
            }
            if fault == Fault::LostAfterDelete {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionReset,
                    "synthetischer DELETE-Antwortverlust",
                ));
            }
            response(StatusCode::NO_CONTENT, "")
        }
        "DELETE" => response(StatusCode::FORBIDDEN, ""),
        _ => response(StatusCode::METHOD_NOT_ALLOWED, ""),
    };
    Ok(result)
}

fn swap_parent(root: &Path) {
    std::fs::rename(root.join("pane"), root.join("pane-before-swap")).unwrap();
    std::fs::create_dir(root.join("pane")).unwrap();
    std::fs::write(root.join("pane/Fremd.txt"), b"Fremder Elternordner").unwrap();
}
