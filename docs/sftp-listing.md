# SFTP-Anzeigelisting und Auslassungen

Stand: 2026-10-18.

Die interaktive SFTP-Liste bleibt eine pfadbasierte Best-Effort-Anzeige. Root,
Elternkette, Hostkey und Sitzung werden geprüft; fehlende atomare Namespacebindung
wird dadurch nicht ersetzt. `complete` bleibt falsch. Diese Anzeige liefert weder
Plannerbelege noch Schreibrechte.

Der gemeinsame READDIR-Transport schließt den Verzeichnishandle auch nach Abbruch
und Fehler genau einmal. Leere NAME-Pakete sind kein EOF. Die Anzeige hat eine
Lesefrist von 60 Sekunden und Grenzen von 100.000 Einträgen, 16 MiB Namen und
100.003 Antwortseiten; ein laufender Request und sein Cleanup können jeweils noch
bis zu 15 Sekunden benötigen. Überschreitungen veröffentlichen keine Teilliste.
Der Inhalts-/Metadatenscanner nutzt denselben Transport mit seinen bestehenden
strengeren Grenzen; ein sekundärer Closeverlust entwertet weiterhin die nächste
Lesesitzung, ohne den primären Abbruch zu verdrängen. Nicht ausgewertete
SFTP-Langnamen werden pro Antwort verworfen und nicht über alle Seiten gesammelt.

Ein vollständig empfangenes Listing wird pro Eintrag ausgewertet. Ein Name, der
kein sicher bedienbares einzelnes Pfadsegment ist, und ein Eintrag ohne belegten
Dateityp werden ausgelassen. Unabhängige gültige Dateien bleiben sichtbar und über
den bestehenden begrenzten Inhaltsweg nutzbar. Symlinks und belegte Spezialtypen
bleiben als solche sichtbar; eine fehlende Dateigröße bleibt `null`, echte 0 Byte
bleiben 0. Fehlende Zeiten und Eigentümer werden nicht erfunden.

`omitted` enthält Anzeigenamen und Gründe. `display_name` maskiert Steuerzeichen
und Backslashes; der Wert ist ausdrücklich kein verwendbarer Dateipfad.
Die Einstellung für versteckte Einträge gilt auch für diese Bilanz. Die GUI
zeigt Anzahl und eine aufklappbare, schreibgeschützte Textbilanz direkt in der
verursachenden Pane. Der Text ist scrollbar und vollständig auswählbar, auch bei
großen Bilanzen. Er wird nicht als HTML ausgeführt. Navigation, Profilwechsel und
fehlgeschlagene Listings verwerfen die alte Bilanz; verspätete Antworten bleiben
an das bestehende Lesetoken gebunden. Die Operationsanzeige wird nicht ersetzt.
Die CLI liefert dieselben Auslassungen in Text und JSON.

Sind alle sichtbaren Einträge ausgelassen, zeigt die Pane „Keine anzeigbaren
Einträge“ neben der vollständigen Auslassungsbilanz. Ein tatsächlich leeres
Listing behält die Beschriftung „Leerer Ordner“.

Doppelte Namen, Protokollfehler, Sitzungsdrift und unvollständige Antworten liefern
weiterhin keine scheinbar erfolgreiche Teilliste. Noch nicht als UTF-8 dekodierbare
Protokollnamen können mit dem verwendeten Parser nicht einzeln isoliert werden;
dieser Fehler stoppt weiterhin das Listing. Rekursive Inhaltsvorbereitung und
Metadatenscans behalten ihre eigenen strengeren Namens-, Mengen- und Fristgrenzen.
Insbesondere werden bestehende Sync-, Mirror- und Mutationsfähigkeiten nicht
geändert.

Reale SSH-/SFTP-Pakete prüfen paginierte Auslassungen und einen weiter lesbaren
unabhängigen Dateiinhalt. Doppelte Namen, Antwortverlust nach einer gültigen Seite,
leere NAME-Pakete und
Closeverlust bleiben Gegenbelege. Ein echter Fristlauf mit endlosen Punktseiten
prüft den Abbruch nach 60 Sekunden und das anschließende Schließen.
Browserfälle prüfen Tastaturbedienung, maskierten Text, unabhängige Panes,
Navigation, eine sonst leere Liste und eine vollständige Bilanz von 4000 Einträgen.

Die native macOS-Sandbox-Abnahme belegt beide Themes, frischen Neustart,
unbekannte Größen und den wirklichen additiven F5-Download einer gültigen Datei
neben ausgelassenen Einträgen. Beim Einklappen kehrt der Fokus auf den Knopf
zurück; die Leertaste öffnet den Bericht erneut. Die Quellen blieben unverändert,
Anzeigezugriffe öffneten keine Dateiinhalte und SFTP-Mutationen fanden nicht statt.
Die eigenen Testressourcen wurden anschließend vollständig freigegeben.
