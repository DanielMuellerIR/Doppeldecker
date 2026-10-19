# Goal: Mehrere Fenster

Stand: 2026-08-06. Verbindliche Produktanforderung: Mehrere gleichzeitig nutzbare Fenster sind eine Grundvoraussetzung des
Dual-Pane-Dateimanagers. Die Produkt- und Sicherheitsentscheidungen wurden am
2026-08-06 konkretisiert und sind unten verbindlich festgehalten.

Diese Datei ist vor jeder Arbeit an Fenstern, `AppState` oder Operations-Queue zu
lesen. Der Testbereich aus `sandbox_root` bleibt prozessweit; jedes Fenster im
markierten Testlauf wirkt also weiterhin ausschließlich unter derselben Sandbox.

## Verbindlicher Vertrag

### Geteilte Verbindungen wie eingehängte Ziele

SFTP, FTP/FTPS, WebDAV und SMB besitzen prozessweit je Profil eine lebende
Provider-Sitzung. Sobald ein Fenster eine Verbindung aufgebaut hat, erscheint sie
in allen Fenstern wie ein eingehängtes Ziel. `connections-updated` ist deshalb
absichtlich das einzige fensterübergreifende Laufzeitereignis neben dem globalen
Beenden-Hinweis. Sein Payload bindet Protokoll, Profil-ID, monotone Revision und
Änderungsart. Änderungen an Profil, Pin oder Secretreferenz entfernen die alte
Sitzung und verwerfen gezielt alle von genau diesem Profil abhängigen
RAM-Vorbereitungen; andere Fenster und Profile bleiben unberührt. Ein zugehöriger
Queueeintrag in `AwaitingConfirmation` wird wieder `Pending` und nur an sein Fenster
gemeldet.

### Eigene Operationen je Fenster

Jedes Fenster besitzt in `queue_runtime::WindowState`:

- seine eigene Single-Flight-Schranke `active_cancel`,
- alle vorbereiteten lokalen, Remote-, Provider- und bidirektionalen Läufe,
- seine eigene `OperationQueue` mit eigenem atomarem Journal und Worker,
- seine eigenen kurzlebigen Realmodus-Permits und Auditdateien.

Das Hauptfenster behält das bestehende `operations-queue.json`. Nebenfenster mit
Label `window-…` verwenden `operations-queue-window-….json`; ihre Audits tragen
denselben Suffix. Ein abgestürztes Nebenfenster-Journal mit unfertigen Einträgen
öffnet beim nächsten App-Start wieder ein Recovery-Fenster. Ein Fenster mit aktiver
Operation oder nicht-terminalem Queue-Eintrag kann nicht versehentlich geschlossen
werden.

Der Prozess belegt weiterhin genau einmal `operations-queue.lock`. Der Core-Typ
`OperationQueueProcessLock` kann innerhalb dieses Besitzers an mehrere getrennte
Journale weitergereicht werden. So bleiben eine zweite GUI-Instanz und die CLI
gleichzeitig ausgeschlossen, ohne die Fensterqueues miteinander zu verschmelzen.

### Parallelität und Kollisionen

Operationen verschiedener Fenster dürfen gleichzeitig laufen, auch von Verbindung A
nach Verbindung B. Es gibt keine künstliche app-weite Pfad- oder Provider-Sperre.
Jeder Lauf behält die Grenzen seiner bestätigten Ausführungsklasse: frische
Listings, Plan-Digest, Root-/Provideridentität und Readback sowie die für die Wirkung
verlangten No-Replace- und Quarantänebelege. Ein abgesicherter No-Replace-Commit
verweigert auch späte Kollisionen. Ein ausdrücklich ausgewiesener Best-Effort-Commit
prüft den Zielbestand frisch, sendet genau eine Mutation und liest zurück; zwischen
Listing und Commit kann eine späte Kollision dennoch genau ein Ziel ersetzen. Diese
Einschränkung gilt auch bei parallelen Fenstern. Fehler und unklare Ausgänge gehen
ausschließlich an das verursachende Fenster; es gibt keinen blinden Retry.

### Realmodus-Permit je Fenster

Ein Realmodus-Permit ist kein dauerhaftes Recht und kein Frontendwert. Es ist das
nicht serialisierbare, höchstens zehn Minuten gültige Einmal-Ticket im Backend-RAM,
das nach vollständigem Scan die exakten Root-Deskriptoren, Identitäten,
Plan-Digest und Restore-Probe an die angezeigte Bestätigungsphrase bindet.

„Je Fenster“ bedeutet: Fenster A kann weder die Vorbereitung von Fenster B
überschreiben noch dessen Phrase oder Permit verbrauchen. Ein Permit gilt weiterhin
nur einmal und nur für den exakt vorbereiteten Lauf. Nach Ablauf, Neustart, Drift oder
Profiländerung muss das betreffende Fenster vollständig neu vorbereiten.

### Erzeugung und Ereignisse

„Fenster → Neues Fenster“ sowie `⌘N` beziehungsweise `Ctrl+N` erzeugen über
`WebviewWindowBuilder` ein Label `window-…`. Das neue Fenster übernimmt die beiden
gerade sichtbaren Pane-Endpunkte des Ursprungsfensters. Die Tauri-Capability deckt
`main` und `window-*` ab.

Fortschritt, Queue-Snapshots, Fehler und native Menüaktionen gehen gezielt nur an das
Ursprungs- beziehungsweise fokussierte Fenster. Ein Rundruf ist für diese Ereignisse
verboten. Fensterübergreifend bleiben ausschließlich bewusst gemeinsame Zustände wie
die Liste lebender Verbindungen und der Beenden-Versuch des gesamten Prozesses.
Der Rust-Versand verwendet dafür `EventTarget::webview(label)`, und die
Frontend-Brücke registriert jeden JavaScript-Listener am aktuellen Webview. Ein
direkter Aufruf von Tauri `event.listen` ohne Ziel registriert dagegen in jedem
Fenster einen prozessweiten Listener und vervielfacht eine einzelne Menüaktion.

### Verifikation vom 2026-08-06

Alle acht lokalen Headless-Gates waren grün. In einer isolierten Linux-Kopie liefen
Core, CLI, Tauri und Workspace-Clippy ebenfalls grün; dabei wurde eine zunächst
ungekapselte macOS-Titelleistenmethode gefunden und korrigiert. Der anschließende
echte macOS-GUI-Test startete den aktuellen Debug-Build ausschließlich im markierten
`/private/tmp`-Sandboxbaum: Nach dem Schließen des zweiten Fensters war genau ein
normales 1180×760-App-Fenster sichtbar, `⌘N` erzeugte wieder ein zweites. Das neue
Fenster zeigte wie das Ursprungsfenster `quelle` links und `ziel` rechts.

## Sitzungswiederherstellung

Seit 2026-09-05 kann die globale Einstellung Fenster samt Pane-Zielen, aktiver Seite,
Position, Größe und Maximierungszustand nach einem Neustart wiederherstellen. Remote-
Ziele verbinden sich über das gespeicherte Profil neu. Unfertige Queue-Recovery bleibt
davon getrennt und folgt weiterhin ihrem eigenen Journalvertrag. Details stehen in
[`settings-session-diff.md`](settings-session-diff.md).
