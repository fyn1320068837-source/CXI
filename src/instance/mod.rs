// Instance Adapters - unified interface for A/B/C/D instances
use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum InstanceKind {
    Code,     // A: Claude Code CLI
    Desktop,  // B: Claude Desktop app
    VsCode,   // C: VS Code extension
    Web,      // D: Claude Web in Chromium
}

#[derive(Debug)]
pub struct ProcessHandle {
    pub root_pid: u32,
    pub kind: InstanceKind,
}

#[derive(Debug)]
pub struct CapabilityReport {
    pub installed: bool,
    pub version: Option<String>,
    pub path: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ValidationStatus {
    Pass,
    Fail,
    Unknown,
    Unsupported,
}

#[derive(Debug)]
pub struct ValidationReport {
    pub status: ValidationStatus,
    pub details: String,
}

#[async_trait]
pub trait InstanceAdapter: Send + Sync {
    fn kind(&self) -> InstanceKind;
    async fn detect(&self) -> CapabilityReport;
    async fn prepare(&self) -> Result<()>;
    async fn launch(&self) -> Result<ProcessHandle>;
    async fn stop(&self, handle: &ProcessHandle) -> Result<()>;
    async fn process_tree(&self, handle: &ProcessHandle) -> Vec<u32>;
    async fn verify_network(&self, handle: &ProcessHandle) -> ValidationReport;
}

// Concrete adapters
mod code;
mod desktop;
mod vscode;
mod web;
mod manager;

pub use code::CodeAdapter;
pub use desktop::DesktopAdapter;
pub use vscode::VsCodeAdapter;
pub use web::WebAdapter;
pub use manager::InstanceManager;
