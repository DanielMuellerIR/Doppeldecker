> Historische Paket-/Plattformbelege bis 2026-10-31. Kein aktueller Kandidatenstatus.
> Aktuelle Anforderungen: [Releaseplan](release-plan.md).

# Release- und Plattformplan

Stand: 2026-10-20; datierte Artefaktbelege beschreiben jeweils ihren damaligen Stand.

Die erste öffentliche Preview [0.1.124](https://github.com/DanielMuellerIR/Doppeldecker/releases/tag/v0.1.124)
ist seit 2026-10-19 veröffentlicht. Für weitere Releases README beider Sprachen,
Funktionsmatrix, Release Notes und Projektwebsite gemeinsam abgleichen. Die
Beschreibung stellt komfortable, umfassende und effiziente Dateioperationen auf
macOS und Linux in den Vordergrund. Vorschau wird beim jeweiligen Sync- oder
Stapelablauf beschrieben; eine allgemeine Vorschaupflicht vor Dateiaktionen wird
nicht versprochen. Neue Fähigkeiten erst dann als Downloadfunktion bezeichnen,
wenn sie in den verlinkten Paketen enthalten sind.

## Unterstützte Ziele

- macOS: native Tauri-App ab 14 auf Apple Silicon (`aarch64`). Intel-Macs gelten
  erst nach einem Universal-/x86_64-Build und echtem Gegenlauf als unterstützt.
- Linux: zunächst Linux Mint 22.2/Ubuntu-24.04-Basis auf x86_64; weitere Distributionen
  gelten erst nach einem dokumentierten GUI-Gegenlauf als unterstützt.

Ungetestete Plattformen werden nicht durch einen erfolgreichen Cross-Build als
unterstützt bezeichnet.

## Festgelegte Plattformgrenzen und externe Freigabe

Festgelegt sind: macOS 14 als Mindestversion ohne zusätzliche
Entitlements, `.app` plus DMG für macOS,
`.deb` plus AppImage für Linux und kein automatischer Updatekanal vor 1.0. Der
manuelle Kanal aus [`update-design.md`](update-design.md) fragt ausschließlich auf
Menüklick und installiert erst nach einer zweiten Bestätigung. Lokale
Entwicklungsartefakte erhalten die von Tauri unterstützte Ad-hoc-Signatur; ein
öffentlicher macOS-Release benötigt weiterhin Developer ID und Notarisierung.
Linux-Pakete zielen auf Linux Mint 22.2/Ubuntu 24.04 x86_64 und deklarieren
`libwebkit2gtk-4.1-0` sowie `libgtk-3-0`; diese Distributionpakete bilden die
unterstützte Mindestgrenze. SMB bringt keine native Systembibliothek mit. Andere
Distributionen oder Bibliotheksstände gelten erst nach eigenem GUI-Gegenlauf als
unterstützt.
Der getrennte Befehl `pnpm bundle:macos:public` verwendet dafür ausschließlich eine
eindeutige installierte Developer-ID-Application-Identität oder
`APPLE_SIGNING_IDENTITY`. `macos-public` und `macos-release` verwenden denselben
`notarytool`-Schlüsselbundprofil-Weg; der Profilname kommt aus `NOTARY_PROFILE` oder
clone-lokal aus `doppeldecker.notaryProfile`. Geheimnisse werden nicht als Argumente
oder in Projektdateien persistiert. Nach dem Build sind Binärpfad-, Signatur-,
Staple-, DMG- und Prüfsummenprüfung verpflichtend.
Queue, Favoriten, Providerprofile, Baselines und Recovery-Journale liegen im nativen
App-Konfigurationsordner der stabilen Kennung `com.doppeldecker.desktop`. Ihre
versionsgebundenen Parser migrieren nur bekannte Vorgängerversionen und stoppen bei
unbekannten Formaten fail-closed. Für die noch unveröffentlichte frühere `.app`-
Kennung existiert keine Nutzerdatenmigration.

### Warum macOS 14 (entschieden 2026-08-03)

Vorher stand an vier Stellen 10.15 und in beiden READMEs 14+ — beides konnte nicht
stimmen. Die Belege für den einen jetzt gültigen Wert:

- Der Release ist fail-closed an `aarch64-apple-darwin` gebunden
  (`tools/build-release.py`, `require_macos_release_arch` mit `lipo`-Beleg). Apple
  Silicon gibt es erst ab macOS 11; 10.15 war für dieses Artefakt von vornherein
  gegenstandslos.
- Die Rust-Toolchain setzt für dieses Ziel ohnehin bei 11.0 auf:
  `rustc --print deployment-target --target aarch64-apple-darwin` meldet 11.0, und
  ein gesetztes `MACOSX_DEPLOYMENT_TARGET=10.15` wird dabei still auf 11.0
  hochgeklammert. Entsprechend trägt das bisher gebaute Binary
  `LC_BUILD_VERSION minos 11.0`, während sein `Info.plist`
  `LSMinimumSystemVersion 10.15` behauptete — die Konfiguration widersprach also
  bereits ihrem eigenen Artefakt.
- Die Oberfläche läuft im System-WebKit. `frontend/styles.css` nutzt an mehreren
  Stellen `color-mix()`, dazu `accent-color` und `:focus-visible`, das Frontend
  zusätzlich `crypto.randomUUID()`. Diese Fassungen bringt weder macOS 10.15 noch
  macOS 11 im Auslieferungszustand mit; unterhalb davon fiele die Darstellung
  sichtbar zurück.
- Belegt getestet ist ausschließlich macOS 14 und neuer. Nach der Regel oben in
  diesem Dokument werden ungetestete Plattformen nicht als unterstützt bezeichnet,
  auch wenn ein Build technisch entstehen könnte.

Gegenprobe zum Bauen: `MACOSX_DEPLOYMENT_TARGET=14.0 cargo check -p doppeldecker`
läuft sauber durch. Die technische Untergrenze bliebe 11.0 — wer sie später nutzen
will, braucht dafür einen echten Gegenlauf auf einem Mac mit dieser Version, keine
Konfigurationsänderung.

## Release-Kandidat

Ein Release-Kandidat benötigt:

1. sauberen Workspace und reproduzierbaren Release-Build,
2. vollständige Kern-Suite auf macOS und Linux-Testhost,
3. echten GUI-Smoke-Test in der markierten Sandbox auf beiden Plattformen,
4. Installieren, Starten, Upgrade von der vorherigen Version und Deinstallieren ohne
   Verlust von Nutzerkonfiguration,
5. Prüfung von Abbruch, Quarantäne-/Papierkorb-Recovery und beschädigter Persistenz,
6. konsistente Versionsnummern, Changelog und öffentliche Produktgrenzen,
7. keine Secrets, lokalen Nutzerpfade oder Testserverdaten in Artefakten und Logs.

Die Produktversion wird erst für einen ausdrücklich benannten Meilenstein oder
Releasekandidaten erhöht. Gewöhnliche Implementierungs-, Test- und Doku-Commits
sammeln ihre Änderungen unter `Unreleased`; dadurch bleiben Cargo-Artefakte zwischen
diesen Commits wiederverwendbar. Jeder tatsächliche Versionswechsel aktualisiert
Workspace, Tauri, Node-Metadaten, Lockfile und Changelog gemeinsam.

Der gemeinsame Bundle-Wrapper setzt Quellpfade über `CARGO_ENCODED_RUSTFLAGS`
plattformneutral auf Build-Pseudopfade um. Direkt nach dem Build scannt eine zweite,
unabhängige Prüfung das auszuliefernde Binary auf reale Workspace-, Cargo-, Rustup-
und Benutzerpfade und stoppt fail-closed. Beide Plattformskripte erzeugen anschließend
SHA-256-Prüfsummen für ihre Paketartefakte.

Der Linux-Bundlelauf extrahiert DEB und AppImage nach dem Tauri-Build getrennt. Er
prüft Paketname, Workspace-Version, amd64-Architektur, WebKitGTK-/GTK-Abhängigkeiten,
Desktop-Datei, 1024er-Icon, ausführbares Binary und dessen Bibliotheksauflösung. Der
Datenschutzscan liest beide tatsächlich eingebetteten Binaries, weil Tauri sie je
Paketformat separat patcht. Ein isolierter 0.1.115-Gegenlauf auf Linux Mint 22.2
bestand diese Prüfungen und die SHA-256-Verifikation. Der interaktive Fenster- sowie
Installations-/Upgrade-/Deinstallationsbeleg bestand am 2026-08-06 auf demselben
System: AppImage und Debug-App liefen ausschließlich in markierten Wegwerfbereichen.
F5 kopierte 13 Byte, F6 verschob 381 Byte jeweils byteidentisch; der F2-Dialog ließ
sich nach Escape erneut öffnen. `Ctrl+N` erzeugte zwei echte Fenster, wobei nur das
auslösende Fenster den abgeschlossenen Queue-Eintrag und das zweite weiterhin Queue
`0` zeigte.

Für den Paketgegenlauf wurde das DEB 0.1.122 installiert und gestartet. Danach lief
das Upgrade auf 0.1.123 im selben Testbereich; eine erneute F5-Kopie war
byteidentisch. Nach der Deinstallation fehlten Paket und `/usr/bin/doppeldecker`,
während sämtliche künstlichen Quell- und Zieldateien unverändert blieben. Die
isolierten Build-, Paket- und GUI-Testbäume wurden anschließend entfernt.

Öffentliche macOS- und Linux-Builds erzeugen zusätzlich die von Tauri zwingend
signierten Update-Artefakte. Der private, passwortgeschützte Schlüssel kommt nur über
`TAURI_SIGNING_PRIVATE_KEY` und `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` in die Umgebung;
Projektdateien und Argumente enthalten ihn nie. Nach dem Sammeln beider Plattformen
erzeugt `tools/write-update-manifest.py` die statische `latest.json` mit genau
`darwin-aarch64-app`, `linux-x86_64-appimage` und `linux-x86_64-deb`. Damit lädt eine
DEB-Installation nie das anders installierbare AppImage. Signaturdatei und
veröffentlichte Nutzlast müssen
direkte Geschwister sein; fehlende oder mehrdeutige Artefakte sperren das Manifest.

Markierte GUI-Smoke-Tests lenken zusätzlich sämtliche App-Persistenz in
`app-config` unter der validierten Sandbox: Queue und Prozesssperre, Favoriten,
Providerprofile, bidirektionale Baselines, Audit sowie Papierkorb-Recovery. Ein
zentraler Tauri-Helfer ist der einzige erlaubte Zugriff auf den nativen
Konfigurationsordner; die Release-Prüfung erzwingt diese Grenze statisch. Ohne
Sandbox bleibt der native Plattformpfad unverändert.

Recovery- und Upgrade-Gegenläufe verwenden denselben Bereich über
`tools/run-test-sandbox.sh --reuse <Testbereich>`. Der Launcher verlangt einen
existierenden Marker sowie getrennte `quelle`-/`ziel`-Ordner; anschließend validiert
das Backend den Root unverändert mit allen harten Sandbox-Regeln. Ein absichtlich
beschädigtes Queue-v8-Journal blieb beim echten Release-Wiederanlauf unangetastet und
erzeugte sichtbar `Queue !` plus Startfehler; Quelldaten und Zielkopie blieben
byteidentisch.
Unter Linux startet `--release` genau ein versionspassendes AppImage; `--installed`
führt das aus einem DEB installierte Programm unter denselben Sandbox- und
Konfigurationsschranken aus.

Vorbeleg 2026-07-13: Der aktuelle macOS-Debug-Build lief ausschließlich über
`tools/run-test-sandbox.sh`. Beide Panes, IPC, F2-Dialog samt Fokus-Rückgabe,
53-B-Vorschau, Sync-Events und byteidentische Zielkopie funktionierten unter der
strikten CSP ohne Terminal- oder Fensterfehler. Dieser Beleg ersetzt nicht den
erneuten Smoke-Test des späteren signierten Release-Artefakts.

Artefaktvorbeleg 2026-07-13 (damaliger Stand, die Grenze steht seit 2026-08-03 auf
macOS 14): Tauri CLI 2.11.4 erzeugte für 0.1.112 ein pfadneutrales
arm64-`.app` und DMG. Bundle-Kennung, Version und die damals gebundene
macOS-10.15-Grenze sind geprüft;
App und die im DMG enthaltene App bestehen `codesign --verify --deep --strict`, das
DMG besteht `hdiutil verify`, und die erzeugte SHA-256-Prüfsumme stimmt bytegenau.
Zusätzlich müssen das separat erzeugte, im Sandbox-Smoke-Test ausgeführte App-Bundle
und der vollständige App-Baum im gemounteten DMG bytegleich sein; damit gilt der
Funktionsbeleg exakt für das ausgelieferte Bundle.
Das gebündelte `.app` 0.1.112 bestand zusätzlich den markierten Sandbox-Smoke-Test mit IPC,
5-B-Vorschau, Sync-Events, byteidentischer Zielkopie und ausschließlich im Testroot
persistierter Queue/Favoriten-Ablage. Die Entwicklungsartefakte sind bewusst nur
Ad-hoc-signiert. Ein öffentlicher Release bleibt bis Developer-ID-Signatur und
gestapelter Notarisierung gesperrt.
Historisch bestand in 0.1.113 der öffentliche Build-Preflight mit simulierten
vollständigen und unvollständigen Zugangssätzen; damals fehlte noch die lokale
Notary-Konfiguration. Diese damalige Sperre ist kein aktueller Buildstatus.
Developer-ID-signierte, notarisierte und gestapelte macOS-Bundles sind inzwischen
belegt. Jeder neue öffentliche Build muss diese Prüfungen für seine eigenen
Artefakte erneut bestehen; eine Veröffentlichung ist weiterhin separat freizugeben.

## Diagnose und Datenschutz

Sichtbar gewordene App-Fehler schreiben ein begrenztes strukturiertes Log
`errors.jsonl` in den plattformüblichen App-Logordner (macOS:
`$HOME/Library/Logs/com.doppeldecker.desktop/`). Ab 1 MiB wird die vorherige Datei
einmalig als `errors.previous.jsonl` aufbewahrt. Ein Eintrag enthält ausschließlich
Unix-Zeit in Millisekunden, einen kurzen UI-Kontext und den stabilen Fehlercode.
Fehlertext, Pfad, Profil-ID, Nutzer, Host, Providerantwort und credentialed URL werden
bewusst nicht geloggt.

Automatische Telemetrie oder Crash-Uploads sind ohne separate Produktentscheidung
nicht vorgesehen.
Ein exportierbares Diagnosepaket muss vor dem Teilen eine sichtbare Vorschau und
Maskierung sensibler Pfade besitzen.

## Definition „Release fertig“

Artefakte sind signiert, auf frischen Testkonten installierbar, Upgrade und Recovery
sind belegt, unterstützte Plattformen sind exakt benannt und die veröffentlichte
Dokumentation entspricht den tatsächlich freigeschalteten Profilen und Providern.
