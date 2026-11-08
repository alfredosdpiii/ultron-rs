//! Locks that survive a panicking holder.

use std::sync::{Mutex, MutexGuard, PoisonError};

/// Lock `mutex`, taking the guard of a poisoned one: the state behind these locks stays consistent across a
/// panic in another holder, and one failed request must not wedge every later one.
pub fn lock<T: ?Sized>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn jsrt_lock_takes_a_poisoned_guard() {
        let shared = Arc::new(Mutex::new(1));
        let poisoner = Arc::clone(&shared);
        let _ = std::thread::spawn(move || {
            let mut guard = poisoner.lock().unwrap();
            *guard = 2;
            panic!("poison the lock");
        })
        .join();
        assert!(shared.is_poisoned());
        assert_eq!(*lock(&shared), 2);
    }

    #[test]
    fn jsrt_lock_accepts_unsized_contents() {
        let shared: Arc<Mutex<dyn std::fmt::Debug + Send>> = Arc::new(Mutex::new(7));
        assert_eq!(format!("{:?}", &*lock(&shared)), "7");
    }
}
