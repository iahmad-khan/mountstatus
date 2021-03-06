/*
    Paranoid mount monitor for POSIX operating systems

    The general idea is that some classes of storage failure require care to
    detect because any access to the mountpoint including your monitoring check
    will block and in the case of certain kernel bugs that may be completely
    irrecoverable or will require a considerable delay – often days by default –
    to exhaust repeated TCP and NFS timeouts.

    This can't be solved easily by using asynchronous I/O APIs because key
    platforms like Linux don't implement an async stat(2) equivalent. This
    program uses the broadly-portable approach of launch an external child
    process asynchronously with a timeout. If it fails to respond by the
    deadline, we'll send it a SIGKILL and avoid further checks until the process
    disappears to avoid accumulating blocked check processes.

    The major improvements of the Rust version compared to the original C
    version are the use of persistent state to avoid having more than one check
    pending for any given mountpoint and the ability to send metrics to a
    Prometheus push-gateway so they will be alertable even if the local system
    is severely degraded.
 */

extern crate argparse;
extern crate libc;
extern crate syslog;
extern crate wait_timeout;
extern crate rayon;

#[macro_use]
extern crate error_chain;

#[cfg(feature = "with_prometheus")]
extern crate hostname;

#[cfg(feature = "with_prometheus")]
#[macro_use]
extern crate lazy_static;

#[cfg(feature = "with_prometheus")]
#[macro_use]
extern crate prometheus;

use std::collections::HashMap;
use std::process;
use std::str;
use std::thread;
use std::time::{Duration, Instant};
use std::path::{Path, PathBuf};

use argparse::{ArgumentParser, Store, StoreOption, Print};
use syslog::Facility;
use wait_timeout::ChildExt;

use rayon::prelude::*;

mod errors;
mod get_mounts;

use errors::*;

fn handle_syslog_error(err: std::io::Error) -> usize {
    // Convenience function allowing all of our syslog calls to use .unwrap_or_else
    eprintln!("Syslog failed: {}", err);
    0
}

#[derive(Debug)]
enum MountStatus {
    Alive,
    CheckFailed(i32),
    CheckSignaled(i32),
    CheckRunning {
        process: process::Child,
        start_time: Instant,
    }
}

impl MountStatus {
    fn success(&self) -> bool {
        if let MountStatus::Alive = *self {
            true
        } else {
            false
        }
    }
}

quick_main!{ real_main }

fn real_main() -> Result<()> {
    let mut poll_interval = 60;
    let mut prometheus_push_gateway: Option<String> = None;

    {
        // this block limits scope of borrows by ap.refer() method
        let mut ap = ArgumentParser::new();
        ap.set_description(concat!(
            "Monitor the status of mounted filesystems and report inaccessible mounts.",
            " Dead mounts will be reported to the local syslog server and optionally",
            " a Prometheus push-gateway service."
        ));

        ap.add_option(
            &["--version"],
            Print(env!("CARGO_PKG_VERSION").to_string()),
            "Show version",
        );

        if cfg!(feature = "with_prometheus") {
            ap.refer(&mut prometheus_push_gateway).add_option(
                &["--prometheus-push-gateway"],
                StoreOption,
                "Location of the Prometheus push-gateway server to send metrics to",
            );
        }

        ap.refer(&mut poll_interval).add_option(
            &["--poll-interval"],
            Store,
            "Number of seconds to wait before checking mounts",
        );

        ap.parse_args_or_exit();
    }

    let poll_interval_duration = Duration::from_secs(poll_interval);

    println!(
        "mount_status_monitor checking mounts every {} seconds",
        poll_interval_duration.as_secs()
    );

    let syslog = syslog::unix(Facility::LOG_DAEMON).chain_err(|| "Unable to connect to syslog")?;

    let mut mount_statuses = HashMap::<PathBuf, MountStatus>::new();

    loop {
        check_mounts(&mut mount_statuses, &syslog);

        // We calculate these values each time because a filesystem may have been
        // mounted or unmounted since the last check:
        let total_mounts = mount_statuses.len();
        let dead_mounts = mount_statuses
            .iter()
            .filter(|&(_, status)| !status.success())
            .count();

        // TODO: consider making this debug or sending it to stdout?
        syslog
            .info(format!(
                "Checked {} mounts; {} are dead",
                total_mounts,
                dead_mounts
            ))
            .unwrap_or_else(handle_syslog_error);

        #[cfg(feature = "with_prometheus")]
        {
            if let Some(ref gateway_address) = prometheus_push_gateway {
                if let Err(e) = push_to_prometheus(gateway_address, dead_mounts, total_mounts) {
                    eprintln!("{}", e);
                }
            }
        }

        // Wait before checking again:
        thread::sleep(poll_interval_duration);
    }
}

