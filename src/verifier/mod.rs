// Network Verifier - lsof-based process-level validation
use anyhow::Result;
use std::net::IpAddr;
use std::process::Command;

const HTTP_PROXY_PORT: u16 = 7890;
const SOCKS_PROXY_PORT: u16 = 7891;

#[derive(Clone, Copy)]
pub struct NetworkVerifier {
    allow_lan: bool,
}

impl NetworkVerifier {
    pub fn new() -> Self {
        Self { allow_lan: true }
    }

    pub fn with_allow_lan(allow_lan: bool) -> Self {
        Self { allow_lan }
    }

    /// Verify that a process tree only connects through the local proxy.
    pub async fn verify_process(&self, pids: &[u32]) -> Result<VerificationResult> {
        let mut result = VerificationResult {
            checked_pids: pids.to_vec(),
            ..VerificationResult::default()
        };

        for &pid in pids {
            for conn in self.get_tcp_connections(pid).await? {
                match self.classify_connection(&conn) {
                    ConnectionType::Proxied => result.proxied_connections += 1,
                    ConnectionType::Local => result.local_connections += 1,
                    ConnectionType::Direct => result.direct_connections.push(conn),
                }
            }
        }

        Ok(result)
    }

    async fn get_tcp_connections(&self, pid: u32) -> Result<Vec<Connection>> {
        let output = Command::new("lsof")
            .args(["-a", "-i", "-p", &pid.to_string(), "-n", "-P"])
            .output()?;

        // lsof returns 1 when a process has no matching sockets. That is a
        // valid empty result, while a missing binary is surfaced to the API.
        if !output.status.success() {
            return Ok(Vec::new());
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(stdout
            .lines()
            .skip(1)
            .filter_map(Self::parse_lsof_line)
            .collect())
    }

    fn parse_lsof_line(line: &str) -> Option<Connection> {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 9 || (fields[4] != "IPv4" && fields[4] != "IPv6") {
            return None;
        }
        if fields[7] != "TCP" && fields[7] != "UDP" {
            return None;
        }

        let name = fields[8..].join(" ");
        let state = name
            .rsplit_once(" (")
            .and_then(|(_, state)| state.strip_suffix(')'))
            .unwrap_or_default()
            .to_string();
        if state == "LISTEN" {
            return None;
        }

        let remote_addr = name
            .split_once("->")
            .map(|(_, remote)| {
                remote
                    .split_once(" (")
                    .map(|(addr, _)| addr)
                    .unwrap_or(remote)
            })
            .unwrap_or_else(|| name.split_once(" (").map(|(addr, _)| addr).unwrap_or(&name))
            .trim()
            .to_string();
        if remote_addr.is_empty() {
            return None;
        }

        Some(Connection {
            pid: fields[1].parse().ok()?,
            remote_addr,
            state,
        })
    }

    fn classify_connection(&self, conn: &Connection) -> ConnectionType {
        if is_proxy_endpoint(&conn.remote_addr) {
            ConnectionType::Proxied
        } else if is_wildcard_binding(&conn.remote_addr)
            || is_loopback(&conn.remote_addr)
            || (self.allow_lan && is_private_ip(&conn.remote_addr))
        {
            ConnectionType::Local
        } else {
            ConnectionType::Direct
        }
    }
}

impl Default for NetworkVerifier {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Default)]
pub struct VerificationResult {
    pub checked_pids: Vec<u32>,
    pub proxied_connections: usize,
    pub local_connections: usize,
    pub direct_connections: Vec<Connection>,
}

impl VerificationResult {
    pub fn status(&self) -> VerificationStatus {
        if !self.direct_connections.is_empty() {
            VerificationStatus::Fail
        } else if self.proxied_connections > 0 {
            VerificationStatus::Pass
        } else {
            VerificationStatus::Unknown
        }
    }

