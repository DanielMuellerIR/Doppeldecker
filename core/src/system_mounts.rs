//! Sichtbare lokale Volumes und gemountete Netzwerkfreigaben.
//!
//! Die Erkennung bleibt GUI-frei und liefert ausschließlich lesbare Mounts samt
//! Speicherbilanz. Linux wird bytegenau aus `/proc/self/mountinfo` gelesen; macOS nutzt
//! `getfsstat`. Pseudo-Dateisysteme verschwinden aus der Nutzerliste. Ein Mount trägt
//! zusätzlich eine nicht serialisierte Auswurfstrategie, damit das Frontend niemals
//! einen freien Pfad oder einen Systembefehl erfinden kann.

use std::ffi::OsStr;
use std::io;
#[cfg(target_os = "macos")]
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

#[cfg(target_os = "macos")]
use std::ffi::CStr;
#[cfg(target_os = "linux")]
use std::ffi::OsString;
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStringExt;

#[cfg(target_os = "linux")]
mod linux_space;

#[cfg(target_os = "linux")]
const MAX_MOUNT_TABLE_BYTES: usize = 8 * 1024 * 1024;
const MAX_MOUNTS: usize = 16_384;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemMountKind {
    Internal,
    External,
    Network,
    /// Lokales macOS-Volume, dessen Disk-Arbitration-Merkmale noch nicht durch
    /// `diskutil info -plist` ergänzt wurden. Es bleibt sichtbar, aber nicht
    /// auswerfbar, bis die Plattformschicht die Eigenschaft positiv belegt.
    Volume,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SystemUnmount {
    MacDiskutil { mount_path: PathBuf },
    LinuxGio { mount_path: PathBuf },
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct SystemMount {
    /// Opaque Bindung an Mountpfad, Quelle, Dateisystem und Plattformidentität.
    /// Freier Frontendtext wird beim Auswerfen nie als Pfad verwendet.
    pub id: String,
    pub name: String,
    /// Nur verlustfrei dekodierbare Pfade sind im Frontend navigierbar. Die interne
    /// Auswurfstrategie behält den rohen `OsString` unabhängig davon.
    pub path: Option<String>,
    pub kind: SystemMountKind,
    pub filesystem: String,
    pub total_bytes: u64,
    pub available_bytes: u64,
    /// Der Mount bleibt auch ohne belegbare Speicherbilanz sichtbar. Größen sind
    /// dann unbekannt; der Fehler wird mit stabilem Code an die Oberfläche gereicht.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub space_error: Option<SystemMountSpaceError>,
    pub read_only: bool,
    pub ejectable: bool,
    #[serde(skip)]
    pub unmount: Option<SystemUnmount>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct SystemMountSpaceError {
    pub code: crate::ErrorCode,
    pub message: String,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug)]
struct Space {
    total: u64,
    available: u64,
}

fn product(left: u64, right: u64) -> u64 {
    (u128::from(left) * u128::from(right)).min(u128::from(u64::MAX)) as u64
}

fn mount_id(
    platform: &[u8],
    path: &[u8],
    source: &[u8],
    filesystem: &[u8],
    extra: &[u8],
) -> String {
    let mut hash = blake3::Hasher::new();
    for field in [platform, path, source, filesystem, extra] {
        hash.update(&(field.len() as u64).to_le_bytes());
        hash.update(field);
    }
    hash.finalize().to_hex().to_string()
}

fn display_name(path: &Path, root_name: &str) -> String {
    if path == Path::new("/") {
        return root_name.to_string();
    }
    path.file_name()
        .and_then(OsStr::to_str)
        .filter(|name| !name.is_empty())
        .unwrap_or("Nicht darstellbares Volume")
        .to_string()
}

fn sort_mounts(mounts: &mut [SystemMount]) {
    mounts.sort_by(|left, right| {
        left.kind
            .cmp(&right.kind)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            .then_with(|| left.id.cmp(&right.id))
    });
}

/// Entscheidet ausschließlich über die Sichtbarkeit in der macOS-Geräteliste.
/// `MNT_DONTBROWSE` bedeutet auch bei einem als Netzwerkziel erkannten Mount, dass
/// macOS ihn nicht als nutzerrelevantes Volume anbietet. Der Systemroot bleibt die
/// einzige Ausnahme, damit die Geräteliste immer das laufende System enthält.
#[cfg(any(target_os = "macos", test))]
fn macos_mount_is_visible(path: &Path, network: bool, dont_browse: bool) -> bool {
    if path == Path::new("/") {
        return true;
    }
    !dont_browse && (network || path.starts_with("/Volumes/"))
}

/// Ermittelt die aktuell sichtbaren, nutzerrelevanten Mounts. Ein zwischen Lesen und
/// Stat nicht mehr erreichbarer Mount bleibt mit unbekannter Speicherbilanz und
/// Fehler sichtbar. Fehler beim Lesen der Mounttabelle bleiben ein Abruffehler.
pub fn list_system_mounts() -> io::Result<Vec<SystemMount>> {
    #[cfg(target_os = "macos")]
    {
        list_macos_mounts()
    }
    #[cfg(target_os = "linux")]
    {
        list_linux_mounts()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Mount-Erkennung wird auf dieser Plattform nicht unterstützt",
        ))
    }
}