#[cfg(feature = "with_prometheus")]
fn push_to_prometheus(
    gateway: &str,
    dead_mounts: usize,
    total_mounts: usize,
) -> prometheus::Result<()> {
    lazy_static! {
        static ref TOTAL_MOUNTS: prometheus::Gauge = register_gauge!(
            "total_mountpoints",
            "Total number of mountpoints"
        ).unwrap();

        static ref DEAD_MOUNTS: prometheus::Gauge = register_gauge!(
            "dead_mountpoints",
            "Number of unresponsive mountpoints"
        ).unwrap();
    }

    let prometheus_instance = match hostname::get_hostname() {
        Some(hostname) => hostname,
        None => return Err(prometheus::Error::Msg("Unable to retrieve hostname".into())),
    };

    // The Prometheus metrics are defined as floats so we need to convert;
    // for monitoring the precision loss in general is fine and it's
    // exceedingly unlikely to be relevant when counting the number of
    // mountpoints:
    TOTAL_MOUNTS.set(total_mounts as f64);
    DEAD_MOUNTS.set(dead_mounts as f64);

    prometheus::push_metrics(
        "mount_status_monitor",
        labels!{"instance".to_owned() => prometheus_instance.to_owned(), },
        gateway,
        prometheus::gather(),
    )
}

fn check_mounts(mount_statuses: &mut HashMap<PathBuf, MountStatus>, logger: &syslog::Logger) {
    let mount_points = get_mounts::get_mount_points().unwrap_or_else(|err| {
        eprintln!("Failed to retrieve a list of mount-points: {:?}", err);
        std::process::exit(2);
    });

    // Remove any mount status entries which are no longer in the current list of mountpoints:
    mount_statuses.retain(|ref k, _| {
        mount_points.iter().position(|i| *i == **k).is_some()
    });

    for mount_point in mount_points {
        mount_statuses
            .entry(mount_point)
            .or_insert(MountStatus::Alive);
    }

    mount_statuses
        .par_iter_mut()
        .for_each(|(mount_point, mount_status)| {
            if let MountStatus::CheckRunning {
                ref mut process,
                start_time,
            } = *mount_status
            {
                match process.try_wait() {
                    Ok(Some(status)) => {
                        logger
                            .info(format!(
                                "Slow check for mount {} exited with {} after {} seconds",
                                mount_point.display(),
                                status,
                                start_time.elapsed().as_secs()
                            ))
                            .unwrap_or_else(handle_syslog_error);
                    }
                    Ok(None) => {
                        logger
                            .warning(format!(
                                "Slow check for mount {} has not exited after {} seconds",
                                mount_point.display(),
                                start_time.elapsed().as_secs()
                            ))
                            .unwrap_or_else(handle_syslog_error);
                        return;
                    }
                    Err(e) => {
                        logger
                            .err(format!(
                                "Stalled check on mount {} returned an error after {} seconds: {}",
                                mount_point.display(),
                                start_time.elapsed().as_secs(),
                                e
                            ))
                            .unwrap_or_else(handle_syslog_error);
                    }
                }
            }
            let new_mount_status = match check_mount(mount_point) {
                Ok(status) => status,
                Err(e) => {
                    eprintln!("{}", e);
                    return;
                }
            };

            match new_mount_status {
                MountStatus::CheckFailed(rc) => {
                    eprintln!("Mount check failed with an unexpected return code: {}", rc);
                }
                MountStatus::CheckSignaled(signal) => {
                    eprintln!("Mount check was killed by signal: {}", signal);
                }
                _ => {}
            }
            if new_mount_status.success() {
                logger
                    .debug(format!(
                        "Mount passed health-check: {}",
                        mount_point.display()
                    ))
                    .unwrap_or_else(handle_syslog_error);
            } else {
                let msg = format!("Mount failed health-check: {}", mount_point.display());
                eprintln!("{}", msg);
                logger.err(msg).unwrap_or_else(handle_syslog_error);
            }

            *mount_status = new_mount_status;
        });
}

fn check_mount(mount_point: &Path) -> Result<MountStatus> {
    let start_time = Instant::now();
    let mut child = process::Command::new("/usr/bin/stat")
        .arg(mount_point)
        .stdout(process::Stdio::null())
        .spawn()
        .chain_err(|| "Unable to spawn process to check mount")?;

    // See https://github.com/rust-lang/rust/issues/18166 for why we can't make this a static value:
    let child_result = child
        .wait_timeout(Duration::from_secs(3))
        .chain_err(|| "Unable to wait on stat command")?;
    match child_result {
        None => {
            /*
                The process has not exited and we're not going to wait for a
                potentially very long period of time for it to recover.

                We'll attempt to clean up the check process by killing it, which
                is defined as sending SIGKILL on Unix:

                https://doc.rust-lang.org/std/process/struct.Child.html#method.kill

                The mount_status structure returned will include this child
                process instance so future checks can perform a non-blocking
                test to see whether it has finally exited:
            */
            if let Err(err) = child.kill() {
                eprintln!("Unable to kill process {}: {:?}", child.id(), err)
            };

            Ok(MountStatus::CheckRunning {
                process: child,
                start_time: start_time,
            })
        }
        Some(exit_status) => {
            let rc = exit_status.code();
            match rc {
                Some(0) => Ok(MountStatus::Alive),
                Some(rc) => Ok(MountStatus::CheckFailed(rc)),
                None => {
                    // If there isn't a return code, there _should_ always be a signal
                    Ok(MountStatus::CheckSignaled(
                        exit_status.unix_signal().unwrap_or(0),
                    ))
                }
            }
        }
    }
}
