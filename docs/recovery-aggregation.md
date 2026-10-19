# Recoverybilanz über mehrere Aktionen

Stand: 2026-10-06.

Ein erfolgreich übertragener Inhalt kann einen nicht sicher bereinigbaren
Arbeitsbereich hinterlassen. Diese belegten Wiederherstellungspfade gehören zum
Ergebnis und müssen auch dann erhalten bleiben, wenn eine spätere Aktion, Prüfung
oder Journalaktualisierung scheitert. Der führende Fehler behält seine Art und
seinen maschinenlesbaren Code; frühere Pfade werden ohne Duplikate ergänzt.

Die Einweg-Engine sammelt die Pfade während der Aktionen. Der bidirektionale Lauf
übernimmt jede abgeschlossene Projektion vor ihrem Journalabschluss und vor der
Abbruchprüfung. Seine Fehlergrenze ergänzt diese Pfade auch bei späteren
Projektionen, aufgeschobenen Wirkungen, finalen Baumsichten und Baseline-Commits.
Ein Fehler innerhalb der aktuellen Projektion bringt deren eigene Pfade mit.

Die bidirektionale Queue speichert die Pfade zusammen mit dem terminalen Zustand
oder dem noch offenen Abschlussmarker. Auch die erneute Einordnung aus dem
Core-Journal übernimmt sie. Scheitert die dauerhafte Speicherung vollständig,
enthält der zurückgegebene Fehler die bekannten Pfade; ein erfolgreicher
Journalcommit wird dann nicht behauptet. Der mutationslose Core-Abschluss entfernt
keine erhaltenen Arbeitsbereiche.

## Prüfung

Die Regressionen verwenden ausschließlich künstliche Dateien und Konfigurationen.
Eine bewusst geänderte Berechtigung verhindert die sichere Arbeitsbereichsbereinigung.
Geprüft werden der aktuelle Projektions-Journalfehler, eine späte Baumdrift nach zwei
vollständigen Kopien, der Abbruch der zweiten Projektion und deren Quellfehler.
Kopierte Bytes, unveränderte Baselinegeneration und sämtliche tatsächlich erhaltenen
Reste werden unabhängig vom Fehlertext geprüft.

Ein weiterer vollständiger Queue-Lauf bestätigt zwei erhaltene Arbeitsbereiche nach
dem Schließen und Wiederöffnen des Journals. Ein gezielt verhinderter terminaler
Queuecommit bewahrt beide Pfade im Fehlerpayload und meldet `outcome_unclear`.
Beide Regressionen scheiterten auf der vorherigen Produktfassung.

Vollständige serielle Gegenprüfung: macOS 803 Core-Tests und 62 CLI-Tests;
Linux 814 Core-Tests und 62 CLI-Tests. Je Core-Lauf vier ausdrücklich ignorierte
Tests. Formatprüfung und Core-/CLI-Clippy mit allen Targets und Warnungen als
Fehler bestehen auf macOS. Die Linux-Testkopie verwendet Rust 1.96.1; macOS
prüft denselben Quellstand. Keine Frontendänderung oder Produktinstallation.

Die bisherigen Einweg-/Provider-/SFTP-Korrekturen und deren Belege stehen in
[Review-Korrekturen 2026-10-03](release-validation.md).