/// Ergänzt ein lokales macOS-Volume um die von Disk Arbitration belegten Merkmale.
/// Der Parser nimmt ausschließlich die geheimnisfreien Felder aus `diskutil info
/// -plist`; Gerätequelle und sonstige Plist-Inhalte verlassen den Core nicht.
pub fn apply_macos_diskutil_info(mount: &mut SystemMount, bytes: &[u8]) -> io::Result<()> {
    if matches!(mount.kind, SystemMountKind::Network) {
        return Ok(());
    }
    let value = plist::Value::from_reader(std::io::Cursor::new(bytes))
        .map_err(|error| invalid(format!("diskutil lieferte kein lesbares Plist: {error}")))?;
    let info = value
        .as_dictionary()
        .ok_or_else(|| invalid("diskutil-Plist besitzt kein Dictionary als Wurzel"))?;
    let expected = mount
        .path
        .as_deref()
        .ok_or_else(|| invalid("Mount besitzt keinen darstellbaren Mountpfad"))?;
    let actual = info
        .get("MountPoint")
        .and_then(plist::Value::as_string)
        .ok_or_else(|| invalid("diskutil-Plist enthält keinen Mountpunkt"))?;
    if expected != actual {
        return Err(invalid(
            "diskutil-Antwort gehört nicht zum angeforderten Mountpfad",
        ));
    }
    let internal = info
        .get("Internal")
        .and_then(plist::Value::as_boolean)
        .ok_or_else(|| invalid("diskutil-Plist enthält kein Internal-Merkmal"))?;
    let ejectable = info
        .get("Ejectable")
        .and_then(plist::Value::as_boolean)
        .unwrap_or(false)
        || info
            .get("RemovableMediaOrExternalDevice")
            .and_then(plist::Value::as_boolean)
            .unwrap_or(false);
    if let Some(name) = info
        .get("VolumeName")
        .and_then(plist::Value::as_string)
        .map(str::trim)
        .filter(|name| !name.is_empty() && name.len() <= 1024)
    {
        mount.name = name.to_string();
    }
    // Auch ein von einem externen Datenträger gestartetes System bleibt gebunden.
    if internal || expected == "/" {
        mount.kind = SystemMountKind::Internal;
        mount.ejectable = false;
        mount.unmount = None;
    } else {
        mount.kind = SystemMountKind::External;
        mount.ejectable = ejectable;
        mount.unmount = ejectable.then(|| SystemUnmount::MacDiskutil {
            mount_path: mount_path_from_record(mount),
        });
    }
    Ok(())
}

