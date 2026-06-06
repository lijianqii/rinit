//! init-event: The main event loop that drives all PID 1 operations.
//!
//! This is a single-threaded tokio runtime that multiplexes:
//! - signalfd -> signal events
//! - child process monitoring -> reap + restart

use anyhow::{Context, Result};
use init_core::signal::{self, REQUIRED_SIGNALS};
use init_unit::UnitRegistry;
use std::collections::HashMap;
use tokio::io::unix::AsyncFd;
use tracing::{debug, info, warn};

pub struct Runtime {
    unit_registry: UnitRegistry,
    signal_fd: AsyncFd<std::os::unix::io::RawFd>,
    running: bool,
    pids: HashMap<libc::pid_t, String>,
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
            pids: HashMap::new(),
        })
    }

    pub fn start_default_target(&mut self) -> Result<()> {
        let default = self.unit_registry.get("default.target")
            .with_context(|| "default.target not found")?;

        let wanted: Vec<String> = default.unit.wants.clone();

        if wanted.is_empty() {
            info!("default.target has no Wants, no services to start");
            return Ok(());
        }

        info!(count = wanted.len(), "starting default.target services");

        let mut to_start = HashMap::new();
        let mut queue: Vec<String> = wanted;

        while let Some(name) = queue.pop() {
            if to_start.contains_key(&name) {
                continue;
            }
            if let Some(unit) = self.unit_registry.get(&name) {
                for dep in &unit.unit.requires {
                    if !to_start.contains_key(dep) {
                        queue.push(dep.clone());
                    }
                }
                for dep in &unit.unit.wants {
                    if !to_start.contains_key(dep) {
                        queue.push(dep.clone());
                    }
                }
                to_start.insert(name.clone(), unit.clone());
            } else {
                warn!(unit = %name, "wanted unit not found");
            }
        }

        let layers = init_unit::deps::resolve_startup_order(&to_start)?;

        for (i, layer) in layers.iter().enumerate() {
            debug!(layer = i, units = ?layer, "starting layer");
            for name in layer {
                if let Some(unit) = to_start.get(name) {
                    if let Some(ref svc) = unit.service {
                        self.start_service_unit(name, svc)?;
                    }
                }
            }
        }

        Ok(())
    }

    fn start_service_unit(
        &mut self,
        name: &str,
        svc: &init_unit::types::ServiceSection,
    ) -> Result<()> {
        let path = &svc.exec_start[0];
        let args: Vec<String> = if svc.exec_start.len() > 1 {
            svc.exec_start[1..].to_vec()
        } else {
            vec![]
        };

        info!(unit = %name, path = %path, args = ?args, "starting service");

        let child = init_core::child::spawn_service(path, &args)?;
        self.pids.insert(child.pid, name.to_string());

        info!(unit = %name, pid = child.pid, "service started");
        Ok(())
    }

    pub async fn run(&mut self) -> Result<()> {
        self.start_default_target()?;

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
            let unit_name = self.pids.get(&child.pid).cloned();
            self.pids.remove(&child.pid);

            debug!(
                pid = child.pid,
                status = child.status,
                unit = ?unit_name,
                "child reaped"
            );

            if let Some(ref name) = unit_name {
                // Clone service config to avoid borrow conflict with start_service_unit
                let svc_clone = self.unit_registry.get(name)
                    .and_then(|u| u.service.clone());
                let name_clone = name.clone();

                if let Some(svc) = svc_clone {
                        let should_restart = match svc.restart {
                            init_unit::types::RestartPolicy::Always => true,
                            init_unit::types::RestartPolicy::OnFailure => child.status != 0,
                            init_unit::types::RestartPolicy::OnAbnormal => {
                                child.status != 0 && child.status != libc::EXIT_SUCCESS
                            }
                            init_unit::types::RestartPolicy::No => false,
                        };

                        if should_restart {
                            info!(
                                unit = %name_clone,
                                pid = child.pid,
                                status = child.status,
                                restart_sec = svc.restart_sec,
                                "restarting service"
                            );

                            tokio::time::sleep(
                                std::time::Duration::from_secs(svc.restart_sec as u64)
                            ).await;

                            if let Err(e) = self.start_service_unit(&name_clone, &svc) {
                                warn!(unit = %name_clone, error = %e, "failed to restart service");
                            }
                        }
                }
            }
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
