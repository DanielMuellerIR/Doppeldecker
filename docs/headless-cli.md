# Headless-CLI

Stand: 2026-10-19

`doppeldecker-cli` ist der unterstützte Headless-Einstieg in denselben lokalen und
providerübergreifenden Sicherheitsvertrag wie die Desktop-App. Das CLI erzeugt keine
eigene Realmodus-Erlaubnis und bietet bewusst weder `--force` noch einen importierbaren
Permit-Token. Remote-Befehle nehmen nur Providerart, Profil-ID und virtuellen Root
entgegen. Öffentliche Pins stammen aus der Profildatei, Secrets ausschließlich aus
deren Schlüsselbund-/Agent-Referenz; Passwort, Schlüsselmaterial und credentialed
URLs sind keine CLI-Argumente.

## Bauen und testen

```bash
cargo build -p doppeldecker-cli --release
cargo test -p doppeldecker-cli
```

Alle automatisierten Schreibtests verwenden künstliche Bäume unter einem temporären
Ordner. Der Realmodus selbst ist nicht auf die Test-Sandbox beschränkt und darf daher
nur mit bewusst gewählten Quelle-/Zielordnern gestartet werden.

Die macOS-App enthält das CLI bereits. Läuft sie aus `/Applications` oder
`~/Applications`, bietet sie einmalig an, einen nicht überschreibenden Symlink unter
`/usr/local/bin/doppeldecker-cli` anzulegen. macOS fragt dafür nach dem
Anmeldepasswort. „Später“ fragt beim nächsten App-Start erneut; „Nicht mehr fragen“
merkt die Entscheidung im App-Konfigurationsordner. Ein vorhandener fremder Eintrag
oder ein Start direkt aus DMG, Download- oder Buildordner wird nie verändert.
Die native exklusive Symlinkanlage lehnt auch spät erschienene Ordner und
Ordner-Symlinks ab; sie legt darin keinen zusätzlichen Link an.
DEB und AppImage enthalten dasselbe CLI unter `usr/bin/doppeldecker-cli`.

## Verbindung aus stdin importieren

`connections import` kennt keinen Passwortspeicher und keinen Eintragsnamen. Es liest
ausschließlich einen mehrzeiligen UTF-8-Text von stdin:

```bash
pass show '<pass-item>' | doppeldecker-cli connections import
# Gleichwertig für eine bereits geschützte Eingabedatei:
doppeldecker-cli connections import < verbindung.txt
```

`>` wäre hier falsch: Es leitet die Ausgabe in eine Datei namens
`doppeldecker-cli` um. Die Pipe `|` verbindet dagegen die Ausgabe des linken
Prozesses direkt mit stdin des Imports. Der Text darf höchstens 64 KiB und 64
nichtleere Zeilen enthalten.

Erkannte Bezeichner sind `user`, `username`, `name`, `user name`, `Benutzer`;
`pw`, `pass`, `password`, `Passwort`, `secret`; sowie `url`, `uri`, `server`,
`Adresse`. Die übliche erste unbeschriftete Passwortzeile wird akzeptiert, wenn
Nutzer und Adresse beschriftet sind. Bei genau drei vollständig unbeschrifteten
Zeilen gilt ausschließlich die Reihenfolge Passwort, Nutzer, Adresse. Sind genau
zwei der drei Felder beschriftet, wird eine einzelne unbeschriftete Zeile per
Ausschluss dem fehlenden Feld zugeordnet. Mehrdeutige oder zusätzliche
unbeschriftete Werte brechen ab.

Eine zweite Zeile wie `url ipv6: 2001:db8::1` wird als alternative Route
übernommen. Ist sie während des Imports erreichbar, müssen SFTP-Hostkey
beziehungsweise FTPS-Zertifikatspin zur Hauptroute passen. Ist etwa IPv6 gerade
nicht geroutet, bleibt die ausdrücklich gelieferte Adresse dennoch gespeichert; vor
einer späteren Anmeldung erzwingt der Provider denselben Pin. Für ausdrücklich
gewähltes Klartext-FTP kann keine kryptografische Routengleichheit behauptet werden.
Ein explizites `sftp://` wählt SFTP, `ftps://` explizites FTPS mit `AUTH TLS` und
privatem Datenkanal und `ftp://` bewusst unverschlüsseltes FTP. Ohne Schema wird
zuerst SFTP und danach FTPS geprüft; Klartext ist niemals ein stiller Fallback.
Automatisch erkanntes SFTP wird nur importiert, wenn der entdeckte Schlüssel bereits
in OpenSSHs `known_hosts` gebunden ist.

