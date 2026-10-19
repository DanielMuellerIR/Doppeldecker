# Dokumentationsindex

Stand: 2026-10-19

Dieser Index ist der kanonische Einstieg in die dauerhafte Projektdokumentation.
Kurzlebige Aufgabenverläufe und Historienarchive gehören nicht hierher.

## Produkt- und Releaseplanung

- [Produktprinzipien](product-principles.md) — praktische Dateiarbeit,
  abgestufte Garantien und die Grenze zwischen Best Effort und echter Absage.
- [Roadmap](roadmap.md) — Produktziele und Voraussetzungen für Version 1.0.
- [Produkt- und Performancechancen](product-opportunities.md) — geordnete
  Ausbauideen mit vorhandenem Anknüpfungspunkt, Sicherheitsgrenze und Messplan.
- [Release- und Plattformplan](release-plan.md) — unterstützte Systeme,
  Paketgrenzen und Release-Gates.
- [Manueller Updatekanal](update-design.md) — zweistufige Bedienung,
  Signaturbindung, Fenstergrenzen und Release-Manifest.
- [Einstellungen, Sitzung und Dateivergleich](settings-session-diff.md) — globale
  Optionen, persistierte Fensterzustände und externe Diff-Werkzeuge.
- [Pane-Tabs](pane-tabs.md) — getrennte Tab-Leisten, Navigation, Bedienung und
  Sitzungsformat 2; native macOS-Abnahme einschließlich App-Neustart belegt.
- [Finder→FTPS](finder-ftps-gui.md) — native Drop-Übergabe, Pane-Ziel,
  Konflikte, Fortschritt, Abbruch und sichtbare Fehler mit künstlichem TLS-Server.
- [GUI-Hintergrundarbeit](gui-background-commands.md) — SFTP-Worker, abbrechbare
  Remote-Dateiöffnung und Reihenfolge der Fensterereignisse.
- [Headless-CLI](headless-cli.md) — Bedienvertrag, JSONL-Protokoll und sichere
  Realmodus-Ausführung ohne GUI.

## Sicherheits- und Datenverträge

- [WebDAV-Mutationsbedingungen](webdav-mutation-boundary.md) — einzelne unmittelbare
  Elternbedingung, passive strikte Sperren und begrenztes reguläres Temp-Cleanup.

- [WebDAV-Ersetzungsgrenze](webdav-replace-boundary.md) — HTTPS-Gegenbeleg zur
  ODER-Semantik getaggter If-Listen, konkrete Absage und korrigierte Elternzusage.

- [Additive WebDAV-Dateiuploads](webdav-upload.md) — gewöhnliche F5-Dateikopien,
  bedingtes PUT/MOVE ohne Collection-/Löschprobe und phasengenaue Temp-Recovery.

- [Gewöhnliche WebDAV-Umbenennung](webdav-rename.md) — bedingte Einzelwirkung,
  bestätigte Best-Effort-Klasse und Readback ohne künstliche Löschprobe.

- [Normaler lokaler Sync](local-sync.md) — wirkungsbezogener AddOnly-/Update-Start,
  klare Simulation, RAM-Vorbereitung und erneut bestätigte Queue-Recovery.
- [Lokaler Realmodus](real-mode-design.md) — Identitäts-, Permit-, Audit- und
  Mirror-Schranken für echte lokale Schreibläufe.
- [Operations-Queue](operations-queue-design.md) — persistenter Zustandsautomat,
  Recovery, Driftprüfung und Bestätigung.
- [Hintergrundarbeit und Datei-Zwischenablage](background-file-work.md) —
  unabhängige Navigation, laufbezogene Ereignisse und native Dateiübergaben.
- [Konfliktentscheidungen](conflict-decisions.md) — vergleichbare Endpunkte, zulässige
  Erstbaseline-Entscheidungen, Konfliktkopien und klare Simulations-/Ausführungszustände.
- [Bidirektionaler Sync](bidirectional-sync-design.md) — Baselines, Konflikte,
  Tombstones, Teilcommits, Resume und Rollback.
- [Metadaten Stufe 2](metadata-stage2-design.md) — Fähigkeiten, Manifestbindung und
  Übertragung nativer Metadatenklassen.
- [Lokale Metadatenfähigkeiten](local-metadata-mounts.md) — Mountabfrage und
  tatsächliche APFS-/ext4-/exFAT-Gegenläufe.
- [Sicherer Papierkorb](trash-toctou-design.md) — descriptor-relative Claims,
  Identitätsprüfung und Crash-Recovery; Zielvertrag für private Claims und Journal v2.
