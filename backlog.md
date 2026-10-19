# Offene Produktarbeit

Stand: 2026-10-19 · Preview 0.1.124.

Die erste öffentliche Preview bietet lokale Dateioperationen, mehrere Fenster,
Sync-Vorschau, Quarantäne, Papierkorb und die in der
[Funktionsmatrix](docs/function-audit.md) beschriebenen Remote-Funktionen.
Sie ist keine Freigabe für vollständige Providerparität oder Version 1.0.

Die nächsten Produktziele stehen in der [Roadmap](docs/roadmap.md):

- SFTP-Mutationen mit konkreten Pfad-, Identitäts- und Recoveryverträgen.
- Problematische Namen und Metadaten konsistent pfadgenau bilanzieren.
- Schnellfilter und begrenzte, abbrechbare rekursive Namenssuche.
- Integrierte Vorschau und später Remote-Bearbeitung mit geprüftem Rückupload.
- Duplizieren, Stapelumbenennung und Archivarbeit mit Vorschau und Konfliktprüfung.

Bekannte Grenzen bleiben sichtbar: SFTP unterstützt nur seine begrenzte
Leseklasse; gewöhnliches WebDAV-Ersetzen besitzt derzeit keine ausreichende
gemeinsame Bindung von Altziel und Eltern. Alte Journale und unbelegte Arbeitsreste
werden nicht nach Alter automatisch gelöscht.

Ein FTP-/FTPS-Endpunkt mit mehreren Serverknoten benötigt eine je Knoten gebundene
Capability-Probe oder Probe und Mutation in derselben Sitzung. Die derzeitige
Freigabe setzt den geprüften Einzelserververtrag voraus.

Fehler und nachvollziehbare Reproduktionen können über
[GitHub Issues](https://github.com/DanielMuellerIR/Doppeldecker/issues) gemeldet
werden. Bitte nur künstliche Daten verwenden und private Pfade oder Zugangsdaten
vor dem Teilen entfernen.
