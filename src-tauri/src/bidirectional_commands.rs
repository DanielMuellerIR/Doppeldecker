//! Dünne Tauri-Schicht für den lokalen bidirektionalen Sync.
//!
//! Planung, Permit, Journal, Resume und Rollback bleiben vollständig im GUI-freien
//! Kern. Diese Datei hält nur nicht serialisierbare Vorbereitungen im RAM, leitet
//! Events weiter und legt den app-eigenen Baseline-Pfad fest.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use doppeldecker_core::bidirectional::{bind_bidirectional_endpoint, RemoteEndpointBinding};
use doppeldecker_core::{
    bind_bidirectional_endpoint_compatibility, prepare_bidirectional_queue_entry,
    prepare_local_bidirectional_rollback, prepare_provider_bidirectional_rollback,
    prepare_resume_local_bidirectional, prepare_resume_provider_bidirectional,
    BidirectionalQueueEndpoint, BidirectionalQueuePreparation, ConflictResolution, EndpointBinding,
    EndpointSide, LocalBidirectionalPreview, LocalBidirectionalRollbackPreview, OperationQueue,
    PairId, PreparedLocalBidirectionalRun, Progress, ProviderEndpoint, QueueEntry,
    QueueExecutionResult, QueueOperation, QueueSnapshot, QueueState,
};
use tauri::{Manager, State, WebviewWindow};

use crate::queue_runtime::{PendingBidirectionalQueueRun, QueueTerminalEvent};
use crate::{
    app_config_dir, emit_queue_snapshot, finish_operation, operation_queue, reserve_operation,
    start_queue_worker, take_pending_for_operation, with_queue_extension, AppError, AppState,
    MutexExt,
};

pub(crate) struct PendingBidirectionalRun {
    prepared: PreparedLocalBidirectionalRun,
    operation: Option<QueueOperation>,
    pub(crate) profile_bindings: Vec<doppeldecker_core::RemoteProfileBinding>,
    recovery_operation_id: Option<String>,
    recovery_preparation_id: Option<String>,
    /// Nach bewiesenem Queuecommit bleibt die ID bis zur sicheren Entfernung des
    /// mutationslosen Laufjournals gebunden. Ein Retry reiht dadurch nie doppelt ein.
    enqueued_operation_id: Option<String>,
}

impl PendingBidirectionalRun {
    pub(crate) fn queue_transition_active(&self) -> bool {
        self.enqueued_operation_id.is_some()
    }
}

pub(crate) struct PendingBidirectionalRollback {
    prepared: doppeldecker_core::PreparedLocalBidirectionalRollback,
    recovery_operation_id: String,
    recovery_preparation_id: String,
    pub(crate) profile_bindings: Vec<doppeldecker_core::RemoteProfileBinding>,
}

struct BidirectionalRunQueueOwner {
    queue: std::sync::Arc<OperationQueue>,
    operation_id: String,
    direct: bool,
}

enum BidirectionalRunQueueIntent {
    Direct(QueueOperation),
    Recovery(String),
}

#[derive(Clone, Copy, Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BidirectionalRecoveryKind {
    Resume,
    Rollback,
}

#[derive(serde::Serialize)]
pub(crate) struct BidirectionalRecoveryPreview<T> {
    #[serde(flatten)]
    preview: T,
    recovery_preparation_id: String,
    recovery_kind: BidirectionalRecoveryKind,
}

#[derive(serde::Serialize)]
pub(crate) struct BidirectionalRecoveryDiscardStatus {
    absent: bool,
}

fn random_recovery_preparation_id() -> Result<String, AppError> {
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random).map_err(|_| {
        AppError::new(
            "app",
            "Die Identität der Recovery-Vorbereitung konnte nicht erzeugt werden",
        )
    })?;
    Ok(random.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Liefert `true` nur für exakt dieselbe noch lebende Vorbereitung. `None`
/// bestätigt dagegen, dass diese Recovery-Art bereits abwesend ist. Eine andere
/// Identität darf ein verspäteter Frontend-Retry niemals verwerfen.
fn recovery_discard_decision(
    expected_operation_id: &str,
    expected_preparation_id: &str,
    current: Option<(&str, &str)>,
) -> Result<bool, AppError> {
    match current {
        None => Ok(false),
        Some((operation_id, preparation_id))
            if operation_id == expected_operation_id
                && preparation_id == expected_preparation_id =>
        {
            Ok(true)
        }
        Some(_) => Err(AppError::new(
            "conflict",
            "Eine andere bidirektionale Recovery-Vorbereitung ist aktiv",
        )),
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PrepareBidirectionalRequest {
    pair_id: PairId,
    pair_name: String,
    left: BidirectionalEndpointRequest,
    right: BidirectionalEndpointRequest,
    resolutions: Vec<ConflictResolution>,
    #[serde(default)]
    expected_unresolved_digest: Option<[u8; 32]>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(
    tag = "provider",
    rename_all = "lowercase",
    rename_all_fields = "camelCase"
)]
pub(crate) enum BidirectionalEndpointRequest {
    Local {
        root: String,
    },
    Sftp {
        profile_id: String,
        #[serde(default)]
        profile_revision: Option<u64>,
        root: String,
    },
    Ftps {
        profile_id: String,
        #[serde(default)]
        profile_revision: Option<u64>,
        root: String,
    },
    Webdav {
        profile_id: String,
        #[serde(default)]
        profile_revision: Option<u64>,
        root: String,
    },
    Smb {
        profile_id: String,
        #[serde(default)]
        profile_revision: Option<u64>,
        root: String,
    },
}

#[derive(Default)]
pub(crate) struct BidirectionalPairRegistry {
    path: Option<PathBuf>,
    entries: BTreeMap<String, StoredBidirectionalPair>,
}

#[derive(Clone, serde::Deserialize, serde::Serialize)]
struct StoredBidirectionalPair {
    pair_id: PairId,
    left: BidirectionalEndpointRequest,
    right: BidirectionalEndpointRequest,
    /// Version-1-Registries besaßen nur Pfadtexte. `None` wird ausschließlich
    /// lesend als Altstand akzeptiert und bei der nächsten Registrierung durch
    /// eine frisch vom Backend gelieferte Bindung ersetzt.
    #[serde(default)]
    left_binding: Option<EndpointBinding>,
    #[serde(default)]
    right_binding: Option<EndpointBinding>,
}

#[derive(Clone)]
struct BoundRegistryEndpoint {
    endpoint: BidirectionalEndpointRequest,
    binding: EndpointBinding,
    compatible_bindings: Vec<EndpointBinding>,
}

// Lese- und Schreibgrenze müssen gleich bleiben, damit jeder neue Bestand wieder
// einlesbar ist. Mitgezählt wird auch der abschließende Zeilenumbruch.
const MAX_PAIR_REGISTRY_BYTES: u64 = 16 * 1024 * 1024;

#[derive(serde::Deserialize)]
struct StoredBidirectionalPairRegistry {
    version: u32,
    entries: BTreeMap<String, StoredBidirectionalPair>,
}

#[derive(serde::Serialize)]
struct StoredBidirectionalPairRegistryRef<'a> {
    version: u32,
    entries: &'a BTreeMap<String, StoredBidirectionalPair>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RegisteredBidirectionalPair {
    pair_id: PairId,
    pair_name: String,
    left: BidirectionalEndpointRequest,
    right: BidirectionalEndpointRequest,
    /// Rolle der beim Command sichtbar linken Pane innerhalb der kanonischen
    /// Paarordnung. Alias-Pfade müssen im Frontend nicht erneut verglichen werden.
    visible_left_role: EndpointSide,
}

fn normalized_registry_root(root: &str) -> Result<String, AppError> {
    if !root.starts_with('/') || root.contains('\0') || root.chars().any(char::is_control) {
        return Err(AppError::new(
            "invalid_input",
            "Bidirektionale Paarregistrierung braucht einen absoluten, eindeutig kodierbaren Root",
        ));
    }
    let mut components = Vec::new();
    for component in root.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                return Err(AppError::new(
                    "invalid_input",
                    "Bidirektionale Paarregistrierung erlaubt keinen Elternpfad",
                ));
            }
            value => components.push(value),
        }
    }
    Ok(if components.is_empty() {
        "/".to_owned()
    } else {
        format!("/{}", components.join("/"))
    })
}

fn normalize_registry_endpoint(
    mut endpoint: BidirectionalEndpointRequest,
) -> Result<(String, BidirectionalEndpointRequest), AppError> {
    let root = normalized_registry_root(endpoint.root())?;
    let (kind, profile_id) = match &mut endpoint {
        BidirectionalEndpointRequest::Local { root: current } => {
            *current = root;
            ("local", String::new())
        }
        BidirectionalEndpointRequest::Sftp {
            profile_id,
            root: current,
            ..
        } => {
            if profile_id.is_empty() || profile_id.chars().any(char::is_control) {
                return Err(AppError::new(
                    "invalid_input",
                    "SFTP-Paar braucht eine gültige Profil-ID",
                ));
            }
            *current = root;
            ("sftp", profile_id.clone())
        }
        BidirectionalEndpointRequest::Ftps {
            profile_id,
            root: current,
            ..
        } => {
            if profile_id.is_empty() || profile_id.chars().any(char::is_control) {
                return Err(AppError::new(
                    "invalid_input",
                    "FTPS-Paar braucht eine gültige Profil-ID",
                ));
            }
            *current = root;
            ("ftps", profile_id.clone())
        }
        BidirectionalEndpointRequest::Webdav {
            profile_id,
            root: current,
            ..
        } => {
            if profile_id.is_empty() || profile_id.chars().any(char::is_control) {
                return Err(AppError::new(
                    "invalid_input",
                    "WebDAV-Paar braucht eine gültige Profil-ID",
                ));
            }
            *current = root;
            ("webdav", profile_id.clone())
        }
        BidirectionalEndpointRequest::Smb {
            profile_id,
            root: current,
            ..
        } => {
            if profile_id.is_empty() || profile_id.chars().any(char::is_control) {
                return Err(AppError::new(
                    "invalid_input",
                    "SMB-Paar braucht eine gültige Profil-ID",
                ));
            }
            *current = root;
            ("smb", profile_id.clone())
        }
    };
    Ok((
        format!("{kind}\u{0}{profile_id}\u{0}{}", endpoint.root()),
        endpoint,
    ))
}