- [Privater Papierkorb-Kern](trash-private-claim.md) — sichere Anlage, Claim, Transfer,
  Recovery und Cleanup mit nativen macOS-/XDG-Adaptern; gemeinsame macOS-GUI-Abnahme belegt.
- [Papierkorb-Journal v2](trash-journal-v2-codec.md) — strikter Bytepfad-Codec,
  Phasen-/Identitätsbelege und konservative Altjournal-Recovery.
- [Recoverybilanz](recovery-aggregation.md) — erhaltene Pfade über mehrere
  Teilaktionen, Fehler und Queue-Journalabschlüsse.
- [Funktionsaudit](function-audit.md) — pflegbare Matrix aller Datei- und
  Sync-Operationen je Provider: heutiges Verhalten, Zielklasse und Stand.

## Remote-Provider und Plattformadapter

- [FTP-/FTPS-STOU](ftps-stou-transport.md) — belegter TCP-Reset beim vorzeitigen
  Datenkanal-Drop, geprüfter Abschluss, wirkungsbezogene Vorbereitung und dauerhafte
  fenstergebundene Proben-/STOU-Intents mit manueller Recovery.

- [Remote-Provider](remote-providers-design.md) — gemeinsamer Datenintegritäts- und
  Capability-Vertrag sowie sichtbare Transportstufen; SFTP bietet interaktives
  Best-Effort-Listing und begrenzte interaktive Inhalte. Übrige Richtungen folgen
  der aktuellen Funktionsmatrix.
- [SFTP-Anzeigelisting](sftp-listing.md) — benannte Auslassungen und unabhängige
  gültige Dateien mit panegebundener Textbilanz.
- [SFTP-Inhalte](sftp-contents.md) — geprüfte lokale Momentaufnahmen, additive
  Baumdownloads, Pfadrisiko, Grenzen und Queue-Recovery.
- [WebDAV-Ordneranlage](webdav-folder.md) — gewöhnliches bedingtes MKCOL als
  Best Effort ohne künstliche Löschprobe; Namenskollisionen und unklarer Ausgang.
- [ACL-Adapter-Spike](acl-adapter-spike.md) — descriptor-relative ACL-Grundlage auf
  macOS und Linux.

## Verbindliche UI-Verträge

- [Beobachtete SFTP-Ordnergrößen](sftp-directory-sizes.md) — begrenzte frische
  Metadatenbilanz ohne Inhaltsübertragung oder Freigabe strikter Listings.

- [Goal: Mehrere Fenster](goal-mehrere-fenster.md) — verbindliche Produktanforderung
  vom 2026-08-03 und beschlossener Vertrag vom 2026-08-06: gemeinsamer
  Verbindungspool, eigene Queue/Permits je Fenster, Parallelität, Kollisionen und
  gezielte Ereignisse. Vor jeder Arbeit an Fenstern, `AppState` oder Queue lesen.
- [Goal: Panes-Spalten und Seitenleiste](goal-spalten-seitenleiste.md) — beschlossener
  Ausbau auf das Niveau ausgereifter Desktop-Dateimanager: Spalten-Splitter, fixe
  Größen-/dynamische Datumsspalte, Darstellungsoptionen, Ordnergrößen,
  Dateiinformationen, Geräte-Seitenleiste und Discovery.

## Umgesetzte Entscheidungen und Grenzen

- [Lokale Temp- und Sicherungsreste](decision-lokale-restbereinigung.md) — drei
  Varianten für das Aufräumen lokaler Zwischennamen. Die freigegebene Variante C2
  ist integriert: privater Laufkontext, explizite ACL-Vererbung, getrennte
  Erfolgs-/Sicherungsbilanz und fail-closed Recovery. Grenzen und Prüfkriterien
  stehen im Entscheidungsdokument.
  Betrifft die historischen Backlog-Punkte 1 und 10; der aktuelle Vertrag und seine
  Prüfbelege sind umgesetzt. Die damalige Recoverybilanz-Lücke ist behoben.

## Releaseprüfung

- [Prüfumfang und Grenzen](release-validation.md) — Plattformgates, künstliche
  Testdaten, native Bedienung und Grenzen der ersten Preview.

## Übergeordnete Einstiege

- [Projektprofil](../AGENTS.md) — Architektur, Sicherheitsregeln und aktueller Stand.
- [Öffentliche Übersicht](../README.md) — Produktumfang und Bedien-/Build-Einstieg.
- [Änderungsprotokoll](../CHANGELOG.md) — Versionen und freigegebene Änderungen.
