//! Running the decoder at full speed the moment a frame ends.
//!
//! Between frames the decoder thread sleeps, and an idle CPU sits at its
//! lowest clock (schedutil drops this laptop's cores to 600 MHz) and, on a
//! big.LITTLE part, a woken thread may land on an efficiency core. A 4 s
//! frame that decodes in 8 ms on a busy performance core then takes 30-50 ms.
//! On Linux the decoder thread asks for a minimum utilisation clamp
//! (`sched_setattr` with `SCHED_FLAG_UTIL_CLAMP_MIN`, kernel uclamp): the
//! scheduler then runs it on the fastest cores at their highest clock as
//! soon as it wakes, without spinning. Threads it starts inherit the clamp.
//! No privileges are needed while the system allows it
//! (`/proc/sys/kernel/sched_util_clamp_min`, 1024 by default).

/// Ask for full-speed scheduling of the calling thread; Err says why not.
#[cfg(target_os = "linux")]
pub fn boost_current_thread() -> Result<(), String> {
    #[repr(C)]
    struct SchedAttr {
        size: u32,
        sched_policy: u32,
        sched_flags: u64,
        sched_nice: i32,
        sched_priority: u32,
        sched_runtime: u64,
        sched_deadline: u64,
        sched_period: u64,
        sched_util_min: u32,
        sched_util_max: u32,
    }
    const SCHED_FLAG_KEEP_POLICY: u64 = 0x08;
    const SCHED_FLAG_KEEP_PARAMS: u64 = 0x10;
    const SCHED_FLAG_UTIL_CLAMP_MIN: u64 = 0x20;
    let attr = SchedAttr {
        size: std::mem::size_of::<SchedAttr>() as u32,
        sched_policy: 0,
        sched_flags: SCHED_FLAG_KEEP_POLICY | SCHED_FLAG_KEEP_PARAMS | SCHED_FLAG_UTIL_CLAMP_MIN,
        sched_nice: 0,
        sched_priority: 0,
        sched_runtime: 0,
        sched_deadline: 0,
        sched_period: 0,
        sched_util_min: 1024,
        sched_util_max: 1024,
    };
    // SAFETY: sched_setattr reads `attr.size` bytes from a valid, initialised
    // struct; pid 0 is the calling thread.
    let r = unsafe { libc::syscall(libc::SYS_sched_setattr, 0, &attr as *const SchedAttr, 0u32) };
    if r == 0 { Ok(()) } else { Err(std::io::Error::last_os_error().to_string()) }
}

/// Ask for full-speed scheduling of the calling thread (not supported here).
#[cfg(not(target_os = "linux"))]
pub fn boost_current_thread() -> Result<(), String> {
    Err("utilisation clamps need Linux".into())
}

#[cfg(test)]
mod tests {
    #[test]
    fn boost_is_harmless() {
        // Allowed or refused, it must not panic or change results.
        let _ = super::boost_current_thread();
    }
}
