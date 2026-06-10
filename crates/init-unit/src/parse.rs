//! TOML unit file parser.

use crate::types::Unit;
use anyhow::{Context, Result};
use tracing::debug;

/// Parse a raw TOML string into a Unit.
pub fn parse_unit_file(content: &str) -> Result<Unit> {
    let unit: Unit = toml::from_str(content).context("failed to parse TOML unit file")?;

    debug!(
        name = %unit.name,
        has_service = unit.service.is_some(),
        has_socket = unit.socket.is_some(),
        "parsed unit"
    );

    Ok(unit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_service_unit() {
        let toml = r#"
name = "nginx.service"

[unit]
description = "Nginx Web Server"
after = ["network.target"]
wants = ["journal.socket"]

[service]
exec_start = ["/usr/sbin/nginx"]
restart = "on-failure"
restart_sec = 5
memory_max = "512M"
cpu_weight = 100
"#;

        let unit = parse_unit_file(toml).unwrap();
        assert_eq!(unit.name, "nginx.service");
        assert_eq!(unit.unit.description, "Nginx Web Server");

        let svc = unit.service.unwrap();
        assert_eq!(svc.exec_start, vec!["/usr/sbin/nginx"]);
        assert_eq!(svc.memory_max, Some("512M".to_string()));
    }

    #[test]
    fn parse_target_unit() {
        let toml = r#"
name = "multi-user.target"

[unit]
description = "Multi-User System"
requires = ["network.target", "sshd.service"]
wants = ["cron.service"]
"#;

        let unit = parse_unit_file(toml).unwrap();
        assert_eq!(unit.name, "multi-user.target");
        assert!(unit.service.is_none());
        assert_eq!(unit.unit.requires.len(), 2);
    }
}
