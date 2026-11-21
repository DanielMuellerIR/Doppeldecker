# Sync-Frontend: Vorbereitung und aktuelle Anzeige

Geprüft wurde das vollständige Delta seit dem letzten Sync-Frontend-Vollreview.
Der neue lokale Wirkungsdialog sowie die aktuellen bidirektionalen, einseitigen
und Ansichtscontroller wurden vollständig gelesen. Unveränderte Plananzeige,
Vergleichsansicht und Realmodus behalten ihren früheren Vollbeleg. Die Rollen- und
Entscheidungsdaten wurden an den tatsächlichen Core-Konfliktansichten abgeglichen,
die Aufrufer an der aktuellen Scope- und Controllerverdrahtung.

## Belegte Fehler

Zwei rote Gegenproben halten die tatsächliche Antwort des bidirektionalen
Einreihaufrufs an der kontrollierten Tauri-Brücke zurück. Die echte Oberfläche
erhält zuvor entweder eine Queue-Rückfrage oder einen terminalen Abschluss.
Beide Anzeigen werden danach fälschlich durch „Bidirektional eingereiht“ ersetzt.
Die Rückfrage ist schon nötig beziehungsweise der Lauf schon fertig, während die
alte Empfangsmeldung noch einen zukünftigen Scan ankündigt.

Der Controller verwendet jetzt die bestehende Anzeigenrevision und den aktuellen
Sync-Scope. Ein neuerer Queuezustand oder eine gewechselte Ansicht wird von der
Einreihantwort nicht überschrieben. Die erfolgreiche Backendübergabe wird weiterhin
abgeschlossen; ein fehlgeschlagenes Enqueue behält seinen bisherigen Retry mit
derselben Vorbereitung. Direkter Start, Recovery, Rollen, Konfliktentscheidungen,
Digests und Ausführungsrechte ändern sich nicht.

Die beiden Gegenproben bestehen nach dieser Korrektur. Sie belegen gezielt die
Event-/Antwortreihenfolge im realen UI-Controller; sie simulieren keinen weiteren
Core-Mutationsfehler und behaupten kein zeitlich gehaltenes natives IPC-Rennen.

Zwei zusätzliche Gegenproben in beiden Themes wählen zunächst ein striktes
einseitiges Profil und Update und wechseln dann in beide Richtungen. Der echte
Frontend-Request übernimmt diese Optionen nicht; die Anzeige bietet sie trotzdem
weiter als wirksame Auswahl an. Der Core akzeptiert für bidirektionale Snapshots
ausschließlich Portable und bindet stets vollständige Inhaltsbelege. Das gilt auch
für die Ersetzungs-/Entfernungsentscheidungen aus Baseline und Konfliktplan.

Die drei Einweg-Optionsgruppen, ihr profilspezifischer Text und der dort
wirkungslose einseitige Realmodusknopf sind in beiden Richtungen jetzt verborgen.
Der Hinweis erklärt BLAKE3, Portable und die Herkunft der Wirkungen. Der bestehende
Backuphinweis bleibt sichtbar. Ein Wechsel zurück erhält alle vorherigen
Einwegeinstellungen. Es entstehen weder ein natives bidirektionales Profil noch
zusätzliche Ausführungsrechte.

## Prüfstand

Alle vier gezielten Fälle bestehen. Gegen den abschließenden eingefrorenen Stand
bestehen 397 Browserprüfungen einschließlich CSP, sechs Tabtests und der echte
App-Build. Alle 383 erfassten Quell- und Werkzeugdateien blieben unverändert.

Der native Aufbau verwendet ausschließlich eine markierte künstliche Quelle,
ein leeres künstliches Ziel und eine frische App-Konfiguration. Die Erstbaseline
verlangt eine ausdrückliche Konfliktentscheidung, bevor der Lauf eingereiht wird.
Ein erster Hilfslauf suchte die Radiowahl fälschlich nur als AXButton und erreichte
den Produktfall nicht. Eine zweite Hilfserwartung verwechselte den tatsächlichen
Ready-Hinweis mit einem Fehleraktionsknopf. Beide Gesamtblöcke zählen als
fehlgeschlagen; Cleanup und erreichte Teilbelege sind getrennt gesichert.

Die korrigierte Wiederholung bestand vor der Optionsänderung. Der abschließende
native Lauf besteht auch mit den bereinigten Optionen: BLAKE3-/Portable-Hinweis und
Backuphinweis sind sichtbar, die wirkungslosen Einwegoptionen fehlen. Nach der
Erstbaseline-Entscheidung wartet der Lauf in AwaitingConfirmation mit RemoteRun;
Ready-Hinweis und aktivierte Planbestätigung sind erreichbar. Vor jeder Wirkung
bleiben Quelle bytegleich und Ziel abwesend. Der eigene Cleanup besteht vollständig.
Die drei nativen Ansichten wurden visuell im dunklen Theme geprüft, die
Browseransicht bei 800 × 600 Pixeln in beiden Themes. Der native Lauf hält keine
IPC-Antwort künstlich zurück; diese Reihenfolge belegen die Browsergegenproben.

Rust, Core, CLI, Provider und Lockfiles sind unverändert; ihre früheren beidseitigen
Vollgates werden weiterverwendet und nicht als neu ausgeführt gezählt. Die
unabhängige menschliche Linux-GUI- und Installationsabnahme bleibt offen.