fn mount_path_from_record(mount: &SystemMount) -> PathBuf {
    match mount.unmount.as_ref() {
        Some(SystemUnmount::MacDiskutil { mount_path })
        | Some(SystemUnmount::LinuxGio { mount_path }) => mount_path.clone(),
        None => mount.path.as_deref().map(PathBuf::from).unwrap_or_default(),
    }
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(target_os = "macos")]
fn list_macos_mounts() -> io::Result<Vec<SystemMount>> {
    let count = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
    if count < 0 {
        return Err(io::Error::last_os_error());
    }
    let capacity = usize::try_from(count)
        .unwrap_or(MAX_MOUNTS)
        .saturating_add(16)
        .min(MAX_MOUNTS);
    let bytes = capacity
        .checked_mul(std::mem::size_of::<libc::statfs>())
        .and_then(|value| libc::c_int::try_from(value).ok())
        .ok_or_else(|| invalid("macOS-Mounttabelle ist unplausibel groß"))?;
    let mut records = Vec::<libc::statfs>::with_capacity(capacity);
    let received = unsafe { libc::getfsstat(records.as_mut_ptr(), bytes, libc::MNT_NOWAIT) };
    if received < 0 {
        return Err(io::Error::last_os_error());
    }
    let received = usize::try_from(received).map_err(|_| invalid("Negative Mountanzahl"))?;
    if received > capacity {
        return Err(invalid("macOS schrieb mehr Mounts als angefordert"));
    }
    unsafe { records.set_len(received) };

    let mut mounts = Vec::new();
    for record in records {
        let path_bytes = unsafe { CStr::from_ptr(record.f_mntonname.as_ptr()) }.to_bytes();
        let source = unsafe { CStr::from_ptr(record.f_mntfromname.as_ptr()) }.to_bytes();
        let filesystem = unsafe { CStr::from_ptr(record.f_fstypename.as_ptr()) }.to_bytes();
        let path = PathBuf::from(OsStr::from_bytes(path_bytes));
        let fs_name = String::from_utf8_lossy(filesystem).into_owned();
        let root = path == Path::new("/");
        let network =
            record.f_flags & libc::MNT_LOCAL as u32 == 0 || is_network_filesystem(&fs_name);
        let dont_browse = record.f_flags & libc::MNT_DONTBROWSE as u32 != 0;
        if !macos_mount_is_visible(&path, network, dont_browse) {
            continue;
        }
        let kind = if root {
            SystemMountKind::Internal
        } else if network {
            SystemMountKind::Network
        } else {
            SystemMountKind::Volume
        };
        let unmount = (kind == SystemMountKind::Network).then(|| SystemUnmount::MacDiskutil {
            mount_path: path.clone(),
        });
        let fsid = unsafe {
            std::slice::from_raw_parts(
                (&record.f_fsid as *const libc::fsid_t).cast::<u8>(),
                std::mem::size_of::<libc::fsid_t>(),
            )
        };
        mounts.push(SystemMount {
            id: mount_id(b"macos", path_bytes, source, filesystem, fsid),
            name: display_name(&path, "Systemvolume"),
            path: path.to_str().map(str::to_string),
            kind,
            filesystem: fs_name,
            total_bytes: product(record.f_blocks, u64::from(record.f_bsize)),
            available_bytes: product(record.f_bavail, u64::from(record.f_bsize)),
            space_error: None,
            read_only: record.f_flags & libc::MNT_RDONLY as u32 != 0,
            ejectable: unmount.is_some(),
            unmount,
        });
    }
    sort_mounts(&mut mounts);
    Ok(mounts)
}

