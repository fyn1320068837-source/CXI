// Network Verifier - lsof-based process-level validation
use anyhow::Result;
use std::process::Command;

pub struct NetworkVerifier;

impl NetworkVerifier {
    pub fn new() -> Self {
        Self
    }

    /// Verify that a process tree only connects through proxy
    pub async fn verify_process(&self, pids: &[u32]) -> Result<VerificationResult> {
        let mut direct_connections = Vec::new();
        let mut proxied_connections = 0;
        let mut local_connections = 0;

        for &pid in pids {
            let connections = self.get_tcp_connections(pid).await?;

            for conn in connections {
                match self.classify_connection(&conn) {
                    ConnectionType::Proxied => proxied_connections += 1,
                    ConnectionType::Local => local_connections += 1,
                    ConnectionType::Direct => {
                        direct_connections.push(conn);
                    }
                }
            }
        }

        Ok(VerificationResult {
            proxied_connections,
            local_connections,
            direct_connections,
        })
    }

    async fn get_tcp_connections(&self, pid: u32) -> Result<Vec<Connection>> {
        // lsof -a -i TCP -p <pid> -n -P
        let output = Command::new("lsof")
            .args(&["-a", "-i", "TCP", "-p", &pid.to_string(), "-n", "-P"])
            .output()?;

        if !output.status.success() {
            return Ok(Vec::new());
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut connections = Vec::new();

        for line in stdout.lines().skip(1) {
            // Parse lsof output
            // Format: COMMAND PID USER FD TYPE DEVICE SIZE/OFF NODE NAME
            if let Some(conn) = self.parse_lsof_line(line) {
                connections.push(conn);
            }
        }

        Ok(connections)
    }

    fn parse_lsof_line(&self, _line: &str) -> Option<Connection> {
        // TODO: Parse lsof output properly
        // Look for ESTABLISHED connections and extract remote address
        None
    }

    fn classify_connection(&self, conn: &Connection) -> ConnectionType {
        // Classify based on remote address
        if conn.remote_addr.starts_with("127.0.0.1:7890") ||
           conn.remote_addr.starts_with("127.0.0.1:7891") {
            ConnectionType::Proxied
        } else if conn.remote_addr.starts_with("127.0.0.1") ||
                  conn.remote_addr.starts_with("::1") {
            ConnectionType::Local
        } else if self.is_private_ip(&conn.remote_addr) {
            ConnectionType::Local  // LAN - configurable
        } else {
            ConnectionType::Direct  // Violation!
        }
    }

    fn is_private_ip(&self, addr: &str) -> bool {
        // Check if IP is in 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16
        addr.starts_with("10.") ||
        addr.starts_with("172.16.") ||
        addr.starts_with("192.168.")
    }
}

#[derive(Debug)]
pub struct Connection {
    pub pid: u32,
    pub remote_addr: String,
    pub state: String,
}

#[derive(Debug)]
pub enum ConnectionType {
    Proxied,
    Local,
    Direct,
}

#[derive(Debug)]
pub struct VerificationResult {
    pub proxied_connections: usize,
    pub local_connections: usize,
    pub direct_connections: Vec<Connection>,
}
