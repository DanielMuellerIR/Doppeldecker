# Linux-Kandidat vom 2026-10-19

Der aktuelle technische Gegenlauf besteht mit signierten DEB- und AppImage-Paketen
der unveränderten Produktversion 0.1.123. Die menschliche Funktionsabnahme in der
normalen Linux-Gerätesitzung ist am 2026-10-19 bestanden. Das anschließend gemeldete
fehlende Dockicon ist korrigiert und nativ geprüft; die menschliche Sichtkontrolle
bestätigt ebenfalls das sichtbare Flugzeug-Icon. Es wurde nichts veröffentlicht
und keine vorhandene Installation auf dem Testgerät ersetzt.

## Korrigierte Fehler

Der reguläre Paketbau reproduziert zunächst einen Fehler des eigenen Prüfers:
Der spezifikationskonforme interne AppImage-Desktoplink wird als fehlende reguläre
Datei abgelehnt. Die enge Korrektur und die negativen Dateisystemfälle stehen im
[Paketprüferbericht](code-review-2026-10-19-linux-paket.md).

Im echten Linux-Fenster erscheinen außerdem macOS-Kürzel. Beenden, Minimieren
und Fensterschließen fehlen, weil Tauri diese vordefinierten Menüaktionen unter
Linux nicht unterstützt. Linux verwendet dafür jetzt eigene native Einträge;
die übrigen Hinweise zeigen Strg statt des macOS-Symbols. Datei- und Pane-Aktionen
bleiben im Frontend gebunden. Der native Menücallback bleibt im Menümodul und
versendet solche Aktionen weiterhin nur an das fokussierte Fenster.

Fensterschließen verwendet `close()` und damit die bestehende
`CloseRequested`-Prüfung. Beenden fordert `app.exit(0)` über die bestehende
appweite Exitprüfung an. Es gibt keinen direkten Zerstörungs- oder Abkürzungspfad.

## Erster Paket-, GUI- und Upgradegegenlauf

Diese Installations- und Upgradebelege gehören zum Menü-Kandidaten vor dem unten
beschriebenen Icon-Delta. Seine Nutzlasthashes sind gesondert festgehalten.

Der Standardbefehl `pnpm bundle:linux` besteht nach den Korrekturen vollständig.
Er prüft Produktversion, Datenschutz, tatsächliche DEB-/AppImage-Layouts,
ELF-Architektur, Bibliotheksauflösung und die beiden eingebetteten CLI-Versionen.
AppImage, DEB und AppImage-Attestation werden mit dem vorhandenen Schlüssel
signiert und unabhängig gegen den eingebauten öffentlichen Schlüssel verifiziert.
Eine vertauschte Payloadsignatur wird abgelehnt; die Prüfsummendatei besteht.

Das frische AppImage startet über den vorgesehenen Sandbox-Launcher in einer
eigenen X11-Anzeige. F2 benennt um, F5 kopiert und F6 verschiebt; alle Inhalte
werden anschließend bytegenau nachgelesen. Zwei vollständig geladene Fenster
zeigen dieselben Pane-Ziele und besitzen getrennte Journale mit zwei
abgeschlossenen beziehungsweise keinem Auftrag. Strg+Q beendet beide Fenster
regulär mit Exit-Code 0.

Für den echten Versionswechsel wird der unveränderte historische Quellstand
`7395bd8198335d2518b5c116eb388f474f688f9b` mit Version 0.1.122 separat neu gebaut.
Dies ist ein historischer Teststand, kein byteidentisches früheres Releasepaket.
Seine 281 Quelldateien bleiben während des Baus unverändert. Das DEB wird in einem
eigenen Ubuntu-24.04-Container installiert und tatsächlich über `/usr/bin/doppeldecker`
gestartet. Eine F5-Kopie ist bytegleich; reguläres Fensterschließen endet mit 0.