Vor dem Speichern werden Anmeldung und vollständiges Root-Listing geprüft. Eine
fehlende öffentliche Zertifikats-/Namensprüfung darf FTPS sichtbar als
`tls_identity_unverified` importieren; TLS und der beobachtete Zertifikatspin bleiben
aktiv. `ftp://` wird als `plaintext` importiert. Erst danach landet das Passwort im
System-Schlüsselbund und das geheimnisfreie Profil
atomar in der gemeinsamen App-Konfiguration. Profilname und stabile ID entstehen
deterministisch aus Host und Nutzer; unzulässige Zeichen im Namen werden ersetzt.
Eine bereits laufende App lädt externe Profiländerungen beim nächsten Fensterfokus.

Der Import bietet bewusst keinen `--dry-run`- oder Verbose-Modus: Weder erkannte
Werte noch Adressen werden auf stdout/stderr gespiegelt. Auch `--json` meldet nur
Erfolg, Protokoll, Transportstatus und Zahl geprüfter Alternativrouten. URLs mit
eingebetteten Zugangsdaten, Query oder Fragment werden abgelehnt.

Scheitert die Funktionsprüfung, nennt die Fehlermeldung ausschließlich feste Stufen
wie „Dienst nicht erreichbar“, „Hostkey fehlt“, „Anmeldung abgelehnt“,
„TLS-/Sitzungsaufbau fehlgeschlagen“ oder „Root-Listing fehlgeschlagen“. Sie enthält
weder Eingabewerte noch rohe Server- oder Bibliotheksantworten. Eine bloße IP-Adresse
ist für FTPS zulässig: Scheitert daran nur die öffentliche Namensprüfung, importiert
Doppeldecker die verschlüsselte Verbindung mit sichtbarer ungeprüfter Identität und
weiterhin aktivem Zertifikatspin.

## Dry-Run

```bash
cargo run -p doppeldecker-cli -- \
  --config-dir "$HOME/.doppeldecker-cli-example" plan \
  --source ~/Quelle --target ~/Ziel \
  --mode add-only --compare fast --metadata portable
```

Die Cargo-Konfiguration dieses Repos setzt
`DOPPELDECKER_NO_DEFAULT_USER_PATHS=1` mit `force = true`; ein geerbter leerer Wert
oder `0` kann diese Cargo-Testsperre nicht abschalten. Deshalb verlangen Aufrufe über
`cargo run` auch für einen reinen Plan einen ausdrücklichen
`--config-dir`. Der Beispielordner ist vom Profil der Desktop-App getrennt.

`plan` kanonisiert beide vorhandenen Roots und berechnet ausschließlich einen Plan.
Es schreibt nichts. Modi sind `add-only`, `update` und `mirror`; Vergleiche sind
`fast` und `content`; Metadatenprofile sind `portable`, `native-best-effort` und
`native-strict`.

## Ausschlussmuster

`plan`, `real` und der providerübergreifende Befehl `sync` akzeptieren
`--exclude MUSTER` wiederholt:

```bash
doppeldecker-cli plan --source ~/Quelle --target ~/Ziel \
  --exclude .DS_Store --exclude .env --exclude '**/*.tmp' \
  --exclude 'build/**'
```

Die Muster beziehen sich auf relative Pfade unter den beiden gewählten Roots und
sind auf allen Providern groß-/kleinschreibungsabhängig. Ein Muster ohne `/`, etwa
`.DS_Store`, `.env` oder `cache`, gilt für diesen Datei- oder Ordnernamen auf jeder
Ebene. In einem Pfadmuster steht `*` für beliebig viele und `?` für genau ein Zeichen
innerhalb einer Pfadkomponente. `**` darf nur eine vollständige Pfadkomponente sein
und umfasst keine bis beliebig viele Ebenen: `**/*.tmp` erfasst auch `datei.tmp` im
Root; `build/**` erfasst den Ordner `build` selbst und seinen gesamten Teilbaum.

