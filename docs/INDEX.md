# Dokumentationsindex

Stand: 2026-11-07. Produktüberblick im [README](../README.de.md), offene Arbeit im
[Backlog](../backlog.md). Dieser Index führt zu den jeweiligen Verträgen.

## Produkt- und Releaseplanung

- [Alltagstauglichkeit und Prüfkosten](usability-audit-2026-10-20.md)
- [Produktprinzipien](product-principles.md)
- [Roadmap](roadmap.md)
- [Dateieigenschaften und Finder-Metadaten](file-properties.md)
- [Metadaten gewöhnlicher Kopien](copy-metadata.md)
- [Metadaten-Parität](forklift-parity.md)
- [Produkt- und Performancechancen](product-opportunities.md)
- [Release- und Plattformplan](release-plan.md)
- [Linux: Funktionen, Installation und Updates](linux.md)
- [Preview 0.1.125](release-notes-0.1.125.md)
- [Weitere Entwicklung](release-notes-next.md)
- [Manueller Updatekanal](update-design.md)
- [Einstellungen, Sitzung und Dateivergleich](settings-session-diff.md)
- [Integrierte Dateivorschau](file-preview.md)
- [Rekursive Namenssuche](name-search.md)
- [Stapelumbenennung](batch-rename.md)
- [Lokale ZIP-Erzeugung](archive-pack.md)
- [ZIP zu einem entfernten Ziel](remote-pack.md)
- [ZIP-Inhalt anzeigen](archive-read.md)
- [ZIP-Archive entpacken](archive-extract.md)
- [Lokal duplizieren](duplicate.md)
- [Pane-Tabs](pane-tabs.md)
- [Finder→FTPS](finder-ftps-gui.md)
- [GUI-Hintergrundarbeit](gui-background-commands.md)
- [Headless-CLI](headless-cli.md)

## Sicherheits- und Datenverträge

- [WebDAV-Mutationsbedingungen](webdav-mutation-boundary.md)
- [WebDAV-Ersetzungsgrenze](webdav-replace-boundary.md)
- [Gewöhnliche WebDAV-Uploads und Ersetzungen](webdav-upload.md)
- [Gewöhnliche WebDAV-Umbenennung](webdav-rename.md)
- [Normaler lokaler Sync](local-sync.md)
- [Gewöhnlicher Inhaltssync](content-sync.md)
- [Lokaler Realmodus](real-mode-design.md)
- [Operations-Queue](operations-queue-design.md)
- [Hintergrundarbeit und Datei-Zwischenablage](background-file-work.md)
- [Konfliktentscheidungen](conflict-decisions.md)
- [Bidirektionaler Sync](bidirectional-sync-design.md)
- [Bidirektionaler Best-Effort-Sync](bidirectional-forward.md)
- [Metadaten Stufe 2](metadata-stage2-design.md)
- [Lokale Metadatenfähigkeiten](local-metadata-mounts.md)
- [Sicherer Papierkorb](trash-toctou-design.md)
- [Privater Papierkorb-Kern](trash-private-claim.md)
- [Papierkorb-Journal v2](trash-journal-v2-codec.md)
- [Recoverybilanz](recovery-aggregation.md)
- [Funktionsaudit](function-audit.md)

## Remote-Provider und Plattformadapter

- [FTP-/FTPS-STOU](ftps-stou-transport.md)
- [Remote-Provider](remote-providers-design.md)
- [SFTP-Anzeigelisting](sftp-listing.md)
- [SFTP-Schreibaktionen](sftp-writes.md)
- [Additive SMB-Downloads](smb-contents.md)
- [Additive WebDAV-Downloads](webdav-contents.md)
- [Additive FTP-/FTPS-Downloads](ftp-contents.md)
- [SFTP-Inhalte](sftp-contents.md)
- [WebDAV-Ordneranlage](webdav-folder.md)
- [ACL-Adapter-Spike](acl-adapter-spike.md)

## Verbindliche UI-Verträge

- [Beobachtete SFTP-Ordnergrößen](sftp-directory-sizes.md)
- [Goal: Mehrere Fenster](goal-mehrere-fenster.md)
- [Goal: Panes-Spalten und Seitenleiste](goal-spalten-seitenleiste.md)

Vor Arbeiten an Fenstern, `AppState` oder Queue den Vertrag zu mehreren Fenstern lesen.

## Umgesetzte Entscheidungen und Grenzen

- [Lokale Temp- und Sicherungsreste](decision-lokale-restbereinigung.md)

<details>
<summary>Prüfberichte und historische Belege</summary>

- [Korrekturen 2026-11-02](code-review-2026-11-02.md)
- [Korrekturen 2026-11-01](code-review-2026-11-01.md)

- [Korrekturen 2026-10-31](code-review-2026-10-31.md)
- [Roadmap-Schnitte 2026-10-24](roadmap-2026-10-24.md)
- [Review-Korrekturen und Schnellfilter 2026-10-22](code-review-2026-10-22.md)
- [Gemeinsame Remote-Verträge 2026-10-19](code-review-2026-10-19-remote.md)
- [Deltareview: Dateioperationen und Sync 2026-10-19](code-review-2026-10-19.md)
- [SMB-Vollreview 2026-10-19](code-review-2026-10-19-smb.md)
- [Review und Alltagstauglichkeit 2026-10-16](code-review-2026-10-16.md)
- [Review-Korrekturen 2026-10-13](code-review-2026-10-13.md)
- [Vollreview ab 2026-10-07](code-review-2026-10-07.md)
- [Dokumentationsabgleich 2026-10-07](documentation-audit-2026-10-07.md)
- [Review 2026-10-02](code-review-2026-10-02.md)
- [Korrekturen 2026-10-03](code-review-2026-10-03.md)
- [Prüfstabilität](test-stability-2026-10-06.md)

</details>

## Übergeordnete Einstiege

- [Projektprofil](../AGENTS.md)
- [Öffentliche Übersicht](../README.md)
- [Änderungsprotokoll](../CHANGELOG.md)
