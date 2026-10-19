# Hintergrundarbeit und Datei-Zwischenablage

Stand: 2026-10-18.

## Während einer Übertragung weiterarbeiten

Die Queue arbeitet je Fenster in Reihenfolge. Navigation, Tabs, Auswahl,
Informationen, Darstellungsoptionen und weitere Copy-/Move-Aufträge bleiben während
eines Transfers erreichbar. Der laufende Auftrag behält seine ursprüngliche
Auswahl, Endpunkte und Profilrevisionen. Direkte Mutationen und neue Sync-
Vorbereitungen verwenden weiterhin die fenstereigene Ausführungsschranke.
Umbenennen, Ordner anlegen und Löschen nennen bei belegter Schranke den laufenden
Auftrag als Grund; ein sichtbarer Menüaufruf bleibt nicht still ohne Rückmeldung.

Die Syncansicht lässt sich während eines direkten Laufs über „Dateiansicht“
schließen. Das beendet den Lauf nicht. Fortschritt und Abbruch bleiben im Dock;
ein Startfehler erhält die Vorbereitung für einen erneuten Versuch.

Remote-Dateiaufträge können während einer belegten Ausführung ohne Providerkontakt
persistiert werden. Die Oberfläche erklärt vorher die mögliche Best-Effort-Klasse.
Der Worker prüft Verbindung, Profilrevision, Fähigkeiten und Konflikte frisch am
Kopf der Queue und nutzt die stärkste verfügbare Ausführung. Vorhandene Ziele werden
bei diesem Weg nicht ersetzt. Neue Konflikte halten vor Wirkung an; eine fehlende
Fähigkeit bleibt eine konkrete Absage. Mutierende Befehle werden nicht blind
wiederholt.

Datei-Queueereignisse tragen Lauf-ID, Versuchszählung und betroffene Endpunkte
einschließlich Profil-ID und Revision. Verspätete Ereignisse eines alten Laufs
überschreiben den nächsten Lauf oder Wiederholversuch nicht. Ein Abschluss liest
aktuell sichtbare betroffene Roots und Unterordner frisch; andere Orte bleiben
stehen. Auswahl und Scrollposition bleiben bei diesem passiven Refresh erhalten.
Die Anzeigezuordnung erteilt keine Ausführungsrechte.
Eine bereits angeforderte Navigation hat Vorrang vor passiven Aktualisierungen
des noch sichtbaren alten Ortes. Das gilt auch während der Auflösung des
Home-Pfads und nach direkten Mutationsabschlüssen. Ein Navigationsfehler gibt
spätere Aktualisierungen wieder frei.
Auch eine später eintreffende Einreihantwort erhält einen bereits sichtbaren
Fortschritt oder Abschluss. Die erfolgreiche Einreihung zählt weiterhin für
mehrere Quellgruppen und den Clipboard-Abschluss. Bei erneuter lokaler
Sync-Bestätigung führt ein Klick auf Abbruch oder Bestätigung zurück zur sichtbaren
Queue und zum passenden Eintrag; entfällt seine Aktion, bleibt eine erreichbare
Queueaktion fokussiert.
Die gleiche Anzeigenbindung gilt für das Einreihen eines bidirektionalen Syncs:
Eine bereits vor der Command-Antwort eingetroffene Rückfrage oder ein Abschluss
bleibt sichtbar. Ein inzwischen gewechselter Sync-Scope erhält keinen alten
Einreihstatus.

Lokale reguläre WebDAV-Dateiuploads verwenden dieselbe Fensterqueue mit einer
privaten bedingten Zielsicht ohne künstliche Collection-/Löschprobe. Die anfängliche
Inhaltsvorbereitung ist abbrechbar; nach Neustart wird eine gespeicherte Best-Effort-
Erwartung frisch bestätigt. Ein unklarer Tempintent erlaubt keinen Retry.
[Uploadvertrag](webdav-upload.md).

## Native lokale Dateien kopieren und verschieben

