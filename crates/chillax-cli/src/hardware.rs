//! How many agents this machine can run at once. Lives in the binary, not in
//! `chillax-core`, because it reads the hardware; the result reaches the core only
//! inside an event.

use sysinfo::System;

/// Memory one agent session is budgeted: the agent process, its tools and a build.
const MEM_PER_AGENT: u64 = 1 << 30;

/// The most agents to run at once: the smaller of the CPU count and the memory budget,
/// and at least one. Inside a container the cgroup memory limit counts, not the host's.
pub fn hardware_max() -> usize {
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
    let mut system = System::new();
    system.refresh_memory();
    let memory = system
        .cgroup_limits()
        .map_or_else(|| system.total_memory(), |c| c.total_memory);
    from_resources(cpus, memory)
}

/// [`hardware_max`] from a CPU count and memory in bytes.
fn from_resources(cpus: usize, memory: u64) -> usize {
    let by_memory = usize::try_from(memory / MEM_PER_AGENT).unwrap_or(usize::MAX);
    cpus.min(by_memory).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_machine_fits_at_least_one_agent() {
        assert!(hardware_max() >= 1);
    }

    #[test]
    fn the_scarcer_of_cpu_and_memory_decides() {
        let gib = 1u64 << 30;
        assert_eq!(from_resources(8, 64 * gib), 8);
        assert_eq!(from_resources(16, 4 * gib), 4);
        assert_eq!(from_resources(4, 0), 1);
    }
}