fn registry_binding_key(binding: &EndpointBinding) -> Result<String, AppError> {
    let bytes = match binding {
        // `canonical_root` ist die sichere Ausführungsdarstellung, aber nicht die
        // physische Identität: Symlink-, Case- und Bind-Mount-Aliase desselben
        // Ordners müssen denselben Paarlock teilen. Device/Inode plus
        // Dateisystemnamespace liefern genau diese stabile lokale Gleichheit.
        EndpointBinding::Local(identity) => serde_json::to_vec(&(
            "local-v1",
            identity.device,
            identity.inode,
            &identity.fsid,
            identity.volume_id,
            &identity.fs_type,
        )),
        // Remote-Bindungen enthalten den kanonischen Providerroot, das Konto und
        // die kryptografisch beziehungsweise protokollseitig gebundene Server-ID.
        other => serde_json::to_vec(&("remote-v1", other)),
    }
    .map_err(|error| AppError::new("invalid_data", error.to_string()))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[derive(serde::Serialize)]
#[serde(tag = "provider", content = "identity", rename_all = "snake_case")]
enum LegacyRegistryBindingRef<'a> {
    Sftp(LegacyRegistryRemoteBindingRef<'a>),
    Ftps(LegacyRegistryRemoteBindingRef<'a>),
    WebDav(LegacyRegistryRemoteBindingRef<'a>),
    Smb(LegacyRegistryRemoteBindingRef<'a>),
}

#[derive(Clone, Copy, serde::Serialize)]
struct LegacyRegistryRemoteBindingRef<'a> {
    canonical_root: &'a str,
    account_digest: [u8; 32],
    public_identity_digest: [u8; 32],
}

fn legacy_registry_binding_key(binding: &EndpointBinding) -> Result<Option<String>, AppError> {
    fn remote(value: &RemoteEndpointBinding) -> LegacyRegistryRemoteBindingRef<'_> {
        LegacyRegistryRemoteBindingRef {
            canonical_root: &value.canonical_root,
            account_digest: value.account_digest,
            public_identity_digest: value.public_identity_digest,
        }
    }
    let legacy = match binding {
        EndpointBinding::Sftp(value) if value.schema_version == 1 => {
            Some(LegacyRegistryBindingRef::Sftp(remote(value)))
        }
        EndpointBinding::Ftps(value) if value.schema_version == 1 => {
            Some(LegacyRegistryBindingRef::Ftps(remote(value)))
        }
        EndpointBinding::WebDav(value) if value.schema_version == 1 => {
            Some(LegacyRegistryBindingRef::WebDav(remote(value)))
        }
        EndpointBinding::Smb(value) if value.schema_version == 1 => {
            Some(LegacyRegistryBindingRef::Smb(remote(value)))
        }
        _ => None,
    };
    let Some(legacy) = legacy else {
        return Ok(None);
    };
    let bytes = serde_json::to_vec(&("remote-v1", legacy))
        .map_err(|error| AppError::new("invalid_data", error.to_string()))?;
    Ok(Some(
        bytes.iter().map(|byte| format!("{byte:02x}")).collect(),
    ))
}

fn accepted_registry_binding_keys(binding: &EndpointBinding) -> Result<Vec<String>, AppError> {
    let mut keys = vec![registry_binding_key(binding)?];
    if let Some(legacy) = legacy_registry_binding_key(binding)? {
        if !keys.contains(&legacy) {
            keys.push(legacy);
        }
    }
    Ok(keys)
}

fn registry_pair_key(
    first_binding: &EndpointBinding,
    second_binding: &EndpointBinding,
) -> Result<String, AppError> {
    let first = registry_binding_key(first_binding)?;
    let second = registry_binding_key(second_binding)?;
    if first == second {
        return Err(AppError::new(
            "invalid_input",
            "Bidirektionale Endpunkte müssen verschieden sein",
        ));
    }
    Ok(if first < second {
        format!("{first}\u{1}{second}")
    } else {
        format!("{second}\u{1}{first}")
    })
}

fn endpoint_with_bound_root(
    mut endpoint: BidirectionalEndpointRequest,
    binding: &EndpointBinding,
) -> Result<BidirectionalEndpointRequest, AppError> {
    let canonical_root = match (&endpoint, binding) {
        (BidirectionalEndpointRequest::Local { .. }, EndpointBinding::Local(value)) => value
            .canonical_root
            .to_str()
            .ok_or_else(|| {
                AppError::new(
                    "invalid_input",
                    "Der kanonische lokale Root ist nicht eindeutig als UTF-8 darstellbar",
                )
            })?
            .to_owned(),
        (BidirectionalEndpointRequest::Sftp { .. }, EndpointBinding::Sftp(value))
        | (BidirectionalEndpointRequest::Ftps { .. }, EndpointBinding::Ftps(value))
        | (BidirectionalEndpointRequest::Webdav { .. }, EndpointBinding::WebDav(value))
        | (BidirectionalEndpointRequest::Smb { .. }, EndpointBinding::Smb(value)) => {
            value.canonical_root.clone()
        }
        _ => {
            return Err(AppError::new(
                "plan_drift",
                "Backend-Bindung gehört zu einer anderen Providerart",
            ))
        }
    };
    match &mut endpoint {
        BidirectionalEndpointRequest::Local { root }
        | BidirectionalEndpointRequest::Sftp { root, .. }
        | BidirectionalEndpointRequest::Ftps { root, .. }
        | BidirectionalEndpointRequest::Webdav { root, .. }
        | BidirectionalEndpointRequest::Smb { root, .. } => *root = canonical_root,
    }
    Ok(endpoint)
}

fn bound_registry_endpoint(
    endpoint: BidirectionalEndpointRequest,
    provider: &ProviderEndpoint,
) -> Result<BoundRegistryEndpoint, AppError> {
    let (binding, compatible_bindings) =
        bind_bidirectional_endpoint_compatibility(provider).map_err(AppError::from)?;
    let endpoint = endpoint_with_bound_root(endpoint, &binding)?;
    Ok(BoundRegistryEndpoint {
        endpoint,
        binding,
        compatible_bindings,
    })
}

fn pair_display_name(
    left: &BidirectionalEndpointRequest,
    right: &BidirectionalEndpointRequest,
) -> String {
    fn name(endpoint: &BidirectionalEndpointRequest, fallback: &str) -> String {
        endpoint
            .root()
            .trim_end_matches('/')
            .rsplit('/')
            .find(|part| !part.is_empty())
            .unwrap_or(fallback)
            .to_owned()
    }
    format!("{} ↔ {}", name(left, "Links"), name(right, "Rechts"))
        .chars()
        .take(80)
        .collect()
}

fn new_pair_id() -> Result<PairId, AppError> {
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random)
        .map_err(|_| AppError::new("app", "Bidirektionale Paar-ID konnte nicht erzeugt werden"))?;
    if random == [0; 16] {
        random[0] = 1;
    }
    Ok(PairId(random))
}

fn read_pair_registry(path: &Path) -> Result<BTreeMap<String, StoredBidirectionalPair>, AppError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // NONBLOCK wirkt bereits beim Öffnen: Die spätere Typprüfung könnte ein
        // FIFO ohne Schreiber sonst nicht mehr erreichen.
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        #[cfg(unix)]
        Err(error) if error.raw_os_error() == Some(libc::ELOOP) => {
            return Err(AppError::new(
                "invalid_data",
                "Bidirektionale Paar-Registry darf kein Symlink sein",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(AppError::from(error)),
    };
    let metadata = file.metadata().map_err(AppError::from)?;
    if !metadata.is_file() || metadata.len() > MAX_PAIR_REGISTRY_BYTES {
        return Err(AppError::new(
            "invalid_data",
            "Bidirektionale Paar-Registry muss eine reguläre Datei mit höchstens 16 MiB sein",
        ));
    }
    // Der Bestand kann nach der Handleprüfung wachsen. Das Leselimit hält auch
    // diesen Fall begrenzt; ein zusätzliches Byte macht Übergröße erkennbar.
    let mut bytes = Vec::new();
    file.take(MAX_PAIR_REGISTRY_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(AppError::from)?;
    if bytes.len() as u64 > MAX_PAIR_REGISTRY_BYTES {
        return Err(AppError::new(
            "invalid_data",
            "Bidirektionale Paar-Registry überschreitet 16 MiB",
        ));
    }
    let stored: StoredBidirectionalPairRegistry = serde_json::from_slice(&bytes)
        .map_err(|error| AppError::new("invalid_data", error.to_string()))?;
    if stored.version != 1 {
        return Err(AppError::new(
            "invalid_data",
            "Bidirektionale Paar-Registry besitzt eine unbekannte Version",
        ));
    }
    for (key, pair) in &stored.entries {
        let (left_key, left) = normalize_registry_endpoint(pair.left.clone())?;
        let (right_key, right) = normalize_registry_endpoint(pair.right.clone())?;
        let expected_key = match (&pair.left_binding, &pair.right_binding) {
            (Some(left_binding), Some(right_binding)) => {
                let bound_left = endpoint_with_bound_root(left.clone(), left_binding)?;
                let bound_right = endpoint_with_bound_root(right.clone(), right_binding)?;
                let left_keys = accepted_registry_binding_keys(left_binding)?;
                let right_keys = accepted_registry_binding_keys(right_binding)?;
                // Der Map-Schlüssel bleibt unabhängig von Pane- und Baseline-Rollen
                // kanonisch sortiert. `pair.left` und `pair.right` bewahren dagegen
                // die einmal festgelegten Rollen des Paars, auch wenn ein neues
                // Bindungsschema die Digestreihenfolge umkehrt.
                let key_matches = left_keys.iter().any(|left_key| {
                    right_keys.iter().any(|right_key| {
                        let candidate = if left_key < right_key {
                            format!("{left_key}\u{1}{right_key}")
                        } else {
                            format!("{right_key}\u{1}{left_key}")
                        };
                        left_key != right_key && *key == candidate
                    })
                });
                if bound_left != pair.left || bound_right != pair.right || !key_matches {
                    return Err(AppError::new(
                        "invalid_data",
                        "Bidirektionale Paar-Registry enthält widersprüchliche Backend-Bindungen",
                    ));
                }
                key.clone()
            }
            (None, None) if left_key < right_key => format!("{left_key}\u{1}{right_key}"),
            _ => {
                return Err(AppError::new(
                    "invalid_data",
                    "Bidirektionale Paar-Registry enthält nur eine halbe Backend-Bindung",
                ))
            }
        };
        if pair.pair_id.0 == [0; 16]
            || left != pair.left
            || right != pair.right
            || *key != expected_key
        {
            return Err(AppError::new(
                "invalid_data",
                "Bidirektionale Paar-Registry enthält einen ungebundenen Eintrag",
            ));
        }
    }
    Ok(stored.entries)
}

fn write_pair_registry(
    path: &Path,
    entries: &BTreeMap<String, StoredBidirectionalPair>,
) -> Result<(), AppError> {
    let parent = path.parent().ok_or_else(|| {
        AppError::new("invalid_input", "Paar-Registry braucht einen Elternordner")
    })?;
    std::fs::create_dir_all(parent).map_err(AppError::from)?;
    let mut random = [0_u8; 8];
    getrandom::fill(&mut random)
        .map_err(|_| AppError::new("app", "Registry-Tempname konnte nicht erzeugt werden"))?;
    let temporary = parent.join(format!(
        ".bidirectional-pairs-{}.tmp",
        random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ));
    write_pair_registry_at_temporary(path, entries, &temporary)
}

fn write_pair_registry_at_temporary(
    path: &Path,
    entries: &BTreeMap<String, StoredBidirectionalPair>,
    temporary: &Path,
) -> Result<(), AppError> {
    let parent = path.parent().ok_or_else(|| {
        AppError::new("invalid_input", "Paar-Registry braucht einen Elternordner")
    })?;
    let mut bytes = serde_json::to_vec(&StoredBidirectionalPairRegistryRef {
        version: 1,
        entries,
    })
    .map_err(|error| AppError::new("io", error.to_string()))?;
    bytes.push(b'\n');
    if bytes.len() as u64 > MAX_PAIR_REGISTRY_BYTES {
        return Err(AppError::new(
            "invalid_input",
            "Bidirektionale Paar-Registry überschreitet 16 MiB; bisheriger Bestand bleibt erhalten",
        ));
    }
    let mut temporary_owned = false;
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(temporary).map_err(AppError::from)?;
        temporary_owned = true;
        file.write_all(&bytes).map_err(AppError::from)?;
        file.sync_all().map_err(AppError::from)?;
        std::fs::rename(temporary, path).map_err(AppError::from)?;
        temporary_owned = false;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(AppError::from)
    })();
    // Ein kollidierter Name oder ein nach dem Rename neu belegter Pfad gehört uns nicht.
    if result.is_err() && temporary_owned {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

fn ensure_pair_registry_loaded(
    registry: &mut BidirectionalPairRegistry,
    path: &Path,
) -> Result<(), AppError> {
    if registry.path.as_deref() != Some(path) {
        registry.entries = read_pair_registry(path)?;
        registry.path = Some(path.to_owned());
    }
    Ok(())
}

fn register_bidirectional_pair_at(
    registry: &mut BidirectionalPairRegistry,
    path: &Path,
    first: BoundRegistryEndpoint,
    second: BoundRegistryEndpoint,
) -> Result<RegisteredBidirectionalPair, AppError> {
    ensure_pair_registry_loaded(registry, path)?;
    let visible_first_binding = first.binding.clone();
    let first_key = registry_binding_key(&first.binding)?;
    let second_key = registry_binding_key(&second.binding)?;
    if first_key == second_key {
        return Err(AppError::new(
            "invalid_input",
            "Bidirektionale Endpunkte müssen verschieden sein",
        ));
    }
    let ((left_key, left), (right_key, right)) = if first_key < second_key {
        ((first_key, first), (second_key, second))
    } else {
        ((second_key, second), (first_key, first))
    };
    let key = format!("{left_key}\u{1}{right_key}");
    let pair = if let Some(existing) = registry.entries.get(&key) {
        let existing_left_binding = existing.left_binding.as_ref().ok_or_else(|| {
            AppError::new(
                "invalid_data",
                "Bidirektionale Paar-Registry enthält keine linke Bindung",
            )
        })?;
        let existing_right_binding = existing.right_binding.as_ref().ok_or_else(|| {
            AppError::new(
                "invalid_data",
                "Bidirektionale Paar-Registry enthält keine rechte Bindung",
            )
        })?;
        let existing_left_key = registry_binding_key(existing_left_binding)?;
        let existing_right_key = registry_binding_key(existing_right_binding)?;
        let (stable_left, stable_right) = if existing_left_key == left_key
            && existing_right_key == right_key
        {
            (&left, &right)
        } else if existing_left_key == right_key && existing_right_key == left_key {
            (&right, &left)
        } else {
            return Err(AppError::new(
                "invalid_data",
                "Bidirektionale Paar-Registry verwechselt kanonischen Schlüssel und feste Endpunktrollen",
            ));
        };
        if existing.left == stable_left.endpoint
            && existing.right == stable_right.endpoint
            && existing.left_binding.as_ref() == Some(&stable_left.binding)
            && existing.right_binding.as_ref() == Some(&stable_right.binding)
        {
            existing.clone()
        } else {
            // Eine neue Profilrevision ändert noch keine physische Paaridentität.
            // Bei gleicher gebundener Server-/Rootidentität bleibt daher Pair-ID
            // und Baseline erhalten, während die sichtbaren Endpunkte auf den
            // aktuell registrierten Profilstand wechseln.
            let pair = StoredBidirectionalPair {
                pair_id: existing.pair_id,
                left: stable_left.endpoint.clone(),
                right: stable_right.endpoint.clone(),
                left_binding: Some(stable_left.binding.clone()),
                right_binding: Some(stable_right.binding.clone()),
            };
            let mut next = registry.entries.clone();
            next.insert(key, pair.clone());
            write_pair_registry(path, &next)?;
            registry.entries = next;
            pair
        }
    } else if let Some((legacy_key, legacy_pair, legacy_left_is_current_left)) = {
        let matches = registry
            .entries
            .iter()
            .flat_map(|(legacy_key, candidate)| {
                let (Some(candidate_left), Some(candidate_right)) =
                    (&candidate.left_binding, &candidate.right_binding)
                else {
                    return Vec::new();
                };
                let mut orientations = Vec::with_capacity(2);
                if left.compatible_bindings.contains(candidate_left)
                    && right.compatible_bindings.contains(candidate_right)
                {
                    orientations.push((legacy_key.clone(), candidate.clone(), true));
                }
                if left.compatible_bindings.contains(candidate_right)
                    && right.compatible_bindings.contains(candidate_left)
                {
                    orientations.push((legacy_key.clone(), candidate.clone(), false));
                }
                orientations
            })
            .collect::<Vec<_>>();
        if matches.len() > 1 {
            return Err(AppError::new(
                "invalid_data",
                "Mehrere alte Registry-Einträge beanspruchen dieselbe frisch gebundene Paaridentität",
            ));
        }
        matches.into_iter().next()
    } {
        // Beide alten Bindungen wurden gegen die gerade verbundenen Provider
        // authentisiert. Nur der Schlüssel und die Bindungsschemata ändern sich;
        // Pair-ID, Baselinepfad und Recoveryjournale bleiben erhalten.
        let pair = if legacy_left_is_current_left {
            StoredBidirectionalPair {
                pair_id: legacy_pair.pair_id,
                left: left.endpoint,
                right: right.endpoint,
                left_binding: Some(left.binding),
                right_binding: Some(right.binding),
            }
        } else {
            StoredBidirectionalPair {
                pair_id: legacy_pair.pair_id,
                left: right.endpoint,
                right: left.endpoint,
                left_binding: Some(right.binding),
                right_binding: Some(left.binding),
            }
        };
        let mut next = registry.entries.clone();
        next.remove(&legacy_key);
        next.insert(key, pair.clone());
        write_pair_registry(path, &next)?;
        registry.entries = next;
        pair
    } else {
        let pair = StoredBidirectionalPair {
            pair_id: new_pair_id()?,
            left: left.endpoint,
            right: right.endpoint,
            left_binding: Some(left.binding),
            right_binding: Some(right.binding),
        };
        let mut next = registry.entries.clone();
        // Ein alter pfadbasierter Eintrag darf weder eine zweite Baseline noch
        // eine fremde Pair-ID für dieselben sichtbaren Endpunkte behalten. Seine
        // ID wird bewusst nicht übernommen, weil sie nie physisch gebunden war.
        next.retain(|_, candidate| {
            candidate.left_binding.is_some()
                || candidate.left != pair.left
                || candidate.right != pair.right
        });
        next.insert(key, pair.clone());
        write_pair_registry(path, &next)?;
        registry.entries = next;
        pair
    };
    let visible_left_role = if pair.left_binding.as_ref() == Some(&visible_first_binding) {
        EndpointSide::Left
    } else if pair.right_binding.as_ref() == Some(&visible_first_binding) {
        EndpointSide::Right
    } else {
        return Err(AppError::new(
            "invalid_data",
            "Sichtbarer Endpunkt gehört nach der Registrierung zu keiner festen Paarrolle",
        ));
    };
    Ok(RegisteredBidirectionalPair {
        pair_id: pair.pair_id,
        pair_name: pair_display_name(&pair.left, &pair.right),
        left: pair.left,
        right: pair.right,
        visible_left_role,
    })
}

fn validate_registered_pair_at(
    registry: &mut BidirectionalPairRegistry,
    path: &Path,
    request: &PrepareBidirectionalRequest,
    left_binding: &EndpointBinding,
    right_binding: &EndpointBinding,
) -> Result<(), AppError> {
    ensure_pair_registry_loaded(registry, path)?;
    let (_, left) = normalize_registry_endpoint(request.left.clone())?;
    let (_, right) = normalize_registry_endpoint(request.right.clone())?;
    let left = endpoint_with_bound_root(left, left_binding)?;
    let right = endpoint_with_bound_root(right, right_binding)?;
    let key = registry_pair_key(left_binding, right_binding)?;
    let Some(pair) = registry.entries.get(&key) else {
        return Err(AppError::new(
            "plan_drift",
            "Das bidirektionale Paar ist nicht backendseitig registriert",
        ));
    };
    if left != request.left
        || right != request.right
        || pair.pair_id != request.pair_id
        || pair.left != left
        || pair.right != right
        || pair.left_binding.as_ref() != Some(left_binding)
        || pair.right_binding.as_ref() != Some(right_binding)
        || pair_display_name(&pair.left, &pair.right) != request.pair_name
    {
        return Err(AppError::new(
            "plan_drift",
            "Paar-ID oder feste Endpunktrollen stimmen nicht mit der Backend-Registry überein",
        ));
    }
    Ok(())
}

fn pair_registry_path(app: &tauri::AppHandle) -> Result<PathBuf, AppError> {
    Ok(app_config_dir(app)?
        .join("bidirectional")
        .join("pairs.json"))
}

pub(crate) fn validate_registry_profile(
    app: &tauri::AppHandle,
    endpoint: &BidirectionalEndpointRequest,
) -> Result<(), AppError> {
    let (requested, current) = match endpoint {
        BidirectionalEndpointRequest::Local { .. } => return Ok(()),
        BidirectionalEndpointRequest::Sftp {
            profile_id,
            profile_revision,
            ..
        } => {
            let profile = crate::sftp_commands::find_versioned_profile(app, profile_id)?;
            (*profile_revision, profile.revision)
        }
        BidirectionalEndpointRequest::Ftps {
            profile_id,
            profile_revision,
            ..
        } => {
            let profile = crate::ftps_commands::find_versioned_profile(app, profile_id)?;
            (*profile_revision, profile.revision)
        }
        BidirectionalEndpointRequest::Webdav {
            profile_id,
            profile_revision,
            ..
        } => {
            let profile = crate::webdav_commands::find_versioned_profile(app, profile_id)?;
            (*profile_revision, profile.revision)
        }
        BidirectionalEndpointRequest::Smb {
            profile_id,
            profile_revision,
            ..
        } => {
            let profile = crate::smb_commands::find_versioned_profile(app, profile_id)?;
            (*profile_revision, profile.revision)
        }
    };
    ensure_requested_profile_revision(requested, current)
}

pub(crate) fn resolve_validated_registry_pair<T>(
    left: &BidirectionalEndpointRequest,
    right: &BidirectionalEndpointRequest,
    mut validate: impl FnMut(&BidirectionalEndpointRequest) -> Result<(), AppError>,
    mut resolve: impl FnMut(&BidirectionalEndpointRequest) -> Result<T, AppError>,
) -> Result<(T, T), AppError> {
    // Beide Revisionen müssen feststehen, bevor auch nur der linke Provider ein
    // Secret liest, eine Sitzung wiederverwendet oder eine Verbindung aufbaut.
    validate(left)?;
    validate(right)?;
    Ok((resolve(left)?, resolve(right)?))
}

#[tauri::command]
pub(crate) fn register_bidirectional_pair(
    app: tauri::AppHandle,
    state: State<AppState>,
    left: BidirectionalEndpointRequest,
    right: BidirectionalEndpointRequest,
) -> Result<RegisteredBidirectionalPair, AppError> {
    // Beide Requests und Profile werden vollständig geprüft, bevor der erste
    // Provider ein Secret liest oder eine Verbindung aufbaut. Erst danach darf
    // die Registry ihre Schlüssel aus echten Backend-Bindungen bilden.
    let (_, left) = normalize_registry_endpoint(left)?;
    let (_, right) = normalize_registry_endpoint(right)?;
    let ((left_provider, _, left_profiles), (right_provider, _, right_profiles)) =
        resolve_validated_registry_pair(
            &left,
            &right,
            |endpoint| validate_registry_profile(&app, endpoint),
            |endpoint| resolve_endpoint(&app, &state, endpoint),
        )?;
    let profiles = crate::queue_runtime::merge_profile_bindings([left_profiles, right_profiles]);
    crate::queue_runtime::ensure_profile_bindings_current(&app, &profiles)?;
    let left = bound_registry_endpoint(left, &left_provider)?;
    let right = bound_registry_endpoint(right, &right_provider)?;
    let path = pair_registry_path(&app)?;
    register_bidirectional_pair_at(
        &mut state.bidirectional_pairs.lock_recover(),
        &path,
        left,
        right,
    )
}

#[cfg(test)]
mod endpoint_request_tests {
    use super::{
        bound_registry_endpoint, ensure_requested_profile_revision, ensure_requested_revision,
        legacy_registry_binding_key, pair_display_name, read_pair_registry,
        recovery_discard_decision, register_bidirectional_pair_at, registry_binding_key,
        resolve_validated_registry_pair, validate_endpoint_request_root,
        validate_registered_pair_at, write_pair_registry, BidirectionalEndpointRequest,
        BidirectionalPairRegistry, BoundRegistryEndpoint, PrepareBidirectionalRequest,
        StoredBidirectionalPair,
    };
    use crate::AppError;
    use doppeldecker_core::bidirectional::{LocalEndpointBinding, RemoteEndpointBinding};
    use doppeldecker_core::{EndpointBinding, EndpointSide, ProviderEndpoint, ProviderKind};
    use std::cell::Cell;
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::sync::{Arc, Barrier, Mutex};

    #[test]
    #[ignore = "nur als eigenes Kind des markierten FIFO-Gegenlaufs"]
    fn registry_fifo_child() {
        let root =
            std::path::PathBuf::from(std::env::var_os("DOPPELDECKER_PAIR_FIFO_ROOT").unwrap());
        assert!(root
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("doppeldecker-pair-fifo-"));
        assert_eq!(std::fs::read(root.join("marker")).unwrap(), b"pair-fifo-v1");
        std::fs::write(root.join("ready"), b"ready").unwrap();
        assert_eq!(
            read_pair_registry(&root.join("pairs.json"))
                .err()
                .unwrap()
                .code,
            "invalid_data"
        );
    }

    #[test]
    fn registry_fifo_ohne_schreiber_blockiert_nicht() {
        use std::os::unix::ffi::OsStrExt;
        use std::process::{Child, Command, Stdio};
        use std::time::{Duration, Instant};
        struct ChildGuard(Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                if self.0.try_wait().ok().flatten().is_none() {
                    let _ = self.0.kill();
                    let _ = self.0.wait();
                }
            }
        }
        let area = tempfile::Builder::new()
            .prefix("doppeldecker-pair-fifo-")
            .tempdir()
            .unwrap();
        std::fs::write(area.path().join("marker"), b"pair-fifo-v1").unwrap();
        let fifo =
            std::ffi::CString::new(area.path().join("pairs.json").as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let name = "bidirectional_commands::endpoint_request_tests::registry_fifo_child";
        let binary = std::env::current_exe().unwrap();
        let list = Command::new(&binary)
            .args(["--list", "--exact", name])
            .output()
            .unwrap();
        assert!(list.status.success());
        assert_eq!(
            String::from_utf8(list.stdout)
                .unwrap()
                .lines()
                .filter(|line| *line == format!("{name}: test"))
                .count(),
            1
        );
        let mut child = ChildGuard(
            Command::new(binary)
                .args(["--exact", name, "--ignored"])
                .env("DOPPELDECKER_PAIR_FIFO_ROOT", area.path())
                .stdout(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let startup = Instant::now() + Duration::from_secs(15);
        while !area.path().join("ready").exists() {
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "FIFO-Kind endete vor Ready"
            );
            assert!(Instant::now() < startup, "FIFO-Kind wurde nicht bereit");
            std::thread::sleep(Duration::from_millis(5));
        }
        let deadline = Instant::now() + Duration::from_secs(1);
        let status = loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                break Some(status);
            }
            if Instant::now() >= deadline {
                break None;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(
            status.is_some_and(|status| status.success()),
            "Paarregistry-Leser blockiert an einem FIFO ohne Schreiber"
        );
    }

    #[test]
    fn registry_liest_16_mib_aber_keinen_groesseren_json_bestand() {
        let area = tempfile::tempdir().unwrap();
        let path = area.path().join("pairs.json");
        let mut bytes = br#"{"version":1,"entries":{}}"#.to_vec();
        bytes.resize(16 * 1024 * 1024, b' ');
        std::fs::write(&path, &bytes).unwrap();
        assert!(read_pair_registry(&path).unwrap().is_empty());
        bytes.push(b' ');
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(
            read_pair_registry(&path).err().unwrap().code,
            "invalid_data"
        );
    }

    #[test]
    fn registry_folgt_keinem_finalen_symlink() {
        let area = tempfile::tempdir().unwrap();
        let target = area.path().join("unveraendert.json");
        let previous = br#"{"version":1,"entries":{}}"#;
        std::fs::write(&target, previous).unwrap();
        let missing = area.path().join("nicht-vorhanden");
        for (name, target) in [("vorhanden", &target), ("fehlend", &missing)] {
            let link = area.path().join(name);
            std::os::unix::fs::symlink(target, &link).unwrap();
            assert_eq!(
                read_pair_registry(&link).err().unwrap().code,
                "invalid_data"
            );
        }
        assert_eq!(std::fs::read(target).unwrap(), previous);
    }

    #[test]
    fn zu_grosse_registry_erhaelt_alten_bestand_ohne_tempdatei() {
        let area = tempfile::tempdir().unwrap();
        let path = area.path().join("pairs.json");
        let temporary = area.path().join(".bidirectional-pairs-eigen.tmp");
        let previous = br#"{"version":1,"entries":{}}"#;
        std::fs::write(&path, previous).unwrap();
        // Die Roottexte werden nur serialisiert, nie als echte Dateipfade geöffnet.
        let (left_key, left) =
            super::normalize_registry_endpoint(BidirectionalEndpointRequest::Local {
                root: format!("/a{}", "a".repeat(9 * 1024 * 1024)),
            })
            .unwrap();
        let (right_key, right) =
            super::normalize_registry_endpoint(BidirectionalEndpointRequest::Local {
                root: "/z".into(),
            })
            .unwrap();
        let entries = BTreeMap::from([(
            format!("{left_key}\u{1}{right_key}"),
            StoredBidirectionalPair {
                pair_id: doppeldecker_core::PairId([1; 16]),
                left,
                right,
                left_binding: None,
                right_binding: None,
            },
        )]);
        let result = super::write_pair_registry_at_temporary(&path, &entries, &temporary);
        assert!(result.is_err(), "Übergröße wurde als Registry gespeichert");
        assert_eq!(result.unwrap_err().code, "invalid_input");
        assert_eq!(std::fs::read(&path).unwrap(), previous);
        assert!(!temporary.try_exists().unwrap());
        assert_eq!(std::fs::read_dir(area.path()).unwrap().count(), 1);
    }

    #[test]
    fn profilwechsel_vor_bidirektionalem_start_erhaelt_code_ohne_wirkung() {
        let area = tempfile::tempdir().unwrap();
        let sentinel = area.path().join("unveraendert");
        std::fs::write(&sentinel, b"vorher").unwrap();

        let error = super::execute_profile_bound(
            || {
                Err(AppError::new(
                    "plan_drift",
                    "Profil wurde inzwischen geändert",
                ))
            },
            || std::fs::write(&sentinel, b"unerlaubte Wirkung"),
        )
        .unwrap_err();

        assert_eq!(error.code, "plan_drift");
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"vorher");
    }

    #[test]
    fn echte_inhaltsdrift_bleibt_im_strukturierten_bidirektionalen_fehler_erkennbar() {
        let area = tempfile::tempdir().unwrap();
        let left = area.path().join("links");
        let right = area.path().join("rechts");
        std::fs::create_dir(&left).unwrap();
        std::fs::create_dir(&right).unwrap();
        for root in [&left, &right] {
            std::fs::write(root.join("gleich.txt"), b"vorher").unwrap();
        }
        let prepared = doppeldecker_core::bidirectional::prepare_local_bidirectional(
            doppeldecker_core::PairId([61; 16]),
            "Fehlerweitergabe",
            &left,
            &right,
            &area.path().join("baseline.json"),
        )
        .unwrap();
        let phrase = prepared.preview().confirmation_phrase.clone();
        std::fs::write(left.join("gleich.txt"), b"bewusst geaendert").unwrap();

        let error =
            super::execute_profile_bound(|| Ok(()), || prepared.confirm(&phrase, true).map(|_| ()))
                .unwrap_err();
        let payload = serde_json::to_value(error).unwrap();

        assert_eq!(payload["code"], "interrupted");
        assert!(payload["message"].is_string());
        assert_eq!(std::fs::read(right.join("gleich.txt")).unwrap(), b"vorher");
        assert_eq!(
            std::fs::read(left.join("gleich.txt")).unwrap(),
            b"bewusst geaendert"
        );
    }

    #[test]
    fn registry_tempkollision_erhaelt_fremde_datei_und_vorherigen_bestand() {
        let area = tempfile::tempdir().unwrap();
        let path = area.path().join("pairs.json");
        let temporary = area.path().join(".bidirectional-pairs-kollision.tmp");
        let previous = br#"{"version":1,"entries":{}}"#;
        std::fs::write(&path, previous).unwrap();
        std::fs::write(&temporary, b"fremder Bestand").unwrap();

        assert!(
            super::write_pair_registry_at_temporary(&path, &BTreeMap::new(), &temporary).is_err()
        );

        assert_eq!(std::fs::read(&path).unwrap(), previous);
        assert_eq!(std::fs::read(&temporary).unwrap(), b"fremder Bestand");
        assert!(read_pair_registry(&path).unwrap().is_empty());
    }

    fn local_bound(root: &std::path::Path) -> BoundRegistryEndpoint {
        let endpoint = BidirectionalEndpointRequest::Local {
            root: root.to_str().unwrap().to_owned(),
        };
        let provider = ProviderEndpoint::local(root).unwrap();
        bound_registry_endpoint(endpoint, &provider).unwrap()
    }

    #[test]
    fn verschachtelter_remote_endpoint_akzeptiert_frontend_feldnamen() {
        let request: BidirectionalEndpointRequest = serde_json::from_value(serde_json::json!({
            "provider": "ftps",
            "profileId": "profil-1",
            "profileRevision": 12,
            "root": "/dm0.de/httpdocs"
        }))
        .unwrap();

        match request {
            BidirectionalEndpointRequest::Ftps {
                profile_id,
                profile_revision,
                root,
            } => {
                assert_eq!(profile_id, "profil-1");
                assert_eq!(profile_revision, Some(12));
                assert_eq!(root, "/dm0.de/httpdocs");
            }
            _ => panic!("FTPS-Endpoint wurde als falsche Variante gelesen"),
        }
    }

    #[test]
    fn remote_endpoint_braucht_exakte_profilrevision() {
        let binding = doppeldecker_core::RemoteProfileBinding::new(
            doppeldecker_core::RemoteProfileProtocol::Sftp,
            "profil-1",
            12,
        )
        .unwrap();
        assert_eq!(
            ensure_requested_revision(None, &binding).unwrap_err().code,
            "plan_drift"
        );
        assert!(ensure_requested_revision(Some(12), &binding).is_ok());
    }

    #[test]
    fn remote_endpoint_stoppt_ungueltigen_root_vor_der_aufloesung() {
        let request = BidirectionalEndpointRequest::Sftp {
            profile_id: "profil-1".to_owned(),
            profile_revision: Some(12),
            root: "/daten/../fremd".to_owned(),
        };

        let error = validate_endpoint_request_root(&request).unwrap_err();
        assert_eq!(error.code, "invalid_input");
    }

    #[test]
    fn registry_prueft_remote_revision_vor_der_provideraufloesung() {
        assert_eq!(
            ensure_requested_profile_revision(None, 12)
                .unwrap_err()
                .code,
            "plan_drift"
        );
        assert_eq!(
            ensure_requested_profile_revision(Some(11), 12)
                .unwrap_err()
                .code,
            "plan_drift"
        );
        assert!(ensure_requested_profile_revision(Some(12), 12).is_ok());

        let left = BidirectionalEndpointRequest::Local {
            root: "/links".to_owned(),
        };
        let right = BidirectionalEndpointRequest::Local {
            root: "/rechts".to_owned(),
        };
        let validations = Cell::new(0);
        let provider_accesses = Cell::new(0);
        let error = resolve_validated_registry_pair(
            &left,
            &right,
            |_| {
                validations.set(validations.get() + 1);
                if validations.get() == 2 {
                    return Err(AppError::new("plan_drift", "zweite Revision ist veraltet"));
                }
                Ok(())
            },
            |_| {
                provider_accesses.set(provider_accesses.get() + 1);
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.code, "plan_drift");
        assert_eq!(validations.get(), 2);
        assert_eq!(provider_accesses.get(), 0);
    }

    #[test]
    fn verschachtelter_remote_endpoint_lehnt_fehlende_profil_id_ab() {
        let error = serde_json::from_value::<BidirectionalEndpointRequest>(serde_json::json!({
            "provider": "ftps",
            "root": "/dm0.de/httpdocs"
        }))
        .err()
        .unwrap();

        assert!(error.to_string().contains("profileId"));
    }

    #[test]
    fn queue_freigabe_verlangt_nur_die_zugehoerige_queue_vorbereitung() {
        let source = include_str!("bidirectional_commands.rs");
        // Der bestehende Strukturtest in `queue_runtime.rs` sucht den echten
        // Command über diesen Namen. Die Fragmente bleiben hier absichtlich
        // getrennt, damit unser Test nicht selbst als vermeintlicher Command gilt.
        let command_start = ["pub(crate) fn approve_bidirectional_", "queue_entry("].concat();
        let command = source
            .rsplit(&command_start)
            .next()
            .unwrap()
            .split("#[tauri::command]")
            .next()
            .unwrap();

        assert!(command.contains("pending_bidirectional_queue"));
        assert!(!command.contains("pending_bidirectional_rollback"));
    }

    #[test]
    fn endpoint_liefert_die_core_providerart() {
        let request: BidirectionalEndpointRequest = serde_json::from_value(serde_json::json!({
            "provider": "webdav",
            "profileId": "profil-1",
            "root": "/daten"
        }))
        .unwrap();

        assert_eq!(request.provider_kind(), ProviderKind::WebDav);
    }

    #[test]
    fn recovery_discard_ist_idempotent_aber_nie_fuer_eine_neue_identitaet() {
        assert!(recovery_discard_decision("operation-1", "prepare-1", None).is_ok_and(|take| !take));
        assert!(recovery_discard_decision(
            "operation-1",
            "prepare-1",
            Some(("operation-1", "prepare-1")),
        )
        .is_ok_and(|take| take));

        let newer = recovery_discard_decision(
            "operation-1",
            "prepare-1",
            Some(("operation-1", "prepare-2")),
        )
        .unwrap_err();
        assert_eq!(newer.code, "conflict");

        let foreign = recovery_discard_decision(
            "operation-1",
            "prepare-1",
            Some(("operation-2", "prepare-1")),
        )
        .unwrap_err();
        assert_eq!(foreign.code, "conflict");
    }

    #[test]
    fn parallele_erstregistrierung_und_pane_tausch_binden_dasselbe_paar() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("bidirectional/pairs.json");
        let left_root = temp.path().join("links");
        let right_root = temp.path().join("rechts");
        std::fs::create_dir(&left_root).unwrap();
        std::fs::create_dir(&right_root).unwrap();
        let left_alias = temp.path().join("links-alias");
        let right_alias = temp.path().join("rechts-alias");
        std::os::unix::fs::symlink(&left_root, &left_alias).unwrap();
        std::os::unix::fs::symlink(&right_root, &right_alias).unwrap();

        let registry = Arc::new(Mutex::new(BidirectionalPairRegistry::default()));
        let barrier = Arc::new(Barrier::new(2));
        let mut workers = Vec::new();
        for swapped in [false, true] {
            let registry = registry.clone();
            let barrier = barrier.clone();
            let path = path.clone();
            let left_root = left_root.clone();
            let right_root = right_root.clone();
            let left_alias = left_alias.clone();
            let right_alias = right_alias.clone();
            workers.push(std::thread::spawn(move || {
                let (left, right) = if swapped {
                    (local_bound(&right_alias), local_bound(&left_alias))
                } else {
                    (local_bound(&left_root), local_bound(&right_root))
                };
                barrier.wait();
                register_bidirectional_pair_at(&mut registry.lock().unwrap(), &path, left, right)
                    .unwrap()
            }));
        }
        let first = workers.remove(0).join().unwrap();
        let second = workers.remove(0).join().unwrap();
        assert_eq!(first.pair_id, second.pair_id);
        assert_eq!(first.left, second.left);
        assert_eq!(first.right, second.right);
        assert_ne!(first.visible_left_role, second.visible_left_role);
        let stored_roots = [first.left.root(), first.right.root()];
        let canonical_left_root = std::fs::canonicalize(&left_root).unwrap();
        let canonical_right_root = std::fs::canonicalize(&right_root).unwrap();
        assert!(stored_roots.contains(&canonical_left_root.to_str().unwrap()));
        assert!(stored_roots.contains(&canonical_right_root.to_str().unwrap()));

        let mut reopened = BidirectionalPairRegistry::default();
        let persisted = register_bidirectional_pair_at(
            &mut reopened,
            &path,
            local_bound(&right_alias),
            local_bound(&left_alias),
        )
        .unwrap();
        assert_eq!(persisted.pair_id, first.pair_id);
        assert_eq!(persisted.visible_left_role, second.visible_left_role);

        let canonical_request = PrepareBidirectionalRequest {
            expected_unresolved_digest: None,
            pair_id: first.pair_id,
            pair_name: pair_display_name(&first.left, &first.right),
            left: first.left.clone(),
            right: first.right.clone(),
            resolutions: Vec::new(),
        };
        let canonical_left = local_bound(Path::new(first.left.root()));
        let canonical_right = local_bound(Path::new(first.right.root()));
        validate_registered_pair_at(
            &mut reopened,
            &path,
            &canonical_request,
            &canonical_left.binding,
            &canonical_right.binding,
        )
        .unwrap();

        let forged_request = PrepareBidirectionalRequest {
            expected_unresolved_digest: None,
            pair_id: doppeldecker_core::PairId([99; 16]),
            pair_name: canonical_request.pair_name.clone(),
            left: canonical_request.left.clone(),
            right: canonical_request.right.clone(),
            resolutions: Vec::new(),
        };
        assert_eq!(
            validate_registered_pair_at(
                &mut reopened,
                &path,
                &forged_request,
                &canonical_left.binding,
                &canonical_right.binding,
            )
            .unwrap_err()
            .code,
            "plan_drift"
        );

        let swapped_request = PrepareBidirectionalRequest {
            expected_unresolved_digest: None,
            left: canonical_request.right.clone(),
            right: canonical_request.left.clone(),
            ..canonical_request
        };
        assert_eq!(
            validate_registered_pair_at(
                &mut reopened,
                &path,
                &swapped_request,
                &canonical_right.binding,
                &canonical_left.binding,
            )
            .unwrap_err()
            .code,
            "plan_drift"
        );

        // Ein Bind-Mount kann denselben Ordner unter einem zweiten kanonischen
        // Namen zeigen. Der Test bildet diese Kernelidentität ohne privilegierten
        // Mount nach: Nur Device/Inode und Dateisystemnamespace bilden den Lock.
        let local_identity = |canonical_root: &str, inode| {
            EndpointBinding::Local(LocalEndpointBinding {
                canonical_root: canonical_root.into(),
                device: 42,
                inode,
                fsid: vec![1, 2, 3, 4],
                volume_id: Some([9; 16]),
                fs_type: "testfs".into(),
            })
        };
        let synthetic_local =
            |canonical_root: &str, binding: EndpointBinding| BoundRegistryEndpoint {
                endpoint: BidirectionalEndpointRequest::Local {
                    root: canonical_root.into(),
                },
                compatible_bindings: vec![binding.clone()],
                binding,
            };
        let mounted_once = register_bidirectional_pair_at(
            &mut reopened,
            &path,
            synthetic_local("/tmp/mount-a", local_identity("/tmp/mount-a", 7)),
            synthetic_local("/tmp/other", local_identity("/tmp/other", 8)),
        )
        .unwrap();
        let mounted_alias = register_bidirectional_pair_at(
            &mut reopened,
            &path,
            synthetic_local("/tmp/mount-b", local_identity("/tmp/mount-b", 7)),
            synthetic_local("/tmp/other", local_identity("/tmp/other", 8)),
        )
        .unwrap();
        assert_eq!(mounted_once.pair_id, mounted_alias.pair_id);

        // Remote-Pfade sind erst mit der vom Backend bestätigten Serveridentität
        // eindeutig. Gleich geschriebene Roots auf zwei Servern teilen keine ID.
        let remote = |public_identity_digest| {
            let binding = EndpointBinding::Sftp(RemoteEndpointBinding {
                schema_version: 1,
                canonical_root: "/daten".into(),
                account_digest: [3; 32],
                public_identity_digest,
            });
            BoundRegistryEndpoint {
                endpoint: BidirectionalEndpointRequest::Sftp {
                    profile_id: "profil".into(),
                    profile_revision: None,
                    root: "/daten".into(),
                },
                compatible_bindings: vec![binding.clone()],
                binding,
            }
        };
        let other = || synthetic_local("/tmp/remote-peer", local_identity("/tmp/remote-peer", 9));
        let server_a =
            register_bidirectional_pair_at(&mut reopened, &path, remote([4; 32]), other()).unwrap();
        let server_b =
            register_bidirectional_pair_at(&mut reopened, &path, remote([5; 32]), other()).unwrap();
        assert_ne!(server_a.pair_id, server_b.pair_id);

        let mut revised = remote([4; 32]);
        let BidirectionalEndpointRequest::Sftp {
            profile_revision, ..
        } = &mut revised.endpoint
        else {
            unreachable!()
        };
        *profile_revision = Some(8);
        let refreshed =
            register_bidirectional_pair_at(&mut reopened, &path, revised, other()).unwrap();
        assert_eq!(refreshed.pair_id, server_a.pair_id);
        assert!([&refreshed.left, &refreshed.right].iter().any(|endpoint| {
            matches!(
                endpoint,
                BidirectionalEndpointRequest::Sftp {
                    profile_revision: Some(8),
                    ..
                }
            )
        }));
        assert!(matches!(
            first.visible_left_role,
            EndpointSide::Left | EndpointSide::Right
        ));
    }

    #[test]
    fn schemafreie_registry_wird_auf_neue_bindung_migriert_und_behaelt_die_pair_id() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("bidirectional/pairs.json");
        let local_root = temp.path().join("lokal");
        std::fs::create_dir(&local_root).unwrap();
        let local = local_bound(&local_root);
        let endpoint = BidirectionalEndpointRequest::Ftps {
            profile_id: "legacy-ftps".into(),
            profile_revision: Some(2),
            root: "/daten".into(),
        };
        let old_binding = EndpointBinding::Ftps(RemoteEndpointBinding {
            schema_version: 1,
            canonical_root: "/daten".into(),
            account_digest: [7; 32],
            public_identity_digest: [8; 32],
        });
        let current_binding = EndpointBinding::Ftps(RemoteEndpointBinding {
            schema_version: 2,
            ..match old_binding.clone() {
                EndpointBinding::Ftps(value) => value,
                _ => unreachable!(),
            }
        });
        let old_remote_key = legacy_registry_binding_key(&old_binding)
            .unwrap()
            .expect("Schema 1 braucht seinen echten schemafreien Registry-Schlüssel");
        let local_key = registry_binding_key(&local.binding).unwrap();
        let pair_id = doppeldecker_core::PairId([71; 16]);
        let remote_stored = (endpoint.clone(), Some(old_binding.clone()));
        let local_stored = (local.endpoint.clone(), Some(local.binding.clone()));
        let (key, left, right) = if old_remote_key < local_key {
            (
                format!("{old_remote_key}\u{1}{local_key}"),
                remote_stored,
                local_stored,
            )
        } else {
            (
                format!("{local_key}\u{1}{old_remote_key}"),
                local_stored,
                remote_stored,
            )
        };
        write_pair_registry(
            &path,
            &BTreeMap::from([(
                key,
                StoredBidirectionalPair {
                    pair_id,
                    left: left.0,
                    right: right.0,
                    left_binding: left.1,
                    right_binding: right.1,
                },
            )]),
        )
        .unwrap();

        let current_remote = BoundRegistryEndpoint {
            endpoint,
            binding: current_binding.clone(),
            compatible_bindings: vec![current_binding, old_binding],
        };
        let mut reopened = BidirectionalPairRegistry::default();
        let migrated =
            register_bidirectional_pair_at(&mut reopened, &path, current_remote, local).unwrap();
        assert_eq!(migrated.pair_id, pair_id);
        assert_eq!(reopened.entries.len(), 1);
        assert_eq!(reopened.entries.values().next().unwrap().pair_id, pair_id);
    }

    #[test]
    fn schemawechsel_bewahrt_paarrollen_wenn_sich_digestreihenfolge_umkehrt() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("bidirectional/pairs.json");
        let pair_id = doppeldecker_core::PairId([72; 16]);
        let endpoint = |profile_id: &str| BidirectionalEndpointRequest::Ftps {
            profile_id: profile_id.into(),
            profile_revision: Some(2),
            root: "/daten".into(),
        };
        let binding = |schema_version, public_identity_digest| {
            EndpointBinding::Ftps(RemoteEndpointBinding {
                schema_version,
                canonical_root: "/daten".into(),
                account_digest: [7; 32],
                public_identity_digest,
            })
        };
        let old_a = binding(1, [1; 32]);
        let old_b = binding(1, [2; 32]);
        let current_a = binding(2, [9; 32]);
        let current_b = binding(2, [8; 32]);
        let old_a_key = legacy_registry_binding_key(&old_a).unwrap().unwrap();
        let old_b_key = legacy_registry_binding_key(&old_b).unwrap().unwrap();
        let current_a_key = registry_binding_key(&current_a).unwrap();
        let current_b_key = registry_binding_key(&current_b).unwrap();
        assert!(
            old_a_key < old_b_key,
            "Testaufbau braucht A links im Altschema"
        );
        assert!(
            current_a_key > current_b_key,
            "Testaufbau braucht die umgekehrte kanonische Reihenfolge im Neuschema"
        );
        let historical_left = endpoint("ftps-a");
        let historical_right = endpoint("ftps-b");
        write_pair_registry(
            &path,
            &BTreeMap::from([(
                format!("{old_a_key}\u{1}{old_b_key}"),
                StoredBidirectionalPair {
                    pair_id,
                    left: historical_left.clone(),
                    right: historical_right.clone(),
                    left_binding: Some(old_a.clone()),
                    right_binding: Some(old_b.clone()),
                },
            )]),
        )
        .unwrap();

        let bound = |endpoint: BidirectionalEndpointRequest,
                     binding: EndpointBinding,
                     legacy: EndpointBinding| BoundRegistryEndpoint {
            endpoint,
            compatible_bindings: vec![binding.clone(), legacy],
            binding,
        };
        let mut registry = BidirectionalPairRegistry::default();
        let migrated = register_bidirectional_pair_at(
            &mut registry,
            &path,
            bound(historical_left.clone(), current_a.clone(), old_a),
            bound(historical_right.clone(), current_b.clone(), old_b),
        )
        .unwrap();

        assert_eq!(migrated.pair_id, pair_id);
        assert_eq!(migrated.left, historical_left);
        assert_eq!(migrated.right, historical_right);
        assert_eq!(migrated.visible_left_role, EndpointSide::Left);
        let request = PrepareBidirectionalRequest {
            expected_unresolved_digest: None,
            pair_id,
            pair_name: pair_display_name(&migrated.left, &migrated.right),
            left: migrated.left.clone(),
            right: migrated.right.clone(),
            resolutions: Vec::new(),
        };
        // Diese feste Rollenfolge entspricht der vorhandenen Baseline. Der neue
        // kanonische Map-Schlüssel darf sie bei der Validierung nicht umdrehen.
        validate_registered_pair_at(&mut registry, &path, &request, &current_a, &current_b)
            .unwrap();
        let reopened = read_pair_registry(&path).unwrap();
        assert_eq!(reopened.values().next().unwrap().left, request.left);
        assert_eq!(reopened.values().next().unwrap().right, request.right);
    }
}

impl BidirectionalEndpointRequest {
    pub(crate) fn root(&self) -> &str {
        match self {
            Self::Local { root }
            | Self::Sftp { root, .. }
            | Self::Ftps { root, .. }
            | Self::Webdav { root, .. }
            | Self::Smb { root, .. } => root,
        }
    }

    pub(crate) fn provider_kind(&self) -> doppeldecker_core::ProviderKind {
        match self {
            Self::Local { .. } => doppeldecker_core::ProviderKind::Local,
            Self::Sftp { .. } => doppeldecker_core::ProviderKind::Sftp,
            Self::Ftps { .. } => doppeldecker_core::ProviderKind::Ftps,
            Self::Webdav { .. } => doppeldecker_core::ProviderKind::WebDav,
            Self::Smb { .. } => doppeldecker_core::ProviderKind::Smb,
        }
    }

    fn is_local(&self) -> bool {
        matches!(self, Self::Local { .. })
    }

    pub(crate) fn shares_remote_profile(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Sftp {
                    profile_id: left, ..
                },
                Self::Sftp {
                    profile_id: right, ..
                },
            )
            | (
                Self::Ftps {
                    profile_id: left, ..
                },
                Self::Ftps {
                    profile_id: right, ..
                },
            )
            | (
                Self::Webdav {
                    profile_id: left, ..
                },
                Self::Webdav {
                    profile_id: right, ..
                },
            )
            | (
                Self::Smb {
                    profile_id: left, ..
                },
                Self::Smb {
                    profile_id: right, ..
                },
            ) => left == right,
            _ => false,
        }
    }
}

