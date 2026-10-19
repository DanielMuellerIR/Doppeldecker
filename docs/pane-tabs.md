# Pane-Tabs: Zustand und Persistenz

Stand: 2026-10-07. Tab-Leisten, Pane-Anbindung und Sitzungsformat 2 sind umgesetzt.
Browserprüfung und native macOS-GUI-Abnahme einschließlich echtem App-Neustart
mit wiederhergestellten Tabs, Pfaden, aktiver Pane und Auswahl bestehen.

## Grenzen

Ein Tab gehört genau einer Pane eines Fensters und besitzt Endpunkt, absoluten
Pfad, Verlauf samt Position, Auswahl nach Namen, Cursor/Anker, Scrollposition,
Sortierung und Darstellungsoptionen. Unterschiedliche Tabs dürfen denselben Ort
zeigen. Spaltenbreiten bleiben am Pane. Ein neuer Tab übernimmt dessen aktuellen
Endpunkt und Darstellung, beginnt aber ohne Auswahl und mit einem eigenen Verlauf.
Der letzte Tab eines Panes bleibt offen. Beim Schließen des aktiven Tabs wird der
nächste, am rechten Ende der vorherige Tab aktiv.

Providerverbindungen bleiben prozessweit sichtbar. Operations-Queue,
Single-Flight, Vorbereitungen und Permits bleiben je Fenster, gemäß dem
[Mehrfenstervertrag](goal-mehrere-fenster.md). Ein Tab erzeugt keine weitere Queue
und übernimmt keinen fremden Permit. Laufende Dateioperationen verwenden weiterhin
ihren bereits bestätigten Endpunkt; ein Tab-Wechsel darf ihn nicht umdeuten.

## Lesen und Wechseln

`frontend/pane-tabs.js` hält Einträge und Leseanfragen ausschließlich im RAM.
Jede Anfrage bekommt einen Token, der an genau das Modell und den konkreten
Anfragebeginn gebunden ist. Neuere Anfragen derselben Art verdrängen ältere.
Beim Verlassen oder Schließen eines Tabs werden dessen Tokens ungültig. Auch nach
Rückkehr darf eine Antwort des alten Aufenthalts keinen Zustand ändern. Nur der
aktive Tab beginnt neue Anfragen. Eine Profiländerung entwertet alle betroffenen
Tabs einschließlich ihrer zwischengespeicherten Einträge; Navigation bleibt erhalten.

Der Pane-Controller listet beim Aktivieren frisch, stellt Auswahl und Cursor nur
gegen tatsächlich gelistete Namen wieder her und lässt die Scrollposition auf den
aktuellen Listenbereich begrenzen. Er bricht laufende Ordnergrößenarbeit ab,
verwirft verspätete Antworten gegen Pane-Sequenz und Tab-Token und verwirft
Realmodus-Vorbereitungen vor einem Ortswechsel. Profilrevisionen werden aus den aktuellen gespeicherten
Profilen aufgelöst, niemals aus einem Tab als neue Provideridentität erfunden.
Fehlende Profile bleiben als sichtbarer Fehler im betroffenen Tab erhalten.

Beim Refresh desselben Endpunkts bleiben die Darstellungsoptionen des aktiven
Tabs erhalten, auch wenn ein anderer Tab für denselben Ordner andere Optionen
speichert. Gemeinsame Ordnerdefaults werden erst beim Navigieren an einen anderen
Ort eingelesen. Ein fehlgeschlagenes Listing überschreibt den letzten erfolgreich
belegten Auswahl-, Cursor-, Anker- und Scrollstand nicht mit der leeren Fehleransicht.
Ein erneuter Refresh stellt diesen Stand nur gegen die frisch gelisteten Namen
wieder her und begrenzt die Scrollposition auf die tatsächliche Liste.

## Bedienung

Jede Pane besitzt eine eigene Tab-Leiste mit aktivem Tab, Schließen-Knöpfen und
„Neuer Tab“. `Cmd+T` beziehungsweise `Ctrl+T` öffnet einen Tab der aktiven Pane;
`Ctrl+Tab` und `Ctrl+Shift+Tab` wechseln vorwärts beziehungsweise rückwärts.
`Cmd+Shift+W` beziehungsweise `Ctrl+Shift+W` schließt den aktiven Tab. Das native
macOS-Kürzel `Cmd+W` schließt weiterhin das Fenster. Auf einem fokussierten Tab
wechseln Pfeiltasten, `Home` und `End`; `Delete` oder `Backspace` schließt ihn.
Während eines Queue-Transfers bleiben Tab-Wechsel, Öffnen und Schließen erreichbar;
der Auftrag behält seine gebundenen Endpunkte. Eine exklusive App-Aktivität sperrt
diese Bedienung weiterhin. Der letzte Tab und die Grenze von 32 Tabs bleiben geschützt.

