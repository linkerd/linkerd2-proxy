use std::fmt;
use std::time::UNIX_EPOCH;

use ctx;
use super::{Gauge, Metric};

use self::system::System;

#[derive(Debug, Default)]
pub struct Process {
    start_time: Gauge,
    system: Option<System>,
}

impl Process {
    metrics! {
        process_start_time_seconds: Gauge {
            "Time that the process started (in seconds since the UNIX epoch)"
        }
    }

    pub fn new(process: &ctx::Process) -> Self {
        let t0 = process.start_time
            .duration_since(UNIX_EPOCH)
            .expect("process start time")
            .as_secs();

        let system = match System::new() {
            Ok(s) => Some(s),
            Err(err) => {
                debug!("failed to load system stats: {}", err);
                None
            }
        };
        Self {
            start_time: t0.into(),
            system,
        }
    }
}

impl fmt::Display for Process {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        Self::process_start_time_seconds.fmt_help(f)?;
        Self::process_start_time_seconds.fmt_metric(f, self.start_time)?;

        if let Some(ref sys) = self.system {
            sys.fmt(f)?;
        }

        Ok(())
    }
}

#[cfg(target_os = "linux")]
mod system {
    use super::*;
    use super::super::{Counter, Gauge};

    use std::{io, fs};

    use procinfo::pid;
    use libc::{self, pid_t};

    #[derive(Debug)]
    pub(super) struct System {
        page_size: u64,
        clock_ticks_per_sec: u64,
    }

    impl System {
        metrics! {
            process_cpu_seconds_total: Counter {
                "Total user and system CPU time spent in seconds."
            },
            process_open_fds: Gauge { "Number of open file descriptors." },
            process_max_fds: Gauge { "Maximum number of open file descriptors." },
            process_virtual_memory_bytes: Gauge {
                "Virtual memory size in bytes."
            },
            process_resident_memory_bytes: Gauge {
                "Resident memory size in bytes."
            }
        }
    }

    impl System {
        pub fn new() -> io::Result<Self> {
            let page_size = Self::sysconf(libc::_SC_PAGESIZE, "page size")?;
            let clock_ticks_per_sec = Self::sysconf(libc::_SC_CLK_TCK, "clock ticks per second")?;
            Ok(Self {
                page_size,
                clock_ticks_per_sec,
            })
        }

        fn open_fds(pid: pid_t) -> io::Result<Gauge> {
            let mut open = 0;
            for f in fs::read_dir(format!("/proc/{}/fd", pid))? {
                if !f?.file_type()?.is_dir() {
                    open += 1;
                }
            }
            Ok(Gauge::from(open))
        }

        fn max_fds() -> io::Result<Option<Gauge>> {
            let limit = pid::limits_self()?.max_open_files;
            let max_fds = limit.soft.or(limit.hard)
                .map(|max| Gauge::from(max as u64));
            Ok(max_fds)
        }

        fn sysconf(num: libc::c_int, name: &'static str) -> Result<u64, io::Error> {
            match unsafe { libc::sysconf(num) } {
                e if e <= 0 => {
                    let error = io::Error::last_os_error();
                    error!("error getting {}: {:?}", name, error);
                    Err(error)
                },
                val => Ok(val as u64),
            }
        }
    }

    impl fmt::Display for System {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            // XXX potentially blocking call
            let stat = match pid::stat_self() {
                Ok(stat) => stat,
                Err(err) => {
                    warn!("failed to read process stats: {}", err);
                    return Ok(());
                }
            };

            let clock_ticks = stat.utime as u64 + stat.stime as u64;
            Self::process_cpu_seconds_total.fmt_help(f)?;
            Self::process_cpu_seconds_total.fmt_metric(
                f,
                Counter::from(clock_ticks / self.clock_ticks_per_sec),
            )?;

            match Self::open_fds(stat.pid) {
                Ok(open_fds) => {
                    Self::process_open_fds.fmt_help(f)?;
                    Self::process_open_fds.fmt_metric(f, open_fds)?;
                }
                Err(err) => {
                    warn!("could not determine process_open_fds: {}", err);
                    return Ok(());
                }
            }

            match Self::max_fds() {
                Ok(None) => {}
                Ok(Some(ref max_fds)) => {
                    Self::process_max_fds.fmt_help(f)?;
                    Self::process_max_fds.fmt_metric(f, *max_fds)?;
                }
                Err(err) => {
                    warn!("could not determine process_max_fds: {}", err);
                    return Ok(());
                }
            }

            Self::process_virtual_memory_bytes.fmt_help(f)?;
            Self::process_virtual_memory_bytes.fmt_metric(
                f,
                Gauge::from(stat.vsize as u64),
            )?;

            Self::process_resident_memory_bytes.fmt_help(f)?;
            Self::process_resident_memory_bytes.fmt_metric(
                f,
                Gauge::from(stat.rss as u64 * self.page_size),
            )
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod system {
    use std::{fmt, io};

    #[derive(Debug)]
    pub(super) struct System {}

    impl System {
        pub fn new() -> io::Result<Sensor> {
            Err(io::Error::new(
                io::ErrorKind::Other,
                "procinfo not supported on this operating system"
            ))
        }
    }

    impl fmt::Display for System {
        fn fmt(&self, _: &mut fmt::Formatter) -> fmt::Result {
            Ok(())
        }
    }
}
