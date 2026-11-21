# Review-Korrekturen und Schnellfilter

Stand: 2026-10-22. Ausgangsstand: `8b5fe0f`.

Alle acht Review-Funde wurden am Datenfluss bestätigt und korrigiert. Kein Fund
wurde als Fehlalarm verworfen oder lediglich in die Roadmap verschoben.

| Fund | Korrektur und relevanter Gegenbeleg |
| --- | --- |
| P1: endgültiges WebDAV-Löschen bindet eine später gelesene Blattversion | Der bestätigte Provider-Token erreicht die interaktive Löschprimitive. Ein frischer abweichender Token stoppt; `If-Match` verwendet den bestätigten Token. Der künstliche HTTPS-Server tauscht gleich große Inhalte nach dem letzten Elternlisting oder erst nach dem Datei-PROPFIND aus. Beide Fälle erhalten die neue Version. |
| P2: große endgültige Löschung ohne Wirkungsdetails | Die Phraseneingabe enthält dieselbe Klartextbilanz mit Bytes, Irreversibilität und Providergrenzen. Der Browser prüft Text, HTML-Escaping und wirkungslos abgelehnte falsche Eingabe. |
| P2: SFTP-Ersetzen ohne konkrete Warnung | Die angeforderte Ersetzungswirkung ergänzt die Warnung unabhängig von der FTPS-Konfliktliste. Lokale und SFTP-Quellen gegen SFTP ohne POSIX-Rename zeigen die irreversible Entfernung vor Rename. |
| P2: lange Snapshotendung überschreitet lokale Namensgrenze | Originalformat verwendet den Originalnamen im bereits eindeutigen privaten Laufordner. Der Manifestname bleibt reserviert. Ein 255-Byte-Name und der reservierte Name werden vollständig gelesen und später manifestgebunden bereinigt. |
| P2: bidirektionale CLI-Zustimmung ohne sichtbare Wirkung | Plan, Run und Resume zeigen beide Richtungen und konkrete Pfade. Die gemeinsame Core-Bilanz zählt entfernte Unterbäume mit Einträgen/Bytes und nennt leere Quellen sowie hohe Anteile. JSON enthält dieselbe Bilanz; Resume beschreibt den Gesamtplan und führt nur offene Projektionen aus. |
| P2: SFTP-Leser erkennt gleich große Attributdrift nicht | Handle und frischer Pfad müssen vor dem ersten READ und am Ende mit Größe, mtime, Rechten und Owner übereinstimmen. atime bleibt wegen eigener Lesezugriffe ausgenommen. Der Stream wandelt `Interrupted` unter Erhalt des Fachcodes in einen nicht automatisch wiederholten Lesefehler um. Vier Paketfälle decken Drift vor/nach Lesen und nur am Pfad ab. |
| P3: veraltete Produktangaben | Deutsche README, Ausbauchancen und aktueller Roadmapabsatz unterscheiden interaktive Provideraktionen von weiterhin fehlenden strikten Sync-/Quarantäneverträgen. |
| P3: doppelte Testzuweisung | Der identische zweite FTPS-Zielbenutzerblock entfällt. |

Collection-Löschungen bleiben Best Effort: Eigene Kindentfernungen können ihre
ETag ändern. Der Provider prüft sie deshalb frisch leer und bindet DELETE an den
frischen Collection-Token; daraus entsteht keine atomare Garantie gegen neu
hinzugekommene Kinder oder Vorfahrenwechsel. Die bestätigte Blattversion wird
hierdurch nicht gelockert. Der HTTPS-Gegenlauf prüft zusätzlich einen normalen
rekursiven Löschabschluss mit drei Records.

## Nächster Roadmap-Schnitt

