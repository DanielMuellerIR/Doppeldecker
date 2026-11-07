# Dateieigenschaften bearbeiten

Stand: 2026-11-07. Der Informationsdialog übernimmt die verfügbaren Editoren vom
Backend. Links und Spezialdateien bleiben unverändert; andere Objekte einer
Auswahl bleiben unabhängig bearbeitbar.

| Eigenschaft | Verfügbar | Wirkung |
| --- | --- | --- |
| Normale Unix-Rechte | Lokal, SFTP, FTP/FTPS | Drei Oktalziffern; wahlweise rekursiv. Sonderbits bleiben erhalten. |
| Eigentümer/Gruppe | Lokal; optional mit Administratorfreigabe auf macOS und Linux | Lokale Namen oder IDs; leere Felder unverändert. Kein `chown` bei bereits passenden IDs. |
| Finder-Tags und Farben | Lokal auf macOS | Je Objekt anzeigen, hinzufügen, umbenennen und entfernen; Sammelzuweisung und Leeren der Auswahl. |
| Finder-Kommentare | Lokal auf macOS | Lesen, speichern und entfernen, auch für Auswahlen; Abgleich mit Finder und Kommentar-Attribut. |
| Dateisperre | Lokal auf macOS | Sperren/entsperren, auch für Auswahlen; Systemflags bleiben unverändert. Gesperrte Objekte müssen vor anderen Änderungen entsperrt werden. |
| Erweiterungsanzeige | Lokale reguläre Dateien auf macOS | Erweiterung im Finder ausblenden/einblenden; tatsächlicher Dateiname und andere Finder-Informationen bleiben erhalten. |
| Verstecktmarkierung | Lokal auf macOS | Einzeln umschalten, für Auswahlen setzen/entfernen. Punktnamen bleiben verborgen; Pane-Filter und Suche berücksichtigen beide Gründe. |

Weitere Hardlinknamen teilen die Wirkung; der Kommentar-Editor lässt solche Dateien wegen der namensbezogenen Finder-Ablage unverändert. Ein Eigentümerwechsel kann Sonderbits
entfernen; die Bilanz nennt das tatsächliche Ergebnis. Rekursive Rechteänderungen
erfassen vorab höchstens 100.000 Objekte und 32 MiB Pfadnamen und ändern Kinder vor
Eltern. Dieselben Rechte gelten für Dateien und Ordner.

Lokal erfolgen Änderung und Readback am gehaltenen Objekt ohne Symlinkfolgen.
SFTP und FTP/FTPS nutzen frische Root-/Eltern- und Objektprüfungen, genau einen
Schreibversuch und Readback. Ein gleichzeitiger serverseitiger Pfadtausch bleibt
möglich; FTP benötigt vollständige Unix-Rechte und SITE CHMOD. Strikte
Syncfähigkeiten werden dadurch nicht erweitert.

Jeder Lauf gehört zu seiner Anfrage und seinem Fenster, lässt sich abbrechen und
bilanziert jedes Objekt. Bestätigte Änderungen bleiben erhalten. Ein unklarer
Ausgang erscheint als `outcome_unclear`; Remoteänderungen werden danach gestoppt.
Kein blinder Retry. Ansichts- und Ergebnisseiten enthalten höchstens 40 Objekte.

Finder-Tags ersetzen keine anderen xattrs oder Resource Forks. Alte
Finder-Farbmarkierungen bleiben separat erhalten und sind ausdrücklich entfernbar.
Beschädigte Tagdaten bleiben unverändert; mögliche Teilwirkungen sind sichtbar.

Finder-Kommentare werden erst beim Öffnen ihres Bereichs gelesen. Eine fehlende
macOS-Freigabe für Finder wird ausdrücklich angefordert. Die native Dateireferenz
kommt vom gehaltenen Objekt; Kommentartext bleibt typisierte Nutzlast. Erfolg
verlangt passenden Finder- und Attribut-Readback; kanonisch gleicher Unicode-Text
gilt als übereinstimmend. Höchstens 16 KiB UTF-8 pro Kommentar.

## Administratorfreigabe auf macOS

Das signierte und notarisierte App-Paket enthält einen optionalen Metadatenhelfer.
Im Informationsdialog zuerst den Administratorzugriff einrichten und in den
Systemeinstellungen erlauben. Anschließend lässt sich die Administratorfreigabe
für Rechte- oder Eigentümeränderungen ausdrücklich anwählen. Der Helfer führt
ausschließlich diese lokalen Metadatenaktionen aus; gewöhnliche Dateiaktionen
erhalten dadurch keine erhöhten Rechte. Der Zugriff lässt sich dort auch entfernen.

App und Helfer prüfen gegenseitig Signatur und Version. Jede Änderungsanfrage
benötigt eine native macOS-Freigabe; Zugangsdaten gelangen nicht ins Frontend oder
Journal. Root und ausgewählte Objekte werden vor der Freigabe erfasst und danach
erneut gebunden. Abbruch und unklare Teilwirkungen bleiben sichtbar, ohne Retry.
Ein betreuter nativer Lauf bestätigt Eigentümerwechsel zu `root`, Rechteänderung
am root-eigenen Objekt und Rückwechsel ohne Inhalts- oder Zeitänderung.

## Administratorfreigabe auf Linux

Der Informationsdialog bietet denselben ausdrücklichen Administratorzugriff für
lokale Rechte und Eigentümer/Gruppe. `pkexec` holt die Polkit-Systemfreigabe ein
und startet den mitgelieferten Helfer ausschließlich für einen typisierten
Metadatenauftrag. Eine Diensteinrichtung ist nicht nötig. Ohne Polkit bleiben
gewöhnliche Änderungen mit den Rechten des App-Prozesses verfügbar.

Private Pipes übertragen die gebundene Auswahl und das Ergebnis. Erst nach der
Systemfreigabe bestätigt die App den Ausführungsstart; ein inzwischen
abgebrochener Auftrag startet nicht mehr. Verbindungsabbruch beendet weitere
Änderungen, bestätigte Teilwirkungen bleiben erhalten. Ein verlorenes Ergebnis
bleibt `outcome_unclear`. Root, Objekte, Rekursion und Readback verwenden denselben
Corevertrag wie auf macOS. Der Helfer besitzt keinen allgemeinen CLI-Parser,
startet keine Programme und erhält keine erhöhten gewöhnlichen Dateiaktionen.

## Grenzen und Prüfstand

Ohne Administratorfreigabe kann macOS O_EVTONLY Leserecht verlangen
(etwa bei Modus 000).
Der SFTP-Infovergleich mit ForkLift belegt reine Owner-/Gruppenanzeigen; ein
entfernter Editor ist keine belegte Releasepflicht. Keine automatische ACL-Übersetzung.
Core-/App-/Browserprüfungen und reale Providerläufe sind vorhanden; native
Linux-Fensterläufe bestätigen Rechte/Gruppe, Rekursion und Sonderbits. Native
macOS-Läufe bestätigen Rechte/Gruppe, Tags und das Verstecken/Wiedereinblenden
nach App-Neustart, Dateisperre und Erweiterungsanzeige sowie Finder-kompatibles
Speichern und Entfernen von Kommentaren.
Unabhängige Attributprüfungen bestätigen unveränderte Inhalte und weitere
Metadaten. Der native macOS-SFTP-Lauf bestätigt Anzeigenamen und direkte
Rechteänderung mit unabhängigem Readback. Frische Finder-Automation-Freigabe und
weitere entfernte macOS-Abläufe bleiben Teil der Kandidatenabnahme.
