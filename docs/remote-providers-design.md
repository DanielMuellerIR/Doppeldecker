# Remote-Provider

Stand: 2026-10-23

## Produktmaßstab

Dieser Vertrag beschreibt sowohl den aktuellen Implementierungsstand als auch die
stärksten nachweisbaren Providerfähigkeiten. Er ist dem praktischen Produktvertrag
aus [`product-principles.md`](product-principles.md) untergeordnet: Eine fehlende
lokale Garantie darf nicht automatisch eine fachlich unterstützte Dateioperation
entfernen. Sie stuft die Wirkung von „abgesichert“ auf „Best Effort“ zurück, sofern
Root, Ziel, Umfang und möglicher Schaden weiterhin begrenzt und verständlich
bestätigt werden können.

Der bisherige Remote-Unterbau wurde aus dem strikten Realmodus für Sync und Mirror
entwickelt. Beim späteren Anschluss normaler Dateiaktionen wurden einige binäre
Sync-Capabilities direkt wiederverwendet. Das erklärt die heutigen Lücken, ist aber
keine dauerhafte Produktentscheidung.

## Aktuell angebotene Providerwirkungen

Die [Funktionsmatrix](function-audit.md) trennt normale Dateiaktionen von strikten
Sync-Zielverträgen. Der [Audit-Umsetzungsstand](usability-audit-2026-10-20.md#umsetzungsstand)
fasst den Abbau unnötiger Einschränkungen zusammen.

- SFTP v3 bietet Listing, Streaminginhalte und gewöhnliche Uploads, mkdir, Rename,
  Move, bestätigtes Ersetzen und dauerhaftes Löschen als Best Effort. Pfade werden
  frisch geprüft; eine gleichzeitige serverseitige Elternpfadumleitung bleibt
  ausdrücklich akzeptiertes Restrisiko. Serverhelpers sind nicht erforderlich.
  Striktes VFS, Sync und Quarantäne erhalten keine allgemeine Freigabe.
- FTP/FTPS und SMB bieten normale Dateiaktionen einschließlich Ersetzen und
  dauerhaftem Löschen ohne vorgeschaltete künstliche Schreib-/Rename-/Löschproben.
  Die tatsächliche Temp-/Ordner-/Renameoperation liefert ihren ersten
  Fähigkeitsnachweis. Fehlende Quarantäne wird als irreversible Wirkung angezeigt.
  Strikte Sync- und Restoreverträge werden dadurch nicht erfunden.
- WebDAV nutzt private bedingte Best-Effort-Verträge für Uploads, reguläres
  Ersetzen, mkdir, Rename und dauerhaftes Löschen. Getaggte `If`-Listen gelten
  alternativ; sie behaupten keine gemeinsame atomare Bindung sämtlicher Eltern.
  Ersetzen verwendet fertig übertragenen Tempinhalt und eine Altzielbedingung,
  Delete die verfügbare Objektbedingung. Vorfahren werden frisch geprüft;
  beobachtete Drift nach möglicher Wirkung bleibt ein unklarer Ausgang.
- FTPS, WebDAV und SMB können Einwegmodi in ein starkes lokales Ziel speisen.
  Remote-Ziel-Sync und bidirektionaler Rollback behalten ihre konkreten stärkeren
  Anforderungen. Normale Ersetzung ersetzt keinen solchen Vertrag.

Gewöhnliches Remote-Löschen plant die vollständige Auswahl und prüft jeden Record
vor seiner Einzelwirkung. Symlinks, Spezialdateien und unvollständige Listings
erteilen kein rekursives Löschrecht. Es gibt keinen mutierenden Blind-Retry oder
stillen Wechsel zwischen Quarantäne und Dauerlöschen. Mehr als 1000 Einträge oder
10 GiB verlangen zusätzlich eine exakte Löschphrase; normale Auswahlen bestätigen
Ziel, Umfang und Irreversibilität mit einer Entscheidung.

Standardsnapshots lesen Inhalte einmal remote mit Streaminghash und abschließenden
Attributchecks. Zusätzlicher vollständiger Readback ist optional. Interaktive
Downloads und lokale WebDAV-Quellen haben keine festen Datei-/Auswahl-/Baumgrößen-
oder Tiefengrenzen und keine pauschale 60-Sekunden-Vorbereitungsfrist. Lokale
Quellhandles werden pro übertragener Datei geöffnet; Transferhash und frische
Versionsprüfung ersetzen vollständiges Vorhashen.

## Verbindungen und konkrete Providerverträge

Die öffentlichen strikten Providerfähigkeiten bleiben von den privaten normalen
Dateiaktionssichten getrennt. Die nachfolgenden älteren Implementierungsschritte
beschreiben teilweise frühere Freigaben oder Absagen; maßgeblich für das heutige
Angebot sind die obige Übersicht und die Funktionsmatrix.

- [SFTP-Inhalte und Remote-Bearbeitung](sftp-contents.md)
- [SFTP-Schreibaktionen](sftp-writes.md)
- [Additive SMB-Downloads](smb-contents.md)
- [Additive WebDAV-Downloads](webdav-contents.md)
- [Additive FTP-/FTPS-Downloads](ftp-contents.md)
- [WebDAV-Uploads und Ersetzen](webdav-upload.md)
- [WebDAV-Mutationsvertrag](webdav-mutation-boundary.md)
- [WebDAV-Umbenennung](webdav-rename.md)

Jede Profildatei führt seit W2b einen pro Protokoll monotonen Revisionszähler und
behält entfernte Profil-IDs als geheimnisfreie Tombstones. Listen, aktive
Verbindungen, Pane-Requests und Queueeinträge tragen die konkrete Revision. Ein
Handshake darf seinen Cacheeintrag erst nach erneutem Lesen desselben Profilstands
veröffentlichen; Plan, Konfliktliste und nicht serialisierbares Permit werden vor
ihrer Veröffentlichung und Verwendung erneut geprüft. Save oder Remove invalidiert
nur abhängige Vorbereitungen und meldet Protokoll, Profil-ID, Revision und
Änderungsart strukturiert über `connections-updated`.

FTP/FTPS ist als zweite Providerfamilie umgesetzt. Das Profil wählt ausdrücklich
Klartext-FTP oder explizites TLS: FTPS verwendet `AUTH TLS` und `PROT P` für private
Kontroll- und Datenkanäle; FTP sendet auch Anmeldung und Dateiinhalte unverschlüsselt.
Neue öffentliche FTPS-Profile prüfen Zertifikatskette, Gültigkeit und Servernamen
ohne dauerhaften Profilpin. Eine reguläre Erneuerung eines dafür gültigen Zertifikats
sperrt neue Verbindungen nicht. Öffentliche WebPKI mit zusätzlichem Pin, ein außerhalb
Doppeldeckers geprüfter privater Pin und ein bewusst ungeprüfter Zertifikatspin
bleiben ausdrückliche getrennte Optionen. Der letzte verschlüsselt und erkennt
spätere Zertifikatswechsel, behauptet aber keine bestätigte Serveridentität.
Der Editor kann bestehende Profile nach öffentlicher TLS-Probe ohne Anmeldung
umstellen oder einen bewusst gewählten festen Pin ausdrücklich erneuern.

Root, Transportmodus, optionales Zertifikat, FEAT- und der längengebundene
Rohbyte-Digest des FTP-Willkommenstexts sowie
Sitzungsgeneration bilden die jeweils verfügbare Reconnect-Identität. `MLSD` wird
unabhängig von der Transportstufe bytebegrenzt, strikt als UTF-8 und mit Pflichtfakten
für Typ, Größe und Änderungszeit gelesen; doppelte Namen/Fakten stoppen fail-closed.
Sind diese Fakten laut `FEAT` verfügbar, aber nicht standardmäßig aktiv, fordert der
Client sie vorher mit `OPTS MLST` an. Verzeichnisse werden zuerst per `CWD` gewählt
und anschließend mit dem standardisierten argumentlosen `MLSD` gelesen, damit der
Server keine nichtportable Pfadsyntax unterstützen muss.

Ein idempotentes `MKD`, das für einen bereits vorhandenen Ordner erneut Erfolg meldet,
bleibt für das reine Ordneranlegen zulässig. Doppeldecker prüft den vollständigen
Elternbestand vor dem Aufruf und liest den entstandenen Typ danach zurück. Eine späte
gleichnamige Verzeichniskollision ersetzt dadurch weder Dateien noch bestehenden
Ordnerinhalt.

Alle künstlichen Schreib-, Rename-, Lösch- und Restore-Proben entstehen unter dem
konkret gebundenen Pane-/CLI-Root. Der kanonische Profilroot bindet weiterhin die
Provideridentität, darf aber nie als Ausweichort für Probeeinträge dienen.

FTPS liest nach jeder Capability-Probe den vollständigen Bestand des gewählten
Roots und des kanonischen Profilroots zurück. Das gilt auch nach Probe-, MKD- und
Cleanup-Fehlern. Beide Prüfungen laufen unabhängig über frische, identitätsgeprüfte
Lesesitzungen: Eine fehlgeschlagene FTP-Datenverbindung kann den ursprünglichen
Kontrollkanal versetzen. Eine Mutation wird dabei niemals wiederholt. Der konkrete
Probename darf in keiner Wurzel verbleiben; fremde oder falsch platzierte Reste
bleiben erhalten und melden `outcome_unclear`. Listingfehler bleiben Fehler und
verhindern die neue Fähigkeitsvergabe.

Der [STOU-Transportvertrag](ftps-stou-transport.md) hält den Datenkanal bis zur
Kontrollantwort und verhindert den unter Linux belegten Reset durch ungelesene
TLS-Sessiontickets. Proben-Cleanup prüft bekannte Namen, vollständigen Inhalt und
leere eigene Ordner; Fehler entziehen alte betroffene Fähigkeiten und erhalten
Recoverypfade. Jede mutierende FTPS-Vorbereitung bindet vor MKD/STOU einen dauerhaften privaten
Intent an das verursachende Fensterjournal. Kopierziel und Move-Quelle prüfen nur
ihre benötigten Wirkungen. Ein noch unbekannter STOU-Name und später bestätigte
Namen bleiben getrennt sichtbar; bestätigte Namen stehen vor dem ersten Inhaltsbyte
im Index. Die lokale Recovery verbindet keinen Server und erteilt kein Lösch- oder
Wiederholungsrecht. [Vertrag und Grenzen](ftps-stou-transport.md#wirkungsbezogene-vorbereitung-und-dauerhafte-intents).

Beleg vom 2026-10-06: `ftps_e2e` prüft den TLS-/PWD-/Reconnect-Vertrag mit einem
lokalen Server und alle vier Proben im gewählten Unterordner. Ein zusätzlicher
Loopback-FTP-Gegenlauf beobachtet die tatsächlich gelisteten Roots bei Erfolg,
abgelehnter/teilweise ausgeführter MKD, fehlgeschlagenem Auswahl-/Profilroot-Listing
und absichtlich falsch platziertem Probenrest. Quelldaten bleiben erhalten.


SMB verlangt ebenfalls nach Erfolgs-, Create- und Cleanup-Fehlern vollständige
Listings des gewählten Roots und des virtuellen Profilroots `/` (gebunden an den
konfigurierten Root, nicht an den gesamten Share). Beide werden unabhängig geprüft
und müssen derselben Sitzungsgeneration angehören. Probenreste und Listingfehler
sperren die neue Fähigkeitsvergabe. Der verschlüsselte Samba-Gegenlauf vom
2026-10-06 prüft einen schreibbaren Unterordner bei verweigertem Profilroot-Listing,
ein verweigertes Create und alle vier Proben unter dem gewählten Unterordner.

Bekannte Einschränkung der FTPS-Proben (bewusst so entschieden, 2026-08-03): Jede der
vier FTPS-Proben — Ordner anlegen, Umbenennen, AddOnly/STOU und die
interne Löschprimitive — läuft auf einer eigenen frischen Verbindung und hinterlegt
ihr Ergebnis im gehaltenen
Sitzungszustand. Die spätere Mutation, etwa der STOU-Commit eines Uploads, öffnet
wieder eine eigene frische Verbindung. Geprüft wird dabei nur, dass beide dieselbe
`FtpsServerIdentity` melden; die bindet Host, Port, Zertifikat, kanonischen Root,
FEAT-Antwort und Banner, also den Server, wie er sich meldet — nicht die konkrete
Adresse oder die einzelne Verbindung. Steht hinter einem Hostnamen ein Verbund
mehrerer gleich konfigurierter Knoten, kann deshalb Knoten A das No-Replace-Verhalten
belegen, während Knoten B beim Commit ersetzt. Der Beweis gilt also je Server, nicht
je Knoten. Die beiden möglichen Abhilfen — eine erneute Probe je Mutationssitzung
oder Probe und Mutation an genau dieselbe Verbindung zu binden — kosten zusätzliche
Umläufe pro Upload beziehungsweise einen Umbau des Sitzungsmodells; sie lohnen erst,
wenn Multi-Node-FTPS real eingesetzt wird. Für Einzelserver, den einzigen bisher
belegten Fall, ändert die Einschränkung nichts.

FTPS→lokal unterstützt AddOnly, Update und Mirror über den lokalen Quarantäne- und
Permitvertrag. Unbeaufsichtigtes Lokal→FTPS-AddOnly wird nur freigeschaltet, wenn eine
künstliche Probe `STOU`, unabhängigen Inhalts-Readback und ein RNTO beweist, das
vorhandene Ziele nicht ersetzt. Bei überschreibendem RNTO bleiben Update/Mirror sowie
unbeaufsichtigtes AddOnly gesperrt; interaktive neue Dateien und bestätigte
Einzeldatei-Ersetzungen laufen über die getrennten Best-Effort-Klassen. Profile,
Remote-Panes, Reconnect,
Direktlauf und persistente Queue sind angebunden. Ein vollständig lokaler TLS-E2E-Test
belegt Pin, MLSD, Stream, Reconnect, Download-Realmodus und Queue; sein überschreibendes
RNTO wird erkannt, alle künstlichen Probeeinträge werden entfernt und Bestandsdaten
bleiben unverändert.

Die interaktive Umbenennung ist seit 2026-07-29 wirkungsspezifisch eingestuft: Die
zweistufige RNFR/RNTO-Probe belegt das gewöhnliche Rename samt Readback
(`best_effort_rename`). No-Replace wird zusätzlich für den tatsächlichen Typ
und Zielroot gebunden: Die flache Ordnerprobe belegt nur Ordner, die Inhaltsprobe
nur Dateien. Das allgemeine Datei-Commit-Feld `atomic_noreplace_rename` erhält
keine Freigabe aus einem Ordnerbeleg. Ein
Server mit überschreibendem `RNTO` verliert damit nicht mehr die gesamte
Umbenennung: F2 und das CLI laufen dort als gekennzeichneter Best-Effort-Weg mit
frischem Elternlisting, sichtbarer Kollision statt stillem Ersetzen, genau einer
Mutation, anschließendem Listing-Readback und eindeutigen Resultaten
(erfolgreich, unverändert, kollidiert, Ausgang unklar mit stabilem Code
`outcome_unclear`). Nach einem Verbindungsverlust klärt höchstens ein erneutes,
identitätsgeprüftes Listing den Zustand; die Mutation wird nie wiederholt. Der
unbeaufsichtigte Sync-Commit und der Move-Schnellweg im selben Namespace verlangen
weiterhin den No-Replace-Beweis.

Fehlt dieser Beweis für einen normalen FTPS-Move, prüfen GUI, CLI und Queue
denselben vorhandenen Copy-/Quellentfernungsweg als Best Effort. Beide schwächeren
Wirkungen werden getrennt angezeigt und bestätigt; die Quelle wird erst nach
vollständiger Zielkopie und frischem Readback entfernt. Der abgesicherte reine
Rename-Weg bindet dagegen alle ausgewählten Typen am RNTO-Zielroot und wechselt
bei einem späteren Fehler nicht unbestätigt zum Kopieren und Entfernen.
Datei- und Ordnerbelege, neuere Gegenbelege und laufende Datenströme behalten ihre
eigene Operationssicht, Sitzungsgeneration und Rootbindung.

Reichweite von „abgesichert" (geklärt 2026-08-03): Beide Ausführungsklassen sagen
ausschließlich etwas über das ZIEL des Umbenennens aus — ob eine späte gleichnamige
Kollision einen vorhandenen Eintrag ersetzen könnte. Über die QUELLE sagen sie
nichts: Sie ist in beiden Klassen und bei allen Providern über ihren Pfad gebunden.
Wird zwischen dem frischen Elternlisting und der Mutation ein gleichnamiger Eintrag
ausgetauscht, benennt der Vorgang den neuen Eintrag um und meldet Erfolg. Diese
klassenunabhängige Einschränkung führen `ProviderRenameCapabilities` und
`RenameReport` deshalb ausdrücklich mit (`SOURCE_PATH_BINDING_LIMITATION` in
`core/src/remote_rename.rs`), sobald überhaupt eine Ausführungsklasse verfügbar ist;
die klassenspezifische Einschränkung steht davor, weil Oberflächen nur den ersten
Eintrag anzeigen.

Ein Rename mit Token-Vorbedingung gibt das Vfs-Modell nicht her, deshalb wird die
Einschränkung ausgewiesen statt scheinbar behoben: SFTP und FTP/FTPS liefern zu
einem Listing-Eintrag überhaupt keinen `provider_token`, der SMB-Token enthält mit
der `ChangeTime` einen Wert, den das Umbenennen selbst ändert, und nur WebDAV kennt
mit `If-Match` eine bedingte Operation — die `rename_noreplace` dort auch nutzt,
allerdings mit einer ETag, die es unmittelbar vorher selbst liest. Das verkleinert
das Fenster für WebDAV, schließt es aber ebenfalls nicht. Der Quell-Austausch
betrifft genau einen umbenannten Eintrag. In der
Best-Effort-Klasse kommt das gesondert ausgewiesene Zielkollisionsfenster hinzu:
Ein inzwischen entstandenes Ziel kann ersetzt werden. SFTP bietet dafür
inzwischen die getrennte gewöhnliche Best-Effort-Klasse; beobachtete Kollisionen
stoppen vor dem Rename.

Neue Dateien folgen seit demselben Schnitt derselben Abstufung: Die zweistufige
AddOnly-Probe trennt STOU-Eindeutigkeit, unabhängigen Inhalts-Readback und das
gewöhnliche Rename (`best_effort_file_commit`) vom vollständigen
AddOnly-/No-Replace-Beweis. Interaktives F5/Upload auf einen Server mit
überschreibendem `RNTO` läuft damit als sichtbarer Best-Effort-Commit: Temp per
STOU (der interne Wechsel vom STOU-Namen auf den Doppeldecker-Tempnamen prüft
den Namen vorab als frei), Hash-Readback, frisches vollständiges Elternlisting
mit konservativer Namensäquivalenz, genau ein Rename an den Zielnamen und ein
Listing-Readback samt Größenabgleich; eine bereits sichtbare oder serverseitig
abgelehnte Kollision stoppt sichtbar. Zwischen Bestandsprüfung und Rename bleibt
in dieser Klasse ein kleines, nicht atomar schließbares Fenster, in dem ein exakt
dann entstehendes Ziel ersetzt werden kann (maximaler Schaden: genau ein Eintrag —
siehe `best_effort_file_commit` in `core/src/remote.rs`). Ein
ungeklärter Ausgang trägt `outcome_unclear`, und die Zustandsklärung nach einem
Verbindungsabbruch nutzt höchstens einen identitätsgeprüften Neuaufbau
(`Vfs::reclaim_verification_session`) für genau ein Listing. Die Commit-Klasse
deklariert der Provider selbst
(`Vfs::file_commit_class`), sodass Vorschau und Ausführung dieselbe Quelle lesen;
die Queue bindet zusätzlich die zur Vorschau gezeigte Klasse und pausiert
sichtbar, falls ein Ziel erst zwischen Vorschau und Claim degradiert.
Unbeaufsichtigte Sync-Modi (AddOnly/Update/Mirror) bleiben unverändert an
`permits_add_only()` und den weiteren atomaren Beweisen gebunden. Ersetzen und
Sync werden nach demselben Prinzip getrennt bewertet.

Bestätigtes Ersetzen ist für FTPS seit 2026-08-05 als eigene
Best-Effort-Wirkung umgesetzt. Die künstliche Probe ersetzt genau eine vorhandene
Datei und liest den neuen Inhalt vollständig zurück; erst dann setzt die Sitzung
`best_effort_file_replace`. FTPS verwendet das bereits als überschreibend erkannte
RNFR/RNTO. Die frühere WebDAV-Probenfreigabe ist seit AP13/AP13R aufgehoben;
der allgemeine Ersetzungsweg bleibt passiv gesperrt. Der FTPS-Lauf bindet
unmittelbar vorher das vollständige Elternlisting, akzeptiert ausschließlich
reguläre Datei gegen reguläre Datei, sendet genau eine Mutation und prüft danach
Temp-Verschwinden, Endname, Typ, Größe und BLAKE3-Inhalt. Die Altversion wird nicht
quarantänisiert und ist unwiederbringlich; GUI und CLI benennen diese Wirkung vor
der Bestätigung ausdrücklich. Ein Verbindungsabbruch erlaubt nur einen
identitätsgeprüften, lesenden Reconnect zur Zustandsklärung, nie einen Retry.
Ordner-Merge bleibt möglich, Typkonflikte, Sync und Mirror nutzen diesen Weg nicht.

WebDAV ist als dritter Provider umgesetzt. Profile binden eine kanonische HTTPS-Origin,
ein Basic-Auth-Konto, Collection-Root, Zertifikat-Pin sowie ausschließlich eine
Schlüsselbundreferenz für das Passwort.
Der TLS-Modus verlangt wahlweise öffentliche WebPKI plus Pin oder ein ausdrücklich
extern geprüftes privates Zertifikat plus Pin. `OPTIONS`, DAV- und der
domänenseparierte, längengebundene Rohbyte-Digest aller `Server`-Header,
kanonischer Root und Sitzungsgeneration bilden die Reconnect-Identität.

Jede Collection wird mit genau einem begrenzten `Depth: 1`-`PROPFIND` gelesen. Der
eigene Parser prüft den DAV-Namensraum, XML-Struktur, jede `propstat`-Statuszeile,
Pflichtmetadaten, direkte Kindbeziehung, eindeutige UTF-8-Namen und starke Datei-ETags;
DTD, unbekannte Entitäten, fremde Origins, Teilfehler und schwache/fehlende ETags
stoppen fail-closed. ETags fließen pro Eintrag und als Listingtoken in den
Realmodus-Proof ein. WebDAV-Quellen erzwingen zusätzlich `ContentHash`; GET verwendet
`If-Match`, sodass Drift auch zwischen Listing und Stream nicht committed wird.

WebDAV→lokal unterstützt AddOnly, Update und Mirror über lokale Quarantäne.
Strikter Sync in ein WebDAV-Ziel bleibt seit AP13R ohne gemeinsame Root-/Elternbindung
passiv gesperrt. Die früheren primitiven PUT/MOVE-/Collection-Proben sind entfernt;
sie konnten die benötigte Garantie nicht beweisen. Normale additive Uploads besitzen
ihren getrennten Best-Effort-Vertrag. Profil, Reconnect, lesende Richtung und Queue
bleiben angebunden; Gegenläufe erhalten vorhandene Inhalte und Recoverypfade.

SMB ist als vierter Provider umgesetzt. Eine reine Rust-Implementierung verbindet
ohne System-Mount oder `libsmbclient`. Profile binden Host, Port, Server-GUID, Share,
Konto, normalisierten Root und nur eine Schlüsselbundreferenz. Der Verbindungsaufbau
verlangt SMB 3.1.1 samt Preauth-Integrität, aktiver Signatur und Verschlüsselung;
bereits `TREE_CONNECT` läuft verschlüsselt. DFS bleibt bis zu einer
referral-gebundenen Identität fail-closed.

`FILE_ID_BOTH_DIRECTORY_INFORMATION` wird begrenzt und bis
`STATUS_NO_MORE_FILES` gepagt. UTF-16-Namen, File-IDs, Change-Time, doppelte Namen,
Recordgrenzen und Reparse-Points werden konservativ geprüft. Schreibfähigkeit wird
nur nach exklusivem Create, Datei-FLUSH, Inhalts-Readback, No-Replace- und Replace-
Rename sowie Platzabfrage freigegeben. Reconnect bindet dieselbe Server-/Share-/Konto-
Identität und setzt alle verhaltensbasierten Fähigkeiten zurück.

SMB→lokal unterstützt AddOnly, Update und Mirror. Lokal→SMB unterstützt AddOnly;
Update/Mirror bleiben vor einer mutierenden Restore-Probe gesperrt, weil SMB den
vollständigen identitätsgebundenen Vertrag nicht implementiert. Ein erfolgreicher
Verzeichnis-FLUSH allein würde diese Sperre nicht beheben. Profile, Remote-Panes,
lesende Quarantäneverwaltung und persistente Queue sind angebunden. Der isolierte
Samba-E2E prüft Verschlüsselung, 1.500 gepagte Einträge, Streams, Reconnect,
Kollision, AddOnly-Upload, die mutationsfreie Update-/Mirror-Sperre, Download,
Queue und bidirektionale Wirkungen ausschließlich auf das lokale Ziel.
`tools/run-smb-e2e.sh` manifestiert dafür Provideridentität, Testshare,
Eigentumskennung und geplante Einträge; ein erfolgreicher Lauf entfernt seinen
eigenen `/tmp`-Root, ein Fehler lässt exakt diesen Recoverypfad zur Analyse stehen.

Gewöhnliche additive Kopien lokaler Quellen nach SMB lassen vollständige
mehrdeutige Case-/NFC-Namensgruppen in direkter Auswahl und in ausgewählten Bäumen
aus. Unabhängige Dateien und Ordner bleiben übertragbar; ausgelassene Ordner
werden nicht betreten. Die bestehende lokale Quellenbindung hält auch ausgelassene
Mitglieder und prüft sie vor weiteren Wirkungen. Nach Queue-Neustart verlangt ein
geänderter Baum eine neue Umfangsbestätigung. Der SMB-Zielvertrag einschließlich
No-Replace und Readback bleibt erhalten. Move, Ersetzen und strikter Sync verwenden
weiter ihre bisherigen Quellenverträge.

Die Provider-Matrix ist seit 2026-07-13 GUI-frei verallgemeinert. Ein nicht
serialisierbarer `ProviderEndpoint` kann jede lokale oder bereits verbundene
Remote-Seite als Quelle beziehungsweise Ziel halten. Die Vorbereitung leitet
AddOnly, Update und Mirror ausschließlich aus der konkreten Zielprobe ab, bindet den
Capability-Digest zusätzlich an Vorschau und Permit und lehnt gleiche oder
verschachtelte Roots im selben Provider-Namespace ab. Die bisherigen acht
Local↔Remote-Funktionen sind nur noch kompatible Wrapper. Ein echter isolierter
WebDAV→WebDAV-Lauf streamt Collection→Collection ohne lokalen Spool; ein Reconnect
zwischen Vorschau und Bestätigung stoppt ohne Zielmutation.

Seit 2026-07-24 verwenden auch die normalen Dateimanager-Aktionen diesen Endpunkt:
Ordner anlegen, Mehrfachauswahl per F5/F6, rekursive Local↔Remote- und
Remote↔Remote-Übertragung, F2-Rename, Remote-Favoriten und schreibgeschütztes Öffnen
einer lokalen Momentaufnahme. F5/F6 laufen durch die persistente Queue; Quell- und
Zielzustand, Konflikte, Fähigkeiten und Reconnect werden vor dem Claim erneut
gebunden. Ordner-`Skip` führt vorhandene Ordner zusammen, überspringt aber nur
kollidierende Blätter. SFTP trennt die interaktive
[Leseklasse](sftp-contents.md) von gewöhnlichen [Schreibaktionen](sftp-writes.md).
FTPS, WebDAV und SMB liefern gewöhnliches reguläres Ersetzen als sichtbar
irreversible Best-Effort-Wirkung; die Queue bindet die angezeigte Klasse.

Der neue gewöhnliche SFTP-Upload benötigt `fsync@openssh.com` nicht als
Eintrittsschranke; fehlt die Erweiterung, bleibt die Dauerhaftigkeit sichtbar
schwächer. Strikte VFS-/Sync-Zugriffe werden daraus nicht freigegeben. Weder fsync
noch Inhaltsreadback beseitigen den ausdrücklich akzeptierten Elternpfadwettlauf.

Das Öffnen einer Remote-Datei streamt in einen privaten lokalen Snapshot. Normales
Öffnen erhält das Originalformat; passives Ansehen ist separat wählbar. Die
Remote-Bearbeitung hält Originalbeleg und Arbeitskopie fenstereigen und reiht ein
explizites Zurückspeichern über die normale Provider-Dateiqueue ein. Ein frischer
Originalvergleich und gebundene Konfliktbelege verhindern das stille Ersetzen
einer seit dem Öffnen veränderten Version. Die Liste gilt bis zum Fensterende
oder Neustart; lokale Arbeitskopien bleiben privat erhalten.

Löschen besitzt einen eigenen rekursiven `RemoteDeletePlan` mit Umfang, frischem
Vergleich und Einzelreadback. Alle vier Remoteprovider besitzen eine normale
Best-Effort-Dauerlöschklasse. Sie behauptet weder Quarantäne noch atomare Bindung
aller Eltern. Fehlerhafte oder unvollständige Listings sind keine leeren Bäume.

## Historische bidirektionale Implementierungsstufen

Die folgenden Stufen beschreiben die frühere Entwicklung und deren damalige
Gegenläufe. Die damaligen SFTP-Freigaben sind durch die heutige Namespace-Sperre
aufgehoben; SMB-Zielprojektionen bleiben ohne gebundenen Restore-Proof gesperrt.
Die aktuelle Matrix am Anfang dieses Dokuments ist maßgeblich.

Die bidirektionale Stufe 6a verwendete dieselben bereits verbundenen Endpunkte für
vollständige Doppel-Snapshots. Dauerhafte Paarbindungen enthalten nur Kontodigest,
öffentlichen Digest der stabilen Serveridentität und konkreten virtuellen Root;
Sitzungsgeneration, SMB-Tree-ID, Handles und Secrets bleiben draußen. Reconnect mit
derselben Identität erhält die Bindung, ein anderes Konto, Zertifikat/Hostkey,
Server-GUID oder Root ändert sie. Die reine Providerplanung schreibt weder Baseline
noch Laufjournal und liefert bis zur providerweisen Quarantäne-/Recovery-Freigabe
immer einen technischen Ausführungsblocker.

Stufe 6b gibt additive Local↔SFTP-Projektionen frei. Weil auch eine neue Datei nach
einem späteren Teilfehler rückwärts entfernt werden können muss, wird ein SFTP-Ziel
nicht mit dem schwächeren AddOnly-Permit gestartet: Es muss vorab den Update-, Remote-
Quarantäne- und Restore-Probenvertrag bestehen. Jeder Teilcommit wird im gemeinsamen
Laufjournal markiert; Resume verbindet frisch und prüft die dauerhafte Bindung sowie
den exakten Projektionspräfix, Rollback verschiebt neue Versionen per No-Replace in
eine eigene manifestierte Quarantäne.

Stufe 6c gibt zusätzlich reguläre Dateiersetzungen in beide Richtungen frei. Die
explizite Gewinnerentscheidung wird weiterhin mit Mirror-Plansemantik von mtime
entkoppelt; enthält der konkrete Plan keine Entfernung, genügt technisch der geprüfte
Update-Vertrag. Die 100-Ersetzungsgrenze, Backup-Bestätigung, Quarantäne und Restore-
Probe bleiben zwingend. Verzeichnisersetzungen, Löschungen und Typwechsel bleiben
gesperrt. Der opt-in SFTP-E2E belegt Additionen, Ersetzungen und den manifestierten
Rollback nach einem künstlichen späten Ziel-Drift gegen einen Wegwerfroot auf dem
Linux-Testrechner.

Stufe 6d gibt Löschpropagierung ausschließlich von SFTP in ein lokales Ziel frei. Die
Remote-Seite liefert dabei den vollständig gebundenen Quellsnapshot; sämtliche
Mirror-Gates, der Zweitscan, die Quarantäne und dauerhafte Namespace-Barrieren liegen
am lokalen Ziel. Die Gegenrichtung bleibt ohne portables SFTP-Verzeichnis-`fsync`
fail-closed. Der echte E2E belegt den sicheren Ablauf leere Baseline → Additionen →
eine Remote-Löschung bei 24 Zieleinträgen; ein eigener Gegenfall belegt das harte Gate
für die umgekehrte Richtung.

Stufe 6e verallgemeinert Vorbereitung, Resume und Rollback auf die gesamte
Provider-Matrix. Jede Wirkung wird aus der konkreten, frisch geprüften Ziel-Capability
abgeleitet: Additionen benötigen sicheren Copy-Commit und rückwärts ausführbare
Quarantäne, Dateiersetzungen zusätzlich den Update-/Restore-Vertrag, Verzeichnis-
ersetzungen und Löschungen zusätzlich starke Namespace-Dauerhaftigkeit. Die lokalen
TLS-/HTTPS-E2E-Server belegen FTPS-/WebDAV-Quellrichtungen; eine Zielrichtung wird
nicht mehr pauschal nach Providername, sondern ausschließlich durch diese Proben
freigegeben oder gesperrt. Weil FTPS, WebDAV und
das SMB-Profil ohne POSIX-Erweiterungen keine POSIX-Modi lesen und schreiben,
normalisiert der Paarvertrag diese Klasse auf beiden Seiten ausdrücklich als nicht
gebunden; Inhalt, Typ und alle übrigen darstellbaren Klassen bleiben digestgebunden.

SMB darf als Ziel Additionen und Ersetzungen erst nach Update-/Quarantäneprobe nutzen;
Löschungen verlangen zusätzlich den echten Mirror-Vertrag samt Verzeichnis-FLUSH.
Die providerneutrale Zustandsmaschine ist für beide Richtungen künstlich belegt. Der
erweiterte echte Samba-E2E bestand am 2026-07-13 auf dem Linux-Testhost: SMB 3.1.1, Signing,
Verschlüsselung, Paging, Reconnect, Upload/Download, Quarantäne, Queue und beide
capability-gesteuerten bidirektionalen Richtungen sind belegt. Ohne erfolgreichen
Verzeichnis-FLUSH bleiben Mirror-Entfernungen in Richtung SMB weiterhin gesperrt.

Stufe 6f integriert Remote-Paare in Tauri, Operations-Queue und Frontend. Queue-
Queue-Journal v8 speichert pro Seite eine lokale Root-Referenz oder die bestehende
öffentliche Providerkonfiguration mit Schlüsselbund-/Agent-Referenz. Vor Vorschau,
Queue-Claim, Resume und Rollback werden Provider frisch verbunden und alle Digests
erneut geprüft. Der Mock-UI-Gegenlauf belegt lokale und SFTP-Paare, providerbezogene
Queue-Beschriftung und eine leere Browserfehlerkonsole.

Für Einwegtransfers zwischen zwei Remote-Panes verwendet `ProviderSync` seit
2026-07-13 dieselben geheimnisfreien Endpunkt-DTOs. Queue-Journal v8 bindet beide
Endpunkte, Optionen und den erwarteten Plan; jeder Claim verbindet beide Seiten neu
und verlangt danach eine neue Einmalphrase. Es gibt keinen protokollspezifischen
Transfer-Fallback und keine persistierte Sitzung.

Stufe 6g gibt atomare Typwechsel ausschließlich aus SFTP-, FTPS- oder WebDAV-Quellen
in ein lokales Ziel frei. Der vollständige Gewinner-Unterbaum wird über den generischen
VFS-Pfad gestaged, anschließend lokal descriptor-relativ atomar getauscht und der
Verlierer journalisiert quarantänisiert. Alle Remote-Zielrichtungen bleiben vor dem
Laufjournal gesperrt. Echte E2E aller drei Provider belegen Datei→Verzeichnis samt
Kindinhalt. Verzeichnisgrößen sind zwischen Protokollen nicht portabel und werden
daher bei semantischer Gleichheit ignoriert; Typ und alle rekursiven Einträge bleiben
weiterhin im Snapshot-Digest und im finalen Doppel-Scan gebunden.

Stufe 6h belegt den Recovery-Vertrag direkt hinter diesem atomaren Commit. Frisch
verbundene, identitätsgleiche Endpunkte können den beobachteten Projektionspräfix
entweder bis zu zwei stabilen Abschlussscans und dem Baseline-Commit fortsetzen oder
Stagingkopie und Typwechsel in umgekehrter Reihenfolge vollständig zurückrollen. Die
alte Baseline bleibt bis dahin unverändert. Ein echter SFTP-E2E belegt außerdem eine
Verzeichnismodusänderung von SFTP nach lokal; dieselbe Wirkung in Richtung SFTP bleibt
ohne portables Verzeichnis-`fsync` fail-closed.

## Gemeinsamer Vertrag

SFTP, FTP/FTPS, WebDAV und SMB implementieren dasselbe `Vfs` wie das lokale Dateisystem.
Ein Provider darf eine lokale Garantie nicht vortäuschen: fehlendes exklusives Create,
unvollständiges Listing, instabile Identität oder nicht atomarer Rename werden als
Fähigkeit gemeldet. Der Aufrufer wählt daraus die stärkste Ausführungsklasse. Eine
fehlende Einzelgarantie sperrt nur dann vollständig, wenn sich die konkrete Wirkung
nicht mehr begrenzen oder sinnvoll bestätigen lässt.

Jeder Lauf bindet:

- die für den gewählten Transport verfügbare Server-/Share-/Origin-Identität,
- Root-Identität und normalisierten relativen Pfad,
- vollständigen Listing-Proof einschließlich Pagination,
- Quell- und Zielzustand im bestätigten Plan,
- Sitzungswechsel und Reconnect an eine erneute Identitätsprüfung.

Secrets liegen weder in Plänen noch Logs oder Queue-Journalen. Authentifizierung wird
über kurzlebige Sitzungsobjekte beziehungsweise den System-Schlüsselbund angebunden.
Ein nach Leerlauf geschlossenes FTPS-Control-Channel-EOF wird als Verbindungsabbruch
klassifiziert. Die App meldet ihn nicht als Antwortsyntaxfehler, sondern baut genau
einmal eine frische Anmeldung auf; erst nach identischer Zertifikat-, Root- und
Serverfähigkeitsidentität wird das angeforderte Listing wiederholt.

## Transfer und Recovery

Dateien laufen grundsätzlich als neuer Temp-Eintrag, werden inhaltlich verifiziert und
erst danach per stärkster verfügbarer exklusiver Operation committed. Gibt es keine
beweisbar exklusive Commit-Semantik, bleibt der vollständig abgesicherte
Update-/Mirror-Modus gesperrt. Ein getrennter Best-Effort-Pfad darf nach frischem
Zielcheck, ausdrücklicher Wirkungsbestätigung und Readback die normale
Provideroperation nutzen, ohne Atomarität oder Wiederherstellbarkeit vorzutäuschen.
Entfernte oder ersetzte Ziele verwenden bevorzugt eine providerseitige Quarantäne
außerhalb des Zielbaums mit demselben Intent-Journal wie lokal; fehlt sie, muss die
Irreversibilität sichtbar Teil von Vorschau und Bestätigung sein.

## Reihenfolge

### SFTP

Der erste implementierte Provider besitzt eine klare Hostkey-Identität. Aktuell
sind interaktives Best-Effort-Listing und Streaminginhalte freigegeben. Die
Inhaltsklasse hält Datei-Handles und prüft BLAKE3, behauptet aber keine atomare
Rootbindung. Rekursive lokale Downloads besitzen eine iterative Baumvorbereitung ohne feste
Datei-/Auswahl-/Gesamtgrößen- oder Tiefengrenzen. Gewöhnliche Mutationen besitzen inzwischen eine eigene
[interaktive Best-Effort-Klasse](sftp-writes.md). Exklusives Create, fsync und
Rename werden dabei nicht als atomare Rootgarantie ausgegeben. Strikter Sync und
Quarantäne behalten ihre getrennten Sperren.

### FTP/FTPS

Expliziter Transportstatus, optionales Zertifikats-/Hostmodell, passive Datenkanäle
und vollständiges `MLSD`-Listing. Klartext und eine ungeprüfte FTPS-Identität bleiben
sichtbar, sperren Browse und Transfer aber nicht allein wegen ihrer Transportstufe.
Server ohne verlässliches MLSD oder sichere Rename-/Existenzsemantik bleiben dagegen
auf die tatsächlich begrenzbaren Operationen und Best-Effort-Wirkungen beschränkt.
Es gibt keinen stillen Transportwechsel. Gewöhnliche Zielwirkungen werden aus dem konkreten Auftrag und seiner
stärksten verfügbaren Ausführungsklasse abgeleitet; fehlende Sync-Garantien sperren normale interaktive
Best-Effort-Wirkungen nicht pauschal.

### WebDAV

Origin, TLS-Identität, Collection-Root, ETag und vollständige `PROPFIND`-Antwort werden
gebunden. Partielle 403-Antworten, wechselnde ETags oder unklare `MOVE`-Semantik
sperren Update/Mirror.
Lesende Richtungen bleiben umgesetzt. Zielseitige strikte Datei-/Syncwirkungen
bleiben seit AP13R passiv gesperrt; normale Uploads, reguläres Ersetzen, mkdir, Rename und Dauerlöschen
besitzen getrennte Best-Effort-Verträge.

### SMB

Server-GUID, Share und Root bilden die Identität. Listing-Paging muss vollständig sein;
Session-/Share-Wechsel oder nicht beweisbare Rename-Semantik brechen fail-closed ab.
Dieser Schnitt ist umgesetzt. Die reine Rust-Abhängigkeit wird auf macOS und Linux
gebaut; ein kleiner vendorter Protokollfix setzt im Client-NEGOTIATE das für
Verschlüsselung erforderliche Capability-Bit.

## Provider-Matrix

Jeder Provider beginnt mit Browse und den normalen interaktiven Operationen, die sein
Protokoll fachlich unterstützt. Vollständig abgesichertes AddOnly und Update folgen
nach atomarem Temp-/Commit- und Quarantäne-Readback, Mirror nach vollständigem
Listing-Proof. Fehlen einzelne Garantien, werden wirkungsbezogene
Best-Effort-Varianten geprüft und angeboten. Freigaben und Einschränkungen gelten pro
Wirkung und Fähigkeit, nicht pauschal für „Remote“.

Abnahmetests umfassen Lokal↔Provider, Provider↔Provider, Verbindungsabbruch,
Reconnect zu anderer Identität, Pagination-Lücken, späte Kollisionen, volle
Zieldatenträger/Quotas, Metadatenverluste und Quarantäne-Restore.
