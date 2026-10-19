# Beobachtete SFTP-Ordnergrößen

Stand: 2026-10-18. Größenspalte und Informationsdialog verwenden für SFTP eine
eigene begrenzte Metadatenbeobachtung. Sie öffnen keine Dateiinhalte und geben
weder ein vollständiges Sync-VFS noch Schreibfähigkeiten frei.

## Umfang und Ergebnis

Pro Anfrage sind 1 bis 100 eindeutige direkte Ordner unter dem sichtbaren Pane-Root
zulässig. Alle Pfade und Namen werden vor Providerzugriff geprüft. Je Ordner gelten
1000 Baumeinträge, 16 Unterordnerebenen und 1 MiB Namen pro Listing. Der gesamte
Lauf hat 60 Sekunden zuzüglich laufendem Request und Handle-Cleanup. Größen werden
mit geprüftem `u64`-Addieren summiert; ein Überlauf erzeugt keinen Größenwert.

Der gemeinsame SFTP-Baumscanner bindet Hostkey, Sitzung, REALPATH/LSTAT-Elternbelege
und beobachtet READDIR bis zum tatsächlichen EOF. Leere NAME-Seiten sind kein EOF;
doppelte Exaktnamen bleiben ungültig. Case- und Unicode-Varianten dürfen bei dieser
reinen Metadatenwirkung getrennt gezählt werden, wenn der Server ihre exakten Pfade
belegt. Die konservative Kollisionsprüfung für lokale Downloadziele bleibt erhalten.

Eine Bilanz ohne Auslassungen trägt den Status `observed`, eine Bilanz mit
ausgelassenen Symlinks, Spezialdateien oder unbekannten Größen `partial`. Fehler
eines Ordners verhindern unabhängige Ergebnisse nicht. Abbruch und ein Wechsel
der Sitzung entwerten den betroffenen Lauf. Alle Ergebnisse werden frisch gelesen;
der Inhalts- oder Größen-Sitzungscache liefert keine vermeintlich neue Beobachtung.

Größenspalte und Informationsdialog markieren die Werte mit „≈“ und Best Effort.
Die Informationen nennen beobachtete Datei-/Unterordnerzahlen, Auslassungen und
Grenzen. Partielle Werte werden nicht als exakte Gesamtsumme oder belastbare
Sortiergröße ausgegeben. Ein `observed`-Wert darf numerisch sortiert werden und
behält dabei seine sichtbare Kennzeichnung.

## Sicherheitsgrenze

SFTP v3 bindet OPENDIR nicht atomar an den gewählten Root. Ein unbemerkter
Pfadtausch zwischen den Prüfungen bleibt möglich; die Bilanz ist keine atomare
Momentaufnahme. Die Metadatenbeobachtung ändert diesen Protokollvertrag nicht.

Metadaten benötigen keine 8-GiB-Datei- oder 16-GiB-Baumgrenze: Es werden keine
Inhalte übertragen. Der gemeinsame Scanner unterscheidet deshalb intern Inhalt
und Metadaten. Der Metadatenkontext und seine Zusammenfassung bleiben privat und
werden niemals als VFS-Kopierquelle oder Queue-Baumbeleg herausgegeben. Nur der
Inhaltszweck erzeugt den validierten `InteractiveSourceTree`; dessen ursprüngliche
Bytegrenzen und Änderungszeitbelege gelten unverändert. Eine fehlende Änderungszeit
allein sperrt die reine Größenbeobachtung nicht, eine fehlende Dateigröße schon.

Die Tauri-Laufzeit behält die ursprüngliche Provider-Arc, Profilrevision, Generation,
Fenster-/Pane-Kennung und Frontend-Token. Ereignisse gehen nur an das auslösende
Fenster. Navigation bricht die eigene Messung ab und nimmt keine verspätete Bilanz
für eine andere Pane an. Die Mutations-Queue wird nicht belegt.

## Prüfung

Die künstlichen SSH-/SFTP-Paketgegenläufe prüfen leere Ordner, große Größen ohne
OPEN, fehlende Zeiten, Auslassungen, Case-/Unicode-Varianten, frisches Nachmessen,
Einzelordnerfehler, Eingabegrenzen, Tiefe, Überlauf, Abbruch und Sitzungswechsel.
Dieselben Gegenläufe erhalten Inhaltsbytegrenzen und strikte VFS-Sperren. Browserfälle
prüfen Größenspalte, zugängliche Beschriftung, Informationsdialog und verspätete
Ereignisse nach Navigation.

## Native Prüfung

Die native macOS-Abnahme ist mit künstlichem OpenSSH-Root bestanden: frische
beobachtete Größen, partielle Bilanzen mit Links und FIFO, leere Ordner, eine
9-GiB-Datei ohne Inhaltslesen, Tiefengrenze, Abbruch während unabhängiger
Navigation und erkannter Elternwechsel. Neustart sowie helles und dunkles Theme
sind bedient und visuell geprüft. Der Protokollbeleg enthält während der
Größenmessungen keinen Datei-OPEN und keine Mutation. Die additive Inhaltskopie
ist davor gesondert geprüft; reine Metadaten lockern deren Bytegrenzen nicht.