Absolute Muster, Rückwärtsschrägstriche, leere sowie `.`-/`..`-Komponenten und ein
mit anderem Text vermischtes `**` werden mit `invalid_input` abgelehnt. Ein Lauf
akzeptiert höchstens 128 Muster mit je höchstens 256 UTF-8-Bytes. Die CLI prüft diese
Syntax, bevor sie einen Pfad auflöst, eine Providerverbindung öffnet oder eine
Capability-Probe startet.

Ein bewusst ausgeschlossener Eintrag ist für den Plan auf beiden Seiten unsichtbar:
Er wird weder kopiert oder aktualisiert noch als nur im Ziel vorhanden behandelt.
Er erzeugt keinen Skip und sperrt Mirror daher nicht. Enthält ein nur im Ziel
vorhandener Ordner einen ausgeschlossenen Nachfahren, bleibt seine Ordnerhülle
erhalten; sichtbare Geschwister werden weiterhin einzeln für die Quarantäne geplant.
Nicht sicher dekodierbare Providernamen werden dagegen nie über ihren verlustbehaftet
dargestellten Namen ausgeschlossen. Sie bleiben sichtbare unsichere Skips und
sperren Mirror unverändert.

## Bestätigter Realmodus

```bash
cargo run -p doppeldecker-cli -- \
  --config-dir "$HOME/.doppeldecker-cli-example" real \
  --source ~/Quelle --target ~/Ziel \
  --mode add-only \
  --audit "$HOME/.doppeldecker-cli-example/real-runs.jsonl"
```

Beim Realmodus verlangt dieselbe Repo-Sperre zusätzlich einen ausdrücklichen
`--audit`-Pfad. Konfigurationsordner und Audit liegen im Beispiel dauerhaft
außerhalb von Quelle und Ziel, damit Queue-Recovery und Audit auch nach einem
unterbrochenen Lauf erhalten bleiben. Für einen anderen Ablageort beide Pfade
bewusst gemeinsam anpassen; nicht in einen Quell-, Ziel- oder Cloud-Sync-Ordner
legen. Das direkt gebaute beziehungsweise installierte CLI setzt die Repo-Sperre
nicht selbst und verwendet ohne diese Optionen weiterhin die normalen App-Pfade.

Der Prozess führt Mount- und Root-Identität, vollständige Listings, Planbindung,
Platzprüfung und Restore-Probe aus. Danach zeigt er eine zufällige Phrase. Nur die
exakt über stdin zurückgegebene Phrase erzeugt das einmalige, zehn Minuten gültige
Permit im selben Prozess. `update` und `mirror` verlangen zusätzlich die Zeile
`BACKUP UND RESTORE BESTÄTIGT`. Der Kern erzwingt für beide Modi `content`.

`Ctrl-C` setzt das gemeinsame Abbruchsignal. Der Lauf beendet die aktuelle sichere
Grenze, schreibt seinen Audit-Abschluss und liefert Exitcode 130. Fehler liefern 2.
Ein erfolgreicher Lauf liefert 0. Außerhalb eines durch
`DOPPELDECKER_NO_DEFAULT_USER_PATHS` gesperrten Test- oder Cargo-Prozesses liegt das
append-only JSONL-Audit ohne `--audit PFAD` im plattformüblichen lokalen
State-/App-Datenordner.

## Maschinenprotokoll

`--json` schreibt genau ein JSON-Objekt je Zeile auf stdout. Hinweise und
Bestätigungsaufforderungen bleiben auf stderr, damit stdout parsebar bleibt.
Fehler liefern neben `code` und `message` die bekannten Core-Prüfpfade in
`recovery_paths`. Bei Dateiaufträgen bleiben die Pfade von Ausführung und
Journalfehlern zusätzlich getrennt in `execution_error`, `journal_error` und
`journal_errors` erhalten. Ein leeres Array bedeutet, dass für diesen Fehler keine
solchen Pfade vorliegen.

Ein Realmodus-Controller:

1. startet `doppeldecker-cli --json real …` mit Pipes,
2. liest `preparing` und `real_preview`,
3. prüft die Vorschau und schreibt `preview.confirmation_phrase` mit Zeilenende auf
   stdin,
4. schreibt bei gefordertem Backup-Gate die zweite Bestätigungszeile,
5. liest `progress` und abschließend `result` oder `error`.