#[cfg(target_os = "linux")]
fn list_linux_mounts() -> io::Result<Vec<SystemMount>> {
    let bytes = std::fs::read("/proc/self/mountinfo")?;
    if bytes.len() > MAX_MOUNT_TABLE_BYTES {
        return Err(invalid("Linux-Mounttabelle überschreitet 8 MiB"));
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    parse_linux_mountinfo_with(&bytes, |path| linux_space::query(path, deadline))
}

#[cfg(target_os = "linux")]
fn parse_linux_mountinfo_with(
    bytes: &[u8],
    mut space_for: impl FnMut(&Path) -> io::Result<Space>,
) -> io::Result<Vec<SystemMount>> {
    let mut mounts = Vec::new();
    for line in bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        if mounts.len() >= MAX_MOUNTS {
            return Err(invalid(
                "Linux-Mounttabelle enthält unplausibel viele Einträge",
            ));
        }
        let fields: Vec<_> = line
            .split(|byte| byte.is_ascii_whitespace())
            .filter(|field| !field.is_empty())
            .collect();
        let separator = fields
            .iter()
            .position(|field| *field == b"-")
            .ok_or_else(|| invalid("Linux-Mountinfozeile besitzt keinen Feldtrenner"))?;
        if separator < 6 || fields.len() < separator.saturating_add(4) {
            return Err(invalid("Linux-Mountinfozeile besitzt zu wenige Felder"));
        }
        let mount_number = std::str::from_utf8(fields[0])
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .ok_or_else(|| invalid("Linux-Mount-ID ist keine positive Zahl"))?;
        let path_bytes = decode_linux_mount_field(fields[4])?;
        let options = decode_linux_mount_field(fields[5])?;
        let filesystem_bytes = decode_linux_mount_field(fields[separator + 1])?;
        let source = decode_linux_mount_field(fields[separator + 2])?;
        let path = PathBuf::from(OsString::from_vec(path_bytes.clone()));
        if !path.is_absolute() {
            return Err(invalid("Linux-Mountpfad ist nicht absolut"));
        }
        let filesystem = String::from_utf8(filesystem_bytes.clone())
            .map_err(|_| invalid("Linux-Dateisystemtyp ist kein UTF-8"))?;
        let root = path == Path::new("/");
        let network = is_network_filesystem(&filesystem);
        let standard_external = is_standard_external_path(&path);
        if !root && !network && !source.starts_with(b"/dev/") && !standard_external {
            continue;
        }
        if !root && is_pseudo_filesystem(&filesystem) {
            continue;
        }
        let (space, space_error) = match space_for(&path) {
            Ok(space) => (space, None),
            Err(error) => (
                Space {
                    total: 0,
                    available: 0,
                },
                Some(SystemMountSpaceError {
                    code: crate::error_code(&error),
                    message: error.to_string(),
                }),
            ),
        };
        let kind = if root {
            SystemMountKind::Internal
        } else if network {
            SystemMountKind::Network
        } else if standard_external {
            SystemMountKind::External
        } else {
            SystemMountKind::Internal
        };
        let unmount = match kind {
            SystemMountKind::External | SystemMountKind::Network => Some(SystemUnmount::LinuxGio {
                mount_path: path.clone(),
            }),
            SystemMountKind::Internal | SystemMountKind::Volume => None,
        };
        mounts.push(SystemMount {
            // Die Mount-ID stammt aus genau diesem Mount-Namespace und wechselt bei
            // einer neuen Mountinstanz. Ein altes UI-Token passt deshalb nicht mehr,
            // selbst wenn Quelle, Pfad und Dateisystem wieder gleich aussehen.
            id: mount_id(
                b"linux",
                &path_bytes,
                &source,
                &filesystem_bytes,
                &mount_number.to_le_bytes(),
            ),
            name: display_name(&path, "Dateisystem"),
            path: path.to_str().map(str::to_string),
            kind,
            filesystem,
            total_bytes: space.total,
            available_bytes: space.available,
            space_error,
            read_only: options
                .split(|byte| *byte == b',')
                .any(|option| option == b"ro"),
            ejectable: unmount.is_some(),
            unmount,
        });
    }
    sort_mounts(&mut mounts);
    Ok(mounts)
}

