//! Ermittelt JSON-Argumente und eindeutig ableitbare Antworten registrierter Tauri-Kommandos.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use heck::ToLowerCamelCase;
use quote::ToTokens;
use syn::{
    Attribute, Expr, FnArg, GenericArgument, Item, Lit, LitStr, Meta, Pat, PathArguments,
    ReturnType, Type,
};

pub(crate) struct CommandSignature {
    pub(crate) arguments: BTreeMap<String, String>,
    wire_result: Option<String>,
}

struct RustSource {
    path: PathBuf,
    items: Vec<Item>,
}

impl RustSource {
    fn read(path: PathBuf) -> Self {
        let source =
            fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let file =
            syn::parse_file(&source).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        Self {
            path,
            items: file.items,
        }
    }
}

fn child_module_path(file: &Path, module: &syn::ItemMod) -> PathBuf {
    let parent = file.parent().expect("Rust-Datei ohne Stammordner");
    if let Some(attribute) = module
        .attrs
        .iter()
        .find(|attr| attr.path().is_ident("path"))
    {
        let Meta::NameValue(pair) = &attribute.meta else {
            panic!("Unerwartete Modulpfad-Syntax")
        };
        let Expr::Lit(literal) = &pair.value else {
            panic!("Modulpfad muss ein Literal sein")
        };
        let Lit::Str(path) = &literal.lit else {
            panic!("Modulpfad muss Text sein")
        };
        return parent.join(path.value());
    }
    let module_directory = match file.file_stem().and_then(|stem| stem.to_str()) {
        Some("main" | "lib" | "mod") => parent.to_path_buf(),
        Some(stem) => parent.join(stem),
        None => panic!("Rust-Datei ohne Modulnamen"),
    };
    let name = module.ident.to_string();
    let flat = module_directory.join(format!("{name}.rs"));
    if flat.is_file() {
        flat
    } else {
        module_directory.join(name).join("mod.rs")
    }
}

