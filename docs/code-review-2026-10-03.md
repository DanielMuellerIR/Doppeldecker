# Review-Korrekturen 2026-10-03

Drei Fehler bei der dauerhaften Bilanz erhaltener Arbeitsbereiche behoben:
spätere Sync-Fehler erhalten zuvor gesammelte Pfade; Provider-Erfolg und Abbruch
speichern Zustand, Fortschritt und Pfade atomar; ein zusätzlicher Auditfehler
bewahrt Code und strukturierte Pfade des ursprünglichen Fehlers.

Die drei Regressionen scheitern auf der ursprünglichen Produktfassung.
Mehrdatei-Fehler, Provider-Erfolg, Abbruch und fehlgeschlagener Journalabschluss
laufen mit echten künstlichen Dateien. Wiedergeöffnete Queue-Journale bestätigen
die gespeicherten Pfade.

Prüfung: macOS 819 Core-Tests und 62 CLI-Tests bestanden; Linux 830 Core-Tests
und 62 CLI-Tests bestanden. Je Core-Lauf vier ausdrücklich ignorierte Tests.
Core-/CLI-Clippy mit allen Targets und Warnungen als Fehler sowie Formatprüfung
bestanden. Linux-CLI seriell geprüft: erster Parallelversuch hatte einen
Dateiübertragungsrest im Quellscan und einen fehlgeschlagenen Laufjournal-Lock;
nach Bereinigung der Testkopie und serieller Ausführung waren beide grün.
Keine Frontendänderung, Produktinstallation oder Veröffentlichung.
