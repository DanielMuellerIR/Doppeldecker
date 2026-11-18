# Technische Arbeitsreferenz

## Einstieg und Architektur

Tauri v2 mit Vanilla-HTML/CSS/JS und drei Rust-Crates:

- `core/`: GUI-freie Datei-/Sync-/Providerlogik. Alles korrektheitskritische lebt hier
  und bleibt auf macOS und Linux headless testbar.
- `src-tauri/`: dünne App-Schicht, Fenster, Commands, Events und Plattformintegration.
- `cli/`: unterstützter Headless-Einstieg über dieselben Core-Pläne, Permits,
  Quarantäne- und Auditverträge wie die GUI.
- `frontend/`: ES-Module ohne Bundlerpflicht; `main.js` bleibt dünne Komposition.
- `docs/INDEX.md`: Einstieg in Sicherheits-, Provider- und Releaseverträge.
- `docs/product-principles.md`: verbindliche Produktidentität und Entscheidung
  zwischen abgesicherter Ausführung, Best Effort und echter Absage.
- `fuzz/`: Parser-Fuzzing; `vendor/smb2`: gezielter SMB-Capability-Patch.

**Doppeldecker unterstützt mehrere gleichzeitig arbeitende Fenster.**
Die Provider-Verbindungen sind wie
eingehängte Ziele prozessweit sichtbar. Jedes Fenster besitzt dagegen zwingend seine
eigene Operations-Queue, Single-Flight-Schranke, Realmodus-Vorbereitungen und
Permits; Operationen verschiedener Fenster dürfen parallel laufen. Fortschritt und
Menüaktionen gehen nur an das verursachende Fenster. Der externe GUI-/CLI-Prozesslock
bleibt gemeinsam, seine interne Weitergabe verschmilzt die Fensterjournale nicht.
Vor jeder Arbeit an Fenstern, `AppState` oder der Operations-Queue
[`docs/goal-mehrere-fenster.md`](docs/goal-mehrere-fenster.md) lesen.

Keine Sicherheitslogik im Frontend duplizieren. Das Frontend darf Wirkung erklären
und bestätigen lassen, aber niemals Pfade, Identitäten, Plan-Digests oder Permits
erfinden. Tauri-Commands nehmen möglichst IDs/Optionen entgegen; vorbereitete echte
Läufe bleiben ausschließlich als nicht serialisierbarer Backendzustand gebunden.

## Dateioperationen, Papierkorb, Queue und Quarantäne

Interaktive F5/F6-/Rename-/mkdir-Operationen sind absichtlich nicht Dry-Run-only,
aber durch dieselbe VFS-/Sandbox-Grenze geschützt. Sie folgen den abgestuften
Ausführungsklassen aus `docs/product-principles.md`: fehlende atomare Garantien
führen zuerst zu einem sichtbaren Best-Effort-Weg, nicht zur pauschalen Absage. Vor
Überschreiben Konflikte prüfen; die UI bietet die tatsächlich möglichen Varianten
Ersetzen/Überspringen/Abbrechen. Overwrite von Ordnern führt zusammen und löscht
nichts. Ist eine Sandbox gesetzt, müssen jedes Schreib-/Lösch-/Move-Ziel und beim
Move auch die Quelle darunter liegen.

- Ein Exaktnamen-Vergleich vor `mkdir` löst Groß-/Kleinschreibungs-Konflikte nicht
  allgemein: Auf case-insensitivem APFS existiert `Foo` bereits, wenn `foo` da ist, und
  das folgende `mkdir` scheitert mit `EEXIST`. Die tatsächliche Anlage ist die Probe;
  jede Änderung an dieser Stelle auf case-sensitivem und case-insensitivem Dateisystem
  testen. Belegt 2026-09-10: Ein Fix über `Vfs::has_exact_names` wurde umgesetzt und
  wieder verworfen, weil er macOS gebrochen hätte (dokumentierter Plattformgegenlauf vom 2026-09-10).

- Löschen immer über den descriptor-relativen macOS-/XDG-Papierkorb: atomarer Claim,
  Identitätsprüfung, Crash-Journal, fail-closed Recovery. Kein Copy-and-delete-
  Fallback und kein echter Systempapierkorb in Tests.
- Quarantäne manifestiert Lauf und Intent vor jedem Ziel-Rename. Restore ersetzt nie
  und nutzt exklusiven Rename. 30 Tage sind nur Aufbewahrungsziel, keine Auto-Löschung;
  endgültiges Leeren verlangt exakten Lauftext.
