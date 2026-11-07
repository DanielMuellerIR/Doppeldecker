# Review-Nachprüfung 2026-11-02

Basis: `8547958e`. Drei Hinweise wurden am aktuellen Quellstand geprüft.

| Hinweis | Entscheidung / Beleg |
| --- | --- |
| Archivuploads behalten dauerhaft ihre vollständigen Ergebnisdaten | Korrigiert für dauerhaft abgeschlossene Uploads und entfernte Pending-Aufträge. Der private Store bindet neue Zwischenstände an das Fensterjournal; Inhalt, Identität und aktive Leser werden vor der Bereinigung geprüft. Cleanupfehler erhalten Prüfpfade und machen keinen Upload wieder ausführbar. Beim erneuten Öffnen werden bekannte abgeschlossene Quellen nachbereinigt. |
| Vier angeblich redundante Namenslängenprüfungen | Die behauptete zentrale Summenprüfung fehlte auf der Reviewbasis. Statt die vier Schutzprüfungen ersatzlos zu entfernen, ist die 32-MiB-Grenze nun zentral und gilt auch für lokale Eigentümeränderungen. Die vier Module verwenden diese gemeinsame Prüfung. |
| Doppelter Rahmen für native Metadatenänderungen | Zusammengeführt. Dateiflags, Sichtbarkeit, Tags und Kommentare verwenden dieselbe fenstereigene Reservierung, Abbruchbindung und Guard-Freigabe. Fachliche Validierung und native Wirkung bleiben in den jeweiligen Commands. |

Die vorgeschlagene Löschung bei jedem Abbruch wurde verworfen: Derselbe
versiegelte Zwischenstand wird für Wiederanlauf und Konfliktfortsetzung benötigt.
Ebenso gibt es keine pauschale Bereinigung nach Ordnername oder Alter. Ältere
Quellen ohne gespeicherte Fensterbindung und unbekannte Crashreste bleiben
bewusst zur Prüfung erhalten. [Archivvertrag](remote-pack.md).

Gezielte Gegenproben erfassen erfolgreichen Upload, Pending-Entfernung vor und
nach Archiverzeugung, Abbruch, Konfliktfortsetzung, verlorene Serverantwort,
Neustart, aktive Leser, fremde Inhalte und getrennte Fensterqueues. Der
Browsergegenlauf prüft die Zwischenstandaktion nach Abschluss, Abbruch und
fehlgeschlagener Bereinigung bei schmaler Ansicht.

Die vollständigen Core-, CLI- und App-Läufe sowie Formatierung, Workspace-Clippy
und gesperrter Fuzz-Build bestehen auf macOS und Linux. Frontend- und
Werkzeugprüfungen bestehen auf macOS; die geänderten Queuezustände sind im
Browser bei 390 CSS-Pixeln visuell geprüft. Der native Paketstand wurde durch
diesen Schnitt nicht ersetzt. Offene Kandidatenabnahme: [Releaseplan](release-plan.md).