Danach installiert der Paketmanager das neue DEB 0.1.123. Alle neun vorhandenen
Konfigurations- und Testdateien bleiben bei dieser Paketwirkung bytegleich.
Die installierten GUI-/CLI-Hashes entsprechen den aus dem geprüften DEB
entnommenen Nutzlasten. Die neue installierte App startet mit derselben Sandbox,
liest das alte Queue-v8-Journal und erhält dessen abgeschlossene Kopie im
aktuellen v27-Journal. F2, F5 und F6 sowie zwei vollständig geladene Fenster
bestehen erneut.

Strg+M minimiert nur das fokussierte zweite Fenster. Nach dem Wiederherstellen
schließt der echte Menüeintrag ebenfalls nur dieses Fenster. Ein pausierter
künstlicher Kopierauftrag verhindert Strg+W mit dem sichtbaren Hinweis
„Fenster bleibt offen“. Strg+Q öffnet die bestehende Rückfrage. „Hier bleiben“
erhält den Auftrag; nach Fortsetzen wird er bytegleich abgeschlossen. Der echte
Beenden-Menüeintrag beendet die nun saubere App mit 0.

Nach Entfernen und Purge fehlen GUI, CLI, Desktopdatei und Icon. Alle 15 zu diesem
Zeitpunkt vorhandenen Konfigurations- und Testdateien bleiben bytegleich.
Beide eigenen Container und alle eigenen Testanzeigen werden anschließend entfernt.
Die betroffenen Fenster, Menüs und Schutzdialoge wurden visuell anhand ihrer
Screenshots geprüft. Die Sicherheitsvorgaben der Container wurden nicht gelockert.

## Gates und Prüfgrenzen

Die vollständigen App-Suiten bestehen auf macOS mit 220 Tests und zwei
umgebungsgebunden ignorierten Fällen sowie auf Linux mit 210 Tests und vier
ignorierten Fällen. Workspace-Clippy für alle Targets mit `-D warnings` und
Formatprüfung bestehen auf beiden Plattformen. Die 385 erfassten prüfrelevanten
Dateien bleiben während der Läufe und des Paketbaus unverändert. Die beiden
Modulscannerfälle bestehen nach der Callbackverlagerung ebenfalls auf beiden
Plattformen. Anschließend präzisierte Menükommentare verändern keinen Rustausdruck.

Der erste macOS-App-Aufruf innerhalb der Werkzeug-Sandbox erreicht AppKit-Dienste
nicht und scheitert außerdem im isolierten Setup-Unterprozess. Der freigegebene
vollständige Lauf außerhalb dieser Werkzeugbeschränkung besteht mit identischem
Quellstand; die Clipboardfälle verwenden private Pasteboards.

Core, CLI, Fuzz und Frontend bleiben gegenüber den zuletzt vollständig geprüften
Ständen bytegleich. Ihre früheren Gates werden weiterverwendet und nicht als
neu ausgeführt gezählt. Für den Paketprüfer bestehen zwölf gezielte Fälle;
die gesamte Werkzeugsuite besteht mit 149 macOS- beziehungsweise 130 Linux-Fällen
und 19 Linux-Skips.
Das Menüdelta ist ein begrenzter Review der beiden geänderten Rust-Dateien
einschließlich ihrer Fenster-/Exitgrenzen, kein neuer Ganzbereichsreview.

| Menü-Kandidat vor dem Icon-Delta | Bytes | SHA-256 |
| --- | ---: | --- |
| DEB 0.1.123 amd64 | 19.840.384 | `8dab707b1fd6b2849495a9029e0cafb3e88d2743fb7b50845d80ed9299cbb5bc` |
| AppImage 0.1.123 amd64 | 94.788.088 | `f045abaaafbd9eff05e82bf48c38e555b0bfa9960f0eb87d03292a0ea52462fc` |

## Menschliche Funktionsabnahme und Dockicon

Die normale, entsperrte Cinnamon-Gerätesitzung startet genau das oben gebundene
AppImage über `tools/run-test-sandbox.sh` mit frischen künstlichen Dateien. Die
menschliche Rückmeldung zu Darstellung, F2, F5, F6 und zweitem Fenster lautet
„Abnahme bestanden“. Die tatsächlich umbenannte und kopierte Datei sowie die
verschobene Datei werden anschließend bytegenau nachgelesen; die Movequelle fehlt
wie erwartet. Beide Queueaufträge sind abgeschlossen.

