# Aktiver Backlog

Stand: 2026-11-18. Nur offene Arbeit; Reihenfolge und Releaseumfang stehen in der
[Roadmap](docs/roadmap.md). Verfügbares Verhalten: [Funktionsmatrix](docs/function-audit.md).

## Später, ohne Releasepflicht

- Weitergehende SMB-ACE-/Vererbungsmodelle, SID-/Domänenmigration, vollständiger
  SMB-Hardlink-Schreibvertrag und DAV-ACL-Editor, sofern kein konkreter
  ForkLift-Gegenbeleg sie zur Paritätslücke macht.
- Fortsetzbare Transfers für SMB, unverschlüsseltes FTP und SFTP- oder FTPS-Server
  ohne vollständigen SHA-256-Beleg sowie für entfernte Schreibziele.
  Fertig mit belastbarem Teilbereichs- und Tempidentitätsbeleg über die
  Wiederverbindung hinweg.
  [Ideensammlung](docs/product-opportunities.md).
- Weitere TAR-Varianten bei konkretem Bedarf. GNU-Sparse und unbekannte
  PAX-Erweiterungen haben bislang eine sichtbare Teilkataloggrenze.
  [Archivansicht](docs/archive-read.md).

## Pflege

Neue Punkte nennen Wirkung, Fertigkriterium und höchstens einen Detailverweis.
Erledigtes aus dieser Liste entfernen; Änderungen ins Changelog, Prüfbelege ins
zuständige Vertrags-/Prüfdokument. Keine Commitchronik oder wiederholten
Testzahlen im Backlog. Historie: [Stand bis 2026-10](docs/history/backlog-history-2026-10.md).