fn registered_function(source: &RustSource, items: &[Item], path: &[&str]) -> CommandSignature {
    let (first, rest) = path
        .split_first()
        .expect("Leerer registrierter Kommandopfad");
    if !rest.is_empty() {
        let module = items
            .iter()
            .find_map(|item| match item {
                Item::Mod(module) if module.ident == *first => Some(module),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{}: Modul {first} fehlt", source.path.display()));
        if let Some((_, inner)) = &module.content {
            return registered_function(source, inner, rest);
        }
        let child = RustSource::read(child_module_path(&source.path, module));
        return registered_function(&child, &child.items, rest);
    }

    let function = items
        .iter()
        .find_map(|item| match item {
            Item::Fn(function) if function.sig.ident == *first => Some(function),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{}: Kommando {first} fehlt", source.path.display()));
    let command = function
        .attrs
        .iter()
        .find(|attribute| {
            let segments = &attribute.path().segments;
            segments.len() == 2 && segments[0].ident == "tauri" && segments[1].ident == "command"
        })
        .unwrap_or_else(|| {
            panic!(
                "{}: tauri::command für {first} fehlt",
                source.path.display()
            )
        });
    let camel_case = argument_case(command);

    let arguments = function
        .sig
        .inputs
        .iter()
        .filter_map(|argument| {
            let FnArg::Typed(argument) = argument else {
                panic!("{first}: Methoden sind keine IPC-Kommandos")
            };
            // Diese drei Typen bezieht Tauri aus dem Aufrufkontext statt aus JSON.
            if let Type::Path(path) = argument.ty.as_ref() {
                let name = &path.path.segments.last().expect("Leerer Typpfad").ident;
                if name == "AppHandle" || name == "State" || name == "WebviewWindow" {
                    return None;
                }
            }
            let Pat::Ident(name) = argument.pat.as_ref() else {
                panic!("{first}: Neuer Parameterausdruck braucht eine IPC-Vertragsregel")
            };
            let name = name.ident.to_string();
            let name = name.strip_prefix("r#").unwrap_or(&name);
            let key = if camel_case {
                name.to_lower_camel_case()
            } else {
                name.to_owned()
            };
            let rust_type = argument
                .ty
                .to_token_stream()
                .to_string()
                .split_whitespace()
                .collect();
            Some((key, rust_type))
        })
        .collect();
    let wire_result = match &function.sig.output {
        ReturnType::Default => Some("null".to_owned()),
        ReturnType::Type(_, ty) => {
            let success = type_parameter(ty, "Result").unwrap_or(ty);
            wire_type(success)
                .or_else(|| archive_read_wire_type(&source.path, first, success))
                .or_else(|| sync_preview_wire_type(first, success))
        }
    };
    CommandSignature {
        arguments,
        wire_result,
    }
}

fn type_parameter<'a>(ty: &'a Type, container: &str) -> Option<&'a Type> {
    let Type::Path(path) = ty else {
        return None;
    };
    let segment = path.path.segments.last()?;
    if segment.ident != container {
        return None;
    }
    let PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        return None;
    };
    match arguments.args.first()? {
        GenericArgument::Type(ty) => Some(ty),
        _ => None,
    }
}

fn wire_type(ty: &Type) -> Option<String> {
    if matches!(ty, Type::Tuple(tuple) if tuple.elems.is_empty()) {
        return Some("null".to_owned());
    }
    if let Some(inner) = type_parameter(ty, "Option") {
        return Some(format!("{} | null", wire_type(inner)?));
    }
    if let Some(inner) = type_parameter(ty, "Vec") {
        return Some(format!("Array<{}>", wire_type(inner)?));
    }
    if let Some(Type::Path(inner)) = type_parameter(ty, "BidirectionalRecoveryPreview") {
        return match inner.path.segments.last()?.ident.to_string().as_str() {
            "LocalBidirectionalPreview" => Some("BidirectionalResumePreviewWire".into()),
            "LocalBidirectionalRollbackPreview" => Some("BidirectionalRollbackPreviewWire".into()),
            _ => None,
        };
    }
    if let Some(inner) = type_parameter(ty, "VersionedProfile") {
        return Some(format!("VersionedProfileWire<{}>", wire_type(inner)?));
    }
    let Type::Path(path) = ty else {
        return None;
    };
    let segment = path.path.segments.last()?;
    if !matches!(segment.arguments, PathArguments::None) {
        return None;
    }
    let name = match segment.ident.to_string().as_str() {
        "ActiveConnection" => "ActiveConnectionWire",
        "ContentSyncQueuePreview" => "ContentSyncQueuePreviewWire",
        "ArchiveExtractPrepared" => "ArchiveExtractPreparedWire",
        "ArchivePackPrepared" => "ArchivePackPreparedWire",
        "BatchRenamePreview" => "BatchRenamePreviewWire",
        "BatchRenameReport" => "BatchRenameReportWire",
        "DefaultDirs" => "DefaultDirsWire",
        "DiffToolAvailability" => "DiffToolAvailabilityWire",
        "DuplicatePreview" => "DuplicatePreviewWire",
        "FilePreview" => "FilePreviewWire",
        "ProviderFilePreview" => "ProviderFilePreviewWire",
        "PreferencesSnapshot" => "PreferencesSnapshotWire",
        "SafetyInfo" => "SafetyInfoWire",
        "FrontendQueueSnapshot" => "QueueSnapshotWire",
        "QuarantineRun" => "QuarantineRunWire",
        "QuarantineEntry" => "QuarantineEntryWire",
        "QuarantineRestoreResponse" => "QuarantineRestoreWire",
        "FtpsConnectionStatus" => "FtpsConnectionStatusWire",
        "FtpsProfile" => "FtpsProfileWire",
        "SftpConnectionStatus" => "SftpConnectionStatusWire",
        "SftpProfile" => "SftpProfileWire",
        "SmbConnectionStatus" => "SmbConnectionStatusWire",
        "SmbProfile" => "SmbProfileWire",
        "WebDavConnectionStatus" => "WebDavConnectionStatusWire",
        "WebDavProfile" => "WebDavProfileWire",
        "FileFlagRow" => "FileFlagRowWire",
        "FinderCommentEditRow" => "FinderCommentEditRowWire",
        "FinderCommentReadRow" => "FinderCommentReadRowWire",
        "FinderTagEditRow" => "FinderTagEditRowWire",
        "FinderTagReadRow" => "FinderTagReadRowWire",
        "VisibilityEditRow" => "VisibilityEditRowWire",
        "VisibilityReadRow" => "VisibilityReadRowWire",
        "DirectoryCreateContract" => "DirectoryCreateContractWire",
        "LocalSyncPreview" => "LocalSyncPreviewWire",
        "SyncPlan" => "SyncPlanWire",
        "DirectoryCreateReport" => "DirectoryCreateReportWire",
        "ProviderDeletePreview" => "ProviderDeletePreviewWire",
        "RenameInteractionContract" => "RenameInteractionContractWire",
        "RenameProviderPathResponse" => "RenameProviderPathResponseWire",
        "RemoteEditView" => "RemoteEditViewWire",
        "CapturedConnectionSecret" => "CapturedConnectionSecretWire",
        "FtpsCertificatePin" => "CertificatePinWire",
        "WebDavCertificatePin" => "CertificatePinWire",
        "SftpHostKeyPin" => "HostKeyPinWire",
        "SmbServerDiscovery" => "SmbServerDiscoveryWire",
        "SftpPublicKeyIdentity" => "SftpPublicKeyIdentityWire",
        "WindowStartState" => "WindowStartStateWire",
        "WindowTabs" => "WindowTabsWire",
        "BidirectionalConflictView" => "BidirectionalConflictWire",
        "BidirectionalRecoveryDiscardStatus" => "BidirectionalRecoveryDiscardWire",
        "ForwardBidirectionalJournalStatus" => "ForwardBidirectionalPairStatusWire",
        "RegisteredBidirectionalPair" => "RegisteredBidirectionalPairWire",
        "ForwardBidirectionalPreview" => "ForwardBidirectionalPreviewWire",
        "LocalBidirectionalPreview" => "LocalBidirectionalPreviewWire",
        "AdminStatus" => "AdminStatusWire",
        "FilePropertyCapabilities" => "FilePropertyCapabilitiesWire",
        "OwnerEditRow" => "OwnerEditRowWire",
        "PermissionEditRow" => "PermissionEditRowWire",
        "SavedSyncJob" => "SavedSyncJobWire",
        "SystemMount" => "SystemMountWire",
        "UpdateCheck" => "UpdateCheckWire",
        "PreparedUpdateStatus" => "PreparedUpdateStatusWire",
        "CliInstallStatus" => "CliInstallStatusWire",
        "DiscoveredNetworkService" => "DiscoveredNetworkServiceWire",
        "NameSearchStarted" => "NameSearchStartedWire",
        "RecoveryOverview" => "RecoveryOverviewWire",
        "TrashStats" => "TrashStatsWire",
        "String" => "string",
        "bool" => "boolean",
        "u8" | "u16" | "u32" | "u64" | "usize" | "i8" | "i16" | "i32" | "i64" | "isize" => "number",
        _ => return None,
    };
    Some(name.to_owned())
}

fn archive_read_wire_type(source: &Path, command: &str, ty: &Type) -> Option<String> {
    // Die kurzen Rust-Namen gelten nur für die drei Antworten dieses Moduls.
    if !source.ends_with("archive_read_commands.rs") {
        return None;
    }
    let Type::Path(path) = ty else {
        return None;
    };
    let name = path.path.get_ident()?.to_string();
    match (command, name.as_str()) {
        ("open_archive_read", "Opened") => Some("ArchiveOpenedWire".into()),
        ("list_archive_directory", "Listing") => Some("ArchiveListingWire".into()),
        ("preview_archive_entry", "Previewed") => Some("ArchivePreviewedWire".into()),
        _ => None,
    }
}

fn sync_preview_wire_type(command: &str, ty: &Type) -> Option<String> {
    // RealRunPreview und ContentSyncPreview werden auch bei der Queue-Wiederaufnahme
    // verwendet. Diese Projektion deckt die regulären Vorbereitungsbefehle ab.
    let Type::Path(path) = ty else {
        return None;
    };
    let name = path.path.segments.last()?.ident.to_string();
    match (command, name.as_str()) {
        ("prepare_content_sync", "ContentSyncPreview") => Some("ContentSyncPreviewWire".into()),
        (
            "prepare_ftps_sync"
            | "prepare_provider_sync"
            | "prepare_real_sync"
            | "prepare_sftp_sync"
            | "prepare_smb_sync"
            | "prepare_webdav_sync",
            "RealRunPreview",
        ) => Some("RealRunPreviewWire".into()),
        _ => None,
    }
}

fn argument_case(attribute: &Attribute) -> bool {
    let mut camel_case = true;
    if matches!(&attribute.meta, Meta::List(_)) {
        // Tauri akzeptiert auch eigenes rename/root. Solche neuen Fälle sollen
        // vor einer Erweiterung des Vertrags ausdrücklich geprüft werden.
        attribute
            .parse_nested_meta(|meta| {
                if meta.path.is_ident("async") {
                    return Ok(());
                }
                if meta.path.is_ident("rename_all") {
                    let name: LitStr = meta.value()?.parse()?;
                    camel_case = match name.value().as_str() {
                        "camelCase" => true,
                        "snake_case" => false,
                        _ => return Err(meta.error("Unbekannte Tauri-Argumentnamensregel")),
                    };
                    return Ok(());
                }
                Err(meta.error("Tauri-Attribut braucht eine IPC-Vertragsregel"))
            })
            .expect("Tauri-Argumentnamensregel prüfen");
    }
    camel_case
}

pub(crate) fn derive(source: &Path, commands: &[&str]) -> BTreeMap<String, CommandSignature> {
    let root = RustSource::read(source.to_path_buf());
    let mut signatures = BTreeMap::new();
    for command in commands {
        let path: Vec<_> = command.split("::").map(str::trim).collect();
        let name = path.last().expect("Leerer Kommandopfad").to_string();
        let signature = registered_function(&root, &root.items, &path);
        assert!(
            signatures.insert(name, signature).is_none(),
            "Doppeltes Kommando {command}"
        );
    }
    signatures
}

pub(crate) fn response_types(signatures: &BTreeMap<String, CommandSignature>) -> String {
    let mut types = String::from(
        "// Aus den registrierten Rust-Signaturen erzeugt. Erneuern mit\n\
         // DOPPELDECKER_UPDATE_IPC_TYPES=1 cargo test -p doppeldecker ipc_registry_matches_the_frontend_command_manifest\n\
         import type { ActiveConnectionWire } from \"./active-connection-wire.js\";\n\
         import type { ArchiveExtractPreparedWire, ArchivePackPreparedWire } from \"./archive-mutation-wire.js\";\n\
         import type { ArchiveListingWire, ArchiveOpenedWire, ArchivePreviewedWire } from \"./archive-read-wire.js\";\n\
         import type { BatchRenamePreviewWire, BatchRenameReportWire } from \"./batch-rename-wire.js\";\n\
         import type { BidirectionalConflictWire, BidirectionalRecoveryDiscardWire, BidirectionalResumePreviewWire, BidirectionalRollbackPreviewWire, ForwardBidirectionalPairStatusWire, ForwardBidirectionalPreviewWire, LocalBidirectionalPreviewWire, RegisteredBidirectionalPairWire } from \"./bidirectional-wire.js\";\n\
         import type { CapturedConnectionSecretWire, CertificatePinWire, FtpsConnectionStatusWire, FtpsProfileWire, HostKeyPinWire, SftpConnectionStatusWire, SftpProfileWire, SftpPublicKeyIdentityWire, SmbConnectionStatusWire, SmbProfileWire, SmbServerDiscoveryWire, VersionedProfileWire, WebDavConnectionStatusWire, WebDavProfileWire } from \"./connection-profile-wire.js\";\n\
         import type { DiffToolAvailabilityWire, PreferencesSnapshotWire } from \"./desktop-preferences-wire.js\";\n\
         import type { ContentSyncQueuePreviewWire } from \"./content-sync-wire.js\";\n\
         import type { DuplicatePreviewWire } from \"./duplicate-wire.js\";\n\
         import type { RemoteEditViewWire } from \"./file-opening-wire.js\";\n\
         import type { FilePreviewWire } from \"./file-preview-wire.js\";\n\
         import type { AdminStatusWire, FileFlagRowWire, FilePropertyCapabilitiesWire, FinderCommentEditRowWire, FinderCommentReadRowWire, FinderTagEditRowWire, FinderTagReadRowWire, OwnerEditRowWire, PermissionEditRowWire, VisibilityEditRowWire, VisibilityReadRowWire } from \"./file-property-wire.js\";\n\
         import type { ProviderFilePreviewWire } from \"./file-transfer-wire.js\";\n\
         import type { LocalSyncPreviewWire, SyncPlanWire } from \"./local-sync-wire.js\";\n\
         import type { QuarantineEntryWire, QuarantineRestoreWire, QuarantineRunWire } from \"./quarantine-wire.js\";\n\
         import type { QueueSnapshotWire } from \"./queue-snapshot-wire.js\";\n\
         import type { WindowStartStateWire, WindowTabsWire } from \"./window-session-wire.js\";\n\
         import type { ContentSyncPreviewWire, RealRunPreviewWire } from \"./real-mode-wire.js\";\n\
         import type { DirectoryCreateContractWire, DirectoryCreateReportWire, ProviderDeletePreviewWire, RenameInteractionContractWire, RenameProviderPathResponseWire } from \"./routine-file-wire.js\";\n\
         import type { SavedSyncJobWire } from \"./saved-sync-job-wire.js\";\n\
         import type { SystemMountWire } from \"./system-mount-wire.js\";\n\
         import type { PreparedUpdateStatusWire, UpdateCheckWire } from \"./update-wire.js\";\n\
         import type { CliInstallStatusWire, DiscoveredNetworkServiceWire, NameSearchStartedWire } from \"./command-read-results.js\";\n\
         import type { RecoveryOverviewWire, TrashStatsWire } from \"./recovery-trash-wire.js\";\n\
         import type { DefaultDirsWire, SafetyInfoWire } from \"./startup-wire.js\";\n\n\
         export interface GeneratedIpcResults {\n",
    );
    for (command, signature) in signatures {
        if let Some(result) = &signature.wire_result {
            types.push_str(&format!("  {command}: {result};\n"));
        }
    }
    types.push_str("}\n");
    types
}

pub(crate) fn assert_connection_profile_wire<Profile, Identity, Status>(
    protocol: &str,
    provider: doppeldecker_core::ProviderKind,
    status: impl FnOnce(
        u64,
        Identity,
        doppeldecker_core::RemoteCapabilities,
        doppeldecker_core::remote_real_mode::ProviderQuarantineManagementCapabilities,
    ) -> Status,
) where
    Profile: serde::de::DeserializeOwned + serde::Serialize,
    Identity: serde::de::DeserializeOwned,
    Status: serde::Serialize,
{
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../tools/tests/connection-profile-wire.json"
    ))
    .unwrap();
    let expected = &fixture[protocol];
    let mut profile = expected["profile"].clone();
    let revision = profile
        .as_object_mut()
        .unwrap()
        .remove("revision")
        .unwrap()
        .as_u64()
        .unwrap();
    let profile: Profile = serde_json::from_value(profile).unwrap();
    assert_eq!(
        serde_json::to_value(doppeldecker_core::VersionedProfile { profile, revision }).unwrap(),
        expected["profile"],
    );

    let mut expected_status = expected["status"].clone();
    expected_status["capabilities"] = fixture["capabilities"].clone();
    let identity = serde_json::from_value(expected_status["identity"].clone()).unwrap();
    let actual = status(
        revision,
        identity,
        doppeldecker_core::RemoteCapabilities::default(),
        provider.quarantine_management_capabilities(),
    );
    assert_eq!(serde_json::to_value(actual).unwrap(), expected_status);
}
