# Doppeldecker — dauerhafte Projektregeln

Stand: 2026-11-18. Dual-Pane-Dateimanager für macOS und Linux. Vorrangiges Produktziel
ist umfassende, komfortable, schnelle und effiziente Dateiarbeit. Datenintegrität
unterstützt dieses Ziel; zusätzliche Absicherung ist kein Selbstzweck und rechtfertigt
keine pauschalen Sperren üblicher Dateiaktionen. Keine allgemeine Vorschaupflicht:
gewöhnliche Copy-/Move-/Rename-/mkdir-Aktionen laufen direkt, Konflikte und konkrete
Löschwirkungen bekommen passende Entscheidungen. Sync und größere Sammeländerungen
besitzen eigene Vorschauen. Fehlende atomare Protokollgarantien führen zu einem
ehrlich benannten Best-Effort-Weg. Aktueller Funktionsstand und Ausbauziel bleiben
in der Dokumentation unterscheidbar.

## Produktziel und Arbeitsfolge

Maßstab ist der vollständige Arbeitsweg im Dateimanager. Eine neue Funktion muss
an den passenden Stellen mit Maus oder Trackpad auffindbar und mit der Tastatur
zügig bedienbar sein. Sinnvolle Vorgaben ermöglichen den Einstieg, weiterführende
Optionen bleiben erreichbar. Bei einer technischen Grenze zuerst die konkret
betroffene Wirkung und den praktikablen Weg für diesen Provider prüfen. Eine
verbleibende Funktionslücke im Backlog erhalten.