- Operations-Queue je Fenster: eigenes atomisches Journal, Worker, Recovery,
  Pause/Stop-on-error und Driftbestätigung. Alle Journale eines App-Prozesses teilen
  nur den externen Prozesslock. Handles, Secrets und Permits nie persistieren. Nur
  Pending-Einträge umordnen/entfernen.
- Prozessweit gleich benannte Temp-Dateien nicht beim Start jeder Fensterqueue
  bereinigen. `cleanup_stale_temps()` löschte alle `.operations-queue-*.tmp` im
  Konfigurationsordner, also auch die gerade schreibender Fenster. Eine solche
  Bereinigung gehört einmalig unter die gemeinsame Prozesssperre oder muss aktive
  Temp-Dateien nachweislich aussparen.
- Audit append-only und geheimnisfrei; keine Passwörter, Schlüssel oder vollständigen
  credentialed URLs protokollieren.

## Remote-Provider und Secrets

SFTP, FTP/FTPS, WebDAV und SMB melden ihre konkreten Plan-, Listing- und
Mutationsfähigkeiten. Eine fehlende lokale Garantie ändert Ausführungsklasse,
Rückfrage und Wiederherstellungsweg; sie macht den Provider nicht pauschal
schreibgeschützt. Transportverschlüsselung und kryptografisch geprüfte
Serveridentität sind ebenfalls sichtbare Fähigkeiten, keine pauschalen
Zugangsschranken: Doppeldecker
unterstützt ausdrücklich Klartext-FTP sowie FTPS mit nicht öffentlich prüfbarer
Zertifikatsidentität. Solche Profile und jede aktive Pane weisen ihren konkreten
Transportstatus sichtbar aus, laufen aber ohne zusätzliche Bestätigungsfrage.

Ein explizites `ftp://` darf unverschlüsselt verbinden; `ftps://` darf niemals still
auf Klartext zurückfallen. FTPS prüft standardmäßig öffentliche Zertifikatskette,
Gültigkeit und Servernamen ohne dauerhaften Profilpin; reguläre gültige Erneuerungen
dürfen neue Verbindungen nicht sperren. Exakte Pins bleiben eine ausdrückliche
Option, insbesondere für private oder ungeprüfte Zertifikate. Reconnect laufender
Vorbereitungen darf nur bei derselben
für den gewählten Modus verfügbaren Provideridentität fortsetzen: Hostkey bei SFTP,
Zertifikat bei FTPS und bei Klartext-FTP mindestens Endpunkt, Root und
Serverfähigkeits-Digests. Unvollständige Listings oder schwache Durability sperren
weiterhin ausschließlich die davon betroffene Richtung beziehungsweise Mirror
fail-closed.

Ein Wiederholversuch nach Reconnect steht ausschließlich lesenden Operationen zu:
`scan_*_dir` wiederholt sein Listing genau einmal, und zwar bei allen vier
Providern. Mutierende Befehle — `create_*_folder`, Sync, Upload — werden nach einem
Verbindungsfehler nie blind erneut ausgeführt, weil der Server sie schon ausgeführt
haben kann, bevor die Antwort verlorenging. Dazu gehört die Fehlerzuordnung: Eine
nach Leerlauf abgelaufene FTP-Kontrollsitzung meldet `suppaftp` als `BadResponse`,
also als Protokoll- statt als Verbindungsfehler. `ftp_error()` übersetzt sie deshalb
nach `UnexpectedEof` — ohne diese Übersetzung greift die Reconnect-Grenze gar nicht
erst, und das Listing bleibt mit einem unspezifischen Fehler liegen.

- Secrets nur als Schlüsselbund-/Agent-/geschützte Referenz, nie als Pfadbestandteil,
  CLI-Argument, persistierter Queueeintrag, Audit oder Frontendpayload.
- Credential-Output nie verbose in Sessionlogs ausgeben. Tests verwenden isolierte
  künstliche Serverroots und Testcredentials, keine Nutzerdaten.
- Der vendorte `smb2`-Patch setzt gezielt das SMB-3.1.1-Encryption-Capability-Bit.
  Nicht durch eine Upstream-Version entfernen, bevor verschlüsselter Samba-E2E belegt,
  dass der Fix enthalten und Verhalten gleich ist.
- Künstliche Capability-Proben (Anlegen, Umbenennen, Löschen) laufen ausschließlich
  unter dem konkret gewählten Pane- oder CLI-Root, nie implizit am Profilroot. Nach
  Erfolgs- **und** Fehlerpfad beide Wurzeln auf Reste prüfen. Belegt 2026-07-28: Proben
  liefen am Profilroot `/` statt unter dem gewählten Ordner.
