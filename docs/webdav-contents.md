# Additive WebDAV-Downloads

Stand: 2026-10-24.

Gewöhnliche additive WebDAV-Kopien in einen lokalen Ordner verwenden eine private
Best-Effort-Lesequelle. Sie umfasst direkte Mehrfachauswahl, reguläre Dateien und
Verzeichnisbäume. Ganze mehrdeutige Case-/NFC-Namensgruppen werden ausgelassen;
unabhängige Inhalte bleiben übertragbar. Ausgelassene Ordner werden nicht betreten.
Ersetzen, Move, Remote-Ziele, Snapshots und strikter Sync behalten ihre bisherigen
Quellenverträge.

## Umfang und bedingter Inhalt

Der Core hält vollständige Depth-1-Listings der ausgewählten Verzeichnisse.
Innerhalb dieser Bäume bindet der Beleg sämtliche Namen, Attribute und gelieferten
Provider-Tokens. Außerhalb bindet er nur benötigte Auswahl- und Vorfahrnamen;
Änderungen unselektierter Geschwister blockieren keine unabhängige Kopie und
ändern nicht deren Digest. Direkte Gruppen entstehen aus der tatsächlichen
Mehrfachauswahl, rekursive Gruppen aus dem vollständigen Geschwisterbestand.

Ausgelassene Mitglieder und ihre beobachteten Vorfahren bleiben vor weiteren
Wirkungen und vor dem lokalen Commit driftgebunden. Die Fensterqueue persistiert
den konkreten Umfang mit Digest und Auslassungsbilanz. Neue oder veränderte
Gruppen benötigen nach Neustart erneut Zustimmung. Aktive Generation und
Serveridentität bleiben während des Versuchs gebunden; die Generation steht
nicht im restartstabilen Digest.

Ein GET verwendet ausschließlich die starke ETag aus dem vorbereiteten
Elternbeleg als `If-Match`. Der Adapter übernimmt keine unmittelbar vor dem GET
neu gefundene Dateiversion. HTTP 412 oder eine abweichende Antwort-ETag meldet
Drift. Streamlänge, Überlänge, erfolgreicher EOF und frische Quellbelege werden
vor dem lokalen Temp-Commit geprüft. Abbruch und Sitzungswechsel beenden den
Versuch; es gibt keine blinde Fortsetzung des GET nach Reconnect.

## Lokale Temp-Recovery

Queue-Läufe verwenden bevorzugt einen privaten dauerhaften Tempindex. Ein
Prozessabbruch nach Teilübertragung lässt die Queue beim Neustart als unterbrochen
stehen; bekannte Tempdateien bleiben konkrete Prüfpfade und starten keinen
zweiten Versuch automatisch. Fehlt ein exklusiver Queue-Namespace für diesen
Index, bleibt die normale Kopie möglich. Ihre Ergebnisbilanz benennt dann die
fehlende dauerhafte Temp-Recovery und mögliche ungebundene Nachbardateien nach
einem Prozessabbruch. Daraus entsteht kein Lösch- oder Wiederholungsrecht.

## Aufwand und Grenzen

Reine Navigation verwendet gehaltene Metadaten; Ordnergrenzen prüfen ihr eigenes
Listing frisch. Vollständige benötigte Vorfahren werden an Inhaltsgrenzen
geprüft. Datei-, Auswahl- und Baumtiefe besitzen keine neuen pauschalen Limits;
vorhandene Protokoll-/Parsergrenzen gelten weiter. Der Baumbeleg braucht Speicher
proportional zum beobachteten Bestand.

WebDAV bindet GET nicht atomar an die vollständige Elternkette. Ein unbemerkter
Vorfahrentausch bleibt möglich. Der vorhandene Parser unterscheidet Collections
und gewöhnliche Ressourcen; serverseitig als normale Ressource dargestellte
Unix-Symlinks sind nicht erkennbar. Unbekannte Ressourcentypen behalten die
bestehende Parserabsage. Die neue Klasse behauptet deshalb keine allgemeine
Symlink- oder Metadatenfreigabe. GUI und CLI erhalten den konkreten
Best-Effort-Lesehinweis. Der lokale Temp-, No-Replace- und Abschlussvertrag bleibt
erhalten. [Prüfstand](roadmap-2026-10-24.md#additive-webdav-downloads).