/// Prüft einen Pane-Root rein lexikalisch, bevor ein Remoteprofil, Secret, Cache
/// oder Handshake berührt wird. `ProviderEndpoint` wiederholt dieselbe Grenze nach
/// dem Verbindungsaufbau; diese frühe Prüfung verhindert bereits den Kontakt für
/// syntaktisch ungültige Requests.
pub(crate) fn validate_endpoint_request_root(
    request: &BidirectionalEndpointRequest,
) -> Result<PathBuf, AppError> {
    let original = Path::new(request.root());
    let normalized =
        doppeldecker_core::validate_file_operation_root(original).map_err(AppError::from)?;
    if normalized != original {
        return Err(AppError::new(
            "invalid_input",
            "Provider-Root muss absolut und lexikalisch normalisiert sein",
        ));
    }
    Ok(normalized)
}

fn baseline_path(app: &tauri::AppHandle, pair_id: PairId) -> Result<std::path::PathBuf, AppError> {
    let id: String = pair_id.0.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(app_config_dir(app)?
        .join("bidirectional")
        .join(format!("{id}.json")))
}

pub(crate) fn resolve_endpoint(
    app: &tauri::AppHandle,
    state: &AppState,
    request: &BidirectionalEndpointRequest,
) -> Result<
    (
        ProviderEndpoint,
        Box<BidirectionalQueueEndpoint>,
        Vec<doppeldecker_core::RemoteProfileBinding>,
    ),
    AppError,