- Erst alle Optionen validieren, dann den Provider ansprechen. Eine Remote-Vorbereitung
  darf Providerzugriffe und mutierende Capability-Proben nicht vor `options.validate()?`
  starten; sonst hat eine anschließend abgelehnte Anfrage schon entfernten Zustand
  verändert oder Probenreste hinterlassen. Die Validierung gehört an den Anfang der
  Funktion.
- Ein Server darf `MKD` auch für einen schon vorhandenen Ordner mit Erfolg beantworten
  (bei manchen Servern beobachtet). Ein Erfolgscode ist damit **kein** Beweis für exklusives Anlegen:
  vorher den vollständigen Elternbestand listen, danach den neuen Ordner zurücklesen.
  Ein bestehender Name bleibt eine sichtbare Kollision, kein stiller Treffer.
- `MLSD` standardkonform aushandeln: benötigte Felder mit `OPTS MLST` anfordern, eine
  Ablehnung dieses optionalen Befehls aber tolerieren und stattdessen das tatsächliche
  Listing streng prüfen. Ist das Verzeichnis per `CWD` gewählt, `MLSD` **ohne**
  Pfadargument senden — nur diese Form garantiert der Standard.
- RFC 3659 erlaubt systemeigene Typwerte der Form `OS.name=type`. Ein MLSD-Parser darf
  sie deshalb nicht pauschal als unbekannt ablehnen: Den im RFC gezeigten Unix-Symlink
  (`OS.unix=slink[:Ziel]`) als Symlink bilanzieren und nicht verfolgen; jeder andere
  nicht unterstützte Systemtyp bleibt eine sichtbar behandelte Spezialdatei. Ein
  fehlerhaft aufgebauter Typwert bleibt ungültig.
- WebDAV-Namen pro Pfadsegment URL-kodieren. `Url::set_path` übernimmt vorhandene
  Prozentsequenzen absichtlich unverändert; ein wörtlicher Dateiname wie
  `literal%2F.txt` würde beim späteren Zugriff sonst als Pfadtrenner umgedeutet.
- Sicherheitskritische Remote-Löschungen an Identität und Readback binden: Der zuvor
  bestätigte Provider-Token wird bis in die bedingte Operation durchgereicht (etwa
  ein `If-Match`-DELETE), und eine erfolgreiche Antwort gilt erst dann als Erfolg,
  wenn ein Readback die Abwesenheit des Pfads bestätigt — die Antwort allein beweist
  weder, dass das bestätigte Objekt gelöscht wurde, noch dass der Pfad fehlt.
- Fehlendes `fsync`, fehlendes atomisches No-Replace-Rename oder fehlende
  Providerquarantäne dürfen eine normale interaktive Operation nicht allein
  pauschal sperren. Der Core muss die stärkste verfügbare Wirkung samt Restrisiko
  liefern; Frontend und CLI zeigen Best Effort, Irreversibilität und unklaren
  Ausgang ausdrücklich an.

## Fehler-, UI- und Plattformverträge

- Maschinenlesbare `ErrorCode`s von Core über `io::Error`, Tauri-Payload, Frontend und
  CLI-JSONL durchreichen. Nie auf deutsche Fehlertexte matchen.
- Tauri benennt **verschachtelte** JSON-Felder nicht zuverlässig so um wie
  Top-Level-Argumente. Request-Strukturen müssen die Schreibweise akzeptieren, die das
  Frontend wirklich sendet (`profileId`), und das braucht einen
  Deserialisierungs-Regressionstest — sonst fällt es erst zur Laufzeit auf. Belegt
  2026-07-28: Remote-Löschen scheiterte mit fehlendem `profile_id`, weil das Frontend
  verschachtelt `profileId` schickte.
- Ob eine Operation auf einer Remote-Pane möglich ist, entscheidet der Capability-/
  VFS-Vertrag des Providers einschließlich seiner Ausführungsklasse, nicht eine
  Abfrage im Frontend. Eine
  `provider !== "local"`-Sperre hat „Ordner anlegen" für alle Remote-Panes blockiert,
  obwohl die damaligen Provider `create_dir` samt Gate und Readback bereits
  umsetzten. Die interaktive SFTP-Klasse darf fehlende atomare Root-/Elternbindung
  ausdrücklich als Best Effort behandeln; strikte Sync-Sperren gelten nicht
  automatisch für sie.
  Vor einer Frontend-Sperre erst den Backend-Vertrag lesen; ist die
  Operation dort nicht vorgesehen, gehört die Absage ebenfalls dorthin.