## Sitzungsdatei

`desktop-settings.json` verwendet Format 2. Je Fenster enthält `tabs` getrennte
`left`-/`right`-Modelle mit `activeId` und einer Liste der Navigationsstände.
Der aktive Endpunkt muss mit dem bisherigen Pane-Endpunkt übereinstimmen.
`remember_window` nimmt den optionalen Tab-Stand entgegen; ohne ihn wird der
bisherige aktive Ort zu genau einem Tab je Pane. `restored_pane_tabs` liefert nur
den Stand des aufrufenden Fensters. Die vorhandene Einstellung zur
Sitzungswiederherstellung bleibt maßgeblich.

Format 1 wird beim Lesen ausschließlich im RAM zu einem Tab je Pane migriert.
Einstellungen, Revision und Fensterdaten bleiben erhalten. Erst der nächste normale
Schreibvorgang ersetzt die alte Datei atomar durch Format 2. Unbekannte Felder,
Versionsnummern oder ungültige Zustände sind Lesefehler; eine solche Datei wird vom
Store nicht überschrieben. Das Frontend serialisiert eine feste Feldliste;
Provider-Handles, Lese-Tokens, Secrets, Queue und Permits werden nie gespeichert.

Die Grenzen gelten beim Lesen und Schreiben: maximal 64 Fenster, 32 Tabs je Pane,
128 Verlaufspfade je Tab, 4096 ausgewählte Namen und insgesamt 2 MiB Dateigröße.
Ein Pfad hat maximal 4096 Bytes, ein Name maximal 255 Bytes. Überschreitet ein sonst
gültiger Gesamtstand die Dateigrenze, bleibt die bisherige Datei unverändert und
das Speichern meldet einen Fehler. Tab-IDs sind pane-lokale UI-Identitäten und
verleihen keine Datei- oder Providerberechtigung.
Für die Persistenz werden die ersten 4096 ausgewählten Namen übernommen;
während der laufenden Sitzung bleibt die vollständige Auswahl erhalten.

Auswahl, Sortierung, Darstellung und Scrollposition stoßen auch ohne Orts- oder
Tab-Wechsel den vorhandenen verzögerten Sitzungsspeicher an. Der aktuell offene
Tab braucht daher keinen zusätzlichen Wechsel, um seinen Stand zu sichern.

## Prüfung

Die Modelltests prüfen mehrere Tabs in beiden Panes, unabhängige Darstellung und
Auswahl nach Neustart, überholte/geschlossene/fensterfremde Lesetokens, Profilwechsel,
letzten Tab und feste Persistenzfelder. Backend-Tests prüfen die tatsächlich vom
Frontend verwendeten verschachtelten CamelCase-Felder, Endpunktbindung und
ungültige Zustände. Temporäre Sitzungsdateien belegen die Migration ohne lesenden
Dateiersatz, zwei getrennte Fenster und den atomaren Fehlerpfad bei Übergröße.
Die Browser-Suite prüft die Tab-Leisten beider Panes, Auswahl und Sortierung,
virtuelle Scrollposition, verspätete Antworten geschlossener Tabs, Listingfehler,
entfernte inaktive Profile, Wiederherstellung und die Tab-Grenze.
Tastaturauswahl und Scrollposition werden auch ohne folgenden Ortswechsel
gespeichert und nach Browser-Neustart geprüft. Im nativen macOS-Fenster sind
Öffnen, Wechseln und Schließen per Tastatur für beide Panes geprüft. Ein echter
App-Neustart mit derselben künstlichen Sandbox stellt je zwei Tabs, ihre Pfade,
die aktive Pane und Auswahl aus der tatsächlichen Sitzungsdatei wieder her.
Der Testbeobachter wartet dabei auf die asynchron abgeschlossene Wiederherstellung;
eine zu frühe Prüfung der aktiven Pane war ein Fehler der ersten Testfixture.

Die ergänzende native Fehlerprüfung entfernt einen künstlichen Quellordner bei
inaktivem Tab. Der folgende Wechsel zeigt die leere Fehleransicht, erhält aber den
bewiesenen Sitzungsstand einschließlich einer Scrollposition von 1200 Pixeln. Nach
Wiederherstellen des Ordners lädt ein direkter Refresh Auswahl, Cursor, Anker und
Scrollposition zurück. Zwei Tabs desselben Pfads behalten beim Refresh ihre
unterschiedliche Einstellung für versteckte Dateien.
