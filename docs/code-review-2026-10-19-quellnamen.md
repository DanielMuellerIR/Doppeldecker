# Additive Ordnerkopien mit bilanzierten unsicheren Quellnamen

Ein echter Linux-Dateigegenlauf mit zwei sicheren Dateien und zwei nicht als UTF-8
dekodierbaren Dateinamen bricht vor jeder sicheren Addition ab. Der vorhandene
Provider-Fault bestätigt dieselbe Grenze auf macOS. Ein weiterer echter
Linux-Gegenlauf scheitert bereits an der Inhaltsbindung des Quellordners.
APFS verweigert die Anlage des künstlichen Rohnamens selbst mit `EILSEQ`; ein
nicht möglicher lokaler macOS-Rohdateilauf wird deshalb nicht behauptet.

## Begrenzte Korrektur

Nur gewöhnliche Copy-Aufträge mit Überspringen und ohne Konfliktkopieziel dürfen
nicht sicher dekodierbare Quellkinder auslassen. Vollständigkeit des Listings,
eindeutige rohe Einzelkomponenten und sichere Pfadnamen der tatsächlich genutzten
Kinder bleiben Pflicht. Die ausgelassenen Anzeigezeichen werden nie angehängt,
geöffnet oder als Zielnamen verwendet. Der Fortschritt zählt sichere Inhalte;
die Abschlussbilanz zählt Auslassungen und führt Elternpfad und rohe Hexbytes
über die vorhandenen Quellhinweise weiter. Die Queue ergänzt ihre Providerhinweise,
statt die bereits gesammelte Bilanz zu ersetzen.

Die Quellversion bindet die sicheren Kinderinhalte und weiterhin den vollständigen
Rohbestand samt Metadaten. Unsichere Kinder werden dafür nicht geöffnet. Eine
Umbenennung verändert den Versionsbeleg. Für vollständig sicher benannte Bäume
bleibt das bisherige Hashformat erhalten. Zielbelege erhalten keine schwächere
Freigabe. Move, Ersetzen, Planner und strikte Metadatenpfade behalten ihre Gates;
„Beide behalten“ bietet keinen unvollständig übertragbaren Ordner als Konfliktkopie
an. Ein tatsächlich unvollständiges Listing und doppelte oder manipulierte Namen
bleiben Fehler. Ein unsicherer Nachbar in einem strikten Zielbeleg wird weiterhin
nicht umgangen.

Ein zusätzlicher echter CLI-Queuegegenlauf zeigt nach der Corekorrektur bereits
die richtige JSON-Bilanz, im Text aber nur „1 übersprungen“ ohne konkreten Rohname.
Die Textausgabe ergänzt jetzt die vorhandenen Quellhinweise. Das erzeugt keine
neue Schnittstelle oder GUI-Komponente; die bestehende Abschlussanzeige rendert
diese Hinweise bereits als aufklappbare, kopierbare Quellbilanz.

## Prüfung

Die tatsächlichen roten Vorläufe und die Plattformgrenze sind getrennt gesichert.
Die gezielten Gegenläufe prüfen sichere Inhalte, unveränderte Rohquellen, keine
Ersatzzeichenkopie, frische Additionen beim Zusammenführen, sichtbare JSON-/Textbilanz
und Drift eines ausgelassenen Namens. Move, Ersetzen und Konfliktkopie erhalten
keine Freigabe; ein Traversalname wird auch auf dem additiven Weg abgewiesen.

Die vollständigen Rust-Gates bestehen: macOS Core 1142, CLI 76, App 219; Linux Core 1164, CLI 77, App 209. Core ignoriert je 22 Fälle, CLI je 1, App macOS 2 und Linux 4. Die expliziten Kindtests laufen über ihre Eltern; echte Mount-/Providerfälle und zwei Linux-GTK-Fälle behalten ihre Voraussetzungen. Workspace-Clippy mit allen Targets und `-D warnings`, Fuzz-Build mit gelocktem Manifest und Formatprüfung bestehen beidseitig. Alle 385 Quellen bleiben während dieser Läufe unverändert. Die vollständige Browser-Suite besteht mit 408 Prüfungen einschließlich CSP sowie sechs Pane-Tabtests; die Quellbilanz ist in beiden Themes bei 800×600 geprüft. Dieser Review betrifft ausschließlich das
Quellnamens-/Bilanzdelta mit den direkten Queue-/CLI-/Anzeigengrenzen. Historische
Vollreviews bleiben erhalten; es wird kein neuer nativer Linux-GUI-Lauf behauptet.