> {
    validate_endpoint_request_root(request)?;
    match request {
        BidirectionalEndpointRequest::Local { root } => Ok((
            ProviderEndpoint::local(Path::new(root)).map_err(AppError::from)?,
            Box::new(BidirectionalQueueEndpoint::Local { root: root.clone() }),
            Vec::new(),
        )),
        BidirectionalEndpointRequest::Sftp {
            profile_id,
            profile_revision,
            root,
        } => {
            let cached = crate::sftp_commands::cached_connection_bound(
                app,
                state,
                profile_id,
                *profile_revision,
            )?;
            ensure_requested_revision(*profile_revision, &cached.binding)?;
            let profile = crate::sftp_commands::find_versioned_profile(app, profile_id)?;
            ensure_profile_and_cache_match(profile.revision, &cached.binding)?;
            Ok((
                ProviderEndpoint::sftp(cached.connection, Path::new(root))
                    .map_err(AppError::from)?,
                Box::new(BidirectionalQueueEndpoint::Sftp {
                    connection: Box::new(profile.profile.connection),
                    root: root.clone(),
                }),
                vec![cached.binding],
            ))
        }
        BidirectionalEndpointRequest::Ftps {
            profile_id,
            profile_revision,
            root,
        } => {
            let cached = crate::ftps_commands::cached_connection_bound(
                app,
                state,
                profile_id,
                *profile_revision,
            )?;
            ensure_requested_revision(*profile_revision, &cached.binding)?;
            let profile = crate::ftps_commands::find_versioned_profile(app, profile_id)?;
            ensure_profile_and_cache_match(profile.revision, &cached.binding)?;
            Ok((
                ProviderEndpoint::ftps(cached.connection, Path::new(root))
                    .map_err(AppError::from)?,
                Box::new(BidirectionalQueueEndpoint::Ftps {
                    connection: Box::new(profile.profile.connection),
                    root: root.clone(),
                }),
                vec![cached.binding],
            ))
        }
        BidirectionalEndpointRequest::Webdav {
            profile_id,
            profile_revision,
            root,
        } => {
            let cached = crate::webdav_commands::cached_connection_bound(
                app,
                state,
                profile_id,
                *profile_revision,
            )?;
            ensure_requested_revision(*profile_revision, &cached.binding)?;
            let profile = crate::webdav_commands::find_versioned_profile(app, profile_id)?;
            ensure_profile_and_cache_match(profile.revision, &cached.binding)?;
            Ok((
                ProviderEndpoint::webdav(cached.connection, Path::new(root))
                    .map_err(AppError::from)?,
                Box::new(BidirectionalQueueEndpoint::WebDav {
                    connection: Box::new(profile.profile.connection),
                    root: root.clone(),
                }),
                vec![cached.binding],
            ))
        }
        BidirectionalEndpointRequest::Smb {
            profile_id,
            profile_revision,
            root,
        } => {
            let cached = crate::smb_commands::cached_connection_bound(
                app,
                state,
                profile_id,
                *profile_revision,
            )?;
            ensure_requested_revision(*profile_revision, &cached.binding)?;
            let profile = crate::smb_commands::find_versioned_profile(app, profile_id)?;
            ensure_profile_and_cache_match(profile.revision, &cached.binding)?;
            Ok((
                ProviderEndpoint::smb(cached.connection, Path::new(root))
                    .map_err(AppError::from)?,
                Box::new(BidirectionalQueueEndpoint::Smb {
                    connection: Box::new(profile.profile.connection),
                    root: root.clone(),
                }),
                vec![cached.binding],
            ))
        }
    }
}

