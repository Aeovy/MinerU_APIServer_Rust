#[cfg(all(target_os = "linux", feature = "jemalloc"))]
use std::{os::raw::c_char, time::Instant};

#[cfg(all(target_os = "linux", feature = "jemalloc"))]
use tikv_jemalloc_ctl::{arenas, epoch, raw, stats};

#[cfg(all(target_os = "linux", feature = "jemalloc"))]
#[global_allocator]
static GLOBAL_ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(all(target_os = "linux", feature = "jemalloc"))]
union JemallocConfigPointer {
    byte: &'static u8,
    char: &'static c_char,
}

#[cfg(all(target_os = "linux", feature = "jemalloc"))]
#[allow(non_upper_case_globals)]
#[export_name = "_rjem_malloc_conf"]
pub static malloc_conf: Option<&'static c_char> = Some(unsafe {
    JemallocConfigPointer {
        byte: &b"background_thread:true,dirty_decay_ms:1000,muzzy_decay_ms:1000,narenas:4\0"[0],
    }
    .char
});

#[derive(Debug, Clone, Copy)]
pub struct AllocatorStats {
    pub allocated_bytes: usize,
    pub resident_bytes: usize,
    pub retained_bytes: usize,
}

/// Read allocator memory counters when jemalloc is active.
pub fn allocator_stats() -> Option<AllocatorStats> {
    allocator_stats_impl()
}

/// Best-effort memory reclamation after a parse task reaches a terminal state.
///
/// Inputs:
/// - `task_id`: task identifier used only for structured logging.
/// - `enabled`: runtime switch from `MINERU_MEMORY_RECLAIM_AFTER_TASK`.
pub fn reclaim_after_task(task_id: uuid::Uuid, enabled: bool) {
    if !enabled {
        tracing::debug!(%task_id, "allocator reclaim skipped by config");
        return;
    }
    reclaim_after_task_impl(task_id);
}

#[cfg(all(target_os = "linux", feature = "jemalloc"))]
fn allocator_stats_impl() -> Option<AllocatorStats> {
    if let Err(error) = epoch::advance() {
        tracing::debug!(%error, "failed to refresh jemalloc stats epoch");
        return None;
    }
    Some(AllocatorStats {
        allocated_bytes: stats::allocated::read().ok()?,
        resident_bytes: stats::resident::read().ok()?,
        retained_bytes: stats::retained::read().ok()?,
    })
}

#[cfg(not(all(target_os = "linux", feature = "jemalloc")))]
fn allocator_stats_impl() -> Option<AllocatorStats> {
    None
}

#[cfg(all(target_os = "linux", feature = "jemalloc"))]
fn reclaim_after_task_impl(task_id: uuid::Uuid) {
    let started_at = Instant::now();
    let before = allocator_stats_impl();
    if let Err(error) = tikv_jemalloc_ctl::background_thread::write(true) {
        tracing::debug!(%task_id, %error, "failed to enable jemalloc background thread");
    }
    let purge_errors = purge_all_arenas();
    let after = allocator_stats_impl();
    tracing::debug!(
        %task_id,
        purge_errors,
        allocated_before = before.map(|stats| stats.allocated_bytes),
        resident_before = before.map(|stats| stats.resident_bytes),
        retained_before = before.map(|stats| stats.retained_bytes),
        allocated_after = after.map(|stats| stats.allocated_bytes),
        resident_after = after.map(|stats| stats.resident_bytes),
        retained_after = after.map(|stats| stats.retained_bytes),
        elapsed_ms = started_at.elapsed().as_millis(),
        "allocator reclaim after task completed"
    );
}

#[cfg(not(all(target_os = "linux", feature = "jemalloc")))]
fn reclaim_after_task_impl(task_id: uuid::Uuid) {
    tracing::debug!(%task_id, "allocator reclaim unavailable on this build");
}

#[cfg(all(target_os = "linux", feature = "jemalloc"))]
fn purge_all_arenas() -> usize {
    let arena_count = arenas::narenas::read().unwrap_or(0);
    let mut errors = 0_usize;
    for arena_index in 0..arena_count {
        let name = format!("arena.{arena_index}.purge\0");
        // jemalloc exposes purge as a command-style mallctl; writing a zero-sized
        // value lets the safe wrapper call the command without retaining buffers.
        if let Err(error) = unsafe { raw::write::<()>(name.as_bytes(), ()) } {
            errors += 1;
            tracing::trace!(arena_index, %error, "jemalloc arena purge failed");
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    #[test]
    fn allocator_stats_is_best_effort() {
        let _ = super::allocator_stats();
    }

    #[test]
    fn reclaim_after_task_is_best_effort() {
        super::reclaim_after_task(uuid::Uuid::new_v4(), true);
        super::reclaim_after_task(uuid::Uuid::new_v4(), false);
    }
}
