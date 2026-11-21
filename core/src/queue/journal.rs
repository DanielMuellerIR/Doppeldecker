//! Begrenzt eingelesene Queue-Journale und die Migration ihrer gespeicherten Schemas.

use std::io::{self, Read};
use std::path::Path;

use crate::local_fs::RootHandle;

use super::{
    before_children_class, before_children_plan_payload, before_children_rights_report,
    created_directory_action, has_prior_execution_evidence, operation_sync_plan,
    queue_entry_before_children_payload, queue_entry_created_payload, BidirectionalQueueEndpoint,
    Progress, QueueAttention, QueueOperation, QueueSnapshot, QueueState,
    LEGACY_REMOTE_IDENTITY_REPLAN_ERROR, LEGACY_REMOTE_IDENTITY_UNCLEAR_ERROR,
    LEGACY_UNBOUND_WEBDAV_ETAG, LEGACY_WEBDAV_REPLAN_ERROR, LEGACY_WEBDAV_UNCLEAR_ERROR,
    MAX_JOURNAL_BYTES, QUEUE_JOURNAL_VERSION,
};

pub(super) fn read_snapshot_bound(root: &RootHandle, path: &Path) -> io::Result<QueueSnapshot> {
    if !root.is_current()? {
        return Err(io::Error::other("Queueordner wurde ausgetauscht"));
    }
    let file = crate::local_fs::open_read_beneath(root, path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::other("Queue-Journal ist keine reguläre Datei"));
    }
    let snapshot = read_snapshot_from_reader(file, metadata.len(), MAX_JOURNAL_BYTES)?;
    if !root.is_current()? {
        return Err(io::Error::other(
            "Queueordner wurde während des Readbacks ausgetauscht",
        ));
    }
    Ok(snapshot)
}

pub(super) fn read_snapshot_from_reader(
    reader: impl Read,
    advertised_len: u64,
    max_bytes: u64,
) -> io::Result<QueueSnapshot> {
    if advertised_len > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Queue-Journal überschreitet die Größenbegrenzung",
        ));
    }
    let mut data = Vec::with_capacity(usize::try_from(advertised_len).unwrap_or(0));
    reader.take(max_bytes + 1).read_to_end(&mut data)?;
    // Die erste Längenprüfung beschreibt nur den Zeitpunkt von `fstat`. Eine
    // reguläre Datei kann danach über die Grenze wachsen. Das zusätzliche Byte
    // und diese zweite Prüfung verhindern, dass ein gültiger JSON-Präfix mit
    // anschließendem, ungelesenem Inhalt als vollständiges Journal gilt.
    if data.len() as u64 > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Queue-Journal überschreitet die Größenbegrenzung",
        ));
    }
    serde_json::from_slice(&data).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Queue-Journal ist beschädigt: {error}"),
        )
    })
}

