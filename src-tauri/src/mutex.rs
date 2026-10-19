//! Gemeinsamer poison-toleranter Mutex-Zugriff der dünnen App-Schicht.

use std::sync::{Mutex, MutexGuard};

/// Holt einen Mutex-Guard auch nach einem Panic im vorherigen Besitzer zurück.
///
/// Rust markiert den Mutex in diesem Fall als „vergiftet“, obwohl sein Wert weiterhin
/// vollständig vorhanden ist. Commands dürfen daraus keinen zweiten Panic erzeugen;
/// der jeweilige Fachcode prüft den wiedergewonnenen Zustand anschließend wie sonst.
pub(crate) trait MutexExt<T> {
    fn lock_recover(&self) -> MutexGuard<'_, T>;
}

impl<T> MutexExt<T> for Mutex<T> {
    fn lock_recover(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn vergifteter_mutex_bleibt_fuer_folgecommands_bedienbar() {
        let value = Arc::new(Mutex::new(0_u8));
        let worker_value = value.clone();
        assert!(std::thread::spawn(move || {
            *worker_value.lock_recover() = 7;
            panic!("künstlicher Worker-Panic");
        })
        .join()
        .is_err());

        assert_eq!(*value.lock_recover(), 7);
        *value.lock_recover() = 8;
        assert_eq!(*value.lock_recover(), 8);
    }
}