- Eine strikte Sync-Capability darf nicht ungeprüft für eine gewöhnliche
  Dateimanageraktion wiederverwendet werden. Insbesondere ist ein serverseitig
  möglicher Rename nicht allein deshalb unzulässig, weil er kein atomarer
  No-Replace-Commit für unbeaufsichtigten Sync ist.
- Strikte CSP: `script-src/style-src 'self'`, kein `unsafe-inline`. Keine Inline-
  Scripts oder `style="..."` in Templates; Theme-Boot bleibt extern. IPC-Connect-
  Quellen gezielt erhalten.
- Ein registriertes Tauri-Plugin braucht seinen vollständigen Konfigurationsblock in
  `tauri.conf.json`. Fehlt zu `tauri-plugin-updater` ein gültiger
  `plugins.updater`-Block, bricht die fertige App schon bei der
  Plugin-Initialisierung ab, also vor dem ersten Fenster, und meldet nur
  `invalid type: null, expected struct Config`. Der Release-Preflight lehnt diesen
  Zustand ab.
- Geräteerkennung darf den macOS-UI-Thread nicht blockieren: `diskutil info` je Volume
  beim App-Start nie synchron auf dem AppKit-/Tauri-Thread abfragen, sondern im
  Hintergrund mit kurzer harter Frist pro Volume. Belegt 2026-08-06: Ein träges Volume
  blockierte den Thread 6,84 s und ließ 7,2 s ein weißes Fenster stehen.
- Tauri-v2-Capability `core:default` gilt für `main` und `window-*`, damit alle
  Dual-Pane-Fenster Event-/Window-/Path-APIs verwenden können. Eigene Commands
  brauchen nicht automatisch zusätzliche ACL-Einträge.
- Pane-Listen bleiben zugänglich (`listbox`/`option`, aktiver Nachfahre, Fokusfalle und
  Rückgabe) und ab großen Beständen virtualisiert. Responsives Grid: Name hat Vorrang;
  Größe/Datum schrumpfen zuerst.
- macOS Overlay-Flächen opak halten; Material-Blur nur klein/stabil. Für GUI-Tests ein
  echtes `.app`-Bundle verwenden; nacktes Cargo-Binary hat keine verlässliche App-
  Präsenz. Linux-GUI benötigt WebKitGTK-4.1-Dev-Libs, der Core nicht.
- F5/F6 sind zusätzlich über Kontext-/Dateimenü erreichbar, weil Mac-Funktionstasten
  je Systemeinstellung Fn erfordern können.
- App-Icon: Das Flugzeug diagonal von links oben nach rechts unten zeigen, den Raum
  ausnutzen und konstruktive Details wie Rippen, Streben, Motor und Fahrwerk behalten.
  Kantige, konstruktive Formen und Komplementärfarben für den Hintergrund verwenden.

## Bauen und testen

Für eine Änderung zuerst einen beobachtbaren Erfolgsfall festlegen. Den kleinsten
passenden Test ausführen und nur für die betroffenen Grenzen ergänzen.
Eine unveränderte Prüfung erneut starten, wenn ein neuer Befund oder eine
geänderte Testvoraussetzung das Ergebnis klären kann. Dieselbe Blockade nach
zwei ergebnislosen Versuchen anders prüfen.

| Änderung | Erster Prüfweg | Wann erweitern |
| --- | --- | --- |
| Planner, Queue, Recovery | Betroffenen Rust-Test oder Integrationstest im jeweiligen Crate ausführen und die Trefferzahl prüfen. | Bei Änderungen an Persistenz oder Dateiwirkungen auch Neustart, Konflikt bzw. Teilwirkung am Ergebnis prüfen. |
| Oberfläche | Mit `node tools/test-mock-ui.mjs --list 'Namensfragment'` passende Browserfälle finden, mit demselben Aufruf ohne `--list` ausführen. Eigenständige Fachsuiten stehen in `package.json`. | Geänderte Ansichten im echten Fenster ansehen, Tastaturfokus und Mausweg prüfen. Bei Provideränderungen zusätzlich ein echtes Providerziel verwenden. |
| Verbindung oder Transfer | Geänderte Aktion mit wenigen künstlichen Dateien gegen den betroffenen Provider ausführen. Für SFTP und SMB gibt es `tools/run-sftp-e2e.sh` und `tools/run-smb-e2e.sh`. | Weitere Richtungen, Fehlerfälle und Plattformen prüfen, soweit sie von der Änderung betroffen sind. |
| Gemeinsamer Meilenstein oder Releasekandidat | Zusammengehörige Änderungen auf demselben Stand prüfen. `python3 tools/check-gates.py --list` zeigt die Gates, `--only NAME` wählt gezielt aus. | Für Releasekandidaten auf beiden Plattformen die vollständigen Gates und die native Paketabnahme gemäß [Releaseplan](docs/release-plan.md) ausführen. |