#[cfg(target_os = "linux")]
fn decode_linux_mount_field(field: &[u8]) -> io::Result<Vec<u8>> {
    let mut decoded = Vec::with_capacity(field.len());
    let mut index = 0;
    while index < field.len() {
        if field[index] != b'\\' {
            decoded.push(field[index]);
            index += 1;
            continue;
        }
        let escape = field
            .get(index + 1..index + 4)
            .ok_or_else(|| invalid("Abgeschnittene Maskierung in /proc/self/mountinfo"))?;
        let value = match escape {
            b"040" => b' ',
            b"011" => b'\t',
            b"012" => b'\n',
            b"134" => b'\\',
            _ => return Err(invalid("Unbekannte Maskierung in /proc/self/mountinfo")),
        };
        decoded.push(value);
        index += 4;
    }
    Ok(decoded)
}

fn is_network_filesystem(filesystem: &str) -> bool {
    matches!(
        filesystem.to_ascii_lowercase().as_str(),
        "afpfs"
            | "cifs"
            | "smb3"
            | "smbfs"
            | "nfs"
            | "nfs4"
            | "webdav"
            | "davfs"
            | "fuse.davfs"
            | "sshfs"
            | "fuse.sshfs"
            | "ceph"
            | "glusterfs"
            | "9p"
    )
}

#[cfg(target_os = "linux")]
fn is_standard_external_path(path: &Path) -> bool {
    path == Path::new("/media")
        || path.starts_with("/media/")
        || path == Path::new("/mnt")
        || path.starts_with("/mnt/")
        || path.starts_with("/run/media/")
}

