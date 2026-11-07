//! Reale Provider führen dieselbe Archivphase mit Datei und leerem Ordner aus.
use doppeldecker_core::archive_extract::{ArchiveExtractRequest, ArchiveExtractSelection};
use doppeldecker_core::archive_pack::{ArchivePackOptions, ArchivePackRequest};
use doppeldecker_core::archive_upload::ArchiveGeneration;
use doppeldecker_core::generated_source::GeneratedSourceStore;
use doppeldecker_core::{
    BidirectionalQueueEndpoint, Cancel, LocalQueueExecution, OperationQueue, QueueAttention,
    QueueExecutionResult, QueueOperation, QueueState, SecretResolver,
};
use std::os::unix::fs::PermissionsExt;
use std::{
    fs,
    io::{Cursor, Read},
    path::Path,
    sync::Arc,
};

pub fn run(target: BidirectionalQueueEndpoint, remote: &Path, secrets: Arc<dyn SecretResolver>) {
    let area = tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let root = area.path().canonicalize().unwrap();
    let source = root.join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("file.txt"), b"archive provider bytes").unwrap();
    let queue = OperationQueue::open(root.join("config/queue.json")).unwrap();
    let store = GeneratedSourceStore::for_queue(&queue).unwrap();
    let workspace = store.begin().unwrap();
    let request = ArchivePackRequest {
        source_root: source.clone(),
        items: vec![source.join("file.txt")],
        target_dir: workspace.path().to_path_buf(),
        target_name: "result.zip".into(),
        options: ArchivePackOptions::default(),
        sandbox: None,
    };
    let expected = request.prepare(&Cancel::new()).unwrap().binding().unwrap();
    let packed = workspace.path().join("result.zip");
    let entry = queue
        .enqueue(QueueOperation::ArchiveUpload {
            generation: Box::new(ArchiveGeneration::Pack {
                request: Box::new(request),
                expected: Box::new(expected),
            }),
            workspace: workspace.reference(),
            target: Box::new(target.clone()),
        })
        .unwrap();
    execute(&queue, secrets.clone());
    let mut uploaded =
        zip::ZipArchive::new(Cursor::new(fs::read(remote.join("result.zip")).unwrap())).unwrap();
    assert_eq!(uploaded.len(), 1);
    let mut bytes = Vec::new();
    uploaded
        .by_name("file.txt")
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(bytes, b"archive provider bytes");
    assert!(!packed.parent().unwrap().parent().unwrap().exists());
    // Ein zweites, leeres Verzeichnis belegt die Ordnerphase neben dem Dateiupload.
    let with_empty = source.join("with-empty.zip");
    {
        use std::io::Write;
        let mut zip = zip::ZipWriter::new(fs::File::create(&with_empty).unwrap());
        let options = zip::write::SimpleFileOptions::default();
        zip.add_directory("tree/", options).unwrap();
        zip.add_directory("tree/empty/", options).unwrap();
        zip.start_file("tree/leaf.txt", options).unwrap();
        zip.write_all(b"extracted provider bytes").unwrap();
        zip.finish().unwrap();
    }
    let workspace = store.begin().unwrap();
    let request = ArchiveExtractRequest {
        source_root: source,
        source_path: with_empty,
        snapshot: None,
        target_dir: workspace.path().to_path_buf(),
        options: Default::default(),
        selection: ArchiveExtractSelection::All,
        sandbox: None,
    };
    let expected = request.prepare(&Cancel::new()).unwrap().binding().unwrap();
    let extracted_path = workspace.path().to_path_buf();
    let extracted = queue
        .enqueue(QueueOperation::ArchiveUpload {
            generation: Box::new(ArchiveGeneration::Extract {
                request: Box::new(request),
                expected: Box::new(expected),
            }),
            workspace: workspace.reference(),
            target: Box::new(target),
        })
        .unwrap();
    execute(&queue, secrets);
    assert_eq!(
        fs::read(remote.join("tree/leaf.txt")).unwrap(),
        b"extracted provider bytes"
    );
    assert!(remote.join("tree/empty").is_dir());
    assert!(!extracted_path.parent().unwrap().exists());
    let entries = queue.snapshot().entries;
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].operation_id, entry.operation_id);
    assert_eq!(entries[1].operation_id, extracted.operation_id);
    assert!(entries
        .iter()
        .all(|e| e.state == QueueState::Completed && e.generated_source.is_some()));
    assert_eq!(
        entries[1].archive_extract_report.as_ref().unwrap().failed,
        0
    );
    let mut names = fs::read_dir(remote)
        .unwrap()
        .map(|p| p.unwrap().file_name())
        .collect::<Vec<_>>();
    names.sort();
    assert_eq!(names, ["result.zip", "tree"]);
    fs::remove_file(remote.join("result.zip")).unwrap();
    fs::remove_dir_all(remote.join("tree")).unwrap();
}
fn execute(queue: &OperationQueue, secrets: Arc<dyn SecretResolver>) {
    for _ in 0..3 {
        let entry = queue.claim_next().unwrap().expect("anhängige Archivphase");
        let id = entry.operation_id.clone();
        let result = doppeldecker_core::execute_provider_file_queue_entry(
            queue,
            entry,
            &LocalQueueExecution {
                sandbox: None,
                sync_quarantine: None,
                max_delete_entries: 100,
                max_delete_fraction: 0.25,
            },
            secrets.clone(),
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap();
        if matches!(result, QueueExecutionResult::File(_)) {
            return;
        }
        let saved = queue
            .snapshot()
            .entries
            .into_iter()
            .find(|e| e.operation_id == id)
            .unwrap();
        assert!(
            matches!(
                saved.attention,
                Some(QueueAttention::BestEffortCommitDowngrade { .. })
            ),
            "{:?}",
            saved.attention
        );
        queue
            .approve_attention_effects(&id, saved.attention.as_ref().unwrap(), true)
            .unwrap();
    }
    panic!("Provider verlangt wiederholt eine Fähigkeitsbestätigung");
}