Für den Provider-Sync gilt dieselbe Abfolge mit den Ereignissen
`provider_sync_preparing`, `provider_sync_preview`, `progress` und
`provider_sync_result` beziehungsweise `error`. Weicht ein erfolgreich wiederholter
Root-, Identitäts-, Capability-, Listing- oder Planbeleg von der Vorschau ab, liefert
das CLI `plan_drift`; der Controller muss neu vorbereiten und erneut bestätigen
lassen. Scheitert schon die erneute Prüfung selbst, bleibt deren konkreter Code wie
`not_found`, `connection_lost` oder `invalid_data` erhalten. Eine falsche Lauf- oder
Backup-Bestätigung liefert `confirmation_mismatch`. Diese Fehler beenden den Prozess
mit Exitcode 2 und mutieren weder Ziel noch Audit.

`sync`, `copy` und `move` prüfen beide gespeicherten Endpunktkonfigurationen vor
Queueöffnung und erster Verbindung. Ein fehlendes Zielprofil löst deshalb weder
Quellanmeldung noch einen neuen Queueeintrag aus. Der Prozesslock schützt weiterhin
die folgenden Verbindungen, Fähigkeitsproben und Wirkungen.

Plan und Quarantäne liefern `plan`, `quarantine_runs`, `quarantine_entries`,
`restored` beziehungsweise `purged`. Die Ausgabe enthält normale Pfade für die
Steuerung; das Audit enthält dagegen nur gehashte Identitäten und keine Inhaltsnamen.

Das `error`-Event trägt neben `message` einen stabilen Maschinencode `code`
(`plan_drift`, `sandbox_violation`, `mirror_blocked`, `confirmation_mismatch`,
`invalid_input`, `invalid_data`, `not_found`, `already_exists`, `unsupported`,
`capability_probe_required`, `permission_denied`, `storage_full`, `interrupted`,
`connection_lost`, `outcome_unclear`, `tls_identity_unverified`, `io`). Controller
sollen auf den Code reagieren — etwa bei `plan_drift` neu planen und erneut
bestätigen lassen — statt auf die deutschen Texte zu matchen. `unsupported` ist
eine dauerhafte Protokoll- oder Produktgrenze. `capability_probe_required` verlangt
eine neue Vorschau samt Capability-Probe, insbesondere nach einem Reconnect; der
Controller soll dafür keine neuen Zugangsdaten anfordern. `permission_denied`
bezeichnet dagegen eine tatsächliche Ablehnung durch Server, Schlüsselbund oder
Betriebssystem. `outcome_unclear` bedeutet: Eine Mutation wurde
genau einmal gesendet, ihr Ergebnis ließ sich aber weder bestätigen noch
ausschließen; der Controller muss zuerst den Zielzustand prüfen und darf die
Mutation nicht automatisch wiederholen.

## Dateioperationen und Provider

WebDAV-`rename` verwendet die [bedingte gewöhnliche Umbenennung](webdav-rename.md)
mit sichtbarer Best-Effort-Klasse und genau einem MOVE ohne künstliche Löschprobe.
`capabilities.interactive_rename` beschreibt diesen eigenen Interaktionsvertrag;
die bisherigen `rename`-Details behalten ihre strengeren Probeanforderungen.
Bei `outcome_unclear` alten und neuen Namen prüfen und nicht blind wiederholen.

Die [aktuelle Fähigkeitsmatrix](function-audit.md) gilt auch für alle CLI-Befehle:
SFTP bietet `list` und additive lokale Datei- und Baumdownloads über `copy` als
sichtbar pfadbasiertes Best Effort. Die Grenze beträgt 100 direkte Auswahlen,
8 GiB je Datei sowie bei Bäumen 1000 Einträge, 16 Unterordnerebenen und 16 GiB.
Ersetzen, Move und Remote-Mutationen
bleiben gesperrt; [Lesebelege und Restrisiko](sftp-contents.md) gelten unverändert.
Baumvorschauen liefern `source_tree` mit Digest, Eintrags-/Datei-/Ordnerzahlen,
Bytes und pfadbezogenen Auslassungen. Eine spätere Abweichung erzeugt dieselbe
`SourceTreeChanged`-Bestätigung wie in der GUI; der Aufruf behauptet keine strikte
SFTP-Listingfähigkeit.
SMB-Zielersetzungen, Update/Mirror und Quarantäne-Restore/Purge bleiben ohne identitätsgebundene Primitive gesperrt. Beispiele mit
Remote-Profilen setzen ein bewusst angelegtes Profil und einen künstlichen oder
ausdrücklich gewählten Root voraus.