fn ensure_requested_revision(
    requested: Option<u64>,
    current: &doppeldecker_core::RemoteProfileBinding,
) -> Result<(), AppError> {
    ensure_requested_profile_revision(requested, current.revision)
}

fn ensure_requested_profile_revision(
    requested: Option<u64>,
    current_revision: u64,
) -> Result<(), AppError> {
    if requested == Some(current_revision) {
        return Ok(());
    }
    Err(AppError::new(
        "plan_drift",
        "Der Endpunkt verwendet keine aktuelle Profilrevision",
    ))
}

fn ensure_profile_and_cache_match(
    profile_revision: u64,
    cached: &doppeldecker_core::RemoteProfileBinding,
) -> Result<(), AppError> {
    if profile_revision != cached.revision {
        return Err(AppError::new(
            "plan_drift",
            "Profil und lebende Verbindung stammen aus verschiedenen Revisionen",
        ));
    }
    Ok(())
}

fn execute_profile_bound<T>(
    validate: impl FnOnce() -> Result<(), AppError>,
    execute: impl FnOnce() -> std::io::Result<T>,
) -> Result<T, AppError> {
    validate()?;
    execute().map_err(AppError::from)
}

fn ensure_bidirectional_preparation_active(
    cancel: &doppeldecker_core::Cancel,
) -> Result<(), AppError> {
    if cancel.is_cancelled() {
        Err(AppError::new(
            "interrupted",
            "Bidirektionale Vorbereitung wurde abgebrochen",
        ))
    } else {
        Ok(())
    }
}