GUI-Schreibtests laufen über `tools/run-test-sandbox.sh`. Für automatisierte
Läufe [den Testbereich an den gesamten Prüfblock binden](docs/build-output-retention.md#automatisierte-gui-prüfblöcke).
Möglichst ohne Fokuswechsel testen, unter Linux bei Bedarf auf einem privaten
virtuellen Display. Für native Eingaben am belegten Arbeitsplatz die geltende
Fokusfreigabe beachten. Während ein Zielsystem fehlt, andere ausführbare Arbeit
fortsetzen und den fehlenden Nachweis beim betroffenen Punkt festhalten.

Plattformabhängige Casts brauchen einen Plattformzweig und müssen auf **beiden**
Systemen geprüft werden. Konkreter Fall in `core/src/local_fs.rs`: `st_dev as u64`
ist auf macOS nötig, auf Linux dagegen ein identischer Cast — und damit unter
`-D warnings` ein Clippy-Fehler. Ein Lauf auf nur einem System belegt hier nichts.

Die Fuzz-Targets liegen außerhalb des Workspace; nur der eigene `cargo check`
übersetzt sie. `--locked` deckt zusätzlich auf, wenn `fuzz/Cargo.lock` nicht mehr
zu den Abhängigkeiten des Kerns passt.

`tools/check-gates.py` bündelt die headless Code-Gates: Exit-Code 0 nur, wenn alle ausgeführten Gates grün sind; `--json`
schreibt JSON Lines auf stdout. Nach einem vollständigen grünen Lauf bereinigt es
`target/debug` oberhalb von 6 GiB; Teilgate-Läufe und Release-Artefakte bleiben erhalten.
Für manuelle Builds, isolierte Kandidaten und deren Abschluss deshalb zusätzlich
[Builddaten und Agentenausgaben](docs/build-output-retention.md) beachten:
unnötige neue Target-Bäume vermeiden, Ausgaben begrenzen und entbehrliche
Zwischenprodukte nach Abnahme aufräumen.

`./build-app` baut eine Developer-ID-signierte, notarisierte `Doppeldecker.app` in
den Projekt-Root (Modus `macos-install`; stabile Signatur hält macOS-Berechtigungen,
Zugangsdaten nur über das notarytool-Schlüsselbundprofil aus `NOTARY_PROFILE` bzw.
clone-lokal aus `git config doppeldecker.notaryProfile`). `./install` ersetzt
zusätzlich die
Kopie in `/Applications` — atomar, über einen Staging-Pfad daneben. `./release`
packt daraus das DMG (Modus `macos-release`) und installiert nie; auch dort
bekommt zuerst die App ihr Ticket, dann das Image.

`tools/tests/test_fleet_rules.py` (läuft im Gate `test-tools`) hält beides an der Quelle
fest: das Ticket vor dem ersten Schreiben ins Installationsverzeichnis, und keinen
absoluten Build-Mac-Pfad im ausgelieferten Bundle. Diese Prüfungen nie dadurch
„belegen“, dass man `./install` oder `./release` als Test aufruft. Die Skripte
ersetzen die installierte App beziehungsweise reichen eine Apple-Notarisierung ein.

Das DMG wird dabei bewusst selbst gepackt statt mit `tauri bundle --bundles dmg`:
Der Tauri-Bundler signiert die App beim Packen neu und löscht sie danach aus
`target/release/bundle/macos/` — das eben angeheftete Notary-Ticket wäre weg
(belegt 2026-07-26). Wer daran etwas ändert, muss danach prüfen, ob die App
**im fertigen DMG** noch `xcrun stapler validate` besteht.

Aus demselben Grund entsteht das macOS-Updater-Archiv (`.app.tar.gz`) erst aus der
fertig notarisierten und gestapelten App. Wird es vorher gepackt, liefert der Updater
einen ungestapelten Vorzustand aus, während App und DMG alle Prüfungen bestehen — der
Fehler fällt also erst beim Aktualisieren auf.

`cargo test <filter>` endet auch dann mit Exit-Code 0, wenn der Filter null Tests
trifft. Ein E2E-Runner darf sich darauf nicht verlassen: vorher per `--list` genau einen
Treffer verlangen und den Test danach mit `--exact` ausführen.

Kein Test darf auf echte Nutzerdaten wirken. Ohne `--config-dir` benutzt
`doppeldecker-cli` den echten Konfigurationsordner der Desktop-App und öffnet dort das
echte Queue-Journal; am 2026-08-02 hat ein Testlauf es genau so auf eine neuere
Journalversion migriert. Deshalb gilt:

- Jeder CLI-Prozess in `cli/tests/cli_tests.rs` entsteht über `CliSandbox`. Die Sandbox
  setzt `--config-dir` in einen Wegwerfbaum, lenkt `HOME` und die XDG-Variablen dorthin
  um und setzt die Sperrvariable. Der Test
  `jeder_cli_start_bekommt_einen_isolierten_konfigurationsordner` hält das mechanisch
  fest, indem er den eigenen Quelltext prüft.
- `DOPPELDECKER_NO_DEFAULT_USER_PATHS` verbietet den stillen Rückfall auf die echten
  Nutzerpfade: Ein fehlendes `--config-dir` oder `--audit` scheitert dann sichtbar mit
  `invalid_input`, statt fremde Daten zu berühren. `.cargo/config.toml` setzt die
  Variable für alles, was Cargo aus diesem Repo startet — also auch für einen neuen
  Test, der den gemeinsamen Helfer umgeht. Nebenwirkung: `cargo run -p doppeldecker-cli`
  verlangt ebenfalls einen ausdrücklichen Ordner. Wer die CLI bewusst gegen die echte
  Desktop-Konfiguration fahren will, ruft das gebaute Binary direkt auf.

Änderungen an Kern/Provider/Metadaten müssen zusätzlich auf dem Linux-Testhost in einer
isolierten Kopie unter `/tmp` laufen. Nie zwei Macs im selben Repo-Arbeitsbaum
committen. Echte Provider-E2Es ausschließlich gegen frische künstliche Roots;
Nutzerdaten bleiben unberührt.

Für die Auswahl der betroffenen Fälle gelten folgende Prüfbereiche:

- Planner/Sync: AddOnly/Update/Mirror, Plan-Drift, Root-Überlappung, Abbruch,
  Symlink-Swap, späte Kollision, leere Quelle, Mengenlimits, Quarantäne und Rechte.
- Metadata/VFS: beide Plattformen, Capabilityabweichungen, exakter Readback und
  atomarer Fehlerpfad ohne Zielverlust.
- Provider: Pin/Identität, Paging/vollständiges Listing, Reconnect, Upload/Download,
  Richtungsmatrix, Quarantäne/Queue und absichtlich schwache Capability.
- Frontend: betroffene Fehler-/Latenz-/Konfliktpfade, A11y, Virtualisierung und
  CSP. `pnpm test:ui` bündelt die Browser- und Pane-Suiten. Echte Fenster-Schreibtests nur im markierten Wegwerfbereich.
- Release: `npm run release:check`, Privacy-/Absolutpfadscan, Paketextraktion,
  Version/Architektur/Abhängigkeiten, Signatur, Prüfsumme und byteidentischer
  Sandbox-Smoke. Public macOS zusätzlich Developer ID, Notarisierung und Staple.

## Verzeichnisstruktur

- [`README.md`](README.md): Nutzer-, Sicherheits- und Betriebsüberblick.
- [`docs/product-principles.md`](docs/product-principles.md): praktische
  Produktidentität und abgestufte Ausführungsklassen.
- `Cargo.toml` / `Cargo.lock`: Rust-Workspace und gesperrte Abhängigkeiten.
- `package.json` / `pnpm-lock.yaml`: Frontend-Werkzeuge und gesperrte Abhängigkeiten.
- `vendor/smb2/`: vendorter Fremdcode mit eigenen Scope-Regeln.
- [`CHANGELOG.md`](CHANGELOG.md): veröffentlichte Änderungen.
- [`backlog.md`](backlog.md): verifizierte offene Arbeit.
- [`docs/INDEX.md`](docs/INDEX.md): Index der Architektur- und Betriebsverträge.
- [`docs/acl-adapter-spike.md`](docs/acl-adapter-spike.md): ACL-Adapter-Spike.
