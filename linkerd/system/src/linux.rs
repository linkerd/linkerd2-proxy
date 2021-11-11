use libc::{self, pid_t};
use procinfo::pid;
use std::{fs, io};
use tracing::{error, warn};

pub use procinfo::pid::Stat;

pub fn page_size() -> io::Result<u64> {
    sysconf(libc::_SC_PAGESIZE, "page size")
}

pub fn ms_per_tick() -> io::Result<u64> {
    // On Linux, CLK_TCK is ~always `100`, so pure integer division
    // works. This is probably not suitable if we encounter other
    // values.
    let clock_ticks_per_sec = sysconf(libc::_SC_CLK_TCK, "clock ticks per second")?;
    let ms_per_tick = 1_000 / clock_ticks_per_sec;
    if clock_ticks_per_sec != 100 {
        warn!(
            clock_ticks_per_sec,
            ms_per_tick, "Unexpected value; process_cpu_seconds_total may be inaccurate."
        );
    }
    Ok(ms_per_tick)
}

pub fn blocking_stat() -> io::Result<Stat> {
    pid::stat_self()
}

pub fn open_fds(pid: pid_t) -> io::Result<u64> {
    let mut open = 0;
    for f in fs::read_dir(format!("/proc/{}/fd", pid))? {
        if !f?.file_type()?.is_dir() {
            open += 1;
        }
    }
    Ok(open)
}

pub fn max_fds() -> io::Result<Option<u64>> {
    let limit = pid::limits_self()?.max_open_files;
    let max_fds = limit.soft.or(limit.hard).map(|max| max as u64);
    Ok(max_fds)
}

#[allow(unsafe_code)]
fn sysconf(num: libc::c_int, name: &'static str) -> Result<u64, io::Error> {
    match unsafe { libc::sysconf(num) } {
        e if e <= 0 => {
            let error = io::Error::last_os_error();
            error!("error getting {}: {:?}", name, error);
            Err(error)
        }
        val => Ok(val as u64),
    }
}
