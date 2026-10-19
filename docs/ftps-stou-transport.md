# FTP-/FTPS-STOU: Transportabschluss und Probenreste

Stand: 2026-10-18.

## Befund und Ursache

Ein kleiner Upload kann vom Client vollständig angenommen und geflusht sein,
während der Server anschließend eine leere Datei erhält. AP14 beobachtete solche
STOU-Probedateien unter dem gewählten Root. Der damalige Transfer wurde nicht auf
Paketebene aufgezeichnet. AP15 reproduziert das Fehlerbild mit derselben gesperrten
Serverbibliothek an einer kontrollierten Lesegrenze.

Der bisherige Abschluss verwarf den Datenstrom, bevor er die FTP-Kontrollantwort
las. `suppaftp` 10.0.0 sendet im Drop seines Rustls-Wrappers `close_notify`, hält
den TCP-Lesekanal aber nicht bis zur Antwort. Sind gültige TLS-Sessiontickets noch
ungelesen, kann das vollständige Schließen unter Linux einen TCP-Reset auslösen.
Der unabhängige FTPS-Peer beobachtet vier Nutzbytes, einen Reset und kein
`close_notify`, obwohl `write_all` und `flush` erfolgreich waren.

Der Gegenlauf mit `libunftp` 0.23.0 hält dessen ersten Inhaltsread bis zum
beobachteten Clientabschluss zurück. Vor der Korrektur meldet der Server
`ConnectionReset`; lokaler Dateireadback und frischer FTP-Readback ergeben null
Bytes. Nach der Korrektur meldet derselbe Server vier Bytes und erfolgreichen
Kontrollabschluss; beide Readbacks ergeben exakt `eins`. Die zusätzliche
Leseschranke verändert nur die Ausführungsreihenfolge. Sie ersetzt weder
Transportantworten noch den Dateibestand und wird nicht ausgeliefert.

