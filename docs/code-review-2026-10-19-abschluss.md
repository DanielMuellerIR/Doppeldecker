# Codeprüfung: Abschluss 2026-10-19

Alle 26 Bereiche des vollständigen Reviews sind aktuell. Alle 71 im
Vollreviewprotokoll erfassten Funde sind korrigiert; die anschließenden
Querschnittsdeltas sind gesondert belegt. Kein Bereich, Querschnittskandidat oder
prüfrelevanter Dateipfad bleibt ohne Zuordnung offen. Historische Vollreviewhashes
bleiben erhalten; begrenzte spätere Deltas werden nicht als neue Vollreviews
ausgegeben.

## Wesentliche Ergebnisse

| Bereich | Ergebnis und Beleg |
| --- | --- |
| Bidirektionaler Sync, Dateioperationen und Realmodus | Typwechsel und Namensalias, gebundene Rücklesebelege, erhaltene Kollisionsdateien und Recoverypfade korrigiert. [Review](code-review-2026-10-19.md), [Namensbelege](code-review-2026-10-19-namensbelege.md), [Quellnamen](code-review-2026-10-19-quellnamen.md). |
| Fehler und Providerproben | Maschinenlesbare Fehlercodes und strukturierte Recoverypfade bleiben über Kontext und Cleanup erhalten. [Fehlerkontext](code-review-2026-10-19-fehlerkontext.md), [Probenrahmen](code-review-2026-10-19-probe-cleanup.md). |
| Queue | Sieben Fortschrittsrahmen und zwei Dateiabschlussrahmen verwenden gemeinsame private Bausteine; fenstereigene Reservierung und terminale Reihenfolge bleiben erhalten. [Prüfung](code-review-2026-10-19-queue-rahmen.md). |
| Native Hintergrundarbeit | Bidirektionale Vorbereitung und verbliebener Startgeometriezugriff blockieren den UI-Thread nicht mehr. Tatsächliche Halteproben und Fensterreaktion, bei Geometrie zusätzlich echter Zwei-Fenster-Neustart. [Vorbereitung](code-review-2026-10-19-bidi-hintergrund.md), [Startgeometrie](code-review-2026-10-19-startgeometrie.md). |
| Oberfläche | Späte Queueantworten, Navigation, Favoriten und Geräteaktualisierung bleiben an die verursachende Anfrage gebunden. Die jeweiligen Vollbereiche und nachfolgenden Deltas sind im Reviewprotokoll erhalten. |
| Test- und Buildrahmen | Signalisierte Testeltern hinterlassen keine eigenen Kinder; kalte SFTP-Attrappen werden außerhalb der gemessenen Fallphase vorbereitet. [Signalprüfung](code-review-2026-10-19-sftp-signale.md). Releaseexport und begrenzte lokale Helfer sind gesondert gegengeprüft. |
| Konfigurations- und Papierkorbreste | Zwei ältere Bereinigungskandidaten begründet verworfen: unbelegte Konfigurationsreste und Recoveryhistorie bleiben absichtlich erhalten; neue Papierkorbläufe verwenden ausschließlich den abschließbaren v2-Claim. [Konfiguration](code-review-2026-10-19-konfigurationsreste.md), [Papierkorb](code-review-2026-10-19-papierkorb-drift.md). |

## Verifikation und Aussagegrenzen

Die letzten vollständigen Core-Suiten bestehen auf macOS mit 1143 und auf Linux
mit 1165 Tests; je 22 umgebungsgebundene Fälle bleiben ignoriert. Workspace-Clippy
mit allen Targets und `-D warnings` sowie Formatprüfung bestehen beidseitig.
385 prüfrelevante Dateien sind an den letzten Prüfstand gebunden.

Die zuletzt ausgeführten unverändert weitergeltenden CLI-Suiten bestehen mit
76 beziehungsweise 77 Tests, die App-Suiten mit 220 beziehungsweise 210 Tests.
Die jeweiligen umgebungsgebundenen Ausnahmen stehen bei den Paketbelegen.
Fuzz mit gesperrtem Lockfile sowie Browser/CSP und Pane-Tabs sind geprüft.
Die Browserbilanz beträgt 408 Prüfungen einschließlich CSP sowie sechs
Pane-Tab-Modelltests. Spätere Prüfungen übernehmen diese Belege nur bei
bytegleichen betroffenen Quellen; sie werden nicht als erneut ausgeführt gezählt.

Tatsächliche macOS-App-Gegenläufe belegen die betroffenen nativen Änderungen.
Sie ersetzen keine menschliche Abnahme der Linux-Releasepakete. Wiederholte
Legacy-Drifthinweise sowie Konfigurations- und Recoveryhistorie werden ausdrücklich
aufbewahrt und nicht als unbelegte automatische Löschung behandelt.

## Architektur und weitere Freigabe

Die Schichtung aus Core, dünnen Tauri-/CLI-Adaptern und ereignisgesteuerter
Oberfläche bleibt passend. Gemeinsame Korrektheits- und Persistenzverträge gehören
in den Core; Reservierung, Fortschritt und Ereignisadressierung bleiben an das
verursachende Fenster gebunden. Die konsolidierten Queue- und Fehlerrahmen
verringern parallele Varianten. Ein Paradigmenwechsel ist für die korrigierten
Befunde nicht begründet.

Die Codeprüfung erteilt keine Veröffentlichungsfreigabe. Die nach dem
[Plattformplan](release-plan.md) erforderlichen Linux-Kandidatenprüfungen und die
menschliche Geräteabnahme sind im anschließenden Gegenlauf dokumentiert.
Öffentliche Veröffentlichung und Versionsmeilenstein werden dadurch nicht
vorweggenommen.

## Anschließender Linux-Kandidatengegenlauf

Der unmittelbar anschließende echte Paketbau reproduziert einen weiteren
Prüferfehler am internen AppImage-Desktoplink. Die native Linux-Prüfung belegt
außerdem falsche macOS-Kürzel und fehlende native Fenster-/Beendenaktionen.
Diese drei zusätzlichen Befunde sind korrigiert; die Aufzeichnung enthält damit
74 behobene Funde. Alle 26 Bereiche sind nach ihren begrenzten Folgedeltas wieder
aktuell; frühere Vollreviewbelege und der erste Abschluss bleiben erhalten.

Neue signierte Pakete bestehen den regulären Build, die unabhängigen
Signatur-/Prüfsummengates, tatsächliche AppImage- und installierte DEB-Gegenläufe
sowie den echten Paketwechsel von einem unverändert neu gebauten 0.1.122 auf
0.1.123. Die Deinstallation erhält die künstliche Konfiguration und Testdateien.
Die menschliche Geräteabnahme von Darstellung, F2/F5/F6 und zweitem Fenster ist
anschließend bestanden. Der dabei nachgemeldete Dockicon-Fehler ist ebenfalls
korrigiert: GTK erhält passende 256-Pixel-Icons, auch das Hauptfenster übernimmt
die gespeicherte Variante. AppImage-/DEB-Fenster und globaler Icon-Wechsel sind
nativ geprüft; das sichtbare Dockicon ist menschlich bestätigt. Damit enthält
die Aufzeichnung 75 behobene Funde und weiterhin 26 aktuelle Bereiche.
Details und genaue Grenzen stehen im
[aktuellen Linux-Kandidatenbericht](linux-kandidat-2026-10-19.md).
