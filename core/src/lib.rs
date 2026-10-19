//! # doppeldecker-core
//!
//! Der plattform- und GUI-unabhängige Kern von Doppeldecker (dual-pane File-Manager).
//!
//! Hier steckt die korrektheitskritische Logik: das Auflisten/Kopieren/Löschen von
//! Dateien über eine Dateisystem-Abstraktion ([`Vfs`]) und die Ordner-**Synchronisation**
//! ([`sync`]). Bewusst OHNE Tauri-/GUI-Abhängigkeiten — dadurch kompiliert und testet
//! sich dieser Kern identisch auf macOS und Linux (auch auf einem headless Server),
//! genau die Teile also, bei denen Fehler wehtun (Sync kann Dateien löschen!).
//!
//! Aufbau:
//! - `atomic_write` — gemeinsamer Write/Sync/Rename-Pfad für kleine Journale.
//! - [`vfs`]       — [`Vfs`]-Trait + lokale Implementierung [`LocalVfs`]. Remote-Provider
//!   (SFTP, FTP/FTPS, WebDAV und SMB) implementieren später dasselbe Trait.
//! - [`sync`]      — Diff-Berechnung (Dry-Run-Plan) und Ausführung.
//! - [`fileops`]   — interaktive Datei-Operationen (Kopieren/Verschieben/Umbenennen/
//!   Ordner anlegen) mit Konflikt-Erkennung und Sandbox-Schranke.
//! - [`favorites`] — Schnellzugriff-Ordner (Seitenleiste), als JSON persistiert.
//! - [`directory_size`] — parallele, abbrechbare und sitzungsweise gecachte Ordnergrößen.
//! - [`finder_favorites`] — Finder-Seitenleisten-Favoriten (macOS) fail-closed parsen.
//! - [`trash`]      — descriptor-relativer Papierkorb mit Claim und Crash-Journal.
//! - [`quarantine`] — Sync-Läufe auflisten, wiederherstellen und bestätigt leeren.
//! - [`queue`]      — atomar persistierte Operations-Queue samt Crash-Recovery.
//! - [`metadata`]   — Stufe-2-Fähigkeiten und kanonische Metadatenmanifeste.
//! - [`path`]       — gemeinsame, verlustfreie Expansion des eigenen Home-Kürzels.
//! - [`progress`]  — Fortschritts-Meldung und Abbruch-Signal für lang laufende Vorgänge.

mod acl;
mod atomic_file;
mod atomic_write;
pub mod bidirectional;
pub mod connections;
pub mod directory_size;
pub mod error;
pub mod favorites;
pub mod fileops;
pub mod finder_favorites;
pub mod ftps;
mod local_fs;
mod local_read;
mod local_staging;
pub mod metadata;
pub mod network_discovery;
pub mod path;
pub mod progress;
pub mod quarantine;
pub mod queue;
pub mod real_mode;
pub mod remote;
pub mod remote_delete;
pub mod remote_real_mode;
pub mod remote_rename;
pub mod remote_snapshot;
mod rename_interaction;
pub mod sftp;
pub mod smb;
pub mod sync;
pub mod system_mounts;
pub mod trash;
mod tree_measure;
pub mod vfs;
pub mod webdav;

