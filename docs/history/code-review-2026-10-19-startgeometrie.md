# Startgeometrie ohne Warten auf einen Einstellungswrite

Der historische Fund enthält bereits korrigierte Teile: Einstellungscommands
laufen asynchron, Close-Writes im Hintergrund und Remember-Aufträge haben eine
Sequenzbindung. Der aktuelle Rest liegt im nativen Fensterbau:
`apply_restored_geometry` liest über `restored_window` denselben Store-Mutex,
den ein Hintergrundwrite bis nach der dauerhaften Dateiumstellung hält.

Der tatsächliche native Gegenlauf hält ausschließlich das atomare Rename eines
künstlichen Settings-Temps unter der markierten App-Sandbox an. Danach wird über
den normalen Fensterbefehl ein zweites Fenster angefordert. Der Hauptthread
wartet nachweislich in `restored_window` und `apply_restored_geometry`; ein anderer
Thread steht im gehaltenen `write_document`. Die native Fensterabfrage liefert
keine Fenster und die Größenänderung Fehlercode 3. Nach der Freigabe sind beide
Fenster erreichbar. Quelldatei und Ziel bleiben unverändert; Prüfauftrag und
eigenständiges Cleanup enden mit Exitcode 0.

## Begrenzte Korrektur

`DesktopState` bindet ausschließlich die validierten Startgeometrien vor dem
ersten Fensterbau in einer eigenen kleinen Map. Die Geometrie wird dort einmalig
entnommen. Der native Fensterpfad greift nicht mehr auf den schreibenden Store zu
und klont nicht mehr den vollständigen gespeicherten Fensterstand. Ein neues
Fenster ohne gespeicherte Geometrie erhält weiterhin die Standarddarstellung.
Der vollständige Live- und Dateistand wird durch das Entnehmen nicht verändert.

Die Map wird nur beim bisherigen Einsetzen der gespeicherten Sitzung befüllt;
kein Dateischreiben hält ihren Mutex. Die Format-, Monitor- und Größenprüfung
bleibt unverändert. Bestehende Schreibserialisierung, Remember-Sequenzen,
Close-Writes und Exit-Barriere werden nicht umgebaut. Das Dateiformat bleibt
unverändert; der zusätzliche Zustand ist rein prozessintern.

Ein tatsächlicher Datei-/Zweithreadtest lädt gespeicherte Geometrie, hält den
schreibenden Store und liest die Startgeometrie dennoch rechtzeitig aus. Danach
sind Live-Stand und Datei weiterhin vollständig, während nur der einmalige
Startbeleg verbraucht ist. Alle 17 Einstellungs-Tests bestehen.

## Prüfung und Abgrenzung

Der erste native Nachherlauf bestätigt die korrigierte Liveness: zwei Fenster
werden während des gehaltenen Writes in 52 ms abgefragt; die Größenänderung
gelingt in 15 ms und ein echter Rahmen ist 1160×820 groß. Die zusätzlich
angehängte Neustartprüfung scheiterte vor dem Einstellungsdialog an einem
programmatischen `⌘,`-Textkeystroke. Daraus wird kein physischer Shortcutfehler
abgeleitet. Für die zusätzliche Prüfung wird die Wiederherstellung daher nur
in der künstlichen Konfiguration vor Appstart aktiviert; Geometrien entstehen
weiterhin durch echte Fenster und App-Writes. Produktquellen bleiben dabei
unverändert.

Eine kleine direkte C-fsync-Halteprobe traf den tatsächlichen Rust-
`File::sync_all`-Einstieg nicht. Diese Ausschlussprobe startete keine Test-App
und zählt nicht als Produktgegenlauf. Die anschließende kleine Rust-Rename-Probe
bestätigt genau den im nativen Gegenlauf verwendeten Haltepunkt vor seiner Wirkung.
Es werden keine globalen Synchronisationsfunktionen oder fremden Dateien blockiert.

Der vollständig bestandene native Nachherlauf bestätigt den freien Hauptthread, beide erreichbaren Fenster während des gehaltenen Writes und den tatsächlichen Zwei-Fenster-Neustart mit exakt erhaltenen Größen 1160×820 und 1180×850 sowie Theme. Prüfauftrag und Cleanup bestehen; beide wiederhergestellten Fenster wurden visuell geprüft. Die finalen App-Suiten bestehen mit 220 Tests auf macOS und 210 auf Linux; 2 beziehungsweise 4 Fälle bleiben ignoriert. Explizite Kindtests werden über ihre Eltern ausgeführt; zwei Linux-GTK-Fälle behalten ihre Umgebungsvoraussetzung. Workspace-Clippy mit allen Targets und `-D warnings` sowie Formatprüfung bestehen auf beiden Systemen. Alle 385 Quellen sind während der Läufe unverändert. Core, CLI, Fuzz und Frontend sind bytegleich zum unmittelbar vorherigen vollständigen Prüfstand; dessen beidseitige Core-/CLI-/Fuzzgates und 408 Browserprüfungen einschließlich CSP plus sechs Pane-Tabtests werden ausdrücklich übernommen. Die vollständige neue Prüfung betrifft App, Clippy und Format; es wird kein erneuter Core-/CLI-/Browserlauf behauptet. Das Delta
betrifft nur die Startgeometrie und ihre direkten Store-/Fenstergrenzen;
historische Vollreviewhashes bleiben erhalten.