#[tauri::command]
pub(crate) async fn prepare_bidirectional_sync(
    app: tauri::AppHandle,
    window: WebviewWindow,
    request: PrepareBidirectionalRequest,
) -> Result<LocalBidirectionalPreview, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let label = window.label();
        let window_state = state.window(label);
        crate::queue_runtime::with_reserved_cancellable_operation(&app, label, &window_state, |cancel| {
            ensure_bidirectional_preparation_active(cancel)?;
            if window_state.pending_bidirectional.lock_recover().is_some() {
                return Err(AppError::new(
                    "busy",
                    "Es existiert bereits eine bidirektionale Vorbereitung",
                ));
            }
            // Erst beide Requests und Profile prüfen, dann Verbindungen öffnen. Die
            // anschließend erneut gelesenen Backend-Bindungen müssen exakt zum
            // registrierten Paar gehören; ein Profil- oder Rootwechsel ist Plan-Drift.
            let (_, normalized_left) = normalize_registry_endpoint(request.left.clone())?;
            let (_, normalized_right) = normalize_registry_endpoint(request.right.clone())?;
            if normalized_left != request.left || normalized_right != request.right {
                return Err(AppError::new(
                    "plan_drift",
                    "Bidirektionaler Request verwendet nicht die registrierten kanonischen Roots",
                ));
            }
            validate_registry_profile(&app, &normalized_left)?;
            validate_registry_profile(&app, &normalized_right)?;
            let (left_endpoint, left_queue, left_profiles) =
                resolve_endpoint(&app, &state, &normalized_left)?;
            let (right_endpoint, right_queue, right_profiles) =
                resolve_endpoint(&app, &state, &normalized_right)?;
            let profile_bindings =
                crate::queue_runtime::merge_profile_bindings([left_profiles, right_profiles]);
            let left_binding = bind_bidirectional_endpoint(&left_endpoint).map_err(AppError::from)?;
            let right_binding = bind_bidirectional_endpoint(&right_endpoint).map_err(AppError::from)?;
            let registry_path = pair_registry_path(&app)?;
            validate_registered_pair_at(
                &mut state.bidirectional_pairs.lock_recover(),
                &registry_path,
                &request,
                &left_binding,
                &right_binding,
            )?;
            let baseline = baseline_path(&app, request.pair_id)?;
            let prepared = if request.left.is_local() && request.right.is_local() {
                doppeldecker_core::bidirectional::prepare_local_bidirectional_with_decisions_bound(
                    request.pair_id,
                    &request.pair_name,
                    Path::new(request.left.root()),
                    Path::new(request.right.root()),
                    &baseline,
                    &request.resolutions,
                    request.expected_unresolved_digest,
                )
            } else {
                doppeldecker_core::bidirectional::prepare_provider_bidirectional_with_decisions_bound(
                    request.pair_id,
                    &request.pair_name,
                    left_endpoint,
                    right_endpoint,
                    &baseline,
                    &request.resolutions,
                    request.expected_unresolved_digest,
                )
            }
            .map_err(AppError::from)?;
            // Nur diese frisch erzeugte, noch unbestätigte Vorbereitung darf ihr
            // eigenes mutationsloses Journal verwerfen. Resume und Rollback
            // behalten beim gleichen Abbruch ihr bestehendes Recovery-Journal.
            if let Err(cancelled) = ensure_bidirectional_preparation_active(cancel) {
                prepared.discard().map_err(AppError::from)?;
                return Err(cancelled);
            }
            let preview = prepared.preview().clone();
            let operation = QueueOperation::Bidirectional {
                pair_id: request.pair_id,
                pair_name: request.pair_name,
                left: left_queue,
                right: right_queue,
                baseline_path: baseline.to_string_lossy().into_owned(),
                resolutions: request.resolutions,
                expected_baseline_digest: preview.plan.baseline_digest,
                expected_plan_digest: preview.plan.plan_digest,
            };
            crate::queue_runtime::store_profile_bound_pending(
                &app,
                &state,
                &window_state.pending_bidirectional,
                &profile_bindings,
                PendingBidirectionalRun {
                    prepared,
                    operation: Some(operation),
                    profile_bindings: profile_bindings.clone(),
                    recovery_operation_id: None,
                    recovery_preparation_id: None,
                    enqueued_operation_id: None,
                },
            )?;
            Ok(preview)
        }, |_| {})?
    })
    .await
    .map_err(|_| AppError::new("app", "Bidirektionaler Vorbereitungsworker wurde unterbrochen"))?
}

#[tauri::command]
pub(crate) fn discard_bidirectional_sync(
    window: WebviewWindow,
    state: State<AppState>,
) -> Result<(), AppError> {
    let window_state = state.window(window.label());
    let mut pending = window_state.pending_bidirectional.lock_recover();
    let prepared = pending
        .as_ref()
        .ok_or_else(|| AppError::new("app", "Keine bidirektionale Vorbereitung vorhanden"))?;
    if prepared.operation.is_none() {
        return Err(AppError::new(
            "app",
            "Eine Resume-Vorbereitung kann nur fortgesetzt oder zurückgerollt werden",
        ));
    }
    if prepared.enqueued_operation_id.is_some() {
        return Err(AppError::new(
            "busy",
            "Der bewiesene Queueeintrag wartet noch auf den Journalübergang; Einreihen erneut versuchen",
        ));
    }
    prepared.prepared.discard().map_err(AppError::from)?;
    pending.take();
    Ok(())
}

#[tauri::command]
pub(crate) fn enqueue_bidirectional_sync(
    app: tauri::AppHandle,
    window: WebviewWindow,
    state: State<AppState>,
) -> Result<QueueSnapshot, AppError> {
    let label = window.label();
    let window_state = state.window(label);
    let queue = with_queue_extension(&state, &window_state, || {
        let mut pending = window_state.pending_bidirectional.lock_recover();
        let prepared = pending
            .as_mut()
            .ok_or_else(|| AppError::new("app", "Keine bidirektionale Vorbereitung vorhanden"))?;
        let operation = prepared.operation.clone().ok_or_else(|| {
            AppError::new(
                "app",
                "Eine Resume-Vorbereitung kann nicht als neuer Queue-Lauf eingereiht werden",
            )
        })?;
        crate::queue_runtime::ensure_profile_bindings_current(&app, &prepared.profile_bindings)?;
        let queue = if let Some(operation_id) = &prepared.enqueued_operation_id {
            let queue = operation_queue(&app, &window_state, label)?;
            let exact_pending = queue.snapshot().entries.iter().any(|entry| {
                entry.operation_id == *operation_id
                    && entry.operation == operation
                    && entry.profile_bindings == prepared.profile_bindings
                    && entry.state == QueueState::Pending
            });
            if !exact_pending {
                return Err(AppError::new(
                    "outcome_unclear",
                    "Der bereits bewiesene bidirektionale Queueeintrag besitzt nicht mehr den erwarteten Zustand",
                ));
            }
            queue
        } else {
            reserve_operation(&window_state)?;
            match crate::queue_runtime::enqueue_operation_with_profile_bindings(
                &app,
                &window_state,
                label,
                &operation,
                &prepared.profile_bindings,
            ) {
                Ok((queue, entry)) => {
                    prepared.enqueued_operation_id = Some(entry.operation_id);
                    queue
                }
                Err(error) => {
                    window_state.active_cancel.lock_recover().take();
                    return Err(error);
                }
            }
        };
        // Erst der bewiesene Queuecommit erlaubt, das mutationslose Laufjournal zu
        // entfernen. Scheitert die Entfernung vor ihrer Wirkung, halten Pending-Slot
        // und Reservierung den Eintrag bis zum duplikatfreien Retry fest.
        prepared
            .prepared
            .finish_enqueue_transition()
            .map_err(AppError::from)?;
        pending.take();
        window_state.active_cancel.lock_recover().take();
        Ok(queue)
    })?;
    emit_queue_snapshot(&app, label, &queue);
    start_queue_worker(app, label.to_owned());
    Ok(queue.snapshot())
}