#[cfg(target_os = "linux")]
fn is_pseudo_filesystem(filesystem: &str) -> bool {
    matches!(
        filesystem,
        "proc"
            | "sysfs"
            | "devtmpfs"
            | "devpts"
            | "tmpfs"
            | "cgroup"
            | "cgroup2"
            | "pstore"
            | "securityfs"
            | "debugfs"
            | "tracefs"
            | "configfs"
            | "fusectl"
            | "mqueue"
            | "hugetlbfs"
            | "bpf"
            | "autofs"
            | "rpc_pipefs"
            | "binfmt_misc"
            | "nsfs"
            | "efivarfs"
            | "squashfs"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn volume(path: &str) -> SystemMount {
        SystemMount {
            id: "id".into(),
            name: "Vorläufig".into(),
            path: Some(path.into()),
            kind: SystemMountKind::Volume,
            filesystem: "apfs".into(),
            total_bytes: 100,
            available_bytes: 40,
            space_error: None,
            read_only: false,
            ejectable: false,
            unmount: None,
        }
    }

    #[test]
    fn diskutil_info_braucht_einen_exakten_mountpunkt() {
        for mount_point in [None, Some(plist::Value::Integer(7.into()))] {
            let mut mount = volume("/Volumes/Backup");
            let before = mount.clone();
            let mut info = plist::Dictionary::new();
            info.insert("Internal".into(), plist::Value::Boolean(false));
            info.insert("Ejectable".into(), plist::Value::Boolean(true));
            info.insert("VolumeName".into(), plist::Value::String("Fremd".into()));
            if let Some(value) = mount_point {
                info.insert("MountPoint".into(), value);
            }
            let mut bytes = Vec::new();
            plist::Value::Dictionary(info)
                .to_writer_xml(&mut bytes)
                .unwrap();
            let error = apply_macos_diskutil_info(&mut mount, &bytes).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert_eq!(mount, before);
        }
    }

    #[test]
    fn diskutil_info_gibt_den_systemroot_nicht_zum_auswerfen_frei() {
        let mut mount = volume("/");
        mount.kind = SystemMountKind::Internal;
        apply_macos_diskutil_info(
            &mut mount,
            br#"<plist version="1.0"><dict>
          <key>MountPoint</key><string>/</string><key>Internal</key><false/>
          <key>Ejectable</key><true/><key>RemovableMediaOrExternalDevice</key><true/>
        </dict></plist>"#,
        )
        .unwrap();
        assert_eq!(mount.kind, SystemMountKind::Internal);
        assert!(!mount.ejectable);
        assert!(mount.unmount.is_none());
    }

    #[test]
    fn macos_netzwerk_mount_mit_dontbrowse_bleibt_unsichtbar() {
        assert!(!macos_mount_is_visible(
            Path::new("/Beliebig/Ausgeblendet"),
            true,
            true
        ));
    }

    #[test]
    fn macos_netzwerk_mount_ohne_dontbrowse_bleibt_sichtbar() {
        assert!(macos_mount_is_visible(
            Path::new("/Beliebig/Freigabe"),
            true,
            false
        ));
    }

    #[test]
    fn macos_lokales_volume_unter_volumes_bleibt_sichtbar() {
        assert!(macos_mount_is_visible(
            Path::new("/Volumes/Backup"),
            false,
            false
        ));
    }

    #[test]
    fn macos_verstecktes_lokales_volume_bleibt_unsichtbar() {
        assert!(!macos_mount_is_visible(
            Path::new("/Volumes/Versteckt"),
            false,
            true
        ));
    }

    #[test]
    fn macos_systemroot_bleibt_auch_mit_dontbrowse_sichtbar() {
        assert!(macos_mount_is_visible(Path::new("/"), false, true));
    }

    #[test]
    fn reale_mountliste_enthaelt_einen_gebundenen_systemroot() {
        let mounts = list_system_mounts().unwrap();
        let root = mounts
            .iter()
            .find(|mount| mount.path.as_deref() == Some("/"))
            .expect("Systemroot fehlt in der Mountliste");
        assert_eq!(root.kind, SystemMountKind::Internal);
        assert_eq!(root.id.len(), 64);
        assert!(root.id.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(root.total_bytes > 0);
        assert!(root.available_bytes <= root.total_bytes);
        assert!(!root.ejectable);
    }

    #[test]
    fn diskutil_info_bindet_mountpunkt_name_und_auswerfbarkeit() {
        let mut mount = SystemMount {
            id: "id".to_string(),
            name: "Vorläufig".to_string(),
            path: Some("/Volumes/Backup".to_string()),
            kind: SystemMountKind::Volume,
            filesystem: "apfs".to_string(),
            total_bytes: 100,
            available_bytes: 40,
            space_error: None,
            read_only: false,
            ejectable: false,
            unmount: None,
        };
        apply_macos_diskutil_info(
            &mut mount,
            br#"<?xml version="1.0" encoding="UTF-8"?>
            <plist version="1.0"><dict>
              <key>MountPoint</key><string>/Volumes/Backup</string>
              <key>VolumeName</key><string>Archiv</string>
              <key>Internal</key><false/>
              <key>Ejectable</key><true/>
            </dict></plist>"#,
        )
        .unwrap();

        assert_eq!(mount.name, "Archiv");
        assert_eq!(mount.kind, SystemMountKind::External);
        assert!(mount.ejectable);
        assert_eq!(
            mount.unmount,
            Some(SystemUnmount::MacDiskutil {
                mount_path: PathBuf::from("/Volumes/Backup")
            })
        );
    }

    #[test]
    fn diskutil_info_lehnt_fremden_mountpunkt_ab() {
        let mut mount = SystemMount {
            id: "id".to_string(),
            name: "Backup".to_string(),
            path: Some("/Volumes/Backup".to_string()),
            kind: SystemMountKind::Volume,
            filesystem: "apfs".to_string(),
            total_bytes: 100,
            available_bytes: 40,
            space_error: None,
            read_only: false,
            ejectable: false,
            unmount: None,
        };
        let error = apply_macos_diskutil_info(
            &mut mount,
            br#"<plist version="1.0"><dict>
              <key>MountPoint</key><string>/Volumes/Fremd</string>
              <key>Internal</key><false/>
            </dict></plist>"#,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_mounts_filtert_pseudo_fs_und_bilanziert_sichtbare_mounts() {
        let table = br#"36 25 259:2 / / rw,relatime - ext4 /dev/nvme0n1p2 rw,relatime
37 36 0:5 / /proc rw,nosuid,nodev,noexec - proc proc rw
38 36 8:17 / /media/dm/Meine\040Platte rw,nosuid shared:3 - ext4 /dev/sdb1 rw
39 36 0:42 /export /srv/archiv ro,relatime - nfs4 server:/daten rw
40 36 0:28 / /run rw,nosuid,nodev - tmpfs tmpfs rw
"#;
        let mounts = parse_linux_mountinfo_with(table, |_| {
            Ok(Space {
                total: 1_000,
                available: 400,
            })
        })
        .unwrap();

        assert_eq!(mounts.len(), 3);
        assert_eq!(mounts[0].kind, SystemMountKind::Internal);
        let external = mounts
            .iter()
            .find(|mount| mount.kind == SystemMountKind::External)
            .unwrap();
        assert_eq!(external.name, "Meine Platte");
        assert_eq!(external.path.as_deref(), Some("/media/dm/Meine Platte"));
        assert_eq!(external.available_bytes, 400);
        assert_eq!(
            external.unmount,
            Some(SystemUnmount::LinuxGio {
                mount_path: PathBuf::from("/media/dm/Meine Platte")
            })
        );
        let network = mounts
            .iter()
            .find(|mount| mount.kind == SystemMountKind::Network)
            .unwrap();
        assert!(network.read_only);
        assert!(network.ejectable);
        assert!(matches!(
            network.unmount,
            Some(SystemUnmount::LinuxGio { .. })
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_mountfehler_erhaelt_den_mount_und_unabhaengige_ziele() {
        let mounts = parse_linux_mountinfo_with(
            b"36 25 259:2 / / rw - ext4 /dev/root rw\n39 36 0:42 / /srv/archiv rw - nfs4 server:/daten rw\n",
            |path| if path == Path::new("/") {
                Ok(Space { total: 1_000, available: 400 })
            } else {
                Err(io::Error::from_raw_os_error(libc::EACCES))
            },
        ).unwrap();
        assert_eq!(
            mounts.len(),
            2,
            "Ein Fehler darf den Mount nicht verschlucken"
        );
        assert_eq!(mounts[0].available_bytes, 400);
        assert_eq!(mounts[1].path.as_deref(), Some("/srv/archiv"));
        assert_eq!(mounts[1].total_bytes, 0);
        let error = mounts[1].space_error.as_ref().unwrap();
        assert_eq!(error.code, crate::ErrorCode::PermissionDenied);
        assert!(!error.message.is_empty());
        let payload = serde_json::to_value(&mounts[1]).unwrap();
        assert_eq!(payload["space_error"]["code"], "permission_denied");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_mount_id_unterscheidet_neue_instanz_am_selben_pfad() {
        let first = parse_linux_mountinfo_with(
            b"101 36 0:42 /export /srv/archiv rw - nfs4 server:/daten rw\n",
            |_| {
                Ok(Space {
                    total: 1,
                    available: 1,
                })
            },
        )
        .unwrap();
        let replacement = parse_linux_mountinfo_with(
            b"102 36 0:42 /export /srv/archiv rw - nfs4 server:/daten rw\n",
            |_| {
                Ok(Space {
                    total: 1,
                    available: 1,
                })
            },
        )
        .unwrap();

        assert_ne!(first[0].id, replacement[0].id);
        assert_eq!(first[0].path, replacement[0].path);
        assert_eq!(first[0].filesystem, replacement[0].filesystem);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_mounts_lehnt_unbekannte_maskierung_ab() {
        let error = parse_linux_mountinfo_with(
            b"41 36 8:1 / /media/Falsch\\999 rw - ext4 /dev/sda1 rw\n",
            |_| {
                Ok(Space {
                    total: 1,
                    available: 1,
                })
            },
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