Gewöhnliche additive Ordnerkopien mit „Überspringen“ dürfen aus einem vollständigen
Quelllisting unabhängig sichere Kinder übertragen, wenn einzelne andere Rohnamen
nicht sicher dekodierbar sind. Diese Einträge werden nie über ihre Anzeigezeichen
geöffnet. Die Abschlussbilanz nennt ihre Anzahl sowie Elternpfad und rohe Hexbytes
in den Quellgrenzen; Queue und CLI erhalten dieselben Hinweise. Ein Quellversionsbeleg
bindet auch den ausgelassenen Rohbestand und erkennt dessen Namensänderung.
Move, Ersetzen und vollständige Konfliktkopien übernehmen diese Auslassungsfreigabe
nicht. Unvollständige Listings, doppelte Rohnamen, Traversalnamen und strikte
Zielbelege bleiben gesperrt.

| System | Kopieren | Einfügen | Verschieben |
| --- | --- | --- | --- |
| macOS | ⌘C | ⌘V | ⌘C, danach ⌥⌘V |
| Linux | Ctrl+C | Ctrl+V | Ctrl+X, danach Ctrl+V |

Kontext- und Dateimenü bieten dieselben Aktionen. „Bearbeiten“ behält die nativen
Textbefehle. Die aktive Pane folgt auch dem Tastaturfokus. F5/F6 und Drag-and-drop
bleiben zusätzliche Wege.

macOS schreibt native `public.file-url`-Einträge; Finder-Dateireferenzen werden
über Foundation zum tatsächlichen Pfad aufgelöst. Linux schreibt `text/uri-list`,
`x-special/gnome-copied-files` und `application/x-kde-cutselection` und liest die
jeweiligen Copy-/Cut-Kennzeichen. Dateinamen mit Leerzeichen, Unicode, Prozent- und
Steuerzeichen bleiben als URI kodiert. Pfadtext wird nicht als Dateiauswahl
interpretiert. Die Übergabe unterstützt bis zu 100.000 Dateien und 16 MiB
URI-Daten; größere Auswahlen erhalten eine sichtbare Fehlermeldung.
Copy übergibt Referenzen ohne Dateisystemscan; frische Quellenprüfungen gehören
zum Einfügen und Executor. Die zuletzt in Doppeldecker angeforderte Kopierauswahl
gewinnt auch dann, wenn eine große ältere Auswahl später aufbereitet ist.
Inzwischen kopierter Text oder ein anderer neuer Clipboard-Inhalt bleibt erhalten.
Sofortiges Einfügen wartet auch aus einem anderen Fenster auf einen noch
ausstehenden eigenen Copy-Auftrag;
ein fehlgeschlagener Copy darf keine vorherige Auswahl übertragen.

Einfügen bindet das aktive Ziel vor dem Lesen der Systemzwischenablage. Navigation
während des Lesens verwirft diese noch nicht eingereihte Absicht. Quellen aus
mehreren Elternordnern werden getrennt über die vorhandenen Queueverträge
eingereiht und unmittelbar vor der Ausführung frisch geprüft. Fehlende Quellen,
Konflikte und nicht eingereihte Gruppen bleiben sichtbar.

Ein Move verbraucht den gelesenen Clipboard-Stand erst nach vollständigem Erfolg
aller Gruppen. Abbruch, ausgelassene Symlinks, übersprungene Konflikte, Teilfehler
und unveränderte Quellen im Zielordner erhalten die Auswahl. Ein späterer
Clipboard-Inhalt bleibt auch bei erfolgreichem Move erhalten. Die flüchtige
Zuordnung der Clipboard-Übergabe wird nicht persistiert; nach einem App-Absturz
bleibt deshalb gegebenenfalls die alte Auswahl in der Systemzwischenablage.
Ein erneutes Einfügen prüft die Quellen wieder frisch.

Remote-Quellen gehören noch nicht zur Datei-Zwischenablage. Sie nutzen F5/F6;
native lokale Clipboard-Dateien können ein erreichbares Remote-Ziel verwenden.
Ohne Clipboard-Manager endet unter Linux die normale GTK-Clipboard-Eigentümerschaft
mit der App. Die native Formatprüfung ersetzt keine interaktive Abnahme mit einem
Linux-Dateimanager; diese bleibt Teil der Plattformabnahme.