Das ist ein Produktfehler beim Umgang mit dem Bibliotheksvertrag. Die Fixture
wird nicht durch akzeptierte leere Inhalte oder größere Fristen angepasst.
`unftp-sbe-fs` 0.4.0 schreibt über Tokios `copy`; Tokio 1.52.3 flushes seinen
Writer am regulären EOF. Der beobachtete Reset beendet diesen Weg mit Fehler.
Rustls 0.23.41 unterscheidet seinerseits angenommene Nutzbytes, Flush und
TLS-Abschluss. Erfolgreiches Schreiben allein bestätigt keinen Serverinhalt.
Der Protokollrahmen bleibt [STOU nach RFC 959](https://www.rfc-editor.org/rfc/rfc959#section-4.1.3)
und [FTPS-Datenabschluss nach RFC 4217](https://www.rfc-editor.org/rfc/rfc4217#section-12.6).

## Ausführung und Cleanup

Der gemeinsame Abschluss von Proben und normalen Uploads:

1. flusht den Datenstrom und erhält jeden Fehler;
2. sendet bei TLS das `close_notify` vollständig und prüft dessen Schreibfehler;
3. schließt ausschließlich die TCP-Schreibrichtung;
4. hält den Lesekanal während der abschließenden Kontrollantwort;
5. erlaubt erst nach erfolgreichem Abschluss den bisherigen Rename und Readback.

Die Kontrollantwort wird genau einmal gelesen. Ein verlorener Abschluss oder
Rename wird nicht mutierend wiederholt. Der normale Writer meldet solche
Teilwirkungen als `outcome_unclear` mit beiden strukturierten Prüfnamen. Eine
fehlgeschlagene Schreibphase kann nicht durch ein späteres `flush` einen
Teilinhalt an den Zielnamen hängen. Ein Renamefehler oder Stream-Drop löscht
keinen inzwischen möglicherweise fremden STOU-Namen.

Die Probenregistry vermerkt den bestätigten STOU-Namen und den erwarteten
vollständigen Hash vor dem ersten Inhaltsbyte. Schreib-/Flushfehler,
Abschlussfehler, Probe und Cleanup bleiben gemeinsam erhalten. Auch ihre
strukturierten Recoverypfade bleiben über die gemeinsame Fehleraggregation
erhalten. Eine verlorene oder ungültige STOU-Namensantwort erzeugt kein
Löschrecht.

Die interne Löschprobe prüft ihren Dateiinhalt vor DELE. Ihr Cleanup verwendet
denselben konservativen Vertrag wie AddOnly: vollständiges Listing, ausschließlich
bekannte reguläre Dateien und vollständiger Inhaltsreadback vor der ersten
Entfernung. Bestätigte eigene Probeordner werden nur nach leerem Listing und mit
gewöhnlichem RMD entfernt. Teilinhalt, ausgetauschter Inhalt, späterer fremder
Name oder ein fremdes Kind bleiben erhalten. Es gibt kein rekursives Cleanup.
Die unabhängige Restprüfung des gewählten Roots und des Profilroots bleibt
auch nach Fehlern erforderlich.

Eine Neuprüfung widerruft die betroffenen alten Fähigkeiten. Eine Probenrevision
verhindert, dass eine ältere Probe derselben Vorbereitung oder eines überlappenden
Roots sie anschließend wieder veröffentlicht.
Proben und normale Übertragungen verschiedener Fenster bleiben parallel.
Ein gehaltener Gegenfall lässt die zweite Probe abschließen, bevor die ältere
Antwort freigegeben wird; die ältere darf danach keine Fähigkeiten veröffentlichen.
Die Ersetzungsbilanz verwendet ausschließlich das erfolgreiche eigene
Teilprobenergebnis und übernimmt keine zwischenzeitliche fremde Cachefreigabe.
Ein erfolgreich beobachtetes überschreibendes RNTO kann keine alte
No-Replace-Freigabe stehen lassen. Ein unklarer oder abgebrochener Teilversuch der
Dateifähigkeitsabfrage wird zurückgegeben und beendet weitere Proben. Eine
sauber begrenzte gewöhnliche Absage bleibt mit ihrer konkreten Begründung in
der Fähigkeitsbilanz.

## Prüfung und Reichweite

Die eingecheckten Gegenfälle in `core/src/ftps_transport_tests.rs` und
`core/tests/ftps_e2e.rs` prüfen Klartext/TLS, leere und nichtleere Inhalte,
ungelesene Sessiontickets, Teiltransfer, Flush-/TLS-Abschlussfehler, verlorene
Kontrollantwort, Rename-Teilwirkungen, ausgetauschte Inhalte, fremde Namen und
nicht mehr leere eigene Probeordner. Ein zusätzlicher begrenzter Protokollgegenlauf
prüft ungültige STOU-Namen ohne nachfolgende Mutation.

Der Sessionticket-Gegenfall ist vor der Korrektur unter Linux rot; macOS zeigte
an dieser kontrollierten Grenze keinen stabilen Reset. Alte Fähigkeitsfreigaben,
der verlorene Löschteilprobenfehler und verlorene strukturierte Recoverypfade
haben eigene rote Gegenbelege. Diese Fälle werden getrennt von vollständigen
Gesamtläufen bilanziert. Der private `libunftp`-Gegenlauf zeigt zusätzlich das
konkrete Nullbytebild vor und den vollständigen Transfer nach der Korrektur.


Die abschließenden Gesamtläufe sind grün: macOS Core 1064, CLI 68 und App 203;
Linux Core 1083, CLI 68 und App 193. Je 21 Core-Spezialtests bleiben ignoriert,
auf Linux zusätzlich zwei native GTK-Clipboardfälle. Formatierung, Clippy mit
`-D warnings` und Fuzz mit `--locked --all-targets` bestehen auf beiden Plattformen;
Browser 339, Pane 6, CSP und Tooltests 135 ebenfalls. Die Linux-Kopie hat einen
eigenen Zielordner sowie einen abgeglichenen und neu datierten Quellbestand.

Die native Prüfung am endgültigen Test-App-Build belegt leere/kleine TLS-Kopien,
Konfliktüberspringen, bestätigtes Ersetzen und Neustart in beiden Themes bei
800×600. Ein Abbruch nach beobachtetem Payloadbeginn erhält einen Teilinhalt,
`outcome_unclear`, strukturierte Recoverypfade und genau einen Versuch. Journal,
Quellen, beide Roots und Screenshot wurden unabhängig abgeglichen. Der letzte
automatische Textcheck erwartete irrtümlich wörtlich „unklar“; die Oberfläche zeigt
„FTPS-Uploadabschluss nicht bestätigt“ samt Restpfad. Der rohe Runnerstatus bleibt
fehlgeschlagen dokumentiert. Der Inhalts- und Zustandsabgleich bestätigt die
Produktwirkung; es gab keinen erneuten Transfer. Ein vorheriger Runnerfehler las
den Abschluss eines älteren Queueeintrags und wurde durch Bindung an die neue
Operations-ID korrigiert. Eigene Prozesse, Listener und künstliche Zugangsdaten
sind bereinigt, die unklare künstliche Datei bleibt als Referenz erhalten.

AP14s leere Erstbaseline und reine Quelle bleiben mutationsfrei. AP13R bleibt
bei genau einer unmittelbaren WebDAV-Elternbedingung, passiven strikten Absagen
und seinen getrennten gewöhnlichen Additionen, mkdir, Rename und Downloads.
Diese Untersuchung erweitert keine Providerfreigabe und ersetzt keinen
vollständigen Bereichsreview.

## Wirkungsbezogene Vorbereitung und dauerhafte Intents

Eine reine Kopierquelle startet weiterhin keine mutierende Probe. Ein FTPS-Kopierziel
prüft nur STOU, Inhaltsreadback und den benötigten Rename; die Probe belegt ihren
eigenen Verzeichniscontainer. Eine zusätzliche mkdir- oder Löschteilprobe ist dafür
nicht nötig. Eine Move-Quelle prüft ausschließlich ihre begrenzte Quellentfernung.
mkdir und gewöhnliches Rename behalten ihre eigenen Proben. Die ausdrücklich
angeforderte vollständige Fähigkeitsbilanz bleibt verfügbar. Ein unklarer Ausgang
oder Verbindungsverlust beendet die weiteren Teilproben und erhält die strukturierten
Prüffade auch in der CLI. Eine passive fehlende Quarantänegarantie sperrt die davon
abhängige bidirektionale Zielwirkung bereits vor einer Dateiprobe.

Jede mutierende FTPS-Vorbereitung benötigt jetzt einen Owner aus dem verursachenden
Fensterjournal. Eine unveränderliche Operationssicht bindet gewählte Pane-Roots,
Serveridentität, Sitzungsgeneration und Kontodigest; der gemeinsame Verbindungspool
trägt keinen veränderlichen Fensterowner. Vor dem ersten MKD oder STOU entsteht
atomar und dauerhaft ein privater Intent. Vor STOU steht ausdrücklich „Name
unbekannt“ darin. Der bestätigte eindeutige Name wird vor dem ersten Inhaltsbyte
nachgetragen. Probecontainer, angeforderter Name und weitere bekannte Wirkungsnamen
bleiben erhalten. Ein fehlgeschlagener Intentwrite erlaubt keinen nachfolgenden
Schreibbefehl.

Die bestätigte Ausführungsklasse gehört ebenfalls zur Operation. Der Beginn einer
Parallelprobe entzieht einem bereits laufenden Upload seine eigene Klasse nicht.
Eine abgeschlossene neuere Gegenprobe im selben Namensraum, die No-Replace widerlegt,
sperrt den älteren abgesicherten Beleg dagegen vor der nächsten Wirkung. Eine spätere
positive Probe repariert diese alte Vorbereitung nicht. Generation und Probenrevision
verhindern weiterhin die Veröffentlichung überholter Ergebnisse.

Der gehaltene Queueordner bindet Prozesslock, Journalwrites, Readback, Audit und
Intentindex. Ein Ordnerwechsel stoppt weitere Wirkungen; der Ersatzordner wird nicht
als neuer Speicherort akzeptiert. Neue Konfigurationsordner werden in GUI und CLI
privat angelegt. Ein bestehender fremd beschreibbarer Ordner erhält keine FTPS-
Intentfreigabe und wird nicht still umberechtigt. Eine reine FTPS-Leseseite
benötigt keinen privaten Probenspeicher; der Owner darf ihre unveränderliche
Lesebindung halten, ohne damit eine Schreibfreigabe zu erhalten. Benötigte
FTPS-Schreibseiten werden vor den Proben anderer Provider auf ihren privaten
Owner geprüft.

Ein Intent wird erst nach bestätigtem Abschluss beziehungsweise bestätigtem
Proben-Cleanup samt unabhängiger Restprüfung entfernt. Ohne bestätigten Abschluss
bleiben Index und bekannte Namen bestehen. Das Queueformat 27 ordnet sie nach einem
Prozessabbruch ausschließlich dem besitzenden Fenster zu. Vor dem Enqueue entstandene
Reste erscheinen als reine `FtpsRecovery` mit Server, Root und Prüffaden; dieser Typ
besitzt keinen Executor. Laufende FTPS-Zieloperationen bleiben auch im Absturzfenster
zwischen abgeschlossenem Einzelintent und terminalem Queuewrite unklar.

Recovery liest nur lokale Belege. Sie verbindet keinen Provider, wiederholt keinen
Schreibbefehl und löscht keine Remote-Datei. Alte Indexversion 1 bleibt lesbar;
Version 2 bindet die FTPS-Beobachtung, begrenzt Größe und Pfade und verweigert
vermischte Versionen sowie Symlinkindizes. Handles, Secrets, Permits und neue
Löschrechte entstehen daraus nicht. Manuelles Schließen bestätigt die Prüfung ohne
Wiederholung; es bereinigt weder unklaren noch späteren fremden Bestand.

### AP16: Verifikation und native Grenzen

Die AP16-Gesamtläufe bestehen mit macOS Core 1078, CLI 69 und App 203 sowie
Linux Core 1097, CLI 69 und App 193. Je 21 Core-Spezialfälle bleiben ignoriert,
unter Linux zusätzlich zwei native GTK-Clipboardfälle. Formatierung, Clippy mit
`-D warnings` und Fuzz mit `--locked --all-targets` bestehen auf beiden Plattformen;
341 Browserfälle, CSP, sechs Pane- und 135 Tooltests ebenfalls. Der letzte
macOS-Runner meldete ausschließlich den während des Laufs geänderten Testquellhash;
die vollständigen 16 FTPS-Integrationstests wurden danach am endgültigen Quellstand
erneut ausgeführt. Die Produktquellen der macOS- und Linux-Prüfung sind bytegleich.

Die native Test-App belegt leere und kleine TLS-Kopien, Überspringen und bestätigtes
Ersetzen, Neustart sowie beide Themes bei 800×600. Nach einem Prozessabbruch vor
der STOU-Namensantwort erscheint ausschließlich im besitzenden Nebenfenster eine
manuelle Recovery. Nach einem weiteren Abbruch während des Nutzdatentransfers
bleiben 512 KiB Teilinhalt, der bestätigte STOU-Name und genau ein Versuch im
Hauptfensterjournal erhalten. Beide Fälle zeigen `outcome_unclear` ohne Retry.
Manuelles Schließen bewahrt den Remote-Bestand einschließlich eines späteren
fremden Eintrags. Journal, Serverbaum und Ansichten wurden unabhängig abgeglichen;
eigene Prozesse, Listener und künstliche Zugangsdaten sind bereinigt.

Die unbekannte Namensantwort im nativen TLS-Fall hinterlässt einen Probecontainer
und den dauerhaften Intent, aber noch keine STOU-Datei: Der Client beginnt den
TLS-Datenhandshake erst nach der zurückgehaltenen Antwort, die Fixture öffnet die
Datei erst danach. Ein vorheriger Runner scheiterte an der gegenteiligen Annahme;
sein roher Fehlerbeleg bleibt erhalten. Eine tatsächliche serverseitige Anlage vor
verlorener Namensantwort ist separat durch den Klartext-Gegenserver und einen
TLS-Protokollgegenlauf mit vorgezogenem Handshake belegt. Das ist keine Behauptung,
dass die native Fixture dieses Zeitfenster ebenfalls erzeugt. Ein weiterer
Testskriptfehler betraf ausschließlich die FIFO-Erneuerung vor der ersten App-Aktion;
der korrigierte autonome Abschlusslauf besteht am unveränderten Produktbuild.

Die offene Frage zur gemeinsamen Probenrevision unabhängiger Roots wurde in AP17
durch einen gehaltenen Protokollgegenlauf bestätigt und korrigiert. Die offenen
Ganzbereichsreviews und die menschliche interaktive Linux-Abnahme bleiben
eigenständige Hürden.

### AP17: parallele Proben und Rootgrenzen

Jede laufende Probe besitzt ein flüchtiges Ticket mit Sitzungsgeneration,
Probenfamilie, physischem Root und Operationssicht. Eine neu begonnene Probe
derselben Familie entwertet nur ältere Tickets derselben Operationssicht oder
überlappender Roots. Die Überlappung wird nach Pfadkomponenten geprüft: `/pane`
und `/pane-other` sind getrennt, `/pane` und `/pane/nested` überlappen. Die
Entwertung bleibt auch nach Abschluss oder Fehler der neueren Probe bestehen.
Ein Reconnect entwertet alle offenen Tickets. Beendete Tickets werden entfernt;
die Verbindung sammelt keine unbegrenzte Historie früher besuchter Probenroots.

Ein weiterhin gültiges Ergebnis aktualisiert ausschließlich die betroffene
Fähigkeitsfamilie seiner eigenen Operationssicht. Der gemeinsame Anzeigecache
nimmt es nur an, wenn seitdem keine neuere Probe dieser Familie begonnen wurde.
Dadurch kann die langsamere Vorbereitung eines getrennten Roots fertig werden,
ohne eine neuere schwächere Beobachtung im Cache zu überschreiben. Gegenbelege
für No-Replace behalten ihre monotonen Revisionen und sperren ältere abgesicherte
Operationen im überlappenden Namensraum weiterhin vor der nächsten Wirkung.

Der Gegenlauf hält die erste MKD-Antwort zurück und beendet die zweite Probe über
eine unabhängige Verbindung. Er prüft getrennte Fensterjournale, gleichnamige und
verschachtelte Roots in beiden Richtungen, verschiedene Rename-Garantien, eine
gemeinsame Operationssicht und Reconnect. Der getrennte Rootfall scheitert vor
der Korrektur mit `interrupted`; die erweiterte Grenze derselben Operationssicht
hat einen zusätzlichen roten Gegenbeleg. Cleanup liest beide Roots und erhält
den vollständigen Ausgangsbestand. Dauerhafte Intents, Queueformat 27 und manuelle
Recovery werden dadurch nicht erweitert.

Die endgültigen AP17-Läufe bestehen mit macOS Core 1079, CLI 69 und App 203 sowie
Linux Core 1098, CLI 69 und App 193. Je 21 Core-Spezialfälle bleiben ignoriert,
unter Linux zusätzlich zwei native GTK-Fälle. Formatierung, Clippy mit
`-D warnings` und Fuzz mit `--locked --all-targets` bestehen beidseitig; 341
Browserfälle, CSP, sechs Pane- und 135 Tooltests ebenfalls. Der ursprüngliche
Rootgegenfall ist auf beiden Plattformen rot. Die Linux-Kopie wurde nach dem
letzten Operationssicht-Gegenfall aktualisiert, neu datiert und vollständig erneut
geprüft; die Produktquellen sind mit macOS bytegleich.

Der macOS-Runner bewahrt einen fehlgeschlagenen Schlussvergleich mit seinem vor
den letzten beiden Änderungen aufgenommenen Quellmanifest. Alle Einzelgates
bestehen. Der getrennte Zeit- und SHA-256-Abgleich belegt, dass jede Rust-Prüfung
erst nach beiden Änderungen begann und der Code danach unverändert blieb.
Dieser Runnerstatus wird nicht als vollständig grüner Originallauf umgedeutet.

Die endgültige native Test-App hält die erste STOU-Namensantwort unter Root A
zurück. Ein zweites Fenster beendet seine echte Kopie unter Root B vollständig,
während A noch keinen Queueeintrag besitzt und seinen unbekannten Namen dauerhaft
festhält. Nach Freigabe beendet auch A seine Kopie. Beide Fensterjournale enthalten
ausschließlich ihre eigene abgeschlossene Operation mit genau einem Versuch;
Dateiinhalte, Quellen, Sentinels und beide Ansichten wurden unabhängig abgeglichen.
Es bleiben keine offenen Intents oder Probenreste. Eigene Prozesse, Listener und
künstliche Zugangsdaten sind bereinigt. Dieser Block prüft den parallelen Ablauf
im dunklen Theme, den ersten Abschluss zusätzlich bei 800×600. Unveränderte
Theme-, Konflikt- und Absturzzustände behalten ihre getrennten AP16-Belege.

Der Kontonamespace direkter FTPS-Moves ist ein eigener anschließender Reviewpunkt:
Der aktuelle Namespacevergleich verwendet Servermerkmale und lexikalische Roots,
obwohl die Intentbindung zusätzlich bereits einen Kontodigest hält. Ein echter
Gegenlauf mit zwei künstlichen Kontosichten muss die Wirkung zuerst bestimmen;
AP17 behauptet dafür weder eine Schadensreproduktion noch eine Korrektur.

### AP18: Eintragstyp, Zielroot und tatsächliche Move-Wirkung

Ein unabhängiger FTP-Gegenserver verweigert das Ersetzen vorhandener Ordner,
ersetzt aber vorhandene Dateien. Die frühere Ordnerprobe gab trotzdem den
allgemeinen Datei-No-Replace-Vertrag frei. Ein anschließendes `rename_noreplace`
ersetzte den erst nach der Probe entstandenen fremden Dateiinhalt. Der rote
Gegenbeleg prüft die tatsächlichen Bytes; der korrigierte Core erhält Quelle
und fremdes Ziel und lehnt die fehlende Dateigarantie vor RNFR ab.

Datei- und Ordnerbelege liegen jetzt getrennt und ausschließlich in der
backendgehaltenen Operationssicht. Jeder Beleg bindet seinen physischen Zielroot,
seine Revision und das beobachtete No-Replace-Verhalten. Die öffentliche
Datei-Commit-Fähigkeit erhält keine Freigabe aus der Ordnerprobe. Neuere
Gegenbelege entwerten ältere Garantien desselben Typs im überlappenden Root;
ein späterer positiver Beleg repariert eine ältere bestätigte Sicht nicht.
Ein laufender Datenstrom hält seinen vor STOU gebundenen Dateibeleg und prüft
ihn vor dem Commit erneut. AP17s laufende Proben-Tickets bleiben erhalten.

GUI, CLI und Queue bestimmen die Move-Route gemeinsam im Core. Der reine
Rename-Weg benötigt Belege für sämtliche ausgewählten Top-Level-Typen am
tatsächlichen RNTO-Zielroot. Reine Ordner-Moves starten keine Inhaltsprobe.
Nur derselbe aktive Queueeintrag darf die Zielbelege zwischen zwei Sichten
derselben gehaltenen Sitzung übernehmen; Vorschauen und andere Fenster erhalten
dadurch keine Schreibfreigabe. Nach einem Fehler erlaubt diese reine Wirkung
keinen stillen Wechsel zu Kopieren und Quellentfernung. Die bisherige Queue
konnte trotz gewählter Rename-Route wegen getrennter VFS-Sichten intern in den
Kopierweg geraten;
der neue Weg prüft die tatsächlichen Befehle und verhindert diese Abweichung.

Fehlt die Garantie bereits bei der Vorbereitung, bleibt der vorhandene
verifizierte Copy-/Remove-Weg verfügbar. Schwache Zielkopie und Quellentfernung
werden getrennt angezeigt und an die Bestätigung gebunden. Gleiche Profil-IDs
dürfen die vom Backend gemeldete Quellentfernung im Frontend nicht auslassen.
Unbestätigte oder inzwischen schwächere Klassen halten vor der Nutzerwirkung an.
Ein bereits als Copy/Remove bestätigter FTPS-Auftrag behält diese Route auch bei
einer inzwischen stärkeren Rename-Probe. Die bestätigte Quellentfernung wird
nicht still durch einen reinen Rename ersetzt.
Für Dateien, nicht leere Ordner und gemischte Auswahl prüft ein echter lokaler
FTP-/FTPS-Server vollständige Inhalte und Quellentfernung sowie fehlende
Bestätigung jeder schwachen Wirkung. Antwortverlust vor und nach dem reinen
RNTO erhält den dauerhaften Intent und `outcome_unclear`; Neustart verbindet,
wiederholt oder bereinigt den Provider nicht.

Der zusätzliche Namespace-Gegenlauf ist rein lesend: Zwei Verbindungen mit
gleicher Konfiguration melden verschiedene kanonische Roots. Die frühere
Verbindungsteilung ersetzte die unabhängige Zielverbindung durch die Quellsicht.
Bei der Verbindungsteilung interaktiver Dateioperationen muss jetzt auch die
beobachtete Profilroot-Abbildung übereinstimmen. Der schon
vorhandene vollständige Konfigurationsvergleich schützt weiterhin Benutzer,
geschützte Secret-Referenz und angeforderten Profilroot. Ein anderer Kontodigest
beweist keine physisch getrennten Dateibäume; die Überlappungsprüfung bleibt
davon getrennt. Die dokumentierte Grenze gleich konfigurierter Serverknoten
und die pfadgebundene Quelle werden dadurch nicht aufgehoben.

Die endgültigen Prüfungen bestehen mit macOS Core 1088, CLI 69 und App 203 sowie
Linux Core 1107, CLI 69 und App 193. Je 21 Core-Spezialfälle bleiben ignoriert,
unter Linux zusätzlich zwei native GTK-Fälle. Formatierung, Clippy mit
`-D warnings` und die Übersetzung aller Fuzz-Targets mit `--locked` bestehen
beidseitig. 343 Browserfälle, CSP, sechs Pane- und 135 Werkzeugtests sind grün.
Nach der letzten Routenbindung wurden sämtliche Rust-Gates erneut ausgeführt;
Frontend und Werkzeuge blieben bytegleich zu ihren vollständigen grünen Läufen.
Beide abschließenden Quellvergleiche bestehen, die Code- und Werkzeugdateien
sind zwischen den Plattformen bytegleich. Ein zusätzlicher unabhängiger
FTP-Protokolltest führt drei echte Transfers mit unterschiedlichen künstlichen
Konten, Secret-Referenzen beziehungsweise Profilroots aus und erhält jeweils die
eigene Zielsicht, den fremden Zielbestand und genau einen Ausführungsversuch.

Die endgültige native Test-App verschiebt über TLS eine Datei und einen nicht
leeren Ordner einschließlich verschachteltem Inhalt und leerem Unterordner.
Beide bestätigten Best-Effort-Erwartungen stehen im jeweiligen Queueeintrag.
Konfliktabbruch und Überspringen erhalten Quelle und Altziel; ein Neustart
wiederholt keinen der drei abgeschlossenen Aufträge. Der Konfliktdialog wurde
in beiden Themes bei 800×600 bedient und visuell geprüft, der Dateiabschluss
zusätzlich im dunklen Theme. Unabhängige Hash-, Vollbaum- und Journalprüfungen
finden keine fremden Änderungen, offenen Intents oder Probenreste; eigene
Prozesse, Listener und künstliche Zugangsdaten sind bereinigt.

Der native Block prüft den gewöhnlichen schwachen Copy-/Remove-Weg. Starke
typisierte Garantien und späte Kollisionen bleiben unabhängige headless
Protokollgegenfälle; AP16/AP17s unveränderte Absturz- und Parallelzustände
behalten ihre getrennten Belege. Die hier geprüfte FTPS-Dateivorbereitung war noch nicht an das
Fenster-Abbruchsignal angeschlossen. Der anschließende AP19-Gegenlauf bestimmt
diesen getrennten Prüfpunkt.


### AP19: Abbruch vor der Nutzerwirkung

Der unveränderte native AP18-Build zeigt beim F6-Gegenlauf einen tatsächlichen
Fehler: Die erste STOU-Namensantwort wird vor Enqueue gehalten; der unbekannte
Intent steht bereits im Fensterindex. Ein echtes Escape erreicht diese
Vorbereitung nicht. Nach kontrollierter Freigabe wird trotzdem ein Move mit einem
Versuch abgeschlossen; die Quelldatei fehlt und das Ziel enthält ihren exakten
Inhalt. Das ist eine Nutzerwirkung im künstlichen Testroot. Der damalige native
Block hat diesen Vorbereitungsabbruch ausdrücklich nicht geprüft.

Die FTP-/FTPS-Dateivorbereitung verwendet jetzt dieselbe abbrechbare Reservierung
wie die vorhandenen SFTP-/WebDAV-Inhaltsvorbereitungen. Der private FTPS-Owner hält
den Fenster-`Cancel`; die tatsächlichen Queueowner halten den Worker-Token. Vor
einer neuen Teilprobe wird Abbruch geprüft, nach der Ziel-/Quellvorbereitung erneut
vor Klassenbestätigung, Konfliktaufbereitung und Einreihung. Die Ereignisse gehen
an das verursachende Fenster. Die neutrale Fortschrittsanzeige behauptet vor dem
Fähigkeitsbefund keine Ausführungsklasse. SFTP-/WebDAV-Grenzen bleiben erhalten.

Eine bereits begonnene Teilprobe wird nicht gewaltsam unterbrochen: Ihre eigene
STOU-Übertragung, Kontrollantwort, Inhaltsprüfung und begrenzte Bereinigung dürfen
fertig werden. Das Abbruchflag erteilt keine zusätzliche Cleanupfreigabe. Ein
Transport-/Cleanupfehler wird vor einem nachfolgenden Abbruchcheck zurückgegeben;
`outcome_unclear` mit bekanntem oder unbekanntem STOU-Namen behält Intent und
Recoverypfade. Es gibt keinen Mutationsretry und keine automatische Remote-Recovery.
Ein sauberer Abbruch vor Enqueue erzeugt keinen Nutzerauftrag; bei der frischen
Queuevalidierung wird er nur ohne Recoverypfade als `Cancelled` abgeschlossen.

Die gezielten Klartext-Protokollfälle prüfen Abbruch vor der ersten Probe,
zwischen Datei- und Ordnerprobe, während der ersten Namensantwort, nach bekanntem
Namen beim ersten Inhaltsbyte und beim Verlust der Abschlussantwort. F5 und F6
behalten unklare Ausgänge; saubere Abbrüche erhalten Quelle und Ziel, beginnen
keine nächste Teilprobe und werden nach Neustart nicht erneut ausgeführt. Die
Browserfälle halten die Antwort bis zum echten Escape in beiden Themes bei
800×600. Sie prüfen Anzeige und Einreihung, keine realen FTP-Bytes.

Endgültig bestehen macOS Core 1092/CLI 69/App 203 und Linux Core 1111/CLI 69/App 193.
Je 21 Core-Spezialfälle und auf Linux 2 GTK-Fälle bleiben ignoriert. fmt,
Clippy mit `-D warnings` und Fuzz mit `--locked --all-targets` sind beidseitig grün;
Browser 351, CSP, Pane 6 und Tools 135 ebenfalls. Nach dem letzten reinen
Frontenddelta wurden App/Assets, nativer Build, Browser und Werkzeuge erneut
geprüft; Linux-App und Clippy ebenfalls. Die unveränderten Rust-Gates bleiben
über den Quellabgleich gebunden. 383 Code-/Werkzeugdateien sind bytegleich.
Ein zusätzlicher Protokollfall prüft den Abbruch bei schwachem No-Replace ohne
Best-Effort-Bestätigung für Copy und Move; er beginnt keine Quellentfernungsprobe.

Die endgültige native App prüft F6 im dunklen und F5 im hellen Theme bei 800×600.
Die erste STOU-Namensantwort bleibt bis zum echten Escape gehalten. Nach Freigabe
bleibt die Quelle unverändert, das Nutzerziel fehlt und die Queue ist leer.
Ein zweites Fenster kopiert unter getrennten Roots vollständig, während das erste
noch wartet; seine eigene Queue bleibt unabhängig. Neue ausdrücklich ausgelöste
F5/F6-Aufträge sind nach dem Abbruch wieder erfolgreich. Neustart wiederholt keinen
Auftrag. Vollständige Server-/Quellbäume, Hashes und Journale stimmen überein;
die erfolgreichen Blöcke hinterlassen keine offenen Probenintents oder Reste.
Eigene Prozesse, Listener und künstliche Schlüsselbundeinträge sind bereinigt.
Die sichtbaren Vorbereitungs-/Abbruchzustände sind in beiden Themes geprüft.

Ein gesonderter nativer Helferlauf wurde wegen einer falschen Pfadart im zweiten
Fenster beendet. Sein absichtlich gestoppter Prozess hinterlässt einen manuellen
Intent samt Probenordner im künstlichen Root; Quelle und Nutzerziel bleiben
unverändert. Ressourcen-Cleanup und tatsächliche Probenreste sind getrennt
bilanziert. Dieser Lauf ist kein erfolgreicher Produktgegenbeleg. Bekannte
STOU-Namen und verlorene Antworten unter Abbruch bleiben die unabhängigen
Klartext-Protokollfälle; ältere Absturzgrenzen werden nicht erneut behauptet.
Die F2-Generationsprüfung verwendet eine tatsächlich passive lokale Vorschau,
hält ihre Antwort bis zum neuen Dialog und beobachtet den nächsten Modal-Timerturn.
Ohne Generationstest erscheint der alte Konflikt danach; der Originalcode sperrt
ihn. Während einer FTPS-Vorbereitung bleibt F2 dagegen entsprechend der vorhandenen
Fensterschranke belegt.
Leere Ordnerauswahlen mit unnötigen Inhaltsproben bleiben ein eigenes folgendes
Paket; diese Korrektur ändert weder die ermittelten Wirkungsklassen noch Sync,
Rootbindung oder die typisierten No-Replace-Belege.


### AP20: Inhaltsfreie Ordnerwirkungen

Der unveränderte AP19-Core verweigert drei tatsächlich inhaltsfreie Wirkungen,
wenn ein künstlicher FTP-Server STOU sauber ablehnt, aber MKD und RMD unterstützt:
lokales F5 zum FTP-Ziel, FTP-F6 zum lokalen Ziel und den ausdrücklich bestätigten
schwachen FTP-Copy-/Quellentfernungsweg. Alle drei beginnen unnötig eine
STOU-Dateiprobe. Der native unveränderte AP19-Build bestätigt denselben unnötigen
STOU-Zugriff beim F6 eines vollständig leeren verschachtelten Ordnerbaums; der
Nutzer-Move selbst gelingt nach Freigabe. Dieser Gegenlauf belegt keine
Quellbeschädigung.

Die gemeinsame Backend-Vorbereitung liest die tatsächliche Auswahl vollständig
und begrenzt auf 100.000 Einträge. Nur eine Auswahl ohne regulären Dateiinhalt
bekommt die private Ordnerklasse. Symlinks und Spezialdateien bleiben gemäß dem
bestehenden Kopiervertrag sichtbar ausgelassen. Unvollständige Listings und
Statfehler ergeben keine Abwesenheit. Eine größere oder inhaltshaltige Auswahl
behält den bisherigen Datei-/STOU-Vertrag. Der vorhandene starke reine
Ordner-Rename wird zuerst geprüft und behält seinen eigenen Typvertrag.

Der private Owner sperrt Datei-Lese-/Schreib-/Commit-/Lösch- und Renamewege sowie
weitere Inhaltsproben und widerruft alte Datei-/Löschbelege. Ordnerbelege gelten
nur unter ihrem tatsächlich geprüften Root, für dieselbe Operation, Sitzung und
Probenrevision. Sie geben weder Datei-No-Replace noch allgemeines Löschen oder
eine stärkere Sync-Fähigkeit frei. Ein neuer ausdrücklicher Auftrag erhält eine
frische Vorbereitung und kann wieder regulären Dateiinhalt übertragen.

Am Ziel erfolgt eine MKD-/RMD-Probe nur, wenn tatsächlich ein Ordner anzulegen
ist. Bestehende Ordner werden additiv zusammengeführt; ein vollständig vorhandener
Zielbaum bleibt einschließlich Zielproben mutationsfrei. Ein bekannt fehlender
Elternordner wird bei der Prüfung seiner Kinder nicht mit einem Providerfehler
verwechselt. Neue Ordner behalten Elternlisting, Kollisionsprüfung, dauerhaften
Intent und Readback. Die Wirkung bleibt sichtbar Best Effort.

Eine FTPS-Move-Quelle verwendet ausschließlich den besonderen, vom gemeinsamen
Quellmanifest aufgerufenen RMD-Weg: neuer vollständiger Readback, echter Ordner,
aktuell leer, eigener MKD-/RMD-Beleg, dauerhafter Delete-Intent vor genau einem RMD
und anschließend geprüfte Abwesenheit. Ein spät auftauchender Dateiinhalt bleibt
in der Quelle; bereits angelegte Zielordner bleiben als Teilwirkung erhalten.
Eine verlorene RMD-Antwort vor oder nach serverseitiger Ausführung erhält
`outcome_unclear`, Intent und Recoverypfad. Queue-Recovery verbindet keinen
Provider und wiederholt nichts. Rekursives Löschen und Dateilöschung bleiben
durch diese Klasse gesperrt. Root-/Serverwechsel, Cancel und bereits begonnene
Proben behalten ihre bestehenden Verträge.

Fünf gezielte Protokollprüfungen belegen zwölf Kombinationen aus drei Richtungen,
flachen/verschachtelten Bäumen und neuen/vorhandenen Zielordnern; außerdem
Ordner-gegen-Datei- und Rootabgrenzung trotz alter positiver Dateibelege, einen
neuen regulären Datei-Auftrag, späten Dateiinhalt, RMD-Antwortverlust auf beiden
Seiten der Serverwirkung sowie ausgelassene Symlinks und Spezialdateien.
macOS besteht Core 1097/CLI 69/App 203, Linux Core 1116/CLI 69/App 193;
je 21 Core-Spezialfälle und Linux zwei GTK-Fälle bleiben ignoriert.
Formatierung, Clippy und Fuzz-Übersetzung mit `--locked --all-targets` sind
beidseitig grün; Browser 351, CSP, Pane 6 und Werkzeuge 135 ebenfalls.
383 Code-/Werkzeugdateien sind plattformübergreifend bytegleich.

Der finale native TLS-Build verschiebt und kopiert die verschachtelten leeren
Ordner ohne Verbrauch der gehaltenen STOU-Grenze. Nach Neustart löst ein neuer
regulärer Datei-Auftrag dieselbe noch aktive Grenze aus: eigener unbekannter
Intent vor Enqueue, nach Freigabe vollständige 42 Bytes bei erhaltener Quelle.
Die Ordnerklasse bleibt somit nicht als allgemeine Einschränkung eines späteren
Auftrags bestehen. Beide Themes bei 800×600 sind visuell geprüft. Drei terminale
Queueeinträge bleiben nach erneutem Neustart unverändert, jeweils ein Versuch.
Ein unabhängiger Vollbaum-/Hashabgleich bestätigt alle künstlichen Source-,
Target- und Profil-Sentinels, keine offenen Intents und Ressourcen-Cleanup0.
Die native TLS-Grenze ist kein vollständiger Paketmitschnitt; die vollständigen
mutierenden Befehle stammen aus den unabhängigen Klartext-Protokollfällen.
Verteilte Source-Path-Bindung und Multi-Node-FTPS bleiben unveränderte Grenzen.

Ein ausschließlich übersprungener Konfliktauftrag ist ein nachfolgender
Prüfkandidat: Ob er weiterhin unnötige Proben startet, ist hier nicht reproduziert.
Acht Ganzbereiche, Querschnitt und menschliche interaktive Linux-Abnahme bleiben
separate offene Hürden.


### AP21: Konfliktentscheidung vor den Proben

Der unveränderte AP20-Core startet bei einem vollständig übersprungenen Move
weiterhin eine MKD-/RMD-Quellprobe. Wird MKD abgelehnt, scheitert der Auftrag
anstatt nur den Konflikt zu überspringen. Die native AP20-App zeigt denselben
Fehler noch vor dem Konfliktdialog. Der selektive Protokollmitschnitt enthält
genau einen abgelehnten MKD-Versuch; Queue, Quelle und belegtes Ziel bleiben
unverändert, ohne offene Intents oder Ressourcenreste.

Die gemeinsame Core-Vorbereitung liefert für eine noch nicht entschiedene
FTP-/FTPS-Konfliktlage zuerst eine passive Anzeige. Alle Mutationsfähigkeiten
bleiben dabei false. Die GUI bietet Überspringen sowie die Prüfung von Ersetzen
oder neuen Namen. Die gewählte Entscheidung wird an den bestehenden privaten
Fenster-/Snapshotcache gebunden und erneut geprüft. Erst dann beginnen die
benötigten Teilproben. Ersetzen und „Beide behalten“ zeigen anschließend ihre
tatsächlichen Folgen beziehungsweise neuen Namen zur Bestätigung. Navigation verwirft verspätete Anzeige und Enqueue. Neue Vorschau,
abgelaufene oder fremde IDs entwerten die bisherige Cachebindung.
Das bestehende fenstereigene Abbruchsignal schützt auch diese zweite Phase.

Für bestätigtes Skip entfernt der Core vollständig übersprungene Top-Level-
Einträge aus der Wirkungsprüfung. Sind alle Einträge ausgeschlossen, erhält der
Executor einen wirkungsfreien Abschluss mit Skip-Zähler und ohne VFS-Mutation.
Er durchläuft weiterhin das normale atomare Queue-/Auditabschlussverfahren;
fehlende alte Best-Effort-Felder erteilen dabei keine Schreibfähigkeit. Neustart
wiederholt nichts. Gemischte Auswahl prüft nur die verbleibenden Top-Level-
Wirkungen: Ein übersprungener Dateikonflikt verlangt etwa keine STOU-Probe für
einen daneben tatsächlich zu kopierenden leeren Ordner.

Vor dem Beginn mutierender Teilproben erfolgt der frische vollständige
Konfliktvergleich. Seine erwartete Menge wird im gemeinsamen Core erneut geprüft;
dieselbe Menge bestimmt anschließend die ausgeschlossenen Einträge. Ein nach
dieser Entscheidung verschwundenes Ziel verwandelt Skip nicht in Kopieren.
Ein weiterer unabhängiger Gegenlauf entfernt das Ziel genau nach dem ersten
MLSD-Ergebnis. Die erste AP21-Fassung startete danach noch STOU-/Rename-Proben;
die korrigierte Fassung stoppt davor ohne neuen Nutzer- oder Probeninhalt.
Stat-/Listingfehler gelten weiterhin niemals als Abwesenheit.

Ein gesperrtes ausgewähltes Ersetzen startet keine Quellentfernungsprobe.
Der Dialog hält stattdessen die verbleibende Skip-Entscheidung erreichbar.
GUI, CLI und Queue verwenden denselben Core-Weg. Die CLI verlangt bei
unentschiedenen Konflikten zuerst eine ausdrückliche Wirkung, bevor sie deren
Fähigkeiten prüft. Queueformat 27, Proben-/STOU-Intents, typisierte No-Replace-
Belege, Transportabschluss, Root-/Kontobindung und mutierende Reconnect-Grenze
bleiben erhalten. Weder die passive Anzeige noch Skip geben eine neue
Provider-Schreibfähigkeit frei.

Fünf gezielte Core-Gegenfälle prüfen acht vollständige Copy-/Move-Skipvarianten
mit Dateien oder Ordnern und erlaubtem/abgelehntem MKD, gemischte Wirkungen,
neue/fehlende Konflikte und echten Listingfehler, passive Anzeige ohne
Mutationsfähigkeit sowie den späten Zielwechsel. 24 gezielte Browserfälle
prüfen F5/F6, beide Themes, Skip/Ersetzen/neue Namen/Abbruch, geänderte Entscheidung,
gesperrtes Ersetzen und verspätete Antwort nach Navigation.
macOS besteht Core 1102/CLI 69/App 203, Linux Core 1121/CLI 69/App 193;
je 21 Core-Spezialfälle und Linux zwei GTK-Fälle bleiben ignoriert.
Formatierung, Clippy und Fuzz-Übersetzung mit `--locked --all-targets` sind
beidseitig grün; Browser 375, CSP, Pane 6 und Werkzeuge 135 ebenfalls.
383 Code-/Werkzeugdateien sind bytegleich. Der erste Gesamtlauf fand auf beiden
Plattformen eine zu breite Owner-Prüfung für reine FTPS-Lesequellen. Die zusätzliche
Prüfung gilt jetzt nur den Schreibseiten; der originale Regressionstest und die
anschließenden Gesamtläufe sind grün.

Der endgültige native TLS-Build hält beide F5/F6-Skip-Aufträge einschließlich
eines gesperrten ausgewählten Ersetzens vollständig ohne mutierenden Befehl.
Abbruch in der ersten Konfliktanzeige verändert weder Queue noch Daten.
Eine ausdrücklich gewählte reguläre Ersetzung startet anschließend die benötigte
STOU-Probe mit dauerhaftem unbekanntem Namen vor Enqueue. Abbruch nach deren
Abschluss, aber vor der konkreten Ersetzungsbestätigung erhält die Altdatei.
Ein neuer ausdrücklich bestätigter Ersetzungsauftrag überträgt 42 Bytes;
der vollständige Mitschnitt enthält genau einen RNTO auf das endgültige Nutzerziel.
Quelle und übriger Bestand bleiben erhalten. Beide Themes bei 800×600 wurden
visuell geprüft. Nach Neustart bleiben drei Completed-Einträge mit je einem
Versuch unverändert. Unabhängige Vollbaum-/Hashprüfung, keine offenen Intents
und Ressourcen-Cleanup0 sind belegt.

Ein vorheriger nativer Helferfehler ist getrennt erhalten: Die letzte Assertion
zählte auch RNTO auf die eigene temporäre Datei, weil deren Name den Endnamen
enthält. Unabhängige Nachprüfung bestätigt bereits dort drei Completed-Einträge,
korrekten Inhalt und keine Intents; der Helferstatus blieb fehlgeschlagen.
Die Wiederholung verwendet den exakten endgültigen RNTO-Pfad und frische Roots.
Das selektive native Protokoll enthält keine USER-/PASS-Befehle oder Nutzdaten.
Verteilte Pfadgarantien werden daraus nicht abgeleitet.

Die Optimierung bindet vollständig übersprungene Top-Level-Einträge.
Zusammengeführte Ordner behalten bislang den rekursiven Inhaltsvertrag.
Die bestehende Konfliktbindung liest weiterhin Dateiinhalt für Hashbelege;
ihr Leseumfang ist ein nachfolgender Prüfpunkt. Multi-Node-FTPS,
Source-Path-Bindung, historische Fastra-Ursache, acht Ganzbereiche, Querschnitt
und menschliche interaktive Linux-Abnahme bleiben gesonderte Grenzen.


## AP22: Rekursive Skip-Wirkungen und passive Metadaten

Ein eigener Gegenlauf am unveränderten AP21-Core reproduziert vier unnötige
STOU-Versuche: Copy/Move mit flachen oder verschachtelten Merge-Ordnern, deren
Kinder sämtlich kollidieren und übersprungen werden. Quelle, Ziel und Sentinels
bleiben beim abgelehnten STOU unverändert. Die Vorbereitung zählt jetzt nach
vollständigem sicherem Listing auch die Nachfahren, mit derselben Grenze von
100.000 Einträgen wie die Ordnerklasse. Bei Überschreitung dieser Optimierungsgrenze
bleibt die vereinfachte Klasse unbekannt; der reguläre Datei-/Quellentfernungsweg
prüft weiterhin seine eigenen Fähigkeiten. Eine unvollständige Bilanz erteilt
keine schwächere Freigabe und sperrt keine sonst unterstützte Dateioperation.
Ein vollständig wirkungsfreier Auftrag
schließt ohne VFS-Mutation mit dem tatsächlichen Blattzähler ab. Keine positive
Schreibfähigkeit wird dafür erfunden.

Mischfälle prüfen ausschließlich verbleibende Wirkungen: Neue leere Ordner
benötigen ihren MKD-/RMD-Beleg, neue Dateien weiterhin STOU, Hash und Commit.
Ein Move mit übersprungenen Kindern behält den vollständigen betroffenen
Quellbaum. Dafür ist kein Inhalts- oder Entfernungsbeleg der ausgelassenen Dateien
nötig. Unabhängig vollständig kopierte Auswahlen benötigen weiterhin ihre eigene
manifestgebundene Quellentfernung. Die vor den Proben frisch geprüften Skip-Pfade
werden bis zum einzelnen Kopieraufruf gehalten; ihr späteres Verschwinden löst
`plan_drift` aus und erteilt keinen neuen Kopierauftrag. Der starke reine
Ordner-Rename bleibt vor dieser rekursiven Inhaltsprüfung vorrangig.

Die passive FTPS-Konfliktanzeige und die ausdrücklich gewählte Skip-Wirkung
binden nur vollständige, sichere Metadatenbelege. Diese bleiben privat im
fenstereigenen ID-/Auswahl-/Root-/Sitzungscache. Sie erlauben weder Ersetzung noch
Aliasziele. Vor tatsächlichem Ersetzen oder neuen Namen werden wieder vollständige
Inhaltsbelege erhoben und konkret bestätigt. Ein abgelehntes benötigtes RETR bleibt
ein Fehler. Lokale und andere Providerverträge bleiben unverändert; keine neuen
serialisierbaren Permits oder Queueformatänderungen entstehen.


Sechs neue Core-Prüfungen decken die vier vollständig übersprungenen Mergefälle,
Ordner-/Dateimischungen mit erhaltener Quelle, unabhängige vollständige Moves,
späten Konfliktverlust, passiven Metadatenumfang und den regulären Rückfall an
der Analysegrenze ab. Der Grenzgegenlauf war vor der Korrektur rot; sein kleiner
realer Dateibaum benutzt einen privaten Testgrenzwert bei unverändertem
Produktionslimit. Das simulierte Ziel belegt dabei nur die Core-Routenentscheidung,
keine FTP-Transportgarantie. Vier zusätzliche Browserfälle prüfen die
Teilkopie ohne Quellentfernung in beiden Themes mit F5/F6.

Der finale Stand besteht macOS Core 1108/CLI 69 und Linux Core 1127/CLI 69/App 194.
Je 21 Core-Spezialfälle und Linux 2 GTK-Fälle bleiben ignoriert. Das erste volle
macOS-App-Gate meldet 201 erfolgreiche und 3 fehlgeschlagene unveränderte
Fastra-Helfertests. Der Ausgabefall besteht einzeln; Fähigkeiten und Exitcodes
scheitern auch einzeln an ihren bisherigen Fristen. Das finale App-Teilgate
besteht 202 Tests und filtert genau diese 2 bekannten Fälle ausdrücklich aus.
Die historische Ursache bleibt offen; keine Frist oder Fastra-Implementierung
wurde geändert. Dies ist keine vollständig grüne macOS-App-Gesamtprüfung.
Formatierung, Clippy und Fuzz mit `--locked --all-targets` sind beidseitig grün;
Browser 379/CSP/Pane 6 sowie Werkzeuge 135 ebenfalls. Nach der letzten einzelnen
Rust-Korrektur wurden die betroffenen Gates erneut ausgeführt; unveränderte
Browser-/Pane-/CSP-Eingaben sind per Hash gebunden.383 Code-/Werkzeugdateien sind
plattformübergreifend bytegleich.

Die finale native TLS-App bestätigt F6/F5 mit vollständig übersprungenen
verschachtelten Merges ohne RETR oder mutierenden Befehl, passive Abbrüche,
neue leere Ordner ohne STOU und eine echte neue Datei mit STOU. Quellbäume mit
Skip bleiben vollständig erhalten; die ausgelassenen Dateien werden nicht
inhaltlich gelesen. Tatsächlich gewähltes Ersetzen verlangt wieder Inhalt;
abgelehntes RETR lässt Daten und Queue unverändert. Abbruch nach vollständiger
Vorbereitung erhält ebenfalls die Altdatei. Erst ein neuer ausdrücklich
bestätigter Auftrag ersetzt 42 Bytes mit genau einem RNTO auf das finale Nutzerziel.
Beide Themes 800×600 sind visuell geprüft. Fünf Completed-Einträge/Versuch 1 bleiben
nach Neustart unverändert. Unabhängiger Vollbaum-/Hashabgleich über 21 Dateien und
21 Ordner, keine offenen Intents im erfolgreichen Lauf und Ressourcen-Cleanup0.

Ein früherer nativer Helfer verglich fälschlich den Vollbaum während einer
absichtlich offenen STOU-Probe. Vier Nutzeraufträge waren korrekt abgeschlossen;
der anschließende Helferabbruch erhält einen künstlichen Probenordner und
manuellen Intent als Referenz. Der fehlgeschlagene Status bleibt getrennt
bestehen; keine automatische Löschung oder Wiederholung. Frische Wiederholungen
prüfen an der gehaltenen Grenze nur Nutzerdateien und lassen die begonnene Probe
vor dem App-Ende geprüft fertig werden. Die ersten beiden RETR-Fehlerfixtures
belegen keinen fertigen UI-Fehler: Die frühe Kontrollabsage beendete den
TLS-Datenweg nicht regulär. Erst die korrigierte Fixture über den normalen
Datenkanal und einen Storagefehler belegt die abgeschlossene Fehlermeldung der
unveränderten Ausgangs-App. Diese Hilfsgrenzen bleiben von Produktgegenbelegen
getrennt; Protokolle enthalten weder USER/PASS noch Nutzdaten.
