//! Process memory reporting.
//!
//! One small wrapper around `GetProcessMemoryInfo`: the OS's own accounting
//! for this process, covering every thread the app owns (UI, recorder
//! workers, encoders, audio mixers) — not just the thread that asks.

use windows::Win32::System::ProcessStatus::{
    GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
};
use windows::Win32::System::Threading::GetCurrentProcess;

/// Physical RAM the whole process is currently using (its working set), or
/// `None` if the query failed.
///
/// This is the same family of number Task Manager's Memory column reports:
/// pages the process actually has in RAM right now. It excludes pages that
/// were swapped out and counts some pages shared with other processes
/// (system DLLs), so it can differ slightly from "private bytes" tools.
pub fn process_working_set() -> Option<u64> {
    let mut counters = PROCESS_MEMORY_COUNTERS_EX::default();
    counters.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;

    unsafe { GetProcessMemoryInfo(
        GetCurrentProcess(),
        &mut counters as *mut PROCESS_MEMORY_COUNTERS_EX as *mut PROCESS_MEMORY_COUNTERS,
        counters.cb,
    ) }
    .ok()
    .map(|_| counters.WorkingSetSize as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn working_set_is_queried_and_plausible() {
        let ram = process_working_set().expect("GetProcessMemoryInfo should succeed");
        // A test binary that just linked the app's dependencies cannot have a
        // zero working set, and nothing here grows to a gigabyte.
        assert!(ram > 0, "working set should be non-zero, got {ram}");
        assert!(ram < 1024 * 1024 * 1024, "working set implausibly large: {ram}");
    }
}
