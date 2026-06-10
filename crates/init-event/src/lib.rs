//! init-event: The main event loop that drives all PID 1 operations.
//!
//! This is a single-threaded tokio runtime that multiplexes:
//! - signalfd -> signal events
//! - child process monitoring -> reap + restart

use anyhow::{Context, Result};
use init_core::net;
use init_core::signal::{self, REQUIRED_SIGNALS};
use init_core::tty;
use init_core::uevent::UeventSocket;
use init_unit::UnitRegistry;
use std::collections::{HashMap, HashSet};
use tokio::io::unix::AsyncFd;
use tracing::{debug, info, warn};

pub struct Runtime {
    unit_registry: UnitRegistry,
    signal_fd: AsyncFd<std::os::unix::io::RawFd>,
    running: bool,
    pids: HashMap<libc::pid_t, String>,
    restart_history: HashMap<String, Vec<std::time::Instant>>,
    uevent_socket: AsyncFd<std::os::unix::io::RawFd>,
}

impl Drop for Runtime {
    fn drop(&mut self) {
        unsafe {
            libc::close(*self.signal_fd.get_ref());
            libc::close(*self.uevent_socket.get_ref());
        }
    }
}

impl Runtime {
    pub fn new(unit_registry: UnitRegistry) -> Result<Self> {
        let sfd = signal::create_signal_fd(REQUIRED_SIGNALS)?;
        let fd = sfd.fd;
        let async_fd = AsyncFd::new(fd)?;

        let uevent_sock = UeventSocket::new()?;
        let uevent_fd = uevent_sock.raw_fd();
        // Prevent Drop from closing the fd — AsyncFd owns it now
        std::mem::forget(uevent_sock);
        let uevent_async = AsyncFd::new(uevent_fd)?;

        Ok(Runtime {
            unit_registry,
            signal_fd: async_fd,
            running: true,
            pids: HashMap::new(),
            restart_history: HashMap::new(),
            uevent_socket: uevent_async,
        })
    }