pub use bidirectional::{
    bind_bidirectional_endpoint_compatibility, build_tree_snapshot,
    discard_unchanged_bidirectional_run, generate_conflict_path, load_bidirectional_run_journal,
    plan_bidirectional, plan_provider_bidirectional, prepare_local_bidirectional,
    prepare_local_bidirectional_rollback, prepare_local_bidirectional_with_decisions,
    prepare_provider_bidirectional_rollback, prepare_provider_bidirectional_with_decisions,
    prepare_resume_local_bidirectional, prepare_resume_provider_bidirectional,
    prepare_resume_sftp_bidirectional, prepare_sftp_bidirectional_rollback,
    prepare_sftp_bidirectional_with_decisions, resolve_conflicts, BaselineStore,
    BidirectionalAction, BidirectionalBaseline, BidirectionalPlan, BidirectionalProjectionJournal,
    BidirectionalProjectionKind, BidirectionalProjectionState, BidirectionalRollbackPhase,
    BidirectionalRollbackQuarantine, BidirectionalRollbackStep, BidirectionalRunJournal,
    BidirectionalRunState, BidirectionalTypeSwapPhase, BidirectionalTypeSwapStep,
    BinaryRelativePath, ConflictCopyPath, ConflictDecision, ConflictKind, ConflictResolution,
    EndpointBinding, EndpointSide, LocalBidirectionalPreview, LocalBidirectionalRollback,
    LocalBidirectionalRollbackPreview, LocalBidirectionalRollbackStats, LocalBidirectionalRun,
    LocalBidirectionalStats, PairId, PreparedLocalBidirectionalRollback,
    PreparedLocalBidirectionalRun, PreparedProviderBidirectionalRollback,
    PreparedProviderBidirectionalRun, ProviderBidirectionalPlanPreview,
    ProviderBidirectionalRollback, ProviderBidirectionalRollbackStats, ProviderBidirectionalRun,
    ProviderBidirectionalStats, SnapshotLimits, SyncDirection, TreeSnapshot,
};
pub use connections::{
    latest_ftps_profile_revision, latest_sftp_profile_revision, latest_smb_profile_revision,
    latest_webdav_profile_revision, load_ftps_profiles, load_ftps_profiles_versioned,
    load_sftp_profiles, load_sftp_profiles_versioned, load_smb_profiles,
    load_smb_profiles_versioned, load_webdav_profiles, load_webdav_profiles_versioned,
    remove_ftps_profile, remove_ftps_profile_versioned, remove_ftps_profile_versioned_if_current,
    remove_sftp_profile, remove_sftp_profile_versioned, remove_sftp_profile_versioned_if_current,
    remove_smb_profile, remove_smb_profile_versioned, remove_smb_profile_versioned_if_current,
    remove_webdav_profile, remove_webdav_profile_versioned,
    remove_webdav_profile_versioned_if_current, save_ftps_profile, save_ftps_profile_versioned,
    save_ftps_profile_versioned_if_current, save_sftp_profile, save_sftp_profile_versioned,
    save_sftp_profile_versioned_if_current, save_smb_profile, save_smb_profile_versioned,
    save_smb_profile_versioned_if_current, save_webdav_profile, save_webdav_profile_versioned,
    save_webdav_profile_versioned_if_current, FtpsProfile, ProfileMutation, RemoteProfileBinding,
    RemoteProfileProtocol, SftpProfile, SmbProfile, VersionedProfile, WebDavProfile,
};
pub use directory_size::{
    DirectorySizeIssue, DirectorySizeIssueKind, DirectorySizeMode, DirectorySizeResult,
    DirectorySizeSession, DirectorySizeStats, DirectorySizeStatus,
};
mod directory_create;
pub use directory_create::{
    directory_create_contract, validate_directory_create_class, validate_directory_create_name,
    validate_directory_create_request, DirectoryCreateContract, DirectoryCreateExecutionClass,
    DirectoryCreateReport,
};