#[tauri::command]
pub(crate) async fn prepare_bidirectional_resume(
    app: tauri::AppHandle,
    window: WebviewWindow,
    operation_id: String,
) -> Result<BidirectionalRecoveryPreview<LocalBidirectionalPreview>, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let label = window.label();
        let window_state = state.window(label);
        crate::queue_runtime::with_reserved_cancellable_operation(
            &app,
            label,
            &window_state,
            |cancel| {
                ensure_bidirectional_preparation_active(cancel)?;
                if window_state.pending_bidirectional.lock_recover().is_some() {
                    return Err(AppError::new(
                        "busy",
                        "Es existiert bereits eine bidirektionale Vorbereitung",
                    ));
                }
                let queue = operation_queue(&app, &window_state, label)?;
                let (pair_id, pair_name, left, right, profile_bindings) =
                    recovery_operation(&queue, &operation_id)?;
                crate::queue_runtime::ensure_profile_bindings_current(&app, &profile_bindings)?;
                let baseline = baseline_path(&app, pair_id)?;
                let prepared = if matches!(&left, BidirectionalQueueEndpoint::Local { .. })
                    && matches!(&right, BidirectionalQueueEndpoint::Local { .. })
                {
                    prepare_resume_local_bidirectional(
                        &pair_name,
                        Path::new(left.root()),
                        Path::new(right.root()),
                        &baseline,
                    )
                } else {
                    prepare_resume_provider_bidirectional(
                        &pair_name,
                        left.connect(std::sync::Arc::new(crate::sftp_commands::KeyringResolver))?,
                        right
                            .connect(std::sync::Arc::new(crate::sftp_commands::KeyringResolver))?,
                        &baseline,
                    )
                }
                .map_err(AppError::from)?;
                // Recovery-Journale bleiben beim Abbruch ausdrücklich erhalten.
                ensure_bidirectional_preparation_active(cancel)?;
                let preview = prepared.preview().clone();
                let recovery_preparation_id = random_recovery_preparation_id()?;
                crate::queue_runtime::store_profile_bound_pending(
                    &app,
                    &state,
                    &window_state.pending_bidirectional,
                    &profile_bindings,
                    PendingBidirectionalRun {
                        prepared,
                        operation: None,
                        profile_bindings: profile_bindings.clone(),
                        recovery_operation_id: Some(operation_id),
                        recovery_preparation_id: Some(recovery_preparation_id.clone()),
                        enqueued_operation_id: None,
                    },
                )?;
                Ok(BidirectionalRecoveryPreview {
                    preview,
                    recovery_preparation_id,
                    recovery_kind: BidirectionalRecoveryKind::Resume,
                })
            },
            |_| {},
        )?
    })
    .await
    .map_err(|_| {
        AppError::new(
            "app",
            "Bidirektionaler Vorbereitungsworker wurde unterbrochen",
        )
    })?
}

#[tauri::command]
pub(crate) fn discard_bidirectional_recovery_preparation(
    window: WebviewWindow,
    state: State<AppState>,
    operation_id: String,
    recovery_preparation_id: String,
    recovery_kind: BidirectionalRecoveryKind,
) -> Result<BidirectionalRecoveryDiscardStatus, AppError> {
    let window_state = state.window(window.label());
    match recovery_kind {
        BidirectionalRecoveryKind::Resume => {
            let mut resume = window_state.pending_bidirectional.lock_recover();
            let should_discard = recovery_discard_decision(
                &operation_id,
                &recovery_preparation_id,
                resume.as_ref().and_then(|pending| {
                    Some((
                        pending.recovery_operation_id.as_deref()?,
                        pending.recovery_preparation_id.as_deref()?,
                    ))
                }),
            )?;
            let discarded = should_discard.then(|| resume.take()).flatten();
            drop(resume);
            // Das Backend-Objekt und seine Handles verschwinden, das Recovery-
            // Journal bleibt wegen `discard_allowed == false` ausdrücklich bestehen.
            drop(discarded);
        }
        BidirectionalRecoveryKind::Rollback => {
            let mut rollback = window_state.pending_bidirectional_rollback.lock_recover();
            let should_discard = recovery_discard_decision(
                &operation_id,
                &recovery_preparation_id,
                rollback.as_ref().map(|pending| {
                    (
                        pending.recovery_operation_id.as_str(),
                        pending.recovery_preparation_id.as_str(),
                    )
                }),
            )?;
            let discarded = should_discard.then(|| rollback.take()).flatten();
            drop(rollback);
            drop(discarded);
        }
    }
    Ok(BidirectionalRecoveryDiscardStatus { absent: true })
}

#[tauri::command(async)]
pub(crate) fn preview_bidirectional_conflicts(
    window: WebviewWindow,
    state: State<AppState>,
    resolutions: Vec<ConflictResolution>,
    expected_plan_digest: [u8; 32],
) -> Result<Vec<doppeldecker_core::bidirectional::BidirectionalConflictView>, AppError> {
    let window_state = state.window(window.label());
    let pending = window_state.pending_bidirectional.lock_recover();
    let prepared = pending.as_ref().ok_or_else(|| {
        AppError::new(
            "plan_drift",
            "Die Konfliktvorbereitung ist nicht mehr vorhanden",
        )
    })?;
    prepared
        .prepared
        .conflict_choices_after_decisions(&resolutions, expected_plan_digest)
        .map_err(AppError::from)
}

#[tauri::command]
pub(crate) fn run_bidirectional_sync(
    app: tauri::AppHandle,
    window: WebviewWindow,
    state: State<AppState>,
    confirmation: String,
    backup_confirmed: bool,
) -> Result<(), AppError> {
    let label = window.label().to_owned();
    let window_state = state.window(&label);
    let (cancel, pending_run, queue_owner) = with_queue_extension(&state, &window_state, || {
        let mut pending_slot = window_state.pending_bidirectional.lock_recover();
        let pending = pending_slot
            .as_ref()
            .ok_or_else(|| AppError::new("app", "Bidirektionale Vorbereitung fehlt im RAM"))?;
        if pending.prepared.preview().confirmation_phrase != confirmation {
            return Err(AppError::new(
                "confirmation_mismatch",
                "Bidirektionale Laufphrase stimmt nicht exakt überein",
            ));
        }
        if pending.prepared.preview().backup_confirmation_required && !backup_confirmed {
            return Err(AppError::new(
                "confirmation_mismatch",
                "Bidirektionaler Lauf benötigt ein bestätigtes unabhängiges Backup",
            ));
        }
        crate::queue_runtime::ensure_profile_bindings_current(&app, &pending.profile_bindings)?;
        // Die vollständige Queue-Absicht muss feststehen, bevor die Fenster-
        // Schranke reserviert wird. Selbst ein beschädigter RAM-Zustand kann sie
        // damit nicht ohne laufende Operation zurücklassen.
        let owner_intent = if let Some(operation_id) = pending.recovery_operation_id.clone() {
            BidirectionalRunQueueIntent::Recovery(operation_id)
        } else {
            BidirectionalRunQueueIntent::Direct(pending.operation.clone().ok_or_else(|| {
                AppError::new(
                    "app",
                    "Direkter bidirektionaler Lauf besitzt keine Queue-Absicht",
                )
            })?)
        };
        let queue = operation_queue(&app, &window_state, &label)?;
        let cancel = reserve_operation(&window_state)?;
        let owner = match owner_intent {
            BidirectionalRunQueueIntent::Recovery(operation_id) => BidirectionalRunQueueOwner {
                queue,
                operation_id,
                direct: false,
            },
            BidirectionalRunQueueIntent::Direct(operation) => {
                match queue.enqueue_claimed_with_profile_bindings(
                    operation,
                    pending.profile_bindings.clone(),
                ) {
                    Ok(entry) => BidirectionalRunQueueOwner {
                        queue,
                        operation_id: entry.operation_id,
                        direct: true,
                    },
                    Err(error) => {
                        window_state.active_cancel.lock_recover().take();
                        return Err(AppError::from(error));
                    }
                }
            }
        };
        let pending = pending_slot.take().expect("unmittelbar zuvor geprüft");
        Ok((cancel, pending, owner))
    })?;
    let profile_bindings = pending_run.profile_bindings;
    let prepared = pending_run.prepared;
    std::thread::spawn(move || {
        let mut terminal_progress = None;
        let result = execute_profile_bound(
            || crate::queue_runtime::ensure_profile_bindings_current(&app, &profile_bindings),
            || {
                let run = prepared.confirm(&confirmation, backup_confirmed)?;
                if queue_owner.direct {
                    queue_owner.queue.mark_running(&queue_owner.operation_id)?;
                }
                let events = window.clone();
                let mut last = Instant::now() - Duration::from_secs(1);
                run.execute(&cancel, &mut |progress| {
                    let terminal = matches!(progress.phase.as_str(), "done" | "cancelled");
                    if terminal {
                        terminal_progress = Some(progress.clone());
                    } else if last.elapsed() >= Duration::from_millis(50) {
                        last = Instant::now();
                        let _ = crate::emit_to_window(&events, "bidirectional-progress", progress);
                    }
                })
            },
        );
        match result {
            Ok(stats) => {
                let progress = terminal_progress.clone().unwrap_or_else(|| Progress {
                    phase: "done".into(),
                    ..Progress::default()
                });
                let queue_result = queue_owner
                    .queue
                    .finish_bidirectional_after_effect(&queue_owner.operation_id, false, progress)
                    .and_then(|()| {
                        queue_owner
                            .queue
                            .finish_bidirectional_completion_recovery(&queue_owner.operation_id)
                    });
                finish_operation(&app, &label, || {
                    emit_queue_snapshot(&app, &label, &queue_owner.queue);
                    match queue_result {
                        Ok(()) => {
                            if let Some(progress) = terminal_progress {
                                let _ = crate::emit_to_window(
                                    &window,
                                    "bidirectional-progress",
                                    progress,
                                );
                            }
                            let _ = crate::emit_to_window(&window, "bidirectional-done", stats);
                        }
                        Err(error) => {
                            // Die Datenwirkung ist bereits erfolgt. Ohne bewiesenen
                            // terminalen Queuecommit darf das Frontend deshalb kein
                            // Erfolgsereignis erhalten und den Lauf auch nicht neu starten.
                            let _ = crate::emit_to_window(
                                &window,
                                "bidirectional-error",
                                AppError::from(error),
                            );
                        }
                    }
                });
            }
            Err(error) => {
                // Auch ein Resume startet aus `Interrupted`. Scheitert erst der
                // mutationslose Core-Journalabschluss, muss derselbe Eintrag auf
                // Completion-Recovery wechseln und darf nicht Rollback anbieten.
                let queue_error = queue_owner
                    .queue
                    .interrupt_bidirectional(
                        &queue_owner.operation_id,
                        terminal_progress.clone().unwrap_or_default(),
                        error.message.clone(),
                    )
                    .err();
                finish_operation(&app, &label, || {
                    emit_queue_snapshot(&app, &label, &queue_owner.queue);
                    if let Some(queue_error) = queue_error {
                        let _ = crate::emit_to_window(
                            &window,
                            "queue-error",
                            AppError::from(queue_error),
                        );
                    }
                    let _ = crate::emit_to_window(&window, "bidirectional-error", error);
                });
            }
        }
    });
    Ok(())
}

/// Der Queue-Worker führt nur den frischen Doppel-Scan aus. Die einmalige Phrase
/// bestätigt der Nutzer danach über einen separaten Command.
pub(crate) fn prepare_queue_entry(
    app: &tauri::AppHandle,
    label: &str,
    queue: &OperationQueue,
    entry: QueueEntry,
) -> QueueTerminalEvent {
    let operation_id = entry.operation_id.clone();
    let profile_bindings = entry.profile_bindings.clone();
    if let Err(error) =
        crate::queue_runtime::ensure_profile_bindings_current(app, &profile_bindings)
    {
        let _ = queue.interrupt_bidirectional(
            &operation_id,
            Progress::default(),
            error.message.clone(),
        );
        return QueueTerminalEvent::error("bidirectional-error", error);
    }
    match prepare_bidirectional_queue_entry(
        queue,
        entry,
        std::sync::Arc::new(crate::sftp_commands::KeyringResolver),
    ) {
        Ok(BidirectionalQueuePreparation::Ready(prepared)) => {
            let state = app.state::<AppState>();
            let window_state = state.window(label);
            let pending = PendingBidirectionalQueueRun {
                prepared: *prepared,
                profile_bindings: profile_bindings.clone(),
            };
            if let Err(error) = crate::queue_runtime::store_profile_bound_pending(
                app,
                &state,
                &window_state.pending_bidirectional_queue,
                &profile_bindings,
                pending,
            ) {
                let error = crate::queue_runtime::reset_consumed_remote_preparation(
                    queue,
                    &operation_id,
                    error,
                );
                return QueueTerminalEvent::error("bidirectional-error", error);
            }
            QueueTerminalEvent::message(
                "bidirectional-ready",
                "Bidirektionaler Lauf ist geprüft; Queue öffnen und Laufphrase bestätigen",
            )
        }
        Ok(BidirectionalQueuePreparation::AwaitingConfirmation { reason }) => {
            QueueTerminalEvent::message("bidirectional-error", reason)
        }
        Err(error) => {
            // Auch ein früher Fehler ohne Laufjournal bleibt bidirektional: Der
            // spezielle Übergang ordnet ihn als nicht-recoverbaren Interrupted-
            // Zustand ein, ohne die späteren Journal-/Markergrenzen zu umgehen.
            let _ = queue.interrupt_bidirectional(
                &operation_id,
                Progress::default(),
                error.to_string(),
            );
            QueueTerminalEvent::error("bidirectional-error", AppError::from(error))
        }
    }
}

