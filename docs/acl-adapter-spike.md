# ACL-Adapter: Spike und Produktionsschnitt

Stand: 2026-07-10

Status: Produktionspfad und Sync-Übertragung auf macOS und Linux-Testhost erfolgreich.

## Ergebnis

Der ursprüngliche Spike bewies den fd-basierten Systemzugriff. `core/src/acl.rs` hat
ihn inzwischen als Produktionsadapter ersetzt. Der Kern liest, schreibt, entfernt und
verifiziert ACLs ausschließlich über einen bereits sicher geöffneten Dateideskriptor:

- macOS nutzt `acl_get_fd_np`/`acl_set_fd_np` aus libSystem. Einträge enthalten
  UUID-Prinzipal, Allow/Deny, Rechte und Vererbungsflags in stabiler Reihenfolge.
- Linux liest und schreibt die Kernel-Binärformate `system.posix_acl_access` und
  `system.posix_acl_default` per `fgetxattr`/`fsetxattr`/`fremovexattr`. Die DTOs
  unterscheiden UserObject/User/GroupObject/Group/Mask/Other samt numerischer ID.
- Textausgaben, Namen, Pfad- und Shell-Fallbacks bleiben ausgeschlossen.

ACLs sind typisiert im `MetadataManifest` enthalten und ändern dessen kanonischen
Digest. Der Sync bindet Quelle und vorhandenes Ziel an diesen Digest, schreibt Datei-
ACLs nur auf die neue Temp-Datei und Ordner-ACLs post-order. Owner folgt zuerst, ACL und
xattrs danach, anschließend normale Modusbits und ganz zuletzt Sonderbits. Der Readback
prüft ACL und Modus gemeinsam.

## Verifikation

- macOS: 38 Kern-Modultests, 24 Dateiops- und 48 Sync-Tests grün.
- Linux-Testhost: 39 Kern-Modultests, 24 Dateiops- und 49 Sync-Tests grün.
- Je Plattform prüft ein echter ACL-Roundtrip Schreiben, Manifest-Readback und Entfernen.
- Ein Sync-Test überträgt ACL und gekoppelte Modusbits atomar auf ein vorhandenes Ziel.