Der Schnellfilter arbeitet ausschließlich auf dem geladenen Bestand jeder Pane.
Er benötigt keine neuen Providerzugriffe. Text findet Namen/Endungen ohne
Groß-/Kleinschreibung; die Typauswahl zeigt Dateien oder Ordner. Der Footer nennt
sichtbare und geladene Einträge. Eine leere Treffermenge wird ausdrücklich so
beschriftet. Sortierung und Filterung halten sichtbare Objekte und ihre Auswahl
zusammen; ausgeblendete Zeilen können nicht versehentlich mitkopiert werden.
Escape oder der Löschknopf leert beide Filterkriterien. Ordner-/Tabwechsel leert
den Filter, frisches Laden desselben Orts erhält ihn. Filter sind flüchtig und
werden nicht als dauerhafte Ordneransicht gespeichert.
Sync plant weiterhin die vollständigen Pane-Roots mit seinen eigenen Optionen.

Rekursive Namenssuche sowie die übrigen offenen Roadmappunkte bleiben getrennte
Arbeit. Insbesondere erklärt dieser Schnitt weder alle Namens-/Metadatenklassen
noch die vollständige Provider-/Komfortparität als abgeschlossen.

## Prüfung

Die sechs funktionalen Regressionen schlagen gegen den Ausgangscode fehl:
WebDAV-Versionstausch, großer Löschdialog, SFTP-Ersetzungswarnung, lange Endung,
bidirektionale Textvorschau und SFTP-Attributdrift. Die Gegenproben verwenden die
neuen Tests mit der jeweiligen alten Produktimplementierung; reine Text- und
Redundanzkorrekturen erhalten keinen künstlichen Test.

Die gezielten positiven Läufe bestehen auf macOS: 865 Core-Unitfälle,
63 SFTP-Paketfälle, ein HTTPS-WebDAV-E2E sowie fünf bidirektionale CLI-Fälle.
Der Browser prüft Filterauswahl, F5-Wirkung, leere Treffer, Sortierung, Navigation
und 5.000 Einträge mit Virtualisierung. Die betroffenen Ansichten werden im hellen
und dunklen Theme sowie bei 1.280 und 1.040 CSS-Pixeln visuell geprüft.

Auf Linux bestehen die vollständigen Core-Suiten, 54 CLI-Integrationstests und
Core-/CLI-Clippy mit `-D warnings` in einer isolierten Kopie. Der bekannte
XDG-Fall mit festem Mountroot `/` wird im Container ausgespart und besteht als
unverändertes Testbinary direkt auf dem Host. Ein anfänglicher CLI-Lauf scheiterte
an binären macOS-Archivbeigaben im Testabbild; nach deren Entfernung besteht die
Suite. Der passende Clippy-Compilerzusatz wurde nur im Wegwerfcontainer ergänzt.

Die neuen Browserabläufe bestehen zusätzlich unter WebKit 26.5. Dabei wurde die
Typauswahl auf eine explizite Höhe von 30 Pixeln korrigiert. Playwright fügt bei
WebKit-Screenshots selbst kurzzeitig ein leeres Inline-Stylesheet ein; die
Bedien- und CSP-Prüfungen laufen deshalb ohne Screenshot. Eine getrennte visuelle
Aufnahme erfolgt anschließend. Die Anwendung lockert ihre CSP nicht.

Auf macOS bestehen Formatprüfung, gesperrte Node-Installation, Werkzeugtests,
Core-, CLI- und App-Suiten, Workspace-Clippy mit `-D warnings` sowie die gesperrte
Fuzz-Kompilierung. Vier bisher paneweit suchende UI-Assertions wurden auf ihre
Dateiliste eingegrenzt: Die Typauswahl besitzt reguläre eigene `option`-Elemente.
Der abschließende vollständige UI-Lauf besteht mit sechs Tab-Modelltests,
416 Browserfällen und der CSP-Prüfung. Die neue Oberfläche wurde mit
kontrollierter IPC-Mockbrücke in Chromium und WebKit geprüft; ein neuer nativer
GUI-Schreibsmoke wurde in diesem Schnitt nicht ausgeführt. Es wurde keine neue
Produktversion installiert, kein Release veröffentlicht und keine Versionsnummer
erhöht.