Ohne `--config-dir` verwendet das CLI denselben plattformüblichen
`com.doppeldecker.desktop`-Konfigurationsordner wie die App. Ein abweichender
Testordner kann explizit gesetzt werden.

WebDAV-`mkdir` verwendet eine eigene Best-Effort-Klasse ohne künstliche Löschprobe.
Vor der Wirkung erscheinen ihre Einschränkungen auf stderr; JSON liefert
`execution_class`, `path` und `limitations`. Namenskollisionen bleiben erhalten,
ein unklarer Ausgang nennt den Prüfpfad und wird nicht automatisch wiederholt.
Starke Eltern-ETags und vollständige Listings bleiben erforderlich.
[Vertrag und Grenzen](webdav-folder.md).

```bash
doppeldecker-cli mkdir --provider smb --profile server --parent /isoliert --name Neu
doppeldecker-cli list --provider ftps --profile server --root /isoliert
doppeldecker-cli sync \
  --source-provider ftps --source-profile server --source-root /isoliert/quelle \
  --target-provider local --target-root /tmp/ziel --mode add-only \
  --exclude .DS_Store --exclude cache
doppeldecker-cli copy \
  --source-provider local --source-root /tmp/quelle \
  --target-provider ftps --target-profile server --target-root /isoliert \
  --item /tmp/quelle/datei.txt
doppeldecker-cli move \
  --source-provider webdav --source-profile labor --source-root /eingang \
  --target-provider smb --target-profile archiv --target-root /ziel \
  --item /eingang/ordner
doppeldecker-cli rename --provider smb --profile server --root /isoliert \
  --path /isoliert/alt.txt --new-name neu.txt
doppeldecker-cli capabilities --provider ftps --profile server --root /isoliert
```

`--item` ist für Mehrfachauswahl wiederholbar. Ordner werden vollständig rekursiv
gestreamt; es gibt keinen lokalen Klartext-Spool. `copy` und `move` verwenden denselben
`ProviderFile`-Queue-Executor wie F5/F6 in der GUI. Ohne Option stoppen vorhandene
Ziele mit `already_exists`; `--skip-conflicts` führt vorhandene Ordner zusammen und
überspringt nur kollidierende Blätter. `--replace` verwendet nach echter
Quarantäne-/Restore-Probe die Phrase `ERSETZEN`; FTPS/WebDAV dürfen eine zuvor
geprüfte reguläre Datei stattdessen sichtbar irreversibel als Best Effort ersetzen
und verlangen dann `DAUERHAFT ERSETZEN`. Die exakte ausgegebene Phrase kommt über
stdin. `move` verlangt zusätzlich seine eigene Quellphrase. Bei bewiesenem
No-Replace-Commit stoppt eine späte Kollision weiterhin, statt ein inzwischen
entstandenes Ziel zu ersetzen; in der Best-Effort-Klasse (nächster Absatz) trägt
diese Garantie allein die frische Bestandsprüfung — das verbleibende Fenster
beschreibt der nächste Absatz.

Beweist ein FTP-/FTPS-Ziel nur das gewöhnliche, potenziell ersetzende Rename,
läuft die Neuanlage als sichtbarer Best-Effort-Commit: Das `file_preview`-Event
meldet `copy_new_best_effort` samt `limitations`, die konkrete Einschränkung geht
zusätzlich an stderr. Der Commit prüft den frischen Elternbestand, benennt genau
einmal um und liest das Ergebnis per Listing zurück; ein ungeklärter Ausgang endet
mit `outcome_unclear` statt mit einem Scheinerfolg. Wichtig für Automatisierung:
Zwischen Bestandsprüfung und Rename bleibt ein kleines, nicht atomar schließbares
Kollisionsfenster — ein exakt in diesem Moment entstehendes gleichnamiges Ziel
kann serverseitig ersetzt werden (maximaler Schaden: genau dieser eine Eintrag).

