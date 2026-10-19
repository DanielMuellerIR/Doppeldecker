use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::{product, Space};

const MAX_PROBES: usize = 4;
const MAX_OUTPUT: usize = 4096;
const PROBE_DEADLINE: Duration = Duration::from_millis(250);

#[derive(Default)]
struct ProbePool {
    active: Arc<AtomicUsize>,
    retired: Mutex<Vec<(Child, ProbeLease)>>,
}

struct ProbeLease(Arc<AtomicUsize>);

impl Drop for ProbeLease {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl ProbePool {
    fn reap(&self) {
        let mut retired = self.retired.lock().unwrap();
        let mut index = 0;
        while index < retired.len() {
            if matches!(retired[index].0.try_wait(), Ok(Some(_))) {
                retired.swap_remove(index);
            } else {
                index += 1;
            }
        }
    }

    fn acquire(&self) -> io::Result<ProbeLease> {
        self.reap();
        self.active
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < MAX_PROBES).then_some(active + 1)
            })
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "Speicherabfrage ausgesetzt: Vier Hilfsprozesse sind noch nicht beendet",
                )
            })?;
        Ok(ProbeLease(self.active.clone()))
    }
}

struct ProbeProcess<'a> {
    child: Option<Child>,
    lease: Option<ProbeLease>,
    pool: &'a ProbePool,
}

impl Drop for ProbeProcess<'_> {
    fn drop(&mut self) {
        if let (Some(mut child), Some(lease)) = (self.child.take(), self.lease.take()) {
            let _ = child.kill();
            // SIGKILL beendet einen im Kernel wartenden Netzwerk-Stat nicht
            // zwingend sofort. Deshalb hier niemals wait(): Der eigene Slot bleibt
            // bis zum tatsächlich beobachteten Exit belegt. Der nächste Abruf reapet
            // nur mit try_wait; auch wiederholte Fensterabrufe erzeugen höchstens
            // vier lebende oder noch nicht eingesammelte Hilfsprozesse.
            self.pool.retired.lock().unwrap().push((child, lease));
        }
    }
}

pub(super) fn query(path: &Path, listing_deadline: Instant) -> io::Result<Space> {
    static POOL: OnceLock<ProbePool> = OnceLock::new();
    let mut command = Command::new("/usr/bin/stat");
    command
        .args(["-f", "--printf=%S %b %a", "--"])
        .arg(path)
        .env("LC_ALL", "C");
    let deadline = listing_deadline.min(Instant::now() + PROBE_DEADLINE);
    parse_space(&run(
        command,
        deadline,
        POOL.get_or_init(ProbePool::default),
    )?)
}

fn run(mut command: Command, deadline: Instant, pool: &ProbePool) -> io::Result<Vec<u8>> {
    if Instant::now() >= deadline {
        pool.reap();
        return Err(expired());
    }
    let lease = pool.acquire()?;
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut process = ProbeProcess {
        child: Some(child),
        lease: Some(lease),
        pool,
    };
    let child = process.child.as_mut().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    nonblocking(&stdout)?;
    nonblocking(&stderr)?;
    let mut output = Vec::new();
    let mut errors = Vec::new();
    loop {
        drain(&mut stdout, &mut output)?;
        drain(&mut stderr, &mut errors)?;
        if let Some(status) = child.try_wait()? {
            drain(&mut stdout, &mut output)?;
            drain(&mut stderr, &mut errors)?;
            process.child.take();
            if !status.success() {
                return Err(io::Error::other(format!(
                    "Speicherabfrage fehlgeschlagen ({status}): {}",
                    String::from_utf8_lossy(&errors).trim()
                )));
            }
            return Ok(output);
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(expired());
        }
        std::thread::sleep((deadline - now).min(Duration::from_millis(10)));
    }
}

fn expired() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        "Speicherabfrage hat ihr Zeitlimit erreicht",
    )
}

