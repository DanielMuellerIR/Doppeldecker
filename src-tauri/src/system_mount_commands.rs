//! Dünne Plattformintegration für erkannte Volumes und Netzwerkfreigaben.
//!
//! Der Core bindet jeden Eintrag an eine opaque ID und eine nicht serialisierte
//! Auswurfstrategie. Diese Schicht wählt nur den fest vorgegebenen Systembefehl,
//! führt ihn ohne Shell aus und bestätigt anschließend durch eine frische
//! Mount-Erkennung, dass genau die gebundene ID verschwunden ist.

use std::io::Read;
use std::os::fd::AsRawFd;
use std::process::{Child, ChildStdout, Command, Output, Stdio};
use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
use doppeldecker_core::{apply_macos_diskutil_info, SystemMountKind};
use doppeldecker_core::{
    list_system_mounts as core_list_system_mounts, SystemMount, SystemUnmount,
};

use crate::AppError;

// Gilt bereits während des Lesens auch für gio und den Fastra-Helfer. Eine
// Frist allein begrenzt die zwischenzeitlich gepufferte Ausgabe nicht.
const MAX_COMMAND_OUTPUT_BYTES: usize = 1024 * 1024;
#[cfg(target_os = "macos")]
const DISKUTIL_INFO_TIMEOUT: Duration = Duration::from_secs(1);

fn load_mounts() -> Result<Vec<SystemMount>, AppError> {
    #[cfg(target_os = "macos")]
    {
        let mut mounts = core_list_system_mounts().map_err(AppError::from)?;
        for mount in &mut mounts {
            if mount.kind == SystemMountKind::Network {
                continue;
            }
            let Some(path) = mount.path.as_deref() else {
                continue;
            };
            let output = match command_output_with_timeout(
                diskutil_info_command(path),
                DISKUTIL_INFO_TIMEOUT,
            ) {
                Ok(Some(output)) if output.status.success() => output,
                _ => continue,
            };
            if output.stdout.len() > MAX_COMMAND_OUTPUT_BYTES {
                continue;
            }
            // Ein einzelnes gerade verschwindendes Volume darf die restliche
            // Geräteliste nicht ausblenden. Ohne gültigen Beleg bleibt es neutral als
            // „Volume“ und ausdrücklich nicht auswerfbar.
            let _ = apply_macos_diskutil_info(mount, &output.stdout);
        }
        Ok(mounts)
    }
    #[cfg(not(target_os = "macos"))]
    {
        core_list_system_mounts().map_err(AppError::from)
    }
}

#[cfg(target_os = "macos")]
fn diskutil_info_command(path: &str) -> Command {
    let mut command = Command::new("/usr/sbin/diskutil");
    command.args(["info", "-plist"]).arg(path);
    command
}

/// Führt einen Systembefehl mit harter Frist aus — auf beiden Plattformen.
///
/// `diskutil info` ist nur eine optionale Ergänzung der bereits gelesenen
/// Mounttabelle. Einzelne virtuelle oder fehlerhafte Volumes dürfen die App deshalb
/// nicht sekundenlang blockieren. Ein Timeout beendet und reapet ausschließlich den
/// von uns gestarteten Hilfsprozess; der neutrale, nicht auswerfbare Mount bleibt in
/// der Liste sichtbar. Derselbe Weg trägt den Aushängebefehl `diskutil unmount`
/// beziehungsweise `gio mount -u`; das Attribut `#[cfg(target_os = "macos")]` an
/// dieser Funktion ließ deshalb den gesamten Linux-Build der App scheitern
/// (E0425, aufgefallen 2026-09-04 beim ersten `cargo check -p doppeldecker` unter
/// Linux).
pub(crate) fn command_output_with_timeout(
    mut command: Command,
    timeout: Duration,
) -> std::io::Result<Option<Output>> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    // stdout wird nichtblockierend während der Statusabfrage geleert. Würde die
    // Ausgabe erst nach dem Prozessende gelesen, bliebe ein Helfer mit mehr als dem
    // Pipe-Puffer (64 KiB) im `write` stehen. Ein Lesethread mit anschließendem
    // `join` ist ebenfalls ungeeignet: Ein Enkelprozess kann das Schreibende erben
    // und den Join über die zugesagte Frist hinaus offen halten.
    let mut stdout = child
        .stdout
        .take()
        .expect("stdout wurde als Pipe angefordert");
    if let Err(error) = set_nonblocking(&stdout) {
        let _ = kill_and_reap(child);
        return Err(error);
    }
    let mut bytes = Vec::new();
    let deadline = Instant::now() + timeout;
    loop {
        if let Err(error) = drain_available(&mut stdout, &mut bytes) {
            let _ = kill_and_reap(child);
            return Err(error);
        }

        match child.try_wait() {
            Ok(Some(status)) => {
                // Nach dem Exit liegen eventuell noch Bytes im Kernelpuffer. Ein
                // überlebender Nachkomme darf uns danach trotzdem nicht auf EOF
                // warten lassen; seine spätere Ausgabe gehört nicht mehr zum
                // bestätigten Ergebnis des direkten Hilfsprozesses.
                drain_available(&mut stdout, &mut bytes)?;
                return Ok(Some(Output {
                    status,
                    stdout: bytes,
                    stderr: Vec::new(),
                }));
            }
            Ok(None) => {}
            Err(error) => {
                let _ = kill_and_reap(child);
                return Err(error);
            }
        }

        let now = Instant::now();
        if now >= deadline {
            kill_and_reap(child)?;
            return Ok(None);
        }
        std::thread::sleep((deadline - now).min(Duration::from_millis(20)));
    }
}