Meldet `file_preview` für ein FTPS-/WebDAV-Ziel `replace_best_effort`, darf
`--replace` ausschließlich die bestätigten vorhandenen regulären Dateien ohne
Quarantäne ersetzen. Die Altversionen sind danach nicht wiederherstellbar. Der Core
bindet unmittelbar vorher das vollständige Elternlisting, mutiert genau einmal und
liest Endname, Größe und Inhalt vollständig zurück. Typkonflikte und Sync verwenden
diesen Weg nicht; `outcome_unclear` wird nie automatisch wiederholt.

`rename` läuft in der stärksten frisch geprüften Ausführungsklasse: abgesichert
(bewiesenes No-Replace-Rename) oder Best Effort (gewöhnliches Provider-Rename nach
frischem Elternlisting, genau eine Mutation, anschließender Listing-Readback). Eine
Bestätigungsphrase ist nicht mehr nötig. Ein bereits belegter Zielname stoppt
sichtbar mit `already_exists`, statt zu ersetzen. In der Best-Effort-Klasse gilt dieser Stopp
für bereits sichtbare oder serverseitig abgelehnte Kollisionen; zwischen frischem
Elternlisting und Rename bleibt ein kleines Fenster, in dem eine exakt dann
entstehende Kollision genau ein vorhandenes Ziel ersetzen kann.
Beide Klassen beschreiben ausschließlich den Schutz des ZIELS. Die Quelle ist in
beiden über ihren Pfad gebunden: Wird zwischen dem frischen Elternlisting und der
Mutation ein gleichnamiger Eintrag ausgetauscht, benennt der Lauf den neuen Eintrag
um und meldet Erfolg. Für Automatisierung ist das der wichtigste Punkt an dieser
Stelle — `limitations` führt diese klassenunabhängige Einschränkung deshalb in
beiden Klassen mit, auch bei `assured`; die klassenspezifische Einschränkung steht
davor.
Erfolg meldet `rename_result` mit `outcome`,
`class` (`assured`/`best_effort`), `limitations` und `verified_after_reconnect`.
Nach einem Verbindungsabbruch klärt genau ein erneutes Listing den Zustand; bleibt
er offen, endet der Lauf mit `outcome_unclear`, und die Mutation wird niemals blind
wiederholt. `capabilities` liefert die für genau diese frisch verbundene
Sitzung verhaltensgeprüften Datei-, Lösch- und Syncfähigkeiten als JSONL; ein Reconnect
setzt solche Beweise zurück. Der `rename`-Block meldet dort `available` nur, wenn
mindestens eine Ausführungsklasse bewiesen ist, samt `assured`, `best_effort`,
`limitations` und `blockers`.
Bei FTP/FTPS schreibt jede mutierende Probe zuvor einen privaten Queueintent. Ein
unklarer Ausgang oder Verbindungsverlust beendet weitere Teilproben und liefert
einen Fehlerstatus; JSONL erhält die einzelnen Absagen und `recovery_paths`.
Ein unbekannter STOU-Name bleibt ausdrücklich unbekannt. Nach Neustart zeigt die
Queue solche Reste nur zur manuellen Prüfung, ohne Schreibwiederholung oder
automatische Remote-Löschung. [FTPS-Intentvertrag](ftps-stou-transport.md#wirkungsbezogene-vorbereitung-und-dauerhafte-intents).

`sync` steuert denselben normalen Einweg-Provider-Sync wie
`prepare_provider_sync` in der App. Quelle und Ziel dürfen lokal oder gespeicherte
Remote-Endpunkte sein. Vorschau, zufällige Laufphrase, optionales Backup-Gate,
Fortschritt, Quarantäne und append-only Audit bleiben im selben Prozess gebunden.

Remote-Löschen ist ein eigener gebundener Vollscan:

```bash
doppeldecker-cli delete --provider webdav --profile labor --root /isoliert \
  --item /isoliert/alt --mode permanent
```

`quarantine` ist nur verfügbar, wenn Parken, Manifest, Restore und Readback bewiesen
sind. `permanent` erscheint nur nach einer getrennten Datei-/Leerordner-Löschprobe und
verlangt die exakte Phrase `DAUERHAFT LÖSCHEN …`. Es gibt in keiner Richtung einen
stillen Fallback zwischen beiden Wirkungen. Derzeit bietet nur WebDAV den
ETag-bedingten endgültigen Weg an. FTPS, SFTP und SMB geben `delete` weder als
Quarantäne noch als Dauerlöschen frei. Das gewöhnliche, begrenzte Entfernen einer
vollständig kopierten Move-Quelle ist ein eigener Best-Effort-Vertrag.

Lokale Einträge verwenden stattdessen denselben descriptor-relativen Systempapierkorb
mit Crash-Journal wie die GUI:

```bash
doppeldecker-cli trash --item "$HOME/Downloads/alt.txt"
doppeldecker-cli trash --item "$HOME/Downloads/eins" --item "$HOME/Downloads/zwei"
```

Vor dem ersten Claim läuft die Recovery unter `--config-dir`; ein ungeklärter Claim
stoppt den neuen Stapel. Es gibt weder dauerhaftes Löschen noch Copy-and-delete als
Fallback. `trash_result` bilanziert `trashed`, `failed`, Fehler und nötige
Recovery-Pfade; ein unvollständiger Stapel endet mit Exitcode 2.

Provider-Datei- und -Löschläufe schreiben zusätzlich ein geheimnis- und pfadfreies
append-only `operations-audit.jsonl` neben das Queue-Journal. Jeder Datensatz nennt
`source_provider` und `target_provider` sowie die nach der frischen Capability-Prüfung
tatsächlich bestätigte `execution_class` (`assured` oder `best_effort`); beim Löschen
ist `source_provider` `null`. Ein bereits offener Queue-Eintrag wird nicht überholt;
das CLI scheitert sichtbar, statt die Reihenfolge zu umgehen.

## Quarantäne

```bash
doppeldecker-cli quarantine list --target ~/Ziel
doppeldecker-cli quarantine entries --target ~/Ziel --run LAUF-ID
doppeldecker-cli quarantine restore --target ~/Ziel --run LAUF-ID --path RELATIVER-PFAD
doppeldecker-cli quarantine purge --target ~/Ziel --run LAUF-ID
```

Jeder Lauf wird gegen den kanonischen Zielroot geprüft. Wiederherstellen ersetzt nie
eine vorhandene Zieldatei und verlangt exakt `RESTORE <Lauf-ID> <relativer Pfad>` über
stdin. `quarantine entries` nennt auch in der normalen Textausgabe jeden relativen
Pfad, die physische Präsenz, die automatische Wiederherstellbarkeit und einen
ausstehenden Restore-Abschluss. Alte Records ohne gebundenen Restore-Claim bleiben
als nur manuell rettbar sichtbar. Nach einem früheren Rename mit
fehlgeschlagenem Manifestabschluss prüft ein erneuter Restore die persistierte
Dateiidentität am Ziel und meldet `reconciled`, ohne erneut umzubenennen; ein neuer
Rename meldet `restored`. Endgültiges Leeren verlangt exakt `LEEREN <Lauf-ID>` und
bleibt gesperrt, solange ein solcher Restore-Abschluss offen ist.

## Bidirektionaler Sync

```bash
doppeldecker-cli bidirectional plan --pair-id 00112233445566778899aabbccddeeff \
  --pair-name Arbeitsdaten --left ~/Links --right ~/Rechts \
  --baseline ~/.local/state/doppeldecker/arbeitsdaten.json
doppeldecker-cli bidirectional resume --pair-name Arbeitsdaten \
  --left ~/Links --right ~/Rechts --baseline ~/.local/state/doppeldecker/arbeitsdaten.json
doppeldecker-cli bidirectional rollback --pair-name Arbeitsdaten \
  --left ~/Links --right ~/Rechts --baseline ~/.local/state/doppeldecker/arbeitsdaten.json
```

`run` verwendet dieselben Argumente wie `plan`. `plan` verwirft sein mutationsloses
Laufjournal nach der Vorschau. `run`, `resume`
und `rollback` halten Root-Handles, Journal-Lock und zufällige Phrase im selben
Prozess. Konfliktentscheidungen können als reguläre JSON-Datei mit höchstens 16 MiB
über `--resolutions` übergeben werden. Der Leser folgt keinem finalen Symlink und
prüft Typ und Größe am geöffneten Handle. Ein nach der ersten Pfadprüfung
eingewechselter FIFO blockiert nicht; Wachstum bleibt auch während des Lesens
begrenzt. Ein Resume-Journal wird weder bei falscher Phrase
noch bei einem abgelehnten Scan verworfen. JSONL verwendet die Events
`bidirectional_preview`, `bidirectional_result`,
`bidirectional_rollback_preview` und `bidirectional_rollback_result`.