fn nonblocking(pipe: &impl AsRawFd) -> io::Result<()> {
    let fd = pipe.as_raw_fd();
    // SAFETY: Der Descriptor gehört der gehaltenen Pipe. Wir lesen ihre Flags und
    // ergänzen nur O_NONBLOCK; geerbte Schreibenden dürfen kein EOF-Warten erzwingen.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn drain(pipe: &mut impl Read, bytes: &mut Vec<u8>) -> io::Result<()> {
    let mut buffer = [0; 512];
    loop {
        match pipe.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(count) => {
                if bytes.len() + count > MAX_OUTPUT {
                    return Err(super::invalid(
                        "Speicherabfrage überschreitet die Ausgabegrenze",
                    ));
                }
                bytes.extend_from_slice(&buffer[..count]);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

fn parse_space(bytes: &[u8]) -> io::Result<Space> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| super::invalid("Speicherabfrage lieferte ungültige Zahlen"))?;
    let values: Vec<_> = text.split_whitespace().collect();
    if values.len() != 3 {
        return Err(super::invalid(
            "Speicherabfrage lieferte nicht genau drei Zahlen",
        ));
    }
    let mut parsed = [0; 3];
    for (destination, value) in parsed.iter_mut().zip(values) {
        *destination = value
            .parse::<u64>()
            .map_err(|_| super::invalid("Speicherabfrage lieferte ungültige Zahlen"))?;
    }
    let [block_size, blocks, available] = parsed;
    if block_size == 0 || available > blocks {
        return Err(super::invalid(
            "Speicherabfrage lieferte unplausible Größen",
        ));
    }
    Ok(Space {
        total: product(blocks, block_size),
        available: product(available, block_size),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell(script: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        command
    }

    #[test]
    fn langsamer_stat_endet_ohne_blockierendes_wait() {
        let pool = ProbePool::default();
        let start = Instant::now();
        let error = run(
            shell("exec sleep 5"),
            start + Duration::from_millis(40),
            &pool,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(1));
        while pool.active.load(Ordering::Acquire) > 0 && start.elapsed() < Duration::from_secs(1) {
            pool.reap();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(pool.active.load(Ordering::Acquire), 0);
    }

    #[test]
    fn belegte_slots_starten_keinen_weiteren_prozess() {
        let pool = ProbePool::default();
        let leases: Vec<_> = (0..MAX_PROBES).map(|_| pool.acquire().unwrap()).collect();
        let error = run(
            Command::new("/nicht-vorhanden"),
            Instant::now() + Duration::from_secs(1),
            &pool,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        drop(leases);
        assert_eq!(pool.active.load(Ordering::Acquire), 0);
    }

    #[test]
    fn mehrere_mountproben_teilen_dieselbe_gesamtfrist() {
        let pool = ProbePool::default();
        let start = Instant::now();
        let deadline = start + Duration::from_millis(60);
        for _ in 0..20 {
            let error = run(shell("exec sleep 5"), deadline, &pool).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        }
        assert!(start.elapsed() < Duration::from_millis(500));
        while pool.active.load(Ordering::Acquire) > 0 && start.elapsed() < Duration::from_secs(1) {
            pool.reap();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(pool.active.load(Ordering::Acquire), 0);
    }

    #[test]
    fn geerbte_pipe_verhindert_keinen_abschluss_und_fehler_bleibt_sichtbar() {
        let pool = ProbePool::default();
        let start = Instant::now();
        let bytes = run(
            shell("(sleep 2) & printf '4096 100 40'"),
            start + Duration::from_secs(1),
            &pool,
        )
        .unwrap();
        assert!(start.elapsed() < Duration::from_secs(1));
        let space = parse_space(&bytes).unwrap();
        assert_eq!(space.total, 409_600);
        assert_eq!(space.available, 163_840);
        let error = run(
            shell("printf 'ESTALE fixture' >&2; exit 1"),
            Instant::now() + Duration::from_secs(1),
            &pool,
        )
        .unwrap_err();
        assert!(error.to_string().contains("ESTALE fixture"));
        assert_eq!(pool.active.load(Ordering::Acquire), 0);
    }

    #[test]
    fn ungueltige_und_uebergrosse_ausgabe_wird_abgelehnt() {
        for bytes in [
            b"0 100 40".as_slice(),
            b"4096 40 100",
            b"4096 -1 0",
            b"1 2 1 3",
        ] {
            assert!(parse_space(bytes).is_err());
        }
        let pool = ProbePool::default();
        let error = run(
            shell("head -c 10000 /dev/zero"),
            Instant::now() + Duration::from_secs(1),
            &pool,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        pool.reap();
    }
}