fn set_nonblocking(stdout: &ChildStdout) -> std::io::Result<()> {
    let fd = stdout.as_raw_fd();
    // SAFETY: `fd` gehört der lebenden `ChildStdout`. F_GETFL liest nur die
    // Descriptor-Flags; F_SETFL ergänzt ausschließlich O_NONBLOCK.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn drain_available(stdout: &mut ChildStdout, bytes: &mut Vec<u8>) -> std::io::Result<()> {
    let mut chunk = [0_u8; 16 * 1024];
    // Ein absichtlich endlos schreibender Helfer darf die äußere Fristprüfung
    // nicht dadurch verhungern lassen, dass die Pipe zwischen zwei Reads nie leer
    // wird. 16 Blöcke leeren einen üblichen Pipe-Puffer mehrfach und geben dann
    // spätestens nach 256 KiB an die Status-/Fristprüfung zurück.
    for _ in 0..16 {
        match stdout.read(&mut chunk) {
            Ok(0) => return Ok(()),
            Ok(read) => {
                if read > MAX_COMMAND_OUTPUT_BYTES.saturating_sub(bytes.len()) {
                    return Err(doppeldecker_core::coded_error(
                        std::io::ErrorKind::InvalidData,
                        doppeldecker_core::ErrorCode::OutcomeUnclear,
                        "Systemhelfer überschreitet die Ausgabegrenze. Das Ergebnis ist nicht bestätigt; mögliche Wirkung vor erneutem Aufruf prüfen.",
                    ));
                }
                bytes.extend_from_slice(&chunk[..read]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn kill_and_reap(mut child: Child) -> std::io::Result<()> {
    // Der Helfer gehört diesem Aufruf. Nach jedem Fehler- oder Fristausstieg wird
    // ausschließlich dieses direkte Kind beendet und eingesammelt; eine
    // Prozessgruppe wäre hier falsch, weil Fastra sein GUI-Bundle starten darf.
    match child.kill() {
        Ok(()) => child.wait().map(|_| ()),
        Err(kill_error) => {
            // Ein bereits beendetes Kind lässt `kill` ebenfalls scheitern; dann
            // kann es sofort eingesammelt werden. Läuft es wider Erwarten noch,
            // darf sein `wait` die zugesagte Frist nicht verlängern. Ein Reaper
            // übernimmt es deshalb außerhalb dieses Aufrufs.
            if matches!(child.try_wait(), Ok(Some(_))) {
                return Ok(());
            }
            let _ = std::thread::Builder::new()
                .name("system-command-reaper".into())
                .spawn(move || {
                    let _ = child.wait();
                });
            Err(kill_error)
        }
    }
}

fn unmount_command(strategy: &SystemUnmount) -> Result<Command, AppError> {
    match strategy {
        #[cfg(target_os = "macos")]
        SystemUnmount::MacDiskutil { mount_path } => {
            let mut command = Command::new("/usr/sbin/diskutil");
            command.arg("unmount").arg(mount_path);
            Ok(command)
        }
        #[cfg(target_os = "linux")]
        SystemUnmount::LinuxGio { mount_path } => {
            let mut command = Command::new("/usr/bin/gio");
            command.args(["mount", "-u"]).arg(mount_path);
            Ok(command)
        }
        #[allow(unreachable_patterns)]
        _ => Err(AppError::new(
            "unsupported",
            "Auswerfen wird für diese Plattformstrategie nicht unterstützt",
        )),
    }
}

#[tauri::command]
pub(crate) async fn list_system_mounts() -> Result<Vec<SystemMount>, AppError> {
    // getfsstat ist schnell, die optionale diskutil-Anreicherung dagegen ein
    // externer Systemzugriff. Beides läuft außerhalb des Tauri-/AppKit-Threads,
    // damit selbst ein träges Volume niemals ein weißes Beachball-Fenster erzeugt.
    tauri::async_runtime::spawn_blocking(load_mounts)
        .await
        .map_err(|_| AppError::new("io", "Mount-Erkennungsworker wurde unterbrochen"))?
}

/// Frist für den externen Aushängebefehl. `diskutil unmount` beziehungsweise
/// `gio mount -u` kann auf einen offenen Dateihandle oder einen trägen Bus warten;
/// ohne Frist bliebe der Command beliebig lange stehen.
const UNMOUNT_TIMEOUT: Duration = Duration::from_secs(20);

#[tauri::command]
pub(crate) async fn eject_system_mount(id: String) -> Result<Vec<SystemMount>, AppError> {
    // Derselbe Grund wie bei `list_system_mounts` direkt darüber: Der Command ruft
    // zweimal `load_mounts()` — je bis zu einer Sekunde `diskutil info` pro Volume —
    // und dazu einen externen Aushängebefehl. Synchron liefe das auf dem
    // Tauri-/AppKit-Thread und liesse das Fenster weiss stehen.
    tauri::async_runtime::spawn_blocking(move || eject_system_mount_blocking(id))
        .await
        .map_err(|_| AppError::new("io", "Auswurfworker wurde unterbrochen"))?
}

fn eject_system_mount_blocking(id: String) -> Result<Vec<SystemMount>, AppError> {
    if id.len() != 64 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(AppError::new(
            "invalid_input",
            "Auswerfen benötigt eine gültige gebundene Mount-ID",
        ));
    }
    let mounts = load_mounts()?;
    let mount = mounts
        .iter()
        .find(|mount| mount.id == id)
        .ok_or_else(|| AppError::new("plan_drift", "Das Volume ist nicht mehr eingehängt"))?;
    if !mount.ejectable {
        return Err(AppError::new(
            "permission_denied",
            "Dieses Volume ist nicht als sicher auswerfbar belegt",
        ));
    }
    let strategy = mount.unmount.as_ref().ok_or_else(|| {
        AppError::new(
            "invalid_data",
            "Auswerfbares Volume besitzt keine interne Plattformstrategie",
        )
    })?;
    let command = unmount_command(strategy)?;
    let Some(output) =
        command_output_with_timeout(command, UNMOUNT_TIMEOUT).map_err(AppError::from)?
    else {
        return Err(AppError::new(
            "outcome_unclear",
            "Der Aushängebefehl antwortete nicht innerhalb der Frist und wurde beendet; Mountzustand manuell prüfen",
        ));
    };
    if !output.status.success() {
        return Err(AppError::new(
            "io",
            format!(
                "Volume konnte nicht ausgeworfen werden (Exit-Code {})",
                output.status.code().unwrap_or(-1)
            ),
        ));
    }

    // diskutil/gio melden synchron Erfolg, die Kernel-Mounttabelle kann
    // aber noch kurz nachziehen. Begrenztes Polling bestätigt den Effekt, ohne einen
    // freien zweiten Systembefehl oder einen GUI-Thread auf Dauer zu blockieren.
    for attempt in 0..4 {
        let current = core_list_system_mounts().map_err(AppError::from)?;
        if current.iter().all(|candidate| candidate.id != id) {
            return load_mounts();
        }
        if attempt < 3 {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    Err(AppError::new(
        "outcome_unclear",
        "Systembefehl war erfolgreich, aber das Volume ist weiterhin eingehängt",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn command_parts(command: &Command) -> (String, Vec<String>) {
        (
            command.get_program().to_string_lossy().into_owned(),
            command
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect(),
        )
    }

    /// Der Command laeuft jetzt ueber `spawn_blocking`; die Pruefung selbst liegt
    /// unveraendert im blockierenden Teil und bleibt damit direkt testbar.
    #[test]
    fn ungueltige_mount_id_stoppt_vor_jedem_systemzugriff() {
        let error = eject_system_mount_blocking("/Volumes/Frei erfunden".to_string()).unwrap_err();
        assert_eq!(error.code, "invalid_input");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_auswerfen_nutzt_diskutil_ohne_shell() {
        let command = unmount_command(&SystemUnmount::MacDiskutil {
            mount_path: OsString::from("/Volumes/Backup").into(),
        })
        .unwrap();
        assert_eq!(
            command_parts(&command),
            (
                "/usr/sbin/diskutil".to_string(),
                vec!["unmount".to_string(), "/Volumes/Backup".to_string()]
            )
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn langsame_diskutil_anreicherung_endet_an_der_harten_grenze() {
        let mut command = Command::new("/bin/sleep");
        command.arg("2");
        let started = Instant::now();
        let output = command_output_with_timeout(command, Duration::from_millis(40)).unwrap();
        assert!(output.is_none());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn qa_local_uebergrosse_helferausgabe_wird_begrenzt() {
        let mut allowed = Command::new("/usr/bin/head");
        allowed.args(["-c", "1048576", "/dev/zero"]);
        let allowed = command_output_with_timeout(allowed, Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert!(allowed.status.success());
        assert_eq!(allowed.stdout.len(), 1048576);
        let mut command = Command::new("/usr/bin/head");
        command.args(["-c", "2097152", "/dev/zero"]);
        let output = command_output_with_timeout(command, Duration::from_secs(5));
        assert!(
            matches!(output, Err(ref error) if error.kind() == std::io::ErrorKind::InvalidData),
            "Zwei MiB Helferausgabe wurden nicht sichtbar begrenzt: {:?}",
            output
                .as_ref()
                .map(|value| value.as_ref().map(|output| output.stdout.len()))
        );
        assert_eq!(AppError::from(output.unwrap_err()).code, "outcome_unclear");
    }

    #[test]
    fn geerbte_stdout_pipe_hebt_die_harte_grenze_nicht_auf() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "(sleep 5) & printf fertig; exit 0"]);
        let started = Instant::now();
        let output = command_output_with_timeout(command, Duration::from_millis(300))
            .unwrap()
            .unwrap();

        assert!(output.status.success());
        assert_eq!(output.stdout, b"fertig");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn macos_bundle_deklariert_lan_zweck_und_genau_die_gesuchten_dienste() {
        let plist =
            plist::Value::from_reader(std::io::Cursor::new(include_bytes!("../Info.plist")))
                .unwrap();
        let dictionary = plist.as_dictionary().unwrap();
        let purpose = dictionary
            .get("NSLocalNetworkUsageDescription")
            .and_then(plist::Value::as_string)
            .unwrap();
        assert!(purpose.contains("Doppeldecker") && purpose.contains("lokale Netzwerk"));
        let services = dictionary
            .get("NSBonjourServices")
            .and_then(plist::Value::as_array)
            .unwrap()
            .iter()
            .map(|value| value.as_string().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            services,
            vec!["_smb._tcp", "_sftp-ssh._tcp", "_webdav._tcp"]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_blockvolume_nutzt_exakten_mountpfad_ohne_shell() {
        let command = unmount_command(&SystemUnmount::LinuxGio {
            mount_path: OsString::from("/media/dm/Backup").into(),
        })
        .unwrap();
        assert_eq!(
            command_parts(&command),
            (
                "/usr/bin/gio".to_string(),
                vec![
                    "mount".to_string(),
                    "-u".to_string(),
                    "/media/dm/Backup".to_string()
                ]
            )
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_freigabe_nutzt_gio_ohne_shell() {
        let command = unmount_command(&SystemUnmount::LinuxGio {
            mount_path: OsString::from("/srv/archiv").into(),
        })
        .unwrap();
        assert_eq!(
            command_parts(&command),
            (
                "/usr/bin/gio".to_string(),
                vec![
                    "mount".to_string(),
                    "-u".to_string(),
                    "/srv/archiv".to_string()
                ]
            )
        );
    }
}
