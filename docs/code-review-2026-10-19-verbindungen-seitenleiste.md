# Verbindungen und Seitenleiste: aktuelle Bestände und sichtbare Fehler

Alle sechs Bereichsdateien mit 2250 Ausgangszeilen wurden vollständig gelesen:
Verbindungseditoren, Profilverwaltung, Geräte, Favoriten, Discovery und
Abschnittsreihenfolge. Geprüft wurden Profilrevisionen, native Einmal-Secret-IDs,
Dialogbesitz, Pin-/Endpointbindung, prozessweite Profilereignisse, Pane-Rückfälle
und verzögerte Antwortfolgen. Der Backendvertrag für Mount-IDs und tatsächlichen
Auswurf-Readback sowie der gemeinsame gesperrte Favoritenstore wurden abgeglichen.

## Belegte Fehler

Eine alte Geräteantwort kann nach bestätigtem Auswurf das verschwundene Volume
wieder anzeigen. Ein verspäteter Fehler derselben alten Abfrage leert stattdessen
auch die aktuelle gesunde Geräte- und Freigabenliste. Beide Gegenproben halten die
tatsächliche Antwort an der Tauri-Brücke des realen Browsercontrollers zurück.
Geräteabfragen übernehmen jetzt nur ihre aktuelle Generation. Nach bestätigtem
Auswerfen werden betroffene Panes weiterhin synchron entwertet, anschließend wird
der tatsächliche Mountbestand frisch gelesen. Auch überholte Gesamtantworten eines
anderen Auswurfs können so keinen dauerhaften alten Bestand festschreiben.

Eine alte Favoriten-Mutationsantwort kann einen später entfernten Eintrag wieder
anzeigen. Eine ältere Startliste blendet umgekehrt einen inzwischen hinzugefügten
Favoriten aus. Nach Hinzufügen, Entfernen, Umsortieren und Finder-Import wird der
gemeinsame Store jetzt frisch gelesen; für diese Reads gilt dieselbe
Generationensperre. Die Mutationsantwort erteilt weiterhin keine Navigations- oder
Dateirechte. Ein reiner Readbackfehler benennt die bereits gespeicherte Änderung.

Der bisherige Erstlesefehler wurde vollständig verschwiegen und als leere Liste
dargestellt. Eine zusätzliche rote Gegenprobe verwendet den tatsächlichen ersten
Favoriten-Read mit invalid_data. Die Oberfläche zeigt jetzt „Favoriten laden“ und
den Fehlertext. Der letzte bekannte Bestand bleibt erhalten; die fehlerhafte
Ablage wird durch die Anzeige nicht ersetzt.

## Prüfung und Grenzen

Fünf gültige rote Gegenproben belegen die drei Fehler vor der Korrektur; danach
bestehen alle sechs gezielten Fälle einschließlich des getrennten
Änderungs-/Readbackfehlers. Ein früher zusätzlicher Hilfsfall wartete auf eine
Folgeabfrage, die der alte Controller noch nicht ausführt. Dieser Block bleibt
fehlgeschlagen; die dafür korrigierte Gegenprobe hält die tatsächliche Startabfrage.

Vor der zusätzlichen Erstlesefehlerkorrektur bestanden 407 Browserprüfungen,
sechs Tabtests, CSP und der echte App-Build mit 383 unveränderten Quellhashes.
Die endgültige zusätzliche Korrektur besteht mit 408 Browserprüfungen einschließlich
CSP, sechs Tabtests und dem echten App-Build. Alle 383 erfassten Quell- und
Werkzeugdateien bleiben auch nach der Schlussabnahme unverändert.

Der erste native Aufbau wurde vor jeder Bedienung beendet: Die eigene Diskimage
lag unter dem markierten Wegwerfbaum, während der bestehende macOS-Vertrag lokale
Nichtroot-Volumes nur unter /Volumes anzeigt. Das Volume wurde korrekt ausgefiltert;
der Hilfsblock gilt als fehlgeschlagen. Der eigene Cleanup besteht einschließlich
exakt imagegebundenem Detach. Der Produktfilter bleibt unverändert.

Eine zweite native Hilfserwartung suchte die Speicherbilanz nur in AXTitle; der
gesicherte Dump liefert sie in AXValue. Der Block scheiterte vor der
Favoritenbedienung und besitzt ebenfalls einen vollständigen eigenen Cleanup.
Eine dritte Hilfserwartung betraf die Anzahl von Koordinatentreffern für Plus
und Abschnittsgriff. Der AX-Dump belegt die beiden echten Plus-Buttons; der
Prüfweg wurde auf die vorhandene AX-Aktion mit anschließender Storeprüfung
vereinfacht. Auch dieser fehlgeschlagene Gesamtblock ist vollständig bereinigt.
Die korrigierte Schlussabnahme verwendet denselben finalen Produktstand.

Die native Schlussabnahme besteht mit echten künstlichen Favoritenänderungen,
der normalen Geräteanzeige nach Fokusrefresh und dem tatsächlichen Fehlerstart
mit künstlicher Konfiguration. Hinzufügen und Entfernen sind am echten Store
belegt; die künstlichen Quelldaten und beschädigten Konfigurationsbytes bleiben
unverändert. Die hinzugefügte Favoritenansicht im hellen Theme sowie leere Liste
und Fehlerstart im dunklen Theme wurden visuell geprüft. Der eigene Cleanup
bestätigt eine leere Restliste. Die gehaltene Auswurf-Reihenfolge wird im Browsercontroller belegt;
ein nativer physischer Auswurf oder ein gehaltenes natives IPC-Rennen wird nicht
behauptet. Rust, Core, CLI, Provider und Lockfiles sind unverändert; frühere
beidseitige Gates werden weiterverwendet. Die menschliche Linux-GUI- und
Installationsabnahme bleibt unabhängig offen.