    pub fn details(&self) -> String {
        let direct = self
            .direct_connections
            .iter()
            .map(|conn| format!("{}:{} ({})", conn.pid, conn.remote_addr, conn.state))
            .collect::<Vec<_>>()
            .join(", ");
        if direct.is_empty() {
            format!(
                "checked {} pids; proxied={}, local={}, direct=0",
                self.checked_pids.len(),
                self.proxied_connections,
                self.local_connections
            )
        } else {
            format!(
                "checked {} pids; proxied={}, local={}, direct={}",
                self.checked_pids.len(),
                self.proxied_connections,
                self.local_connections,
                direct
            )
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerificationStatus {
    Pass,
    Fail,
    Unknown,
}

#[derive(Debug)]
pub struct Connection {
    pub pid: u32,
    pub remote_addr: String,
    pub state: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnectionType {
    Proxied,
    Local,
    Direct,
}

fn is_proxy_endpoint(addr: &str) -> bool {
    let port = endpoint_port(addr);
    let host = endpoint_host(addr);
    matches!(port, Some(HTTP_PROXY_PORT | SOCKS_PROXY_PORT))
        && matches!(host.as_deref(), Some("127.0.0.1" | "::1" | "[::1]"))
}

fn is_loopback(addr: &str) -> bool {
    endpoint_host(addr)
        .and_then(|host| host.parse::<IpAddr>().ok())
        .map(|ip| ip.is_loopback())
        .unwrap_or_else(|| addr.starts_with("127.") || addr.starts_with("::1"))
}

fn is_wildcard_binding(addr: &str) -> bool {
    addr.starts_with("*:") || addr.starts_with("[::]:") || addr.starts_with(":::")
}

fn is_private_ip(addr: &str) -> bool {
    endpoint_host(addr)
        .and_then(|host| host.parse::<IpAddr>().ok())
        .map(|ip| match ip {
            IpAddr::V4(ip) => ip.is_private() || ip.is_link_local(),
            IpAddr::V6(ip) => {
                let first = ip.segments()[0];
                (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
            }
        })
        .unwrap_or_else(|| {
            addr.starts_with("10.")
                || addr.starts_with("172.16.")
                || addr.starts_with("192.168.")
                || addr.starts_with("169.254.")
                || addr.starts_with("fc")
                || addr.starts_with("fd")
        })
}

fn endpoint_port(addr: &str) -> Option<u16> {
    let endpoint = addr.trim_matches(['[', ']']);
    endpoint.rsplit_once(':')?.1.parse().ok()
}

fn endpoint_host(addr: &str) -> Option<String> {
    let endpoint = addr.trim();
    if let Some(stripped) = endpoint.strip_prefix('[') {
        return stripped.split(']').next().map(ToOwned::to_owned);
    }
    let (host, _) = endpoint.rsplit_once(':')?;
    Some(host.trim_matches(['[', ']']).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_established_lsof_line() {
        let line = "Google 80888 youzhisan 24u IPv4 0x0 0t0 TCP 127.0.0.1:57306->127.0.0.1:7891 (ESTABLISHED)";
        let conn = NetworkVerifier::parse_lsof_line(line).unwrap();
        assert_eq!(conn.pid, 80888);
        assert_eq!(conn.remote_addr, "127.0.0.1:7891");
        assert_eq!(conn.state, "ESTABLISHED");
    }

    #[test]
    fn ignores_listeners_and_classifies_ipv6_proxy() {
        assert!(NetworkVerifier::parse_lsof_line(
            "Google 1 user 1u IPv4 0x0 0t0 TCP 127.0.0.1:56533 (LISTEN)"
        )
        .is_none());
        assert!(is_proxy_endpoint("[::1]:7891"));
        assert!(is_loopback("[::1]:5353"));
        let verifier = NetworkVerifier::new();
        assert!(matches!(
            verifier.classify_connection(&Connection {
                pid: 1,
                remote_addr: "*:5353".to_string(),
                state: String::new(),
            }),
            ConnectionType::Local
        ));
    }

    #[test]
    fn direct_connection_fails_validation() {
        let verifier = NetworkVerifier::new();
        let result = VerificationResult {
            checked_pids: vec![1],
            direct_connections: vec![Connection {
                pid: 1,
                remote_addr: "8.8.8.8:443".to_string(),
                state: "ESTABLISHED".to_string(),
            }],
            ..VerificationResult::default()
        };
        assert_eq!(result.status(), VerificationStatus::Fail);
        assert!(matches!(
            verifier.classify_connection(&result.direct_connections[0]),
            ConnectionType::Direct
        ));
    }
}