Bei einem offenen Entwicklungsauftrag den ersten ausführbaren Punkt im
[Backlog](backlog.md) bis zum genannten Fertigkriterium bearbeiten und im Rahmen
des Auftrags direkt fortfahren. Bei einer extern blockierten Prüfung den fehlenden
Nachweis festhalten und unabhängige Arbeit vorziehen. Bei längeren Läufen am
vereinbarten Zeit- oder Umfangslimit den geprüften Stand, die offene Prüfung und
den nächsten ausführbaren Schritt festhalten. Testauswahl, Bedienung und
Aufräumen stehen unter [Bauen und testen](AGENT-REFERENCE.md#bauen-und-testen).

## Einstieg und Architektur

Vor Änderungen an Core, CLI, Tauri oder Frontend [Einstieg und Architektur](AGENT-REFERENCE.md#einstieg-und-architektur) lesen. Korrektheits- und Sicherheitslogik bleibt im gemeinsamen Core. Provider sind prozessweit, Queue, Vorbereitungen und Permits gehören dem jeweiligen Fenster.

## VFS- und Pfadvertrag

Alle Provider implementieren dasselbe `Vfs`-Modell. Lokale Mutation erfolgt
descriptor-relativ mit `openat`/`renameat`/`unlinkat`, `O_NOFOLLOW` und gehaltenem
Root. Jede Elternkomponente wird ohne Symlinkfolgen geöffnet; finale Identität und
Mount/Volume bleiben bis zum Ende gebunden.

Für gewöhnliche interaktive SFTP-Aktionen gilt zusätzlich der ausdrücklich
zugelassene pfadbasierte Best-Effort-Vertrag: lexikalisch begrenzte Auswahl,
frische LSTAT-/REALPATH-/Elternprüfungen, keine beobachteten Symlinks, genau ein
mutierender Versuch und Ergebnisprüfung. SFTP v3 kann einen gleichzeitigen
serverseitigen Elternpfadtausch nicht atomar ausschließen; eine Umleitung auf einen
anderen Pfad desselben Serverkontos bleibt ein sichtbares akzeptiertes Restrisiko.
Eine zusätzliche SSH-Exec-/Serverhelferpflicht ist dafür nicht vorgesehen. Diese
interaktive Klasse erteilt keine strikte Sync-/Mirror- oder Quarantänefähigkeit.

- Quelle und Ziel dürfen weder gleich noch ineinander verschachtelt sein.
- Symlinks, Spezialdateien, unsicher dekodierbare Namen und unbekannte Provider-
  Fähigkeiten sichtbar bilanzieren. Nur die konkret davon abhängige Wirkung
  überspringen oder sperren; unabhängige praktische Funktionen bleiben verfügbar.
- Neue Dateien in kollisionsfreie Temps schreiben, vollständig übertragen, BLAKE3-
  prüfen, soweit verfügbar fsyncen und mit der stärksten Provideroperation committen.
  Fehlt atomarer No-Replace, ist ein sichtbar bestätigter Best-Effort-Commit mit
  frischem Zielcheck und Readback zulässig; stilles Ersetzen bleibt verboten.
- Metadaten folgen dem gewählten Profil. `Portable` ist sicherer Default;
  `NativeBestEffort` verlangt ContentHash und bleibt für strikten Mirror gesperrt;
  gewöhnlicher Inhaltssync bilanziert technisch begründete Metadatenauslassungen
  gesondert, ohne unvollständige Dateibestände als vollständig zu behandeln.
  `NativeStrict` nur nach vollständigem Scan ohne unbekannte/ausgelassene Fähigkeit.
- Owner, ACL, xattrs, Resource Forks, sparse Extents, Hardlinks, Rechte und Sonderbits
  sind manifestgebunden. Nicht darstellbare Klassen sichtbar bilanzieren, nie still
  als erfolgreich melden.
- Numerische Owner-/Gruppen-IDs brauchen eine bestätigte gemeinsame Identitätsdomäne.
  Gewöhnlicher Remote-Sync darf gleiche Zahlen nicht als gleiche Benutzer deuten;
  fehlende Zuordnung sichtbar auslassen und NativeStrict nicht herabstufen.
- Vor sicherheits- oder datenrelevanten Wirkungen (etwa dem Quarantäne-`purge`)
  entscheidet nie ein boolesches `exists()` über Abwesenheit: Ein Stat-,
  Verbindungs- oder Providerfehler ist dort kein „nicht vorhanden“. Fehlererhaltend
  `stat` prüfen und nur ein echtes `NotFound` als Abwesenheit werten.

## Sync-Sicherheitsvertrag

Gewöhnlicher Inhaltssync über Remote-Endpunkte besitzt den getrennten Vertrag aus
[`docs/content-sync.md`](docs/content-sync.md). Remote-Mirror darf nach konkreter
Wirkungsbestätigung vorhandene Fassungen endgültig ersetzen und zusätzliche
Zielinhalte endgültig entfernen (Produktentscheidung 2026-10-28). Fehlende
Wiederherstellung und tatsächliche Teilwirkungen müssen sichtbar bleiben;
Quarantäne darf nicht vorgetäuscht werden. Starke lokale Ziele behalten ihre echte
Quarantäne. Die folgenden Quarantänegates beschreiben den getrennten strikten
Syncvertrag und werden nicht als pauschale Remote-Sync-Sperre wiederverwendet.

`plan()` verändert nichts; `execute()` führt nur den exakt bestätigten, frisch
gebundenen Plan aus. Dieser Vertrag schützt besonders Ersetzungen und rekursive
Entfernungen. Er darf nicht unverändert als pauschale Eintrittsschranke für normale,
interaktiv ausgelöste Dateioperationen verwendet werden. Modi:

- `AddOnly`: Standard, nur Fehlendes hinzufügen.
- `Update`: hinzufügen/aktualisieren, nichts entfernen; für echte Läufe ContentHash.
- Beim strikten `Mirror` überzählige Ziele in laufbezogene Quarantäne verschieben.
  Für gewöhnlichen Inhaltssync gilt der oben verlinkte Providervertrag.

Verbindliche Schranken:

- Normaler lokaler AddOnly-/Update-Sync verwendet eine backendgehaltene,
  wirkungsbezogen bestätigte Vorbereitung und die Fensterqueue. Update verlangt
  ContentHash. Abgesicherte Ersetzungen behalten Quarantäne und Restore-Probe.
  Auf einem dafür geeigneten schwachen lokalen Ziel kann die Vorschau eine
  ausdrücklich zu bestätigende dauerhafte Ersetzung anbieten. Additionen brauchen
  keine Laufphrase oder Quarantäneprobe. Mirror behält seinen getrennten Assistenten.
  Eine Umgebungsvariable allein erteilt niemals eine lokale Schreibfreigabe.
  Vertrag und konkrete Grenzen: [`docs/local-sync.md`](docs/local-sync.md).
- GUI-Schreibtests ausschließlich mit `./tools/run-test-sandbox.sh`. Der Launcher
  erzeugt Marker sowie getrennte `quelle/`-/`ziel/`-Panes. Nie `/`, `$HOME`, Repo,
  Cloud-Sync-Ordner, Originaldaten oder einen gefährlichen Elternpfad als Sandbox setzen.
- Lokaler Realmodus hält Root-Deskriptoren, Volume-/Mountidentitäten, vollständige
  Listings, Plan-Digest und Platzprüfung. Die Restore-Probe gehört zu Wirkungen,
  für die der Plan Wiederherstellung zusagt. Vorbereitung und Permit gelten höchstens zehn Minuten;
  Permits sind einmalig und nicht serialisierbar. Die Oberfläche bestätigt die
  konkrete Wirkung ohne Routine-Laufphrase.
- Strikter Mirror verlangt ContentHash, vollständigen Zweitscan, Quarantäne und eine
  wirkungsbezogene Bestätigung. Leere Quellen, hohe Entfernungszahlen und -anteile
  werden deutlich gewarnt und dürfen bewusst bestätigt werden; die früheren
  50-/100-Eintrags- und 5-/25-Prozent-Grenzen sind keine Sperren mehr. Ordner
  rekursiv nach Einträgen und Bytes zählen. Normale Sync-Starts benötigen keine
  Laufphrase oder behauptete externe Backupbestätigung.
- Sobald Konflikte, Symlinks, Spezialdateien oder unvollständige Listings ausgelassen
  wurden, jede Mirror-Entfernung des Laufs sperren.
- Kopien vollständig vor Entfernungen ausführen. Im strikten Sync ersetzte/überzählige Ziele in
  `.doppeldecker-quarantine/` außerhalb des Zielbaums sichern.

## Dateioperationen, Papierkorb, Queue und Quarantäne

Vor Änderungen an Dateioperationen oder Wiederaufnahme [Dateioperationen, Papierkorb, Queue und Quarantäne](AGENT-REFERENCE.md#dateioperationen-papierkorb-queue-und-quarantäne) lesen.

## Remote-Provider und Secrets

Bei Provider-, Verbindungs- und Credential-Arbeit [Remote-Provider und Secrets](AGENT-REFERENCE.md#remote-provider-und-secrets) lesen. Secrets aus öffentlichem Zustand, Diagnose und Protokollen heraushalten.

## Fehler-, UI- und Plattformverträge

Vor Änderungen an Fehlerzuständen, Oberfläche oder Plattformintegration [Fehler-, UI- und Plattformverträge](AGENT-REFERENCE.md#fehler--ui--und-plattformverträge) lesen.

## Bauen und testen

Vor Build, Installation, Release oder Tests der betroffenen Plattform [Bauen und testen](AGENT-REFERENCE.md#bauen-und-testen) lesen.

## Release, Code und Git

- Produktversionen nur für ausdrücklich benannte Meilensteine oder Releasekandidaten
  erhöhen; gewöhnliche Implementierungs-, Test- und Doku-Commits bleiben unter
  `Unreleased`. Bei einem Versionswechsel Cargo/Tauri/package.json synchron halten
  und einen passenden Changelog-Abschnitt anlegen.
- Identifier Englisch, Doku/Kommentare Deutsch; Sicherheitslogik ausführlich und
  anfängerfreundlich kommentieren. Kommentare bei Refactor/Rename erhalten.
- Nur konkrete Pfade stagen, fremdes WIP und Artefakte erhalten. Kein `git add .`,
  `git add -A`, Reset/Clean oder destruktive Dateioperation.
- Nach vollständiger Verifikation committen; Tags, Releases und öffentliche Pakete
  nur auf ausdrücklichen konkreten Auftrag.
- Öffentliche Artefakte auf lokale Buildpfade, private Hosts/Kontakte, Secrets und
  interne Formulierungen prüfen.

## Aktive Arbeit und progressive Details

Die offene Arbeit steht in [`backlog.md`](backlog.md). Erledigte Schnitt-/Testchronik gehört
in `CHANGELOG.md` oder die Release Notes, nicht in den Startkontext.
READMEs bleiben kurze Produkteinstiege und verlinken Fachdetails. Im Backlog nennt
jeder Punkt Wirkung, Fertigkriterium und höchstens einen Detailverweis; keine
wiederholten Prüfzahlen oder Ablaufchroniken.
Backlog und Roadmap kurz halten: nur offene Aufgaben, Prioritäten, Fertigkriterien
und Detailverweise; keine wiederholten Testzahlen oder erledigte Chronik.
Metadaten-Releaseziel ist belegte ForkLift-Parität; darüber hinausgehende
Spezialfähigkeiten bleiben Zukunftswünsche, keine pauschalen Releasegates.

Vor Spezialarbeit gezielt [`docs/INDEX.md`](docs/INDEX.md) und das verlinkte Vertragsdokument lesen.
Unter `vendor/smb2/**` liegen fremde `CLAUDE.md`-Dateien: vendorte, verschachtelte
Scope-Regeln, nur bei Änderungen innerhalb des jeweiligen Vendorbaums zusätzlich
anwenden; nicht in die Root-AGENTS kopieren.