Danach wird das fehlende Dockicon gemeldet. Am Haupt- und Nebenfenster fehlt die
X11-Eigenschaft `_NET_WM_ICON`. Ein unabhängiger GTK-Lauf mit eigenen Fenstern
reproduziert: 1024- und 512-Pixel-Icons werden verworfen, 256 Pixel funktionieren.
GTK begrenzt die Eigenschaft auf höchstens 262144 CARDINALs; schon 512² plus die
beiden Größenfelder überschreitet diese Grenze. Die
[GTK-Implementierung](https://github.com/GNOME/gtk/blob/3.24.52/gdk/x11/gdkwindow-x11.c)
und der [EWMH-Iconvertrag](https://specifications.freedesktop.org/wm/latest-single/)
erklären Format und Grenze.

Eine gemeinsame Linux-Funktion dekodiert die gewählte Variante und skaliert sie
auf 256 Pixel. Sowohl neue Fenster als auch Änderungen der globalen Icon-Wahl
verwenden diese Funktion. Das Hauptfenster erhält nun ebenfalls ausdrücklich
die gespeicherte Variante. Ein kosmetischer Fehler bleibt im vorhandenen
geheimnisfreien Fehlerlog; die Fenster-/Queueverträge ändern sich nicht.

Die neue Linux-App-Suite besteht mit 211 Tests und vier ignorierten Fällen;
ein zusätzlicher Fall prüft alle sechs Varianten auf gültige RGBA-Daten und die
GTK-Grenze. macOS besteht seriell mit 220 Tests und zwei ignorierten Fällen.
Ein erster paralleler macOS-Lauf scheitert einmal in der unveränderten
Finder-Metadatenprobe; derselbe Fall besteht einzeln und danach in der gesamten
seriellen Suite. Clippy und Formatprüfung bestehen auf beiden Plattformen.
Alle 385 erfassten Dateien bleiben während Gates und erneutem Standardpaketbau
unverändert. Drei unabhängige Signaturprüfungen und Prüfsummen bestehen;
eine vertauschte Payloadsignatur wird weiterhin abgelehnt.

Das neue AppImage liefert am Haupt- und Nebenfenster tatsächlich 256 × 256 Pixel
an `_NET_WM_ICON`; sämtliche ARGB-Werte entsprechen dem gewählten PNG.
Ein Wechsel im echten Einstellungsdialog auf Türkis und zurück auf Blau erreicht
beide Fenster mit den richtigen Pixeln. Die tatsächlich aus dem neuen DEB
entnommene GUI besteht dieselbe Haupt-/Nebenfensterprobe. Beide privaten
App-Läufe enden regulär mit 0. Der neue DEB-Lauf ist keine erneute
Paketinstallation; deren Beleg steht im ersten Gegenlauf. Die beiden
Bundleformate besitzen wegen Tauris Pakettyp-Patch unterschiedliche ELF-Hashes.

Der korrigierte AppImage-Kandidat ist anschließend in der normalen Gerätesitzung
mit einer neuen markierten Sandbox geöffnet. Sein Hauptfenster liefert auch
dort die richtige Icongröße. Die kleinen tatsächlich übertragenen Icon-Pixel
und das eigene Fenster sind visuell geprüft. Die menschliche Dock-Sichtkontrolle
bestätigt am 2026-10-19: „Ja, Icon ist sichtbar“. Damit ist auch dieser
Folgebefund abgenommen. Die eigene Test-App wird anschließend regulär beendet.

| Aktueller Icon-Kandidat | Bytes | SHA-256 |
| --- | ---: | --- |
| DEB 0.1.123 amd64 | 19.840.154 | `f8ed4c06a99b35f2459b39a7ce9a53daa4ac9b5d3f1ae4c11685766973b829d9` |
| AppImage 0.1.123 amd64 | 94.792.184 | `c99a4fc154bde5a8b5791c5578681a0626651f11cd591f4881bb9ff633ff92cb` |

Die Geräteabnahme erteilt keine Veröffentlichungsfreigabe.
