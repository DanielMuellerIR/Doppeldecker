# Dokumentationsabgleich 2026-10-07

Codebasis: `366fe6fdda91801e1255f3f83efb70a4bf2aa882`. Der Abgleich prüft die
Bedien- und Sicherheitszusagen gegen die aktuellen Einstiegspunkte, Gates, DTOs,
Defaults und vorhandenen Prüfbelege. Er ist kein neuer vollständiger Code-Review.

## Umfang

Das Inventar umfasst beide READMEs, Projektregeln, Backlog, Changelog, sämtliche
Markdown-Dateien unter `docs/` sowie die Einstiege für Fuzzing und Icons. Aktuelle
Verträge wurden gegen ihre Codegrenzen geprüft; datierte Entwicklungs-/Testchroniken
bleiben als historische Belege erhalten. Unveränderter vendorter Fremdcode ist
kein eigener Dokumentationsvertrag dieses Projekts.

| Dokumente | Gegenstand des Abgleichs |
| --- | --- |
| README.md, README.de.md | Produktumfang, Bedienung, Providergrenzen, Plattformstatus, CLI und Build |
| AGENTS.md, backlog.md, CHANGELOG.md | Dauerhafte Grenzen, tatsächlicher Reststand und historische Änderungen |
| docs/INDEX.md, docs/roadmap.md, docs/product-principles.md, docs/product-opportunities.md | Verlinkung, aktuelle Freigaben gegenüber Ausbauzielen und Chronik |
| docs/function-audit.md, docs/remote-providers-design.md | Datei-/Sync-Richtungen, SFTP-Namespace, SMB-Restoregrenzen, Löschen, Best Effort |
| docs/real-mode-design.md, docs/bidirectional-sync-design.md, docs/operations-queue-design.md | Permits, Mengenlimits, Journal 20, Quarantäne, Rollback und Fensterzuordnung |
| docs/trash-toctou-design.md, docs/trash-private-claim.md, docs/trash-journal-v2-codec.md | Produktive v2-Adapter, private Claims, Codec, konservative v1-Recovery und native Belege |
| docs/metadata-stage2-design.md, docs/local-metadata-mounts.md, docs/acl-adapter-spike.md | Mountgebundene Fähigkeiten, native Profile, Metadatenreihenfolge und Plattformgrenzen |
| docs/decision-lokale-restbereinigung.md, docs/recovery-aggregation.md | C2-Arbeitsbereiche, sichere Restbilanz über mehrere Aktionen und Queue-Reopen |
| docs/goal-mehrere-fenster.md, docs/goal-spalten-seitenleiste.md, docs/pane-tabs.md, docs/settings-session-diff.md | Fenster-/Tabzustand, Defaults, Größenberechnung, Persistenz, Diff-Integration |
| docs/gui-background-commands.md, docs/finder-ftps-gui.md | Worker-/Abbruchgrenzen, native Finder-Übergabe, Fehleranzeige und tatsächliche Prüfungen |
| docs/headless-cli.md, docs/release-plan.md, docs/update-design.md | CLI-Befehle/Optionen, Build-/Installationsgrenzen, Signatur-/Notary-Konfiguration und Updatekanal |
| docs/code-review-2026-10-02.md, docs/code-review-2026-10-03.md, docs/test-stability-2026-10-06.md | Historische Ausgangsstände, spätere Korrekturen, ausdrücklich begrenzte Testaussagen |
| fuzz/README.md, src-tauri/icons/README.md, design/icon-concepts/2026-09-02/README.md | Fuzz-Einstieg, vorhandene Quellen, Renderer, Herkunft und datierte Entwurfsbelege |

## Wesentliche Korrekturen

- Die [Funktionsmatrix](function-audit.md) trennt Quelle, Ziel, interaktives
  Best Effort und strengeren Sync. SFTP bietet derzeit nur interaktives Listing;
  Dateiinhalt, Snapshot, rekursive Größenberechnung, Sync und Mutationen sind
  gesperrt. SMB bietet keine aktuelle Ersetzungs-/Restore-/Update-/Mirrorfreigabe.