pub use error::{capability_probe_required, coded_error, error_code, ErrorCode};
pub use favorites::{Favorite, FavoriteProvider};
pub use fileops::{
    bind_local_file_operation_scope, capture_file_conflict_proofs, copy_items, create_dir,
    find_recursive_conflicts, move_items, validate_file_name, validate_file_operation_paths,
    validate_file_operation_root, validate_file_operation_scope,
    validate_interactive_rename_request, FileConflictProof, FileOperationRequest, OpStats,
    OverwritePolicy,
};
pub use ftps::{discover_ftps_certificate, FtpsInteractiveDirectoryListing, FtpsVfs};
pub use metadata::{
    Acl, AclModel, BlobSummary, Capability, DataExtent, ForkKind, MetadataCapabilities,
    MetadataManifest, MetadataProfile, NamedBlobSummary, Nfs4AceKind, Nfs4Acl, Nfs4AclEntry, Owner,
    PosixAcl, PosixAclEntry, PosixAclTag, XattrValue,
};
pub use network_discovery::{
    discover_network_services, DiscoveredNetworkService, NetworkServiceProtocol,
};
pub use path::expand_home_path;
pub use progress::{Cancel, Progress};
pub use quarantine::{
    lease_quarantine_run_for_target, list_quarantine_entries_for_target, list_quarantine_runs,
    purge_quarantine_run_for_target, purge_quarantine_run_for_target_with_lease,
    restore_quarantine_entry_for_target, restore_quarantine_entry_for_target_with_lease,
    QuarantineEntry, QuarantineLease, QuarantineRestoreOutcome, QuarantineRestoreResult,
    QuarantineRun, QuarantineRunStatus, QuarantineScope, QuarantineState,
    QUARANTINE_RETENTION_DAYS,
};
pub use queue::{
    execute_local_queue_entry, execute_prepared_bidirectional_queue_entry,
    execute_prepared_sftp_queue_entry, execute_provider_delete_queue_entry,
    execute_provider_file_queue_entry, prepare_bidirectional_queue_entry, prepare_ftps_queue_entry,
    prepare_provider_queue_entry, prepare_sftp_queue_entry, prepare_smb_queue_entry,
    prepare_webdav_queue_entry, BidirectionalQueueEndpoint, BidirectionalQueuePreparation,
    FileQueueKind, LocalQueueExecution, OperationQueue, OperationQueueProcessLock,
    PreparedBidirectionalQueueRun, PreparedSftpQueueRun, QueueAttention, QueueEntry,
    QueueExecutionResult, QueueOperation, QueueOperationClass, QueueSnapshot, QueueState,
    SftpQueueDirection, SftpQueuePreparation, QUEUE_JOURNAL_VERSION,
};
pub use real_mode::{
    prepare_local_real_run, prepare_local_sync_run, prepare_local_sync_run_with_sandbox,
    ListingError, ListingProof, LocalRealRun, PreparedLocalRealRun, PreparedLocalSyncRun,
    RealRunImpact, RealRunPreview, RootIdentity, REAL_PERMIT_LIFETIME,
};
pub use remote::{
    FtpSecurityStatus, FtpTransportSecurity, FtpsCertificatePin, FtpsCertificatePolicy,
    FtpsConnectionConfig, FtpsRootIdentity, FtpsServerIdentity, KeychainSecretReference,
    RemoteCapabilities, RemoteProtocol, SftpAuthenticationReference, SftpConnectionConfig,
    SftpHostKeyPin, SftpRootIdentity, SftpServerIdentity, SmbConnectionConfig, SmbRootIdentity,
    SmbServerIdentity, WebDavCertificatePin, WebDavCertificatePolicy, WebDavConnectionConfig,
    WebDavRootIdentity, WebDavServerIdentity,
};
pub use remote_delete::{
    execute_remote_delete, plan_remote_delete, validate_remote_delete_request, RemoteDeleteItem,
    RemoteDeleteMode, RemoteDeletePlan, RemoteDeleteRecord, RemoteDeleteSourceProof,
    RemoteDeleteStats,
};
pub use remote_real_mode::{
    interactive_file_preparation_notice, prepare_ftps_download, prepare_ftps_upload,
    prepare_provider_transfer, prepare_sftp_download, prepare_sftp_upload, prepare_smb_download,
    prepare_smb_upload, prepare_webdav_download, prepare_webdav_upload,
    validate_interactive_file_transfer_request, InteractiveFileContexts,
    InteractiveListingOmission, PreparedProviderRealRun, PreparedSftpRealRun,
    ProviderDeleteCapabilities, ProviderEndpoint, ProviderFileCapabilities,
    ProviderFileTargetPreparation, ProviderFileTargetProbe, ProviderInteractiveEntry,
    ProviderInteractiveListing, ProviderKind, ProviderRealRun, ProviderTransferCapabilities,
    SftpRealRun,
};
pub use remote_rename::{
    ProviderRenameCapabilities, RenameExecutionClass, RenameOutcome, RenameReport,
    SOURCE_PATH_BINDING_LIMITATION,
};
pub use remote_snapshot::{
    cleanup_remote_snapshots, download_remote_snapshot, RemoteSnapshot, SNAPSHOT_RETENTION,
};
pub use rename_interaction::{
    ordinary_rename_contract, validate_ordinary_rename_class, validate_ordinary_rename_request,
    RenameInteractionContract,
};
pub use sftp::{
    discover_sftp_host_key, list_sftp_agent_keys, sftp_host_key_is_known, SecretResolver,
    SecretString, SftpPublicKeyIdentity, SftpReadSecurity, SftpVfs,
};
pub use smb::{discover_smb_server, SmbInteractiveDirectoryListing, SmbServerDiscovery, SmbVfs};
pub use sync::{
    execute, plan, validate_local_sync_roots, validate_test_sandbox_root, Action, Compare,
    ComparisonAction, ComparisonCategoryCounts, ComparisonEntry, ComparisonEntryKind,
    ComparisonRow, ComparisonSide, ExecuteRequest, ExecutionMode, MetadataClass,
    MetadataClassStats, MetadataIssue, MetadataIssueKind, MetadataReport, MetadataTransfer, Safety,
    SyncComparison, SyncMode, SyncOptions, SyncPlan, SyncStats, TEST_SANDBOX_MARKER,
    TEST_SANDBOX_MARKER_CONTENT,
};
pub use system_mounts::{
    apply_macos_diskutil_info, list_system_mounts, SystemMount, SystemMountKind, SystemUnmount,
};
pub use trash::{recover_trash_journals, trash_items, RecoveryStats, TrashStats};
pub use vfs::{Entry, LocalMountIdentity, LocalRootIdentity, LocalVfs, Vfs};
pub use webdav::{discover_webdav_certificate, WebDavInteractiveDirectoryListing, WebDavVfs};

pub mod conflict_view;
