use std::sync::atomic::{AtomicUsize, Ordering};

const STACK_SIZE_BYTES: usize = 256 * 1024;
const MAX_BACKGROUND_THREADS: usize = 32;

static ACTIVE_THREADS: AtomicUsize = AtomicUsize::new(0);

struct ThreadCountGuard;

impl ThreadCountGuard {
    fn new() -> Self {
        Self
    }
}

impl Drop for ThreadCountGuard {
    fn drop(&mut self) {
        ACTIVE_THREADS.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Lightweight helper to spawn background threads with a lower stack size and
/// a descriptive, namespaced thread name. Keeps a simple global cap to avoid
/// runaway spawns when review flows create timers repeatedly.
pub(crate) fn spawn_lightweight<F>(name: &str, f: F) -> Option<std::thread::JoinHandle<()>>
where
    F: FnOnce() + Send + 'static,
{
    let mut observed = ACTIVE_THREADS.load(Ordering::SeqCst);
    loop {
        if observed >= MAX_BACKGROUND_THREADS {
            tracing::error!(
                active_threads = observed,
                max_threads = MAX_BACKGROUND_THREADS,
                thread_name = name,
                "background thread spawn rejected: limit reached"
            );
            return None;
        }
        match ACTIVE_THREADS.compare_exchange(
            observed,
            observed + 1,
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(_) => break,
            Err(updated) => observed = updated,
        }
    }

    let thread_name = format!("code-{name}");
    let builder = std::thread::Builder::new()
        .name(thread_name)
        .stack_size(STACK_SIZE_BYTES);

    match builder.spawn(move || {
        let _guard = ThreadCountGuard::new();
        f();
    }) {
        Ok(handle) => Some(handle),
        Err(error) => {
            ACTIVE_THREADS.fetch_sub(1, Ordering::SeqCst);
            tracing::error!(thread_name = name, %error, "failed to spawn background thread");
            None
        }
    }
}