pub(super) fn migrate_snapshot(snapshot: &mut QueueSnapshot) -> io::Result<bool> {
    if snapshot.version < 40
        && snapshot
            .entries
            .iter()
            .any(|entry| matches!(entry.operation, QueueOperation::ArchiveUpload { .. }))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Archivphasen benötigen Journalversion 40",
        ));
    }
    // Version 2 ergänzte SFTP, Version 3 FTPS, Version 4 WebDAV, Version 5 SMB
    // und Version 6 bidirektionale Läufe. Version 8 ergänzt die generische
    // Provider→Provider-Variante; Version 9 ergänzt interaktive Provider-Dateien,
    // Version 10 Remote-Löschen und Version 11 rekursive Zielkonfliktpfade.
    // Version 12 ergänzt den eigenen Attention-Typ `BestEffortCommitDowngrade`.
    // Version 13 bindet jeden bestätigten Zielkonflikt an Metadaten/Hash und
    // trennt die Best-Effort-Klassen für neue Namen und Ersetzungen. Version 14
    // bindet zusätzlich die sichtbare Klasse des Move-Quellabbaus. Version 15
    // bindet die lokale Best-Effort-Ersetzung getrennt und ausdrücklich.
    // Version 16 ergänzt geheimnisfreie Profilrevisionen je Queueeintrag.
    // Version 17 migriert ältere WebDAV-Löschpläne, denen die damals noch
    // nicht persistierte starke Root-ETag fehlt, in einen sichtbaren und nicht
    // erneut ausführbaren Fehlerzustand. Version 18 persistiert Fehlercodes
    // und migriert zusätzlich WebDAV-v17-Pläne mit dem früheren leeren
    // Server-Header-Digest in denselben fail-closed Zustand. Außerdem
    // kennzeichnet Version 18 ältere fehlgeschlagene oder unterbrochene
    // Mutationen ohne damals noch nicht persistierten Fehlercode konservativ
    // als unklar, damit sie nach dem Upgrade nicht blind wiederholbar werden.
    // Version 19 stellt FTPS-/WebDAV-Löschpläne mit den früheren
    // Serveridentitäts-Digests zustandsabhängig auf Neuplanung oder manuelle
    // Recovery, statt sie unter der neuen Digestbedeutung wieder auszuführen.
    // Ältere Dokumente besitzen den jeweils neuen Variantentag nicht.
    // Ältere Dokumente enthalten den jeweils neueren Variantentag per Definition
    // nicht und lassen sich deshalb ohne Bedeutungsänderung atomar hochstufen.
    // Version 22 bindet backendseitig vergebene Konfliktkopie-Ziele. Ältere
    // Executoren dürfen sie nicht als gewöhnliches Skip ausführen.
    // Version 23 unterscheidet normale lokale Ausführung ausdrücklich von
    // persistentem Dry-Run; ältere Aufträge werden niemals hochgestuft.
    let source_version = snapshot.version;
    if source_version < 39
        && snapshot
            .entries
            .iter()
            .any(|entry| entry.generated_source.is_some())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Altes Queuejournal enthält eine dort nicht erlaubte erzeugte Quelle",
        ));
    }
    if source_version < 38
        && snapshot
            .entries
            .iter()
            .any(|entry| matches!(entry.operation, QueueOperation::RenameCycleRecovery { .. }))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Altes Queuejournal enthält eine dort nicht erlaubte Namenstausch-Recovery",
        ));
    }
    // Version 24 bindet rekursive interaktive Quellen und deren Auslassungen.
    // Ein älterer Executor darf den neuen Baumbeleg nicht still ignorieren.
    // Version 25 erhält Remote-Dateiintents und verlangt nach Reopen eine
    // frische Best-Effort-Bestätigung für gewöhnliche WebDAV-Additionen.
    // Version 26 erlaubt die begrenzte lokale Baumquelle für additive WebDAV-Ziele.
    // Ältere Executoren dürfen die Ordnerintents und diese Wirkungsbindung nicht ignorieren.
    // Version 27 zeigt auch vor einem Queueauftrag unterbrochene FTPS-Proben
    // als reine Recovery. Ältere Executoren dürfen sie nicht wiederholen.
    // Version 28 ergänzt lokale ZIP-Erzeugung samt authentischer Bestandsbindung.
    // Version 30 bindet Datei-/Unterbaumauswahl; ältere Executoren dürfen sie
    // niemals als Ganzarchivauftrag ignorieren. Version 29 erhält All als Default.
    if source_version < 30 && snapshot.entries.iter().any(|entry| {
        matches!(&entry.operation, QueueOperation::ArchiveExtract { request, expected }
            if !matches!(request.selection, crate::archive_extract::ArchiveExtractSelection::All)
                || !matches!(expected.preview.selection, crate::archive_extract::ArchiveExtractSelection::All))
    }) {
        return Err(io::Error::new(io::ErrorKind::InvalidData,
            "Altes Queuejournal enthält eine dort nicht unterstützte ZIP-Auswahl"));
    }
    // Version 31 erlaubt ausschließlich manifestgebundene interne Snapshotquellen.
    // Kein älteres Journal darf diese Quellenklasse nachträglich einschleusen.
    if source_version < 31 && snapshot.entries.iter().any(|entry| {
        matches!(&entry.operation, QueueOperation::ArchiveExtract { request, expected }
            if request.snapshot.is_some() || expected.preview.snapshot.is_some())
            || matches!(&entry.attention, Some(QueueAttention::ArchiveExtractChanged { current, .. }) if current.preview.snapshot.is_some())
            || entry.archive_extract_report.as_ref().is_some_and(|report| report.preview.snapshot.is_some())
    }) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "Altes Queuejournal enthält eine dort nicht unterstützte Archivmomentaufnahme"));
    }
    // Version 32 erweitert nur Snapshotquellen um Ganzarchiv und Unterbaum.
    // Diese Kombination darf in älteren Journalen auch nicht über eine
    // Rückfrage oder historische Teilbilanz nachträglich eingeschleust werden.
    if source_version < 32 && snapshot.entries.iter().any(|entry| {
        let needs_new_schema = |snapshot: bool, selection: &crate::archive_extract::ArchiveExtractSelection| {
            snapshot && !matches!(selection, crate::archive_extract::ArchiveExtractSelection::Entry { .. })
        };
        matches!(&entry.operation, QueueOperation::ArchiveExtract { request, expected }
            if needs_new_schema(request.snapshot.is_some(), &request.selection)
                || needs_new_schema(expected.preview.snapshot.is_some(), &expected.preview.selection))
            || matches!(&entry.attention, Some(QueueAttention::ArchiveExtractChanged { current, .. })
                if needs_new_schema(current.preview.snapshot.is_some(), &current.preview.selection))
            || entry.archive_extract_report.as_ref().is_some_and(|report|
                needs_new_schema(report.preview.snapshot.is_some(), &report.preview.selection))
    }) {
        return Err(io::Error::new(io::ErrorKind::InvalidData,
            "Altes Queuejournal enthält einen dort nicht unterstützten Snapshot-Entpackumfang"));
    }
    if source_version < 33
        && snapshot
            .entries
            .iter()
            .any(|entry| matches!(entry.operation, QueueOperation::ContentSync { .. }))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Altes Queuejournal enthält eine dort nicht unterstützte Inhaltssync-Klasse",
        ));
    }
    if source_version < 34
        && snapshot
            .entries
            .iter()
            .any(|entry| matches!(entry.operation, QueueOperation::BidirectionalForward { .. }))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Altes Queuejournal enthält eine dort nicht unterstützte bidirektionale Forwardklasse",
        ));
    }
    // Version 35 bindet objektweite Datei-DACL-Wirkungen samt Aliasstimmen
    // und Recoverybericht; ältere Executoren dürfen diese Klasse nicht ignorieren.
    if source_version<35 && snapshot.entries.iter().any(|entry| {
        entry.file_dacl_recovery || !entry.file_dacl_report.is_empty() || !entry.progress.file_dacl_report.is_empty()
            || matches!(&entry.operation,QueueOperation::ContentSync{expected_plan,..} if !expected_plan.file_dacl_selections.is_empty() || expected_plan.actions.iter().any(|action|matches!(action,crate::Action::SetFileDacl{..})))
    }) { return Err(io::Error::new(io::ErrorKind::InvalidData,"Altes Queuejournal enthält eine dort nicht unterstützte objektweite Datei-DACL-Klasse")); }
    if source_version < 36
        && snapshot.entries.iter().any(|entry| {
            entry.created_directory_dacl_recovery
                || !entry.created_directory_dacl_report.is_empty()
                || !entry.progress.created_directory_dacl_report.is_empty()
                || queue_entry_created_payload(entry)
        })
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Altes Queuejournal enthält eine dort nicht unterstützte Neuordner-Rechteklasse",
        ));
    }
    if source_version < 37
        && snapshot
            .entries
            .iter()
            .any(queue_entry_before_children_payload)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Altes Queuejournal enthält eine dort nicht unterstützte V8-Rechteklasse",
        ));
    }
    for entry in &snapshot.entries {
        let invalid_original = operation_sync_plan(&entry.operation).is_some_and(|plan| {
            before_children_plan_payload(plan) && !before_children_class(&entry.operation, plan)
        });
        let invalid_preview = matches!(&entry.attention, Some(QueueAttention::PlanChanged { current_plan, .. })
            if before_children_plan_payload(current_plan) && !before_children_class(&entry.operation, current_plan));
        let has_rights = before_children_rights_report(&entry.created_directory_dacl_report)
            || before_children_rights_report(&entry.progress.created_directory_dacl_report);
        let original_class = operation_sync_plan(&entry.operation)
            .is_some_and(|plan| before_children_class(&entry.operation, plan));
        if invalid_original || invalid_preview || (has_rights && !original_class) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "V8 benötigt passenden NativeBestEffort-ContentSync/CreateDir; Rechtebelege behalten ihre ursprüngliche Absicht"));
        }
    }
    for entry in &snapshot.entries {
        let has_rights = entry.created_directory_dacl_recovery
            || entry.created_directory_dacl_report.has_rights_effects()
            || entry
                .progress
                .created_directory_dacl_report
                .has_rights_effects();
        let has_creation = !entry.created_directory_dacl_report.created.is_empty()
            || !entry
                .progress
                .created_directory_dacl_report
                .created
                .is_empty();
        if has_rights
            && !matches!(&entry.operation,QueueOperation::ContentSync{expected_plan,..} if expected_plan.actions.iter().any(created_directory_action))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Neuordner-Rechtebeleg ohne passende ursprüngliche CreateDir-Absicht",
            ));
        }
        if has_creation && !matches!(entry.operation, QueueOperation::ContentSync { .. }) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Historische Neuordneranlage benötigt ihren ursprünglichen Inhaltssync",
            ));
        }
    }
    if source_version < 41
        && snapshot
            .entries
            .iter()
            .any(|entry| matches!(entry.operation, QueueOperation::ResumableCopy { .. }))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Fortsetzbare Kopien benötigen Journalversion 41",
        ));
    }
    if source_version < 42
        && snapshot.entries.iter().any(|entry| match &entry.operation {
            QueueOperation::ArchiveExtract { request, .. } => !request.format.is_zip(),
            QueueOperation::ArchiveUpload { generation, .. } => matches!(
                generation.as_ref(),
                crate::archive_upload::ArchiveGeneration::Extract { request, .. } if !request.format.is_zip()
            ),
            _ => false,
        })
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "TAR-Entpackaufträge benötigen Journalversion 42",
        ));
    }
    if source_version < 43 && snapshot.entries.iter().any(|entry| {
        matches!(&entry.operation, QueueOperation::ResumableCopy { plan, .. } if plan.webdav_endpoint().is_some())
    }) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "Fortsetzbare WebDAV-Downloads benötigen Journalversion 43"));
    }
    if source_version < 44 && snapshot.entries.iter().any(|entry| {
        matches!(&entry.operation, QueueOperation::ResumableCopy { plan, .. } if plan.ftps_endpoint().is_some())
    }) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "Fortsetzbare FTPS-Downloads benötigen Journalversion 44"));
    }
    if source_version < 45 && snapshot.entries.iter().any(|entry| {
        matches!(&entry.operation, QueueOperation::ResumableCopy { plan, .. } if plan.sftp_endpoint().is_some())
    }) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "Fortsetzbare SFTP-Downloads benötigen Journalversion 45"));
    }
    let migrated = if matches!(source_version, 1..=39) {
        for entry in &mut snapshot.entries {
            if let QueueOperation::File {
                destination,
                expected_conflicts,
                ..
            } = &mut entry.operation
            {
                for conflict in expected_conflicts {
                    if !Path::new(conflict).is_absolute() {
                        *conflict =
                            crate::fileops::path_to_utf8(&Path::new(destination).join(&*conflict))?;
                    }
                }
            }
        }
        if source_version <= 10 {
            for entry in &mut snapshot.entries {
                if let QueueOperation::ProviderFile {
                    destination,
                    expected_conflicts,
                    ..
                } = &mut entry.operation
                {
                    for conflict in expected_conflicts {
                        if !Path::new(conflict).is_absolute() {
                            *conflict = crate::fileops::path_to_utf8(
                                &Path::new(destination).join(&*conflict),
                            )?;
                        }
                    }
                }
            }
        }
        if source_version <= 12 {
            for entry in &mut snapshot.entries {
                if let QueueOperation::ProviderFile {
                    overwrite,
                    best_effort_commit_expected,
                    copy_new_best_effort_expected,
                    replace_best_effort_expected,
                    ..
                } = &mut entry.operation
                {
                    let legacy = best_effort_commit_expected.take();
                    if *overwrite {
                        *replace_best_effort_expected = legacy;
                    } else {
                        *copy_new_best_effort_expected = legacy;
                    }
                }
                // Die alte Bestätigung konnte nicht ausdrücken, welche der
                // beiden Wirkungen degradiert war. Sie wird deshalb verworfen;
                // der nächste Claim erzeugt bei Bedarf eine eindeutige Phrase.
                if matches!(
                    entry.attention,
                    Some(QueueAttention::BestEffortCommitDowngrade { .. })
                ) {
                    entry.state = QueueState::Pending;
                    entry.attention = None;
                    entry.error = None;
                    entry.error_code = None;
                }
            }
        }
        if source_version <= 14 {
            for entry in &mut snapshot.entries {
                if let QueueOperation::File {
                    overwrite,
                    local_replace_best_effort_expected,
                    ..
                } = &mut entry.operation
                {
                    // Alte überschreibende Einträge besitzen keine belastbare
                    // Sichtbarkeitsbestätigung und müssen beim nächsten Claim
                    // pausieren. Ohne Überschreiben ist die Klasse eindeutig.
                    *local_replace_best_effort_expected = (!*overwrite).then_some(false);
                }
            }
        }
        if source_version <= 17 {
            for entry in &mut snapshot.entries {
                if matches!(entry.state, QueueState::Failed | QueueState::Interrupted)
                    && entry.error_code.is_none()
                {
                    // Vor Version 18 blieb vom Executorfehler nur der Text im
                    // Journal. Daraus lässt sich nachträglich nicht sicher
                    // ableiten, ob eine Mutation bereits gesendet wurde. Ein
                    // erneuter Lauf bleibt deshalb bis zur ausdrücklichen
                    // Prüfung gesperrt; sicher mutationsfreie Pending-Einträge
                    // und bereits terminale Abschlüsse bleiben unverändert.
                    entry.error_code = Some(crate::ErrorCode::OutcomeUnclear);
                }
            }
        }
        if source_version <= 19 {
            for entry in &mut snapshot.entries {
                let QueueOperation::ProviderDelete {
                    endpoint,
                    expected_plan,
                    ..
                } = &entry.operation
                else {
                    continue;
                };
                if !matches!(
                    endpoint.as_ref(),
                    BidirectionalQueueEndpoint::Ftps { .. }
                        | BidirectionalQueueEndpoint::WebDav { .. }
                ) {
                    continue;
                }
                // Version 18 konnte bereits einen WebDAV-v15..17-Plan mit
                // diesem ungültigen ETag-Sentinel stillgelegt haben. Dieser
                // Zustand belegt durch Status, Fortschritt und Fehlertext
                // getrennt, ob der Plan sicher nie startete oder eine Wirkung
                // unklar blieb. Die allgemeinere Schema-Migration darf diese
                // schon abgeschlossene Einordnung weder verschärfen noch
                // unbrauchbar machen; die nachfolgende Journalvalidierung
                // akzeptiert ausschließlich die vollständig passende Form.
                let already_classified_legacy_webdav = matches!(
                    &expected_plan.root_identity,
                    crate::RootIdentity::WebDav(identity)
                        if identity.root_etag == LEGACY_UNBOUND_WEBDAV_ETAG
                );
                if already_classified_legacy_webdav {
                    continue;
                }
                // Die älteren WebDAV-Migrationen besitzen einen eigenen,
                // ausdrücklich quittierbaren Fehlerzustand. Sie laufen direkt
                // danach und dürfen nicht durch die allgemeinere
                // Identitätsschema-Migration überschrieben werden.
                if source_version <= 17
                    && matches!(endpoint.as_ref(), BidirectionalQueueEndpoint::WebDav { .. })
                    && matches!(
                        &expected_plan.root_identity,
                        crate::RootIdentity::WebDav(identity)
                            if identity.root_etag.is_empty()
                                || identity.server.server_header_digest
                                    == *blake3::hash(&[]).as_bytes()
                    )
                {
                    continue;
                }
                // Version 19 hatte diesen Zustand nur als deutschen Text plus
                // `plan_drift` gespeichert. Beim Öffnen eines bereits von der
                // fehlerhaften Migration geschriebenen Journals heben wir exakt
                // diesen typisierten Altstand auf `replan_required` an.
                if source_version == 19 {
                    if entry.state == QueueState::Failed
                        && entry.error_code == Some(crate::ErrorCode::PlanDrift)
                        && entry.error.as_deref() == Some(LEGACY_REMOTE_IDENTITY_REPLAN_ERROR)
                    {
                        entry.error_code = Some(crate::ErrorCode::ReplanRequired);
                    }
                    continue;
                }
                match entry.state {
                    QueueState::Pending | QueueState::AwaitingConfirmation => {
                        let safely_unstarted = entry.attempts == 0
                            && entry.progress == Progress::default()
                            && entry.error.is_none()
                            && entry.error_code.is_none()
                            && entry.temp_recovery_paths.is_empty()
                            && !entry.recovery_available
                            && entry.bidirectional_completion_recovery.is_none();
                        entry.attention = None;
                        if safely_unstarted {
                            entry.state = QueueState::Failed;
                            entry.progress = Progress::default();
                            entry.error = Some(LEGACY_REMOTE_IDENTITY_REPLAN_ERROR.into());
                            entry.error_code = Some(crate::ErrorCode::ReplanRequired);
                        } else {
                            // Retry setzt einen abgebrochenen Eintrag wieder auf
                            // Pending und leert seinen Fortschritt, behält aber
                            // die Versuchszahl. Ein früherer Claim kann bereits
                            // eine Löschwirkung gehabt haben und darf deshalb
                            // nie als sicher verwerfbarer Altplan erscheinen.
                            entry.state = QueueState::Interrupted;
                            entry.progress.phase = "recovery".into();
                            entry.progress.current.clear();
                            entry.error = Some(LEGACY_REMOTE_IDENTITY_UNCLEAR_ERROR.into());
                            entry.error_code = Some(crate::ErrorCode::OutcomeUnclear);
                        }
                    }
                    QueueState::Validating
                    | QueueState::Running
                    | QueueState::Failed
                    | QueueState::Interrupted => {
                        entry.state = QueueState::Interrupted;
                        entry.progress.phase = "recovery".into();
                        entry.progress.current.clear();
                        entry.attention = None;
                        entry.error = Some(LEGACY_REMOTE_IDENTITY_UNCLEAR_ERROR.into());
                        entry.error_code = Some(crate::ErrorCode::OutcomeUnclear);
                        entry.recovery_available = false;
                        entry.bidirectional_completion_recovery = None;
                    }
                    QueueState::Completed | QueueState::Cancelled => {}
                }
            }
        }
        if source_version <= 17 {
            for entry in &mut snapshot.entries {
                let QueueOperation::ProviderDelete {
                    endpoint,
                    expected_plan,
                    ..
                } = &mut entry.operation
                else {
                    continue;
                };
                if !matches!(endpoint.as_ref(), BidirectionalQueueEndpoint::WebDav { .. }) {
                    continue;
                }
                let crate::RootIdentity::WebDav(identity) = &mut expected_plan.root_identity else {
                    continue;
                };
                let lacks_root_etag = source_version <= 15 && identity.root_etag.is_empty();
                let has_legacy_empty_server_digest =
                    identity.server.server_header_digest == *blake3::hash(&[]).as_bytes();
                if lacks_root_etag || has_legacy_empty_server_digest {
                    // Eine ETag lässt sich weder aus Pfad noch Serveridentität
                    // ableiten. Der ungültige Sentinel ist ausdrücklich **kein**
                    // Providerbeleg; er unterscheidet ausschließlich migrierte
                    // Alteinträge von beschädigten aktuellen Journalen.
                    identity.root_etag = LEGACY_UNBOUND_WEBDAV_ETAG.into();
                    match entry.state {
                        QueueState::Pending | QueueState::AwaitingConfirmation => {
                            let safely_unstarted = !has_prior_execution_evidence(entry)
                                && entry.error.is_none()
                                && entry.error_code.is_none();
                            entry.attention = None;
                            if safely_unstarted {
                                entry.state = QueueState::Failed;
                                entry.error = Some(LEGACY_WEBDAV_REPLAN_ERROR.into());
                                entry.error_code = Some(crate::ErrorCode::PlanDrift);
                            } else {
                                entry.state = QueueState::Interrupted;
                                entry.progress.phase = "recovery".into();
                                entry.progress.current.clear();
                                entry.error = Some(LEGACY_WEBDAV_UNCLEAR_ERROR.into());
                                entry.error_code = Some(crate::ErrorCode::OutcomeUnclear);
                            }
                            entry.recovery_available = false;
                            entry.bidirectional_completion_recovery = None;
                        }
                        QueueState::Validating
                        | QueueState::Running
                        | QueueState::Failed
                        | QueueState::Interrupted => {
                            entry.state = QueueState::Interrupted;
                            entry.progress.phase = "recovery".into();
                            entry.progress.current.clear();
                            entry.attention = None;
                            entry.error = Some(LEGACY_WEBDAV_UNCLEAR_ERROR.into());
                            entry.error_code = Some(crate::ErrorCode::OutcomeUnclear);
                            entry.recovery_available = false;
                            entry.bidirectional_completion_recovery = None;
                        }
                        QueueState::Completed | QueueState::Cancelled => {
                            // Terminale Datenwirkung und ihre Bilanz bleiben
                            // unverändert. Der Sentinel dient nur dazu, den
                            // migrierten Ursprung von aktueller Korruption zu
                            // unterscheiden.
                        }
                    }
                }
            }
        }
        snapshot.version = QUEUE_JOURNAL_VERSION;
        true
    } else if matches!(source_version, 40..=45) {
        snapshot.version = QUEUE_JOURNAL_VERSION;
        true
    } else {
        false
    };
    if snapshot.version != QUEUE_JOURNAL_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "Queue-Journalversion {} wird nicht unterstützt (erwartet {})",
                snapshot.version, QUEUE_JOURNAL_VERSION
            ),
        ));
    }
    Ok(migrated)
}