#[tauri::command]
pub(crate) fn approve_bidirectional_queue_entry(
    app: tauri::AppHandle,
    window: WebviewWindow,
    state: State<AppState>,
    operation_id: String,
    confirmation: String,
    backup_confirmed: bool,
) -> Result<QueueSnapshot, AppError> {
    let label = window.label().to_owned();
    let window_state = state.window(&label);
    let queue = operation_queue(&app, &window_state, &label)?;
    crate::queue_runtime::ensure_queue_pending_current_or_reset(
        &app,
        &label,
        &queue,
        &window_state.pending_bidirectional_queue,
        &operation_id,
        |pending| &pending.profile_bindings,
        |pending| pending.prepared.operation_id(),
    )?;
    let (cancel, prepared, ()) = take_pending_for_operation(
        &state,
        &window_state,
        &window_state.pending_bidirectional_queue,
        "Bidirektionale Queue-Vorbereitung fehlt im RAM",
        |prepared| {
            if prepared.prepared.operation_id() != operation_id {
                return Err(AppError::new(
                    "app",
                    "Bidirektionale Queue-Vorbereitung gehört zu einem anderen Eintrag",
                ));
            }
            if prepared.prepared.preview().confirmation_phrase != confirmation {
                return Err(AppError::new(
                    "confirmation_mismatch",
                    "Bidirektionale Laufphrase stimmt nicht exakt überein",
                ));
            }
            if prepared.prepared.preview().backup_confirmation_required && !backup_confirmed {
                return Err(AppError::new(
                    "confirmation_mismatch",
                    "Bidirektionaler Lauf benötigt ein bestätigtes unabhängiges Backup",
                ));
            }
            Ok(())
        },
    )?;
    let profile_bindings = prepared.profile_bindings;
    let prepared = prepared.prepared;
    let queue_for_worker = queue.clone();
    let operation_for_worker = operation_id.clone();
    std::thread::spawn(move || {
        let events = window.clone();
        let mut last = Instant::now() - Duration::from_secs(1);
        let mut terminal_progress = None;
        let result = execute_profile_bound(
            || crate::queue_runtime::ensure_profile_bindings_current(&app, &profile_bindings),
            || {
                doppeldecker_core::execute_prepared_bidirectional_queue_entry(
                    &queue_for_worker,
                    prepared,
                    &confirmation,
                    backup_confirmed,
                    &cancel,
                    &mut |progress| {
                        let terminal = matches!(progress.phase.as_str(), "done" | "cancelled");
                        if terminal {
                            terminal_progress = Some(progress.clone());
                        } else if last.elapsed() >= Duration::from_millis(50) {
                            last = Instant::now();
                            let _ =
                                crate::emit_to_window(&events, "bidirectional-progress", progress);
                        }
                    },
                )
            },
        );
        finish_operation(&app, &label, || {
            if let Some(progress) = terminal_progress {
                let _ = crate::emit_to_window(&window, "bidirectional-progress", progress);
            }
            match result {
                Ok(QueueExecutionResult::Bidirectional(stats)) => {
                    emit_queue_snapshot(&app, &label, &queue_for_worker);
                    let _ = crate::emit_to_window(&window, "bidirectional-done", stats);
                }
                Ok(_) => {
                    emit_queue_snapshot(&app, &label, &queue_for_worker);
                    let _ = crate::emit_to_window(
                        &window,
                        "bidirectional-error",
                        AppError::new(
                            "app",
                            "Bidirektionale Queue lieferte einen unerwarteten Ergebnistyp",
                        ),
                    );
                }
                Err(mut error) => {
                    let still_waiting = queue_for_worker.snapshot().entries.iter().any(|entry| {
                        entry.operation_id == operation_for_worker
                            && entry.state == doppeldecker_core::QueueState::AwaitingConfirmation
                    });
                    if still_waiting {
                        error = crate::queue_runtime::reset_consumed_remote_preparation(
                            &queue_for_worker,
                            &operation_for_worker,
                            error,
                        );
                    }
                    emit_queue_snapshot(&app, &label, &queue_for_worker);
                    let _ = crate::emit_to_window(&window, "bidirectional-error", error);
                }
            }
        });
    });
    Ok(queue.snapshot())
}

#[tauri::command]
pub(crate) async fn prepare_bidirectional_rollback(
    app: tauri::AppHandle,
    window: WebviewWindow,
    operation_id: String,
) -> Result<BidirectionalRecoveryPreview<LocalBidirectionalRollbackPreview>, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let label = window.label();
        let window_state = state.window(label);
        crate::queue_runtime::with_reserved_cancellable_operation(
            &app,
            label,
            &window_state,
            |cancel| {
                ensure_bidirectional_preparation_active(cancel)?;
                if window_state
                    .pending_bidirectional_rollback
                    .lock_recover()
                    .is_some()
                {
                    return Err(AppError::new(
                        "busy",
                        "Es existiert bereits eine Rollback-Vorbereitung",
                    ));
                }
                let queue = operation_queue(&app, &window_state, label)?;
                let (pair_id, pair_name, left, right, profile_bindings) =
                    recovery_operation(&queue, &operation_id)?;
                crate::queue_runtime::ensure_profile_bindings_current(&app, &profile_bindings)?;
                let baseline = baseline_path(&app, pair_id)?;
                let prepared = if matches!(&left, BidirectionalQueueEndpoint::Local { .. })
                    && matches!(&right, BidirectionalQueueEndpoint::Local { .. })
                {
                    prepare_local_bidirectional_rollback(
                        &pair_name,
                        Path::new(left.root()),
                        Path::new(right.root()),
                        &baseline,
                    )
                } else {
                    prepare_provider_bidirectional_rollback(
                        &pair_name,
                        left.connect(std::sync::Arc::new(crate::sftp_commands::KeyringResolver))?,
                        right
                            .connect(std::sync::Arc::new(crate::sftp_commands::KeyringResolver))?,
                        &baseline,
                    )
                }
                .map_err(AppError::from)?;
                // Recovery-Journale bleiben beim Abbruch ausdrücklich erhalten.
                ensure_bidirectional_preparation_active(cancel)?;
                let preview = prepared.preview().clone();
                let recovery_preparation_id = random_recovery_preparation_id()?;
                crate::queue_runtime::store_profile_bound_pending(
                    &app,
                    &state,
                    &window_state.pending_bidirectional_rollback,
                    &profile_bindings,
                    PendingBidirectionalRollback {
                        prepared,
                        recovery_operation_id: operation_id,
                        recovery_preparation_id: recovery_preparation_id.clone(),
                        profile_bindings: profile_bindings.clone(),
                    },
                )?;
                Ok(BidirectionalRecoveryPreview {
                    preview,
                    recovery_preparation_id,
                    recovery_kind: BidirectionalRecoveryKind::Rollback,
                })
            },
            |_| {},
        )?
    })
    .await
    .map_err(|_| {
        AppError::new(
            "app",
            "Bidirektionaler Vorbereitungsworker wurde unterbrochen",
        )
    })?
}

#[tauri::command]
pub(crate) fn run_bidirectional_rollback(
    app: tauri::AppHandle,
    window: WebviewWindow,
    state: State<AppState>,
    confirmation: String,
) -> Result<(), AppError> {
    let label = window.label().to_owned();
    let window_state = state.window(&label);
    let (cancel, pending_rollback, (queue, operation_id)) = take_pending_for_operation(
        &state,
        &window_state,
        &window_state.pending_bidirectional_rollback,
        "Rollback-Vorbereitung fehlt im RAM",
        |pending| {
            if pending.prepared.preview().confirmation_phrase != confirmation {
                return Err(AppError::new(
                    "confirmation_mismatch",
                    "Rollback-Phrase stimmt nicht exakt überein",
                ));
            }
            let queue = operation_queue(&app, &window_state, &label)?;
            Ok((queue, pending.recovery_operation_id.clone()))
        },
    )?;
    let profile_bindings = pending_rollback.profile_bindings;
    let prepared = pending_rollback.prepared;
    std::thread::spawn(move || {
        let result = execute_profile_bound(
            || crate::queue_runtime::ensure_profile_bindings_current(&app, &profile_bindings),
            || prepared.confirm(&confirmation)?.execute(&cancel),
        );
        match result {
            Ok(stats) => {
                let queue_result = queue
                    .finish_bidirectional_after_effect(&operation_id, true, Progress::default())
                    .and_then(|()| {
                        // Der erste Schritt hat den Rollback bereits terminal in der
                        // Queue gebunden. Erst jetzt darf der mutationslose Core-Marker
                        // verschwinden; ein Fehler kann daher nie einen Daten-Retry
                        // freigeben.
                        queue.finish_bidirectional_completion_recovery(&operation_id)
                    });
                finish_operation(&app, &label, || {
                    emit_queue_snapshot(&app, &label, &queue);
                    match queue_result {
                        Ok(()) => {
                            let _ = crate::emit_to_window(
                                &window,
                                "bidirectional-rollback-done",
                                stats,
                            );
                        }
                        Err(error) => {
                            let _ = crate::emit_to_window(
                                &window,
                                "bidirectional-error",
                                AppError::from(error),
                            );
                        }
                    }
                });
            }
            Err(error) => {
                finish_operation(&app, &label, || {
                    let _ = crate::emit_to_window(&window, "bidirectional-error", error);
                });
            }
        }
    });
    Ok(())
}

/// Schließt ausschließlich den persistierten Queue-Marker nach bereits erfolgter
/// Datenwirkung. Der Befehl öffnet weder Pane-Pfade noch Provider und kann deshalb
/// niemals den bidirektionalen Lauf ein zweites Mal ausführen.
#[tauri::command]
pub(crate) fn finish_bidirectional_completion_recovery(
    app: tauri::AppHandle,
    window: WebviewWindow,
    state: State<AppState>,
    operation_id: String,
) -> Result<QueueSnapshot, AppError> {
    let label = window.label();
    let window_state = state.window(label);
    let queue = with_queue_extension(&state, &window_state, || {
        let queue = operation_queue(&app, &window_state, label)?;
        queue
            .finish_bidirectional_completion_recovery(&operation_id)
            .map_err(AppError::from)?;
        Ok(queue)
    })?;
    emit_queue_snapshot(&app, label, &queue);
    start_queue_worker(app, label.to_owned());
    Ok(queue.snapshot())
}

/// Recovery-Pfade stammen ausschließlich aus dem backend-seitig persistierten
/// Queue-Eintrag. Das Frontend darf damit weder andere Wurzeln noch eine fremde
/// Baseline unter eine vorhandene Operation schieben.
fn recovery_operation(
    queue: &OperationQueue,
    operation_id: &str,
) -> Result<
    (
        PairId,
        String,
        BidirectionalQueueEndpoint,
        BidirectionalQueueEndpoint,
        Vec<doppeldecker_core::RemoteProfileBinding>,
    ),
    AppError,
> {
    // Zwei vor dem Rename gescheiterte Queue-Schreibversuche verändern die
    // In-Memory-Sicht nicht. Der Core-Marker kann die Wirkung trotzdem bereits als
    // vollständig belegen; deshalb wird unmittelbar vor jedem Recovery-Einstieg
    // ohne Providerkontakt neu klassifiziert.
    queue
        .refresh_bidirectional_completion_recovery(operation_id)
        .map_err(AppError::from)?;
    let snapshot = queue.snapshot();
    let entry = snapshot
        .entries
        .iter()
        .find(|entry| entry.operation_id == operation_id)
        .ok_or_else(|| AppError::new("app", "Bidirektionaler Queue-Eintrag fehlt"))?;
    if !matches!(entry.state, QueueState::Failed | QueueState::Interrupted) {
        return Err(AppError::new(
            "app",
            "Bidirektionaler Queue-Eintrag benötigt kein Recovery",
        ));
    }
    if entry.bidirectional_completion_recovery.is_some() || !entry.recovery_available {
        return Err(AppError::new(
            "outcome_unclear",
            "Die Datenwirkung ist bereits abgeschlossen; nur der persistierte Queue-Abschluss darf geprüft werden",
        ));
    }
    let QueueOperation::Bidirectional {
        pair_id,
        pair_name,
        left,
        right,
        ..
    } = &entry.operation
    else {
        return Err(AppError::new(
            "app",
            "Queue-Eintrag ist kein bidirektionaler Lauf",
        ));
    };
    Ok((
        *pair_id,
        pair_name.clone(),
        (**left).clone(),
        (**right).clone(),
        entry.profile_bindings.clone(),
    ))
}
