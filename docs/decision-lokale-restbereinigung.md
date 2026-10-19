# Entscheidung: lokale Temp- und Sicherungsreste

Stand: 2026-10-02. **Status: Variante C2 ist integriert; die ursprüngliche Variante C
bleibt wegen der macOS-ACL-Gegenprobe ausgeschlossen.** Betroffen sind die
Backlog-Punkte 1 (`local-parked-backup-outcome`) und 10. Die ersten Abschnitte
dokumentieren den Ausgangsbefund und die verworfenen Alternativen; der aktuelle
Vertrag steht ab „Überarbeitete Variante C2".

## Worum es geht

Doppeldecker schreibt jede neue Datei zuerst in eine Zwischendatei („Temp") neben dem
Ziel und hängt sie erst nach vollständiger Übertragung und Prüfsumme an den
endgültigen Namen. Muss dabei ein vorhandenes Ziel weichen, wird dieses vorher auf
einen Sicherungsnamen umgehängt („geparkt"), damit ein Fehler mitten im Vorgang nicht
die alten Daten mitnimmt.

Beide Zwischennamen müssen am Ende wieder verschwinden — nach einem Erfolg ebenso wie
nach einem Abbruch. Genau dieses Aufräumen ist heute die schwächste Stelle des
lokalen Schreibwegs. Es geschieht entweder über den bloßen Pfad, also ohne Beweis,
dass der Name im Moment des Löschens noch unser Objekt bezeichnet — oder es
unterbleibt ganz, und der Vorgang meldet dann einen unklaren Ausgang, obwohl die
eigentliche Wirkung vollständig gelungen und zurückgelesen ist.

## Die zwei Symptome

### Symptom A: eine vollständig gelungene Ersetzung meldet „unklar"

Trifft ein Kopiervorgang auf einen Typkonflikt — eine Datei soll dorthin, wo ein
Ordner liegt, oder umgekehrt —, parkt `park_existing_target`
(`core/src/fileops.rs:2097`) das Altziel auf einem versteckten Nachbarnamen. Es
benutzt dafür `Vfs::rename_noreplace_with_removal_proof`. Für `LocalVfs` liefert
diese Methode (`core/src/vfs.rs:1382`) bei Erfolg immer `Ok(None)`: Das Rename
gelingt, aber es entsteht kein Beleg, der das geparkte Objekt an genau diese
Operation bindet.

Ist der neue Zielbaum danach fertig geschrieben, geprüft und zurückgelesen, will
`remove_bound_backup` die Sicherung entfernen. Ohne bestätigten Konfliktbeleg
verweigert es das mangels Entfernungsbeleg. Mit einem bestätigten Konfliktbeleg
kann `verify_parked_replacement` nach dem Parken einen Beleg liefern; dann scheitert
jedoch `LocalVfs::remove_if_matches` mit `Unsupported`, weil lokal keine atomare
bedingte Entfernung existiert. Beide Pfade erhalten die Sicherung und melden
`ErrorCode::OutcomeUnclear`. Seit 2026-09-30 gelangt der Sicherungspfad dabei auch
in die strukturierte Fehlerbilanz.

Ergebnis: Der lokale Ersetzungsfall mit Typkonflikt endet **immer** als unklarer
Ausgang, obwohl das Ziel nachweislich korrekt steht. Das widerspricht der
Ausführungsklasse „Abgesichert" aus
[`docs/product-principles.md`](product-principles.md): Wir melden eine vollzogene,
belegte Wirkung als möglicherweise misslungen.

Der frühere zweite Mangel ist behoben: Der Pfad der nach einem Cleanup-Fehler
stehengebliebenen Sicherung wird seit 2026-09-30 über `record_temp_recovery_path`
in `FileOperationActuals::temp_recovery_paths` aufgenommen. Er steht damit auch im
Queue-Eintrag und Audit, statt ausschließlich im Fehlertext.

### Symptom B: das Aufräumen nach Abbruch löscht über den Pfad

`cleanup_failed_temp` (`core/src/sync/atomic.rs:609`) unterscheidet über die Marke
`temp_released`, ob der Commit den Temp-Namen schon freigegeben hat. Nur danach räumt
`cleanup_bound_temp` identitätsgebunden auf — was lokal nichts tut, weil
`supports_remove_if_matches()` für `LocalVfs` `false` ist.

Vorher, also bei Abbruch durch den Nutzer, bei Transferfehlern und im Hardlinkpfad,
löscht der Fehlerweg weiterhin allein über den Pfad: `core/src/sync/atomic.rs` in den
Zeilen 274, 395, 622, 730 und 746.

Der Temp-Name ist dabei ab dem exklusiven Anlegen im gemeinsamen Zielverzeichnis
sichtbar. Bis 2026-09-30 bildete `sibling_candidate` ihn allein aus Zielname,
Rolle, PID und laufender Nummer. Seitdem kommt ein Zufallsanteil hinzu, der die
Vorhersagbarkeit reduziert, aber einen bereits sichtbaren Namen nicht schützt. Ein fremder Prozess mit Schreibrecht in genau diesem Zielordner kann
die Temp-Datei entfernen und denselben Namen mit eigenem Inhalt neu belegen; unser
Aufräumen träfe dann fremde Daten.

Zur Einordnung: Das ist durch Codelesen belegt, nicht durch einen beobachteten
Vorfall, und es setzt einen Schreibberechtigten im Zielordner voraus. In einem
gewöhnlichen Nutzerordner ist das niemand; in einem gruppenschreibbaren Projekt- oder
Serverordner ohne Sticky-Bit sehr wohl.

### Die gemeinsame Ursache

Beide Symptome kommen aus derselben Lücke: **Doppeldecker legt seine Zwischennamen in
einem Verzeichnis an, das Fremde beschreiben dürfen, und hat danach kein Mittel, das
eigene Objekt beim Aufräumen wiederzuerkennen.** Symptom A wählt daraufhin die
vorsichtige Seite (nichts tun, unklar melden), Symptom B die unvorsichtige (über den
Pfad löschen).

## Was die Betriebssysteme wirklich hergeben

Dieser Abschnitt korrigiert meine frühere Empfehlung. Ich hatte eine
„deskriptorgebundene Entfernung mit Inode-Vergleich" vorgeschlagen. Das ist zu
unbestimmt formuliert, denn:

**Ein bedingtes Entfernen gibt es auf macOS und Linux nicht.** `unlinkat()` kennt nur
das Flag `AT_REMOVEDIR`; eine Bedingung „Gerät und Inode müssen noch dem geprüften
Eintrag entsprechen" existiert in keinem der beiden Kernel. Der Kommentar an
`LocalVfs::remove_if_matches` (`core/src/vfs.rs:1272`) sagt das bereits richtig: Ein
zweites `statat` vor dem `unlinkat` schließt das Rennen nicht, es eröffnet nur ein
neues.

Vorhanden sind dagegen:

- **`renameat`/`renameat2(RENAME_NOREPLACE)`** (Linux) und
  **`renamex_np(RENAME_EXCL)`** (macOS): exklusives Umbenennen. Nutzt Doppeldecker
  bereits.
- **`openat`/`mkdirat` mit gehaltenem Elterndeskriptor** und `O_NOFOLLOW`. Nutzt
  Doppeldecker bereits (`core/src/local_fs.rs`).
- **Verzeichnisrechte einschließlich ACL.** Der Modus `0700` und der richtige
  Besitzer reichen bei einer zusätzlich erlaubenden macOS-ACL nicht aus. Ein
  privater Namensraum braucht eine wirksame Zugriffsschranke schon bei der Anlage.

Der letzte Punkt ist der Hebel: Was der Kernel nicht als atomare Bedingung anbietet,
lässt sich durch die Wahl des Namensraums erreichen. Der Papierkorbweg verwendet
bereits `ensure_private_directory` (`core/src/trash.rs:780`) mit
`PRIVATE_DIR_MODE = 0o700` und einer descriptor-gebundenen Besitzer-/Modusprüfung.
Diese Prüfung allein genügt aber nicht als ACL-Vertrag für den neuen Arbeitsbereich.

## Die drei Varianten

### Variante A — Identitätsprüfung unmittelbar vor dem Löschen

`LocalVfs::remove_if_matches` würde den Elternordner deskriptorrelativ öffnen,
`statat` auf den Namen ausführen, Gerät und Inode mit dem Beleg vergleichen und bei
Übereinstimmung `unlinkat` aufrufen.

- **Aufwand:** klein, rund 60 Zeilen plus Tests. `open_parent` gibt es schon
  (`core/src/local_fs.rs:371`).
- **Restrisiko:** Zwischen `statat` und `unlinkat` bleibt ein Fenster. Der Angriff
  aus Symptom B wird sehr viel unwahrscheinlicher, aber nicht unmöglich.
- **Der eigentliche Einwand:** Um damit etwas zu gewinnen, müsste
  `supports_remove_if_matches()` für `LocalVfs` auf `true` gehen. Diese Flagge ist im
  Trait ausdrücklich als „Prüfung und Wirkung atomar, jeder Fehler mutationsfrei"
  dokumentiert (`core/src/vfs.rs:826`). Sie wäre dann eine Falschaussage — und sie
  steuert mehr als das Aufräumen: sie ist Eintrittsschranke für das dauerhafte
  Löschen in `execute_remote_delete` (`core/src/remote_delete.rs:364`) und eine von
  sieben Bedingungen in `supports_bound_restore_probe` (`core/src/vfs.rs:835`).

Heute ist das folgenlos, weil GUI (`src-tauri/src/file_commands.rs:126`) und CLI
(`cli/src/main.rs:1951`) lokale Einträge vor dem Löschweg abweisen. Das ist aber eine
heutige Wegführung, keine strukturelle Garantie. Wer die Flagge kippt, hinterlässt
eine Falle für den nächsten Umbau.

Ein sauberes A bräuchte deshalb eine eigene, schwächere Fähigkeit neben der starken —
womit A nicht mehr wesentlich billiger ist als C.

### Variante B — Reste bewusst stehen lassen und sichtbar machen

Kein neues Aufräumen. Stattdessen wird konsequent gemeldet, was liegen bleibt:
Recoverypfade in `temp_recovery_paths`, im Audit, im Queue-Eintrag und in beiden
GUI-Abschlussansichten, dazu eine Bedienung „Reste dieses Laufs entfernen".

- **Aufwand:** mittel, hauptsächlich Bilanz und Oberfläche.
- **Risiko:** null. Es wird nichts gelöscht, was nicht bewiesen ist.
- **Preis:** Nach jedem abgebrochenen Kopiervorgang bleibt eine sichtbare Temp-Datei
  im Zielordner liegen — bei einem abgebrochenen Ordnersync sind das potenziell
  hunderte. Und Symptom A bleibt: Eine vollständig gelungene Ersetzung meldet
  weiterhin „unklar". Das ist der schwerere Teil des Preises, weil es der
  Ausführungsklasse widerspricht und den Nutzer bei jedem Typkonflikt zu einer
  Handprüfung zwingt, die nichts finden wird.

### Variante C — privates Zwischenverzeichnis je Lauf

Doppeldecker legt im Zielbereich einmal je Lauf und Dateisystem ein Verzeichnis mit
Modus `0700` an, prüft nach dem Öffnen am Deskriptor Typ, Besitzer und Rechte — genau
wie der Papierkorb es heute tut — und benutzt es als Namensraum für alle
Zwischennamen:

1. **Temp-Dateien entstehen direkt darin** statt als Nachbar des Ziels. Damit kann
   kein fremder Nutzer den Namen mehr austauschen; das Aufräumen über den Pfad ist
   dort *durch Rechte* sicher, ohne atomare Kernelbedingung. Symptom B verschwindet
   an der Wurzel, weil es die gefährliche Namensraumfreigabe gar nicht mehr gibt.
2. **Ein Altziel wird exklusiv dorthin geparkt** statt auf einen Nachbarnamen. Erst
   *nach* dem Umhängen wird die Identität im privaten Verzeichnis geprüft — und dort
   ist diese Prüfung belastbar, weil niemand mehr eingreifen kann. Stimmt sie, darf
   die Sicherung nach erfolgreichem Commit über den Pfad entfernt werden: Symptom A
   verschwindet, der Fall meldet ehrlich Erfolg. Stimmt sie nicht, wurde ein fremdes
   Objekt geparkt — dann zurückhängen und Plan-Drift melden.

Wichtig für die Bewertung: Dieses Risiko besteht **heute schon**, weil auch das
heutige Park-Rename über den Pfad geht. C fügt es nicht hinzu, sondern macht es
erstmals erkennbar. 3. Der abschließende Commit bleibt ein Rename innerhalb desselben
Dateisystems und damit atomar; nur der Ausgangsordner ändert sich.

- **Aufwand:** groß, geschätzt 600 bis 900 Zeilen mit Tests über fünf Dateien.
  `ensure_private_directory` wandert aus `trash.rs` in `local_fs.rs` und wird
  wiederverwendet. Das `Vfs`-Trait bekommt einen schmalen, optionalen Zugang
  („liefere mir ein eigenes Zwischenverzeichnis nahe diesem Ziel") mit sicherem
  Default `None`; Remote-Provider behalten unverändert ihr heutiges Verhalten.
  Berührte Aufrufstellen: `create_neighbor_temp` (`core/src/atomic_file.rs:71`), der
  Hardlinkpfad in `core/src/sync/atomic.rs:510`, `park_existing_target`
  (`core/src/fileops.rs:2097`) und `core/src/fileops.rs:3635`.
- **Kein Fähigkeitsflag ändert sich.** `supports_remove_if_matches()` bleibt für
  `LocalVfs` `false`, weil die Aussage weiterhin stimmt. Die Sicherheit kommt aus dem
  Namensraum, nicht aus einer behaupteten Atomarität. Das ist der entscheidende
  Vorteil gegenüber A.
- **Restrisiko und offene Punkte, die vor der Umsetzung geprüft werden müssen:**
  - Ein Prozess *desselben* Nutzers kann weiterhin alles. Das ist keine Schwäche der
    Variante: Wer als derselbe Nutzer läuft, kann uns ohnehin per `ptrace` steuern.
    Gehört ausdrücklich in die Grenzbeschreibung.
  - Vererbte Metadaten. Unter Linux erben neue Dateien Default-ACLs vom Elternordner,
    unter macOS die vererbbaren ACL-Einträge. Das Zwischenverzeichnis erbt sie vom
    Zielordner und gibt sie weiter, die Kette sollte also stimmen — **das muss auf
    beiden Systemen gemessen werden, nicht angenommen.** Gleiches gilt für
    SELinux-Kontexte.
  - Ein nach einem Absturz liegengebliebenes `0700`-Verzeichnis in einem geteilten
    Ordner ist für dessen Eigentümer nicht aufräumbar. Doppeldecker läuft als der
    richtige Nutzer und muss es deshalb selbst beim nächsten Lauf unter demselben
    Zielbereich erkennen und anbieten — das ist genau der Bilanzteil aus Variante B
    und fällt nicht weg.
  - Zusätzliche Systemaufrufe: ein `mkdirat` und ein `rmdir` je Lauf und Dateisystem,
    nicht je Datei. Vernachlässigbar.

## Vergleich

| | A: Prüfung vor dem Löschen | B: Reste stehen lassen | C: privates Verzeichnis |
|---|---|---|---|
| Symptom A (falsches „unklar") | gelöst | bleibt | gelöst |
| Symptom B (Pfadlöschung) | Fenster verkleinert | gelöst (es wird nicht gelöscht) | gelöst |
| Fähigkeitsflagge | wird zur Falschaussage | unverändert | unverändert |
| sichtbare Reste im Normalbetrieb | keine | nach jedem Abbruch | keine |
| Aufwand | klein, aber ehrlich nur mit Zusatzfähigkeit | mittel | groß |
| neues Risiko | Restfenster bleibt | keines | Metadatenvererbung prüfen |

## Empfehlung

**Die ursprüngliche Empfehlung C ist durch die Gegenprobe unten blockiert.**
Die unabhängigen Bilanz- und Namenskorrekturen sind umgesetzt. Eine sichere
Weiterentwicklung von C braucht einen neuen, geprüften Vertrag für ACL-Vererbung
und einen gehaltenen privaten Namensraum; der bisherige Vorschlag reicht nicht.

Die ursprüngliche Begründung war, dass C beide Symptome ohne falsche
Fähigkeitsaussage lösen sollte und ein vorhandenes Verfahren wiederverwendet.
Die Gegenprobe widerlegt dessen hinreichende Absicherung für diesen neuen Zweck.

Gegen A spricht, dass es sein Ziel nur erreicht, indem es eine dokumentierte Zusage
über Atomarität falsch setzt — oder eine zweite, schwächere Fähigkeit einführt und
damit fast so teuer wird wie C. Gegen B allein spricht, dass eine belegte, gelungene
Wirkung weiter als unklar gemeldet würde.

Der Bilanzteil aus B ist **kein Alternativvorschlag, sondern in jedem Fall fällig**:
Auch mit C bleiben nach einem Absturz Reste liegen, und sie müssen auffindbar sein.

## Unabhängig von der Entscheidung umgesetzt (2026-09-30)

Diese beiden Punkte sind unabhängig von der Architekturwahl korrigiert:

1. Scheitert das Entfernen der geparkten Sicherung nach einer vollständig
   geschriebenen Ersetzung, gelangt ihr Pfad in `FileOperationActuals::temp_recovery_paths`.
   Das gilt für beide Typkonfliktrichtungen. Das Altziel bleibt erhalten;
   die offene lokale Aufräumgarantie wird weiter als `outcome_unclear` gemeldet.
2. `sibling_candidate` enthält zusätzlich acht Zufallsbytes aus der
   Betriebssystem-Zufallsquelle. Die Kürzung berücksichtigt das längere Suffix.
   Exklusives Anlegen bleibt erforderlich; das Löschrennen aus Symptom B bleibt offen.

## Gegenprobe vom 2026-09-30

Auf macOS wurde in einem markierten Wegwerfbaum ein Zielordner mit einer
vererbbaren `everyone`-ACL angelegt (`read,write,execute,delete,add_file,
add_subdirectory,delete_child,file_inherit,directory_inherit`). Ein darin mit
`mkdir(..., 0700)` angelegtes Zwischenverzeichnis zeigte zwar `drwx------`, erbte
aber weiterhin diese Zugriffsrechte. Die Prüfung von Besitzer und Modus aus dem
Papierkorbweg beweist damit allein keinen privaten Namensraum. Die Prüfung
benutzte synthetische Dateien; ein fremder Benutzerzugriff wurde nicht ausgeführt.

Eine direkt angelegte Datei und die über das Zwischenverzeichnis umbenannte Datei
trugen dieselbe geerbte ACL. Nach `chmod -N` am Zwischenverzeichnis trug die nächste
darüber erzeugte Zieldatei dagegen keine ACL mehr. Einfaches Entfernen der ACL
verliert also die geforderte Metadatenvererbung. Dies erfüllt das vorher vereinbarte
Abbruchkriterium; C wurde nicht implementiert.

Auf Linux wurde dieselbe Anlage mit einer POSIX-Default-ACL für einen zusätzlichen
Benutzer geprüft. `0700` setzte die Access-ACL-Maske des Zwischenverzeichnisses auf
`---`; die Default-ACL blieb erhalten und die über das Verzeichnis erzeugte Datei
hatte dieselbe ACL wie die direkt angelegte. Das Linux-Ergebnis hebt den
macOS-Ausschluss nicht auf. SELinux war auf dem Testhost nicht aktiv; eine
SELinux-Kontextgleichheit ist damit nicht belegt.

Zusätzlich muss eine neue Architektur das Zwischenverzeichnis per Deskriptor
halten: Schreibrecht im gemeinsamen Elternordner erlaubt dessen Umbenennung und
Austausch, auch wenn die Kinder durch `0700` geschützt sind. Ein späteres Öffnen
allein über den ursprünglichen Pfad erhält diese Bindung nicht.

## Überarbeitete Variante C2: freigegebener lokaler Laufkontext

C2 trennt den privaten Arbeitsbereich von der Metadatenvererbung am endgültigen
Ziel. Der Arbeitsbereich erbt keine fremden Zugriffsrechte. Die für neue Zielobjekte
vorgesehenen ACLs werden stattdessen aus dem gehaltenen tatsächlichen
Ziel-Elternverzeichnis abgeleitet und am neuen Objekt exakt zurückgelesen.
Explizite Quellenmetadaten folgen anschließend weiterhin dem gewählten
Metadatenprofil; Zielvererbung und Quellenübertragung sind getrennte Schritte.

Der Arbeitsbereich muss innerhalb des freigegebenen Roots und auf demselben Mount
liegen. Seine Anlage erfolgt unter einer bereits zuverlässig privaten,
descriptor-gebundenen Elternkette. Ein erst im gemeinsamen Ordner angelegtes und
anschließend ACL-bereinigtes Verzeichnis genügt nicht: Fremde könnten vorher einen
Deskriptor öffnen. Ohne sicheren Arbeitsbereich gibt es keine neue Cleanup-Garantie;
die praktische Dateioperation bleibt mit sichtbar erhaltenen Resten verfügbar.
Derzeit wird der gehaltene Root beziehungsweise Sandbox-Root als Anker verwendet;
ohne einen solchen Root muss das tatsächliche Ziel-Elternverzeichnis selbst privat
sein. Ein allgemein beschreibbarer Root bekommt keine neue Cleanup-Zusage.

Root, Arbeitsbereich und Ziel-Elternverzeichnisse bleiben als Deskriptoren gehalten.
Temp-Anlage, Übertragung, Metadaten, Parken, Commit und Cleanup dürfen nicht erneut
über ihre angezeigten Pfade aufgelöst werden. Die allgemeinen starken
VFS-Fähigkeiten bleiben unverändert; insbesondere bleibt lokales
`supports_remove_if_matches()` weiterhin `false`. Remote-Provider werden nicht
durch diese lokale Stagingabstraktion verändert.

Auch ein privater neuer Elternordner macht eine geparkte Altversion nicht automatisch
exklusiv: Fremde können vor dem Parken bereits Deskriptoren auf Unterverzeichnisse
geöffnet haben. Ohne zusätzlichen Nachweis werden alte Verzeichnissicherungen
deshalb erhalten. Ein belegter Zielcommit wird als erfolgreiche Ersetzung mit
getrenntem Cleanuphinweis und Recoverypfad bilanziert werden. Unklarer Commit,
Restore, Identität oder fehlende Dauerhaftigkeitsbestätigung bleiben dagegen
`outcome_unclear`.

Crash-Recovery verwendet ein privat gehaltenes, versionsgebundenes Intent-/Phasenjournal
vor Parken, Commit und Cleanup. Ein Recoveryindex im gegen fremdes Schreiben geschützten Queueordner bindet
Fensterjournal und Operation. Nach Neustart werden Anker, Mount und privater
Arbeitsbereich frisch gebunden. Sichtbare Pfade sind ausschließlich Hinweise:
Es gibt keine automatische Löschung oder Wiederholung alter Wirkungen. Unvollständige oder fremde Reste werden nicht
automatisch gelöscht; aktive Läufe verschiedener Fenster bleiben getrennt.

### Machbarkeitsproben vom 2026-09-30

Die Proben verwendeten eigenständige Prototypen und ausschließlich markierte
künstliche Dateibäume. Sie sind keine Verifikation der bestehenden Produktpfade.

- **macOS:** Nach Korrektur einer falschen Vererbungsregel stimmen 268 ACL-Vergleiche
  mit direkt vom Kernel angelegten Dateien, Ordnern und Enkelobjekten überein.
  Geprüft wurden Kombinationen der Vererbungsflags, Allow/Deny, geerbte und explizite
  Ausgangseinträge, Reihenfolge sowie das ACL-Flag `NO_INHERIT`. Ein allein mit
  `FILE_INHERIT` markierter Eintrag wird beim Verzeichniskind verworfen; ihn dort
  künstlich mit `INHERIT_ONLY` weiterzugeben war falsch. Modus und GID der direkten
  Datei-/Ordnervergleiche stimmen ebenfalls überein.
- **Fremder Benutzer auf macOS:** Öffnen und Anlegen im privaten Arbeitsbereich
  scheitern mit `EACCES`. Nach Umbenennung und Austausch seines sichtbaren Namens
  entfernt das Cleanup über den gehaltenen Deskriptor nur die eigene Tempdatei;
  der gleichnamige fremde Testbestand bleibt erhalten. Ein vor dem Parken geöffnetes
  Backup-Unterverzeichnis bleibt dagegen tatsächlich veränderbar.
- **Linux:** 60 Vergleiche für POSIX-Default-/Access-ACLs, Masken, Other-Rechte,
  Erstellmodi, GID, `setgid` und Enkelobjekte stimmen überein. Der Austausch des
  sichtbaren Arbeitsordners lenkt das Cleanup ebenfalls nicht um. SELinux war nicht
  aktiv; aktive Sicherheitskontexte sind weiterhin ungeprüft.

Diese Ergebnisse erlauben noch keine allgemeine Metadaten- oder Recoveryzusage.
Weitere Metadatenklassen, Hardlinks, Fehlerphasen, Crash-Zwischenstände,
Sandboxbindung und beide GUI-Abschlussansichten müssen in der Produktintegration
separat geprüft werden. Für SELinux-aktive Ziele wird ohne Kontextnachweis keine
neue C2-Garantie behauptet. Der lokale Laufkontext ist inzwischen integriert;
SELinux-aktive sowie unbekannte Rechte- oder Owner-Modelle verwenden den bisherigen
praktischen Weg mit erhaltener Restbilanz. Lokale Freigaben ohne belegte native
Unix-Rechte werden nicht als private Staging-Anker akzeptiert.

## Integrierte Prüfkriterien

Alle Punkte auf **macOS und Linux**, weil Rechte-, ACL- und Vererbungsverhalten sich
unterscheiden:

- Der Arbeitsbereich entsteht unter einer schon geschützten Elternkette, gehört
  dem aufrufenden Nutzer und hat wirksam private Modus-/ACL-Rechte. Ein erst
  nachträglich bereinigter öffentlicher Ordner wird nicht als privat akzeptiert.
- Ein Abbruch mitten im Transfer entfernt eigene Temps im privaten Laufkontext.
  Ohne privaten Kontext bleibt der Temp mit explizitem Recoverypfad erhalten;
  die alte Zieldatei bleibt in beiden Fällen unverändert.
- Frühe Quell-/Hardlinkfehler schließen den besitzenden Arbeitsbereich ebenfalls
  ab. Scheitert die sichere Bereinigung, bleiben Daten und Prüfpfad erhalten.
  `OutcomeUnclear` bewahrt das Intentjournal auch ohne verbliebene Tempdatei.
- Ein belegtes Typkonflikt-Ersetzen meldet **Erfolg** mit getrennter Bilanz einer
  gegebenenfalls erhaltenen Sicherung. Unbewiesene Commit-/Restorephasen bleiben
  `outcome_unclear`. Bei fremder Sicherungsidentität wird nichts gelöscht.
- Metadatenvererbung: Eine Datei, die über das Zwischenverzeichnis ins Ziel gelangt,
  trägt dieselben Datei-ACLs, Rechte und Gruppen wie eine direkt im Ziel angelegte.
  Native Metadaten einschließlich xattrs werden danach profilgebunden übertragen
  und zurückgelesen. SELinux-aktive Ziele bleiben ohne Kontextnachweis ausgeschlossen.
- Der Hardlinkpfad funktioniert über die Verzeichnisgrenze hinweg und bleibt an die
  Inode-Prüfung des Ankers gebunden.
- Absturzrest: Bei Queueoperationen wird ein zurückgelassenes Zwischenverzeichnis
  beim Öffnen derselben Queue über den privaten Recoveryindex erkannt, in der Bilanz
  genannt und nicht stillschweigend gelöscht. Direkte Core-Aufrufe ohne Queuebesitzer
  hinterlassen ein privates Journal, besitzen aber keinen automatischen Queueindex.
- Sandbox: Jeder Pfad im Zwischenverzeichnis unterliegt weiterhin `guard_path`.
- Remote-Provider bleiben unverändert — der Default des neuen Trait-Zugangs ist
  `None`, und die vorhandenen Provider-Tests müssen ohne Anpassung grün bleiben.

## Freigabe und Grenzen

Die ursprüngliche Variante C bleibt ausgeschlossen. C2 wurde nach den positiven
Machbarkeitsproben zur Umsetzung freigegeben. Die Freigabe ersetzt keine starke
allgemeine VFS-Fähigkeit: Der private Namensraum gilt nur im konkreten gehaltenen
Laufkontext. Verzeichnissicherungen werden ohne stärkeren Nachweis nie rekursiv
bereinigt. Bereits fremd geöffnete reguläre Dateien können weiterhin über deren
Deskriptor verändert werden; C2 behauptet keine exklusive Kontrolle ihres Inhalts.

Die integrierten Prüfungen umfassen Kernelvergleiche für Datei-ACL-Vererbung,
Eltern-/Staging-Swap, ACL-/Rechtedrift vor dem Commit, beide Typkonfliktrichtungen,
Abbruch mit und ohne privaten Kontext, native Metadaten, Sparse-Dateien und
Hardlinks. Echte Prozessabbrüche nach Temp-Anlage, Parken und Commit belegen den
Recoverypfad; aktive Fenster und Abbrüche während der Abschlussbereinigung bleiben
getrennt. Die GUI-Prüfung verwendet ausschließlich das aktuelle signierte,
notarisierte und gestapelte Bundle in einer markierten Sandbox.

Das Review vom 2026-10-02 ergänzt 21 gezielte C2-Gegenproben für Fehler- und
Abschlusswege. Der damalige Restbefund, dass spätere Teilaktionen bereits
gesammelte Wiederherstellungspfade verdrängen konnten, ist inzwischen behoben.
Die [Recoverybilanz](recovery-aggregation.md) beschreibt den aktuellen Vertrag
und die Gegenproben über mehrere Aktionen einschließlich Queue-Reopen.
