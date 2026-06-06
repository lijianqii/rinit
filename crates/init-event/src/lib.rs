//! init-event: The main event loop that drives all PID 1 operations.
//!
//! This is a single-threaded tokio runtime that multiplexes:
//! - signalfd -> signal events
//! - child process monitoring -> reap + restart
//! - D-Bus IPC -> management commands (future)
//! - Timer events -> .timer unit triggers (future)
//! - Socket activation -> .socket unit events (future)

use anyhow::Result;
use init_core::signal::{self, REQUIRED_SIGNALS};
use init_unit::UnitRegistry;
use tokio::io::unix::AsyncFd;
use tracing::{debug, info, warn};

pub struct Runtime {
    unit_registry: UnitRegistry,
    signal_fd: AsyncFd<std::os::unix::io::RawFd>,
    running: bool,
}

impl Runtime {
    pub fn new(unit_registry: UnitRegistry) -> Result<Self> {
        let sfd = signal::create_signal_fd(REQUIRED_SIGNALS)?;
        let fd = sfd.fd;
        let async_fd = AsyncFd::new(fd)?;

        Ok(Runtime {
            unit_registry,
            signal_fd: async_fd,
            running: true,
        })
    }

    pub async fn run(&mut self) -> Result<()> {
        info!(units = self.unit_registry.len(), "entering event loop");

        while self.running {
            tokio::select! {
                ready = self.signal_fd.readable() => {
                    match ready {
                        Ok(mut guard) => {
                            let signals = signal::read_signals_from_fd(self.signal_fd.get_ref())?;
                            guard.clear_ready();
                            self.handle_signals(&signals).await?;
                        }
                        Err(e) => {
                            warn!(error = %e, "signal fd error");
                        }
                    }
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                    self.reap_and_restart().await?;
                }
            }
        }

        Ok(())
    }

    async fn handle_signals(&mut self, signals: &[i32]) -> Result<()> {
        for &sig in signals {
            match sig {
                libc::SIGCHLD => {
                    debug!("SIGCHLD received - reaping children");
                    self.reap_and_restart().await?;
                }
                libc::SIGTERM | libc::SIGINT => {
                    warn!(signal = sig, "shutdown signal received");
                    self.shutdown().await?;
                    self.running = false;
                }
                libc::SIGHUP => {
                    info!("SIGHUP received - reloading unit configuration");
                    self.reload_units().await?;
                }
                libc::SIGPWR => {
                    warn!("SIGPWR received - power failure imminent");
                    self.emergency_shutdown().await?;
                    self.running = false;
                }
                _ => {
                    debug!(signal = sig, "ignored signal");
                }
            }
        }
        Ok(())
    }

    async fn reap_and_restart(&mut self) -> Result<()> {
        let exited = init_core::child::reap_children()?;

        for child in &exited {
            debug!(pid = child.pid, status = child.status, "child reaped");
        }

        Ok(())
    }

    async fn shutdown(&mut self) -> Result<()> {
        info!("initiating graceful shutdown");
        Ok(())
    }

    async fn emergency_shutdown(&mut self) -> Result<()> {
        warn!("emergency shutdown - killing all services immediately");
        Ok(())
    }

    async fn reload_units(&mut self) -> Result<()> {
        self.unit_registry = init_unit::load_all_units()?;
        info!(units = self.unit_registry.len(), "units reloaded");
        Ok(())
    }
}