- Der vollständige gebundene Restore-Proof fehlt bei den heutigen Remote-Zielen.
  Gewöhnliche erfolgreiche Create-/Rename-/Flush-Proben ersetzen ihn nicht.
  WebDAV-Dauerlöschen ist vom begrenzten Move-Quellentfernen getrennt; FTPS gibt
  keinen eigenständigen Dauerlöschbefehl frei. Zwei missverständliche FTPS-
  Schnittstellenkommentare sind entsprechend korrigiert, ohne Verhaltensänderung.
- Papierkorb-Journal v2 ist auf macOS und Linux/XDG produktiv. Frühere v1-Rennen
  und bereits behobene Recoverybilanz-Lücken stehen als historische Befunde da.
- Beide READMEs beschreiben Tabs, Finder-Kopie und die derzeitigen Providergrenzen.
  Die optionale Sitzung ist umgesetzt; Defaults und tatsächliche Persistenzgrenzen
  sind dokumentiert. Eine Dateitransferfunktion ist kein Telemetrie-Upload.
- Parallele Fenster behalten ihre jeweilige Ausführungsklasse. Ein ausgewiesener
  Best-Effort-Commit kann eine späte Kollision nicht atomar verhindern; daraus darf
  kein universelles No-Replace-Versprechen werden.
- Allgemeine Mirror-Grenzen (100 Einträge / 25 %) sind von den strengeren lokalen
  Realmodusgrenzen (50 / 5 %) getrennt. Historische Versionen und Testzahlen sind
  keine Behauptung einer erneuten Prüfung des heutigen Stands.
- Notary-Profilnamen kommen aus `NOTARY_PROFILE` oder clone-lokal aus Git-Konfiguration.
  Die frühere fehlende Notary-Konfiguration ist historisch. Die Icon-Ecken werden
  vom vorhandenen SVG-Renderer ausgeschnitten, nicht erst pauschal durch macOS.

## Tatsächlich ausgeführte Prüfungen

- In 38 Markdown-Dateien 170 lokale Verweise und Bildpfade sowie 53 benannte
  Codepfade auf Existenz geprüft; keine fehlenden Ziele.
- Alle 22 Bash-/sh-Beispielblöcke mit `bash -n` auf Syntax geprüft. Die 19 CLI-Beispiele
  gegen die tatsächlichen Unterbefehle und Optionslisten des gebauten CLI geprüft;
  jeder Hilfsaufruf lief mit isoliertem HOME/XDG/Konfigurationsordner. Kein
  dokumentierter mutierender Beispielbefehl wurde gegen Nutzerdaten ausgeführt.
- `pnpm release:check` bestanden: Version 0.1.123, strikte CSP und 1024er-Icon.
  `cargo fmt --all -- --check` und `git diff --check` bestanden.
- Für die beiden FTPS-Kommentarkorrekturen alle 19 FTPS-Modultests unter macOS und
  in einer isolierten Linux-Kopie bestanden; kein Verhalten geändert.
- PNG-Header bestätigen 1254 × 1254 für die Rasterquelle und 1024 × 1024 für das
  Produkticon; der Quellenbestand enthält die dokumentierte Erzeugerangabe.
- Die in der Icon-Dokumentation verlinkten EU-Primärquellen zu Artikel 50 erneut
  gelesen. Die Einordnung des konkreten Motivs bleibt ausdrücklich eine
  projektspezifische Ableitung, kein allgemeiner Rechtsnachweis.

## Erhaltene Grenzen

Die interaktive Linux-GUI-/Installationsabnahme ist für den späteren
Releasekandidaten zu wiederholen. Die tatsächliche Integration des installierten
Fastra-Diff-Helfers bleibt separat zu prüfen. Kein Release, keine Installation,
Notarisierung oder Veröffentlichung wurde für diesen Dokumentationsabgleich
angestoßen. Das 1.0-Ziel der Roadmap bleibt eine noch nicht vollständig erfüllte
Anforderung; die Dokumentationskorrektur senkt dieses Ziel nicht ab.

Vorhandene Plattform-/E2E-/native Belege wurden zugeordnet, nicht durch erneute
Ausführung behauptet. Der anschließende vollständige Code-Review erhält einen
eigenen frischen Abdeckungsnachweis.
