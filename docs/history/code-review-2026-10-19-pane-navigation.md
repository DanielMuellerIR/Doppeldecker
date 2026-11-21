# Pane-Navigation: Vorrang vor Hintergrundaktualisierungen

Geprüft wurde das vollständige Bereichsdelta seit dem letzten Vollreview:
Komposition, Ereignisse, Eingabe, Kontextmenü, Dateiinfo, Pane-Controller sowie
geänderte HTML- und CSS-Grenzen. Die aktuellen Event-, Input-, Kontextmenü- und
Dateiinfo-Controller wurden vollständig gelesen; an der Pane-Navigation wurden
Tab-/Profilwechsel, Listingantworten, Auswahl, Auslassungen und Fokus abgeglichen.
Unveränderte Tabmodelle behalten ihren früheren Vollbeleg. Die beiden passiven
Dateiaktions-Aufrufer gehören zum geprüften Schnitt.

## Belegter Fehler

Drei Gegenproben halten Antworten an der tatsächlichen Tauri-Brücke des
Browsercontrollers zurück: ein neues Listing, die Home-Pfadauflösung oder eine
Rename-Antwort bei inzwischen begonnenem Listing. Ein Dateiabschluss startet
jeweils noch einmal das alte Listing und entwertet damit die bereits angeforderte
Navigation. Das betrifft nicht nur die Anzeige eines alten Status: Der gewünschte
neue Ordner wird nach Freigabe der Antwort nicht übernommen.

Passive Aktualisierungen nach Queue- und Mutationsabschlüssen sind jetzt als solche
markiert. Während einer bereits angeforderten Navigation starten sie keinen
konkurrierenden Read des noch sichtbaren alten Ortes. Der Navigationseigentümer
wird vor dem ersten Await gebunden, auch während Home seinen Zielpfad auflöst.
Nur die aktuelle Anfrage räumt diesen Zustand auf; ein verworfener alter Read kann
keine neuere Navigation freigeben. Tabfehler, Listingfehler, Homefehler und das
Deaktivieren einer Pane geben spätere Aktualisierungen wieder frei. Pfade,
Provideridentitäten, Auswahlrechte und Backend-Ausführung bleiben unverändert.

## Prüfung

Die drei gezielten Gegenproben sind vor der Produktänderung rot und danach grün.
Zwei zusätzliche Fälle prüfen die wieder mögliche passive Aktualisierung nach
Listing- und Homefehlern. Die erste Ausführung wurde vor dem Mock-Serverstart durch
die lokale Portsperre abgewiesen und zählt nicht als Produktgegenprobe.

Gegen den abschließenden eingefrorenen Stand bestehen 402 Browserprüfungen
einschließlich CSP, sechs Tabtests und der echte App-Build. Alle 383 erfassten
Quell- und Werkzeugdateien blieben unverändert. Die native Queue-/Navigationsabnahme
besteht gegen denselben Stand. Der native
Aufbau verwendet ausschließlich markierte künstliche Quellen und Ziele. Die
gehaltene Antwortreihenfolge wird gezielt im Browser geprüft; der native Lauf
verwendet die unveränderten Produkt-Commands und hält kein IPC künstlich zurück.

Ein erster nativer Hilfsblock erreichte Navigation während echter Queuearbeit,
überschritt mit 1600 künstlichen Dateien aber seine interne Abschlusswartezeit.
Ein verkleinerter Block erreichte Completed mit 120 bytegleichen Zielkopien und
erhaltenem Navigationsort, scheiterte danach an einer zu engen AX-Erwartung:
Der tatsächliche Zeilentitel enthält auch Größe und Datum. Beide Gesamtblöcke
bleiben als fehlgeschlagen mit erfolgreichem eigenem Cleanup dokumentiert. Die
Wiederholung verwendet dasselbe Produktbinary und korrigiert nur die Hilfserwartung.

Der abschließende native Block besteht vollständig: Die Pane navigiert während
Running an einen anderen künstlichen Ort und bleibt nach Completed dort. Alle
120 Zieldateien sind bytegleich; Quelle und Navigationsdatei bleiben unverändert.
Ein neuer Tab öffnet einen anderen Ort, der anschließende Tabwechsel stellt den
vorherigen Navigationsort wieder her. Die laufende Ansicht und der Abschluss im
hellen Theme sowie die Tabansicht im dunklen Theme wurden visuell geprüft. Der
eigene Prozess-Cleanup besteht mit leerer Restliste; die Zwischenablage wurde
nicht verwendet. Dieser Normalablauf ersetzt keine gehaltene native IPC-Gegenprobe.

Rust, Core, CLI, Provider und Lockfiles sind unverändert. Ihre früheren beidseitigen
Gates werden weiterverwendet und nicht als neu ausgeführt gezählt. Die unabhängige
menschliche Linux-GUI- und Installationsabnahme bleibt offen.