    pub fn start_default_target(&mut self) -> Result<()> {
        let default = self
            .unit_registry
            .get("default.target")
            .with_context(|| "default.target not found")?;

        let wanted: Vec<String> = default.unit.wants.clone();

        if wanted.is_empty() {
            info!("default.target has no Wants, no services to start");
            return Ok(());
        }

        info!(count = wanted.len(), "starting default.target services");

        let mut to_start = HashMap::new();
        let mut queue: Vec<String> = wanted;
        let mut seen: HashSet<String> = HashSet::new();

        while let Some(name) = queue.pop() {
            if !seen.insert(name.clone()) {
                continue;
            }
            if let Some(unit) = self.unit_registry.get(&name) {
                for dep in &unit.unit.requires {
                    if !seen.contains(dep) {
                        queue.push(dep.clone());
                    }
                }
                for dep in &unit.unit.wants {
                    if !seen.contains(dep) {
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

        // Configure .network units
        self.configure_networks()?;

        Ok(())
    }

    /// Process .network units: configure static IP or start DHCP clients.
    fn configure_networks(&mut self) -> Result<()> {
        let networks: Vec<_> = self
            .unit_registry
            .values()
            .filter(|u| u.is_network())
            .cloned()
            .collect();

        if networks.is_empty() {
            return Ok(());
        }

        debug!(count = networks.len(), "configuring network interfaces");

        for unit in &networks {
            let net = match &unit.network {
                Some(n) => n,
                None => continue,
            };

            if net.dhcp {
                debug!(ifname = %net.name, "running DHCP client");
                match net::run_dhcp(&net.name) {
                    Ok(lease) => {
                        info!(
                            unit = %unit.name,
                            _ip = ?lease.ip,
                            "DHCP lease obtained and applied"
                        );
                    }
                    Err(e) => {
                        warn!(unit = %unit.name, error = %e, "DHCP failed");
                    }
                }
            } else if let Some(ref addr) = net.address {
                let dns = net.dns.clone().unwrap_or_default();
                debug!(ifname = %net.name, addr = %addr, "configuring static IP");
                if let Err(e) = net::configure_static(&net.name, addr, net.gateway.as_deref(), &dns)
                {
                    warn!(unit = %unit.name, error = %e, "failed to configure static IP");
                }
            } else {
                warn!(unit = %unit.name, "network unit has no dhcp or address");
            }
        }

        Ok(())
    }

    fn start_service_unit(
        &mut self,
        name: &str,
        svc: &init_unit::types::ServiceSection,
    ) -> Result<()> {
        if svc.exec_start.is_empty() {
            anyhow::bail!("service '{}' has empty exec_start", name);
        }
        let path = &svc.exec_start[0];
        let args: Vec<String> = if svc.exec_start.len() > 1 {
            svc.exec_start[1..].to_vec()
        } else {
            vec![]
        };

        debug!(unit = %name, path = %path, args = ?args, "starting service");

        let child_pid = if let Some(ref tty_device) = svc.tty {
            let baud = svc.tty_baud.unwrap_or(115200);
            debug!(unit = %name, tty = %tty_device, baud, "starting terminal session");
            tty::spawn_terminal(path, &args, tty_device, baud)?
        } else {
            init_core::child::spawn_service(path, &args)?.pid
        };
        self.pids.insert(child_pid, name.to_string());

        debug!(unit = %name, pid = child_pid, "service started");
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
                uevent_ready = self.uevent_socket.readable() => {
                    match uevent_ready {
                        Ok(mut guard) => {
                            self.handle_uevent().await?;
                            guard.clear_ready();
                        }
                        Err(e) => {
                            warn!(error = %e, "uevent socket error");
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
                    debug!("SIGHUP received - reloading unit configuration");
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

    /// Handle incoming kernel uevents — create /dev nodes for device add events.
    async fn handle_uevent(&self) -> Result<()> {
        let fd = self.uevent_socket.get_ref();
        let sock = UeventSocket::from_raw_fd(*fd);

        while let Some(uevent) = sock.recv()? {
            if uevent.action == "add" {
                if let (Some(ref devname), Some(devtype), Some(major), Some(minor)) =
                    (&uevent.devname, &uevent.devtype, uevent.major, uevent.minor)
                {
                    let dt = devtype.chars().next().unwrap_or('c');
                    init_core::fs::create_device_node(devname, dt, major, minor).unwrap_or_else(
                        |e| {
                            tracing::warn!(
                                devname = %devname,
                                error = %e,
                                "failed to create device node"
                            );
                        },
                    );
                }
            }
        }

        // Prevent Drop from closing the fd
        std::mem::forget(sock);
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
                let svc_clone = self.unit_registry.get(name).and_then(|u| u.service.clone());
                let name_clone = name.clone();

                if let Some(svc) = svc_clone {
                    let should_restart = match svc.restart {
                        init_unit::types::RestartPolicy::Always => true,
                        init_unit::types::RestartPolicy::OnFailure => child.status != 0,
                        init_unit::types::RestartPolicy::OnAbnormal => child.was_signaled,
                        init_unit::types::RestartPolicy::No => false,
                    };

                    if should_restart {
                        // Rate limiting: check restart count within time window
                        let now = std::time::Instant::now();
                        let window = std::time::Duration::from_secs(10);
                        let max_burst = 5;

                        let timestamps =
                            self.restart_history.entry(name_clone.clone()).or_default();
                        timestamps.retain(|t| now - *t < window);
                        timestamps.push(now);

                        if timestamps.len() > max_burst {
                            warn!(
                                unit = %name_clone,
                                burst = timestamps.len(),
                                max_burst,
                                "restart rate limit exceeded, not restarting"
                            );
                            continue;
                        }

                        info!(
                            unit = %name_clone,
                            pid = child.pid,
                            status = child.status,
                            restart_sec = svc.restart_sec,
                            "restarting service"
                        );

                        tokio::time::sleep(std::time::Duration::from_secs(svc.restart_sec as u64))
                            .await;

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

        // Phase 1: Send SIGTERM to all services
        let pids: Vec<i32> = self.pids.keys().copied().collect();
        for &pid in &pids {
            unsafe { libc::kill(pid, libc::SIGTERM) };
            debug!(pid, "sent SIGTERM");
        }

        // Phase 2: Wait up to 10 seconds for children to exit
        for _ in 0..100 {
            self.reap_and_restart().await?;
            if self.pids.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        // Phase 3: SIGKILL any remaining processes
        if !self.pids.is_empty() {
            warn!(
                remaining = self.pids.len(),
                "sending SIGKILL to remaining services"
            );
            let remaining: Vec<i32> = self.pids.keys().copied().collect();
            for &pid in &remaining {
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
            // Brief wait for SIGKILL to be delivered
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            self.reap_and_restart().await?;
        }

        // Phase 4: Sync filesystems and power off
        debug!("syncing filesystems and powering off");
        unsafe { libc::sync() };
        unsafe { libc::reboot(libc::LINUX_REBOOT_CMD_POWER_OFF) };

        Ok(())
    }

    async fn emergency_shutdown(&mut self) -> Result<()> {
        warn!("emergency shutdown - sending SIGKILL to all services");

        // Skip SIGTERM, go straight to SIGKILL
        let pids: Vec<i32> = self.pids.keys().copied().collect();
        for &pid in &pids {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        self.reap_and_restart().await?;

        debug!("syncing filesystems and powering off");
        unsafe { libc::sync() };
        unsafe { libc::reboot(libc::LINUX_REBOOT_CMD_POWER_OFF) };

        Ok(())
    }

    async fn reload_units(&mut self) -> Result<()> {
        self.unit_registry = init_unit::load_all_units()?;
        debug!(units = self.unit_registry.len(), "units reloaded");
        Ok(())
    }
}
