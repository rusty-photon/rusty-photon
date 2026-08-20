//! PHD2 process management for starting and stopping PHD2

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex;
use tracing::debug;

use crate::client::Phd2Client;
use crate::config::Phd2Config;
use crate::error::{Phd2Error, Result};
use crate::io::{
    ConnectionFactory, ProcessHandle, ProcessSpawner, TcpConnectionFactory, TokioProcessSpawner,
};

/// Get the default PHD2 executable path for the current platform
///
/// This function checks for PHD2 in the following order:
/// 1. Local build in the external/phd2/tmp directory (relative to workspace root)
/// 2. System-installed PHD2 in standard locations
/// 3. PHD2 in PATH (Linux only)
pub fn get_default_phd2_path() -> Option<PathBuf> {
    // First, try to find the local build relative to the cargo manifest directory
    // The workspace structure is: workspace_root/services/phd2-guider/
    // The local build is at: workspace_root/external/phd2/tmp/phd2.bin
    if let Some(local_path) = get_local_phd2_build_path() {
        if local_path.exists() {
            debug!("Found local PHD2 build at: {}", local_path.display());
            return Some(local_path);
        }
    }

    #[cfg(target_os = "linux")]
    {
        // Check common Linux locations
        let paths = ["/usr/bin/phd2", "/usr/local/bin/phd2"];
        for path in paths {
            let p = PathBuf::from(path);
            if p.exists() {
                return Some(p);
            }
        }
        // Try to find in PATH
        if let Ok(output) = std::process::Command::new("which").arg("phd2").output() {
            if output.status.success() {
                let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !path.is_empty() {
                    return Some(PathBuf::from(path));
                }
            }
        }
        None
    }

    #[cfg(target_os = "macos")]
    {
        let path = PathBuf::from("/Applications/PHD2.app/Contents/MacOS/PHD2");
        if path.exists() {
            Some(path)
        } else {
            None
        }
    }

    #[cfg(target_os = "windows")]
    {
        let paths = [
            r"C:\Program Files (x86)\PHDGuiding2\phd2.exe",
            r"C:\Program Files\PHDGuiding2\phd2.exe",
        ];
        for path in paths {
            let p = PathBuf::from(path);
            if p.exists() {
                return Some(p);
            }
        }
        None
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        None
    }
}

/// Get the path to the local PHD2 build in the external directory
///
/// Returns the path to external/phd2/tmp/phd2.bin relative to the workspace root.
/// This works by looking for the Cargo.toml workspace file and navigating from there.
fn get_local_phd2_build_path() -> Option<PathBuf> {
    // Try using CARGO_MANIFEST_DIR if available (set during cargo build/test)
    if let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") {
        // manifest_dir is services/phd2-guider, go up two levels to workspace root
        let workspace_root = PathBuf::from(manifest_dir)
            .parent()? // services/
            .parent()? // workspace root
            .to_path_buf();
        let local_build = workspace_root.join("external/phd2/tmp/phd2.bin");
        if local_build.exists() {
            return Some(local_build);
        }
    }

    // Try finding workspace root from current directory
    if let Ok(cwd) = std::env::current_dir() {
        let mut dir = cwd.as_path();
        // Walk up the directory tree looking for Cargo.toml with [workspace]
        while let Some(parent) = dir.parent() {
            let cargo_toml = dir.join("Cargo.toml");
            if cargo_toml.exists() {
                if let Ok(content) = std::fs::read_to_string(&cargo_toml) {
                    if content.contains("[workspace]") {
                        let local_build = dir.join("external/phd2/tmp/phd2.bin");
                        if local_build.exists() {
                            return Some(local_build);
                        }
                    }
                }
            }
            dir = parent;
        }
    }

    None
}

/// PHD2 process manager for starting and stopping PHD2
pub struct Phd2ProcessManager {
    config: Phd2Config,
    process: Arc<Mutex<Option<Box<dyn ProcessHandle>>>>,
    process_spawner: Arc<dyn ProcessSpawner>,
    connection_factory: Arc<dyn ConnectionFactory>,
}

impl Phd2ProcessManager {
    /// Create a new process manager with the given configuration
    ///
    /// Uses the default tokio process spawner and TCP connection factory
    /// for production use.
    #[must_use]
    pub fn new(config: Phd2Config) -> Self {
        Self::with_spawner(
            config,
            Arc::new(TokioProcessSpawner::new()),
            Arc::new(TcpConnectionFactory::new()),
        )
    }

    /// Create a new process manager with custom spawner and connection factory
    ///
    /// This is useful for testing with mock process spawners and connection factories.
    pub fn with_spawner(
        config: Phd2Config,
        process_spawner: Arc<dyn ProcessSpawner>,
        connection_factory: Arc<dyn ConnectionFactory>,
    ) -> Self {
        Self {
            config,
            process: Arc::new(Mutex::new(None)),
            process_spawner,
            connection_factory,
        }
    }

    /// Check if PHD2 is already running (by attempting to connect)
    pub async fn is_phd2_running(&self) -> bool {
        let addr = format!("{}:{}", self.config.host, self.config.port);
        self.connection_factory.can_connect(&addr).await
    }

    /// Get the PHD2 executable path (from config or default)
    fn get_executable_path(&self) -> Result<PathBuf> {
        if let Some(ref path) = self.config.executable_path {
            if path.exists() {
                return Ok(path.clone());
            }
            return Err(Phd2Error::ExecutableNotFound(path.display().to_string()));
        }

        get_default_phd2_path().ok_or_else(|| {
            Phd2Error::ExecutableNotFound(
                "PHD2 executable not found in default locations".to_string(),
            )
        })
    }

    /// Start PHD2 process
    pub async fn start_phd2(&self) -> Result<()> {
        // Check if already running
        if self.is_phd2_running().await {
            debug!("PHD2 is already running");
            return Ok(());
        }

        // Check if we already have a managed process
        {
            let process = self.process.lock().await;
            if process.is_some() {
                return Err(Phd2Error::ProcessAlreadyRunning);
            }
        }

        let executable = self.get_executable_path()?;
        debug!("Starting PHD2 from: {}", executable.display());

        // Use the process spawner trait to spawn the process
        let child = self
            .process_spawner
            .spawn(&executable, &self.config.spawn_env)
            .await?;

        debug!("PHD2 process started with PID: {:?}", child.id());

        // Store the child process
        {
            let mut process = self.process.lock().await;
            *process = Some(child);
        }

        // Wait for PHD2 to be ready (TCP port available)
        self.wait_for_ready().await?;

        debug!("PHD2 is ready and accepting connections");
        Ok(())
    }

    /// Wait for PHD2 to be ready (TCP port accepting connections)
    async fn wait_for_ready(&self) -> Result<()> {
        let addr = format!("{}:{}", self.config.host, self.config.port);
        let timeout = self.config.connection_timeout;
        let start = std::time::Instant::now();
        let poll_interval = std::time::Duration::from_millis(500);

        debug!("Waiting for PHD2 to be ready at {}...", addr);

        while start.elapsed() < timeout {
            if self.connection_factory.can_connect(&addr).await {
                return Ok(());
            }

            // Check if process is still running
            {
                let mut process = self.process.lock().await;
                if let Some(ref mut child) = *process {
                    match child.try_wait().await {
                        Ok(Some(status)) => {
                            return Err(Phd2Error::ProcessStartFailed(format!(
                                "PHD2 process exited prematurely with status: {status}"
                            )));
                        }
                        Ok(None) => {
                            // Still running, continue waiting
                        }
                        Err(e) => {
                            return Err(Phd2Error::ProcessStartFailed(format!(
                                "Failed to check process status: {e}"
                            )));
                        }
                    }
                }
            }

            tokio::time::sleep(poll_interval).await;
        }

        Err(Phd2Error::Timeout(format!(
            "PHD2 did not become ready within {:?}",
            self.config.connection_timeout
        )))
    }

    /// Stop PHD2 process gracefully
    ///
    /// If a client is provided, it will first try to send the shutdown RPC command.
    /// If that fails or no client is provided, it will kill the process directly.
    pub async fn stop_phd2(&self, client: Option<&Phd2Client>) -> Result<()> {
        // Try graceful shutdown via RPC first
        if let Some(client) = client {
            if client.is_connected().await {
                debug!("Attempting graceful shutdown via RPC...");
                match client.shutdown_phd2().await {
                    Ok(()) => {
                        debug!("Shutdown command sent, waiting for process to exit...");
                        // Wait for process to exit
                        if self.wait_for_exit().await.is_ok() {
                            return Ok(());
                        }
                    }
                    Err(e) => {
                        debug!("Graceful shutdown failed: {}, will force kill", e);
                    }
                }
            }
        }

        // Force kill the process
        self.kill_process().await
    }

    /// Wait for the PHD2 process to exit
    async fn wait_for_exit(&self) -> Result<()> {
        let timeout = std::time::Duration::from_secs(10);
        let start = std::time::Instant::now();
        let poll_interval = std::time::Duration::from_millis(500);

        while start.elapsed() < timeout {
            {
                let mut process = self.process.lock().await;
                if let Some(ref mut child) = *process {
                    match child.try_wait().await {
                        Ok(Some(_)) => {
                            *process = None;
                            drop(process);
                            return Ok(());
                        }
                        Ok(None) => {
                            // Still running
                        }
                        Err(e) => {
                            debug!("Error checking process status: {}", e);
                        }
                    }
                } else {
                    // No process being managed
                    return Ok(());
                }
            }
            tokio::time::sleep(poll_interval).await;
        }

        Err(Phd2Error::Timeout(
            "Process did not exit within timeout".to_string(),
        ))
    }

    /// Kill the PHD2 process forcefully
    async fn kill_process(&self) -> Result<()> {
        let mut process = self.process.lock().await;
        if let Some(mut child) = process.take() {
            debug!("Killing PHD2 process...");
            if let Err(e) = child.kill().await {
                debug!("Error killing process: {}", e);
            }
            // Wait for the process to be reaped
            let _ = child.wait().await;
        }
        drop(process);
        Ok(())
    }

    /// Check if we are managing a PHD2 process
    pub async fn has_managed_process(&self) -> bool {
        let process = self.process.lock().await;
        process.is_some()
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::{Arc, Mutex as StdMutex};
    use std::time::Duration;

    use crate::io::{ConnectionFactory, ConnectionPair, ProcessHandle, ProcessSpawner};
    use async_trait::async_trait;

    // ============================================================================
    // Mock implementations
    // ============================================================================

    /// Mock process handle for testing
    struct MockProcessHandle {
        running: StdMutex<bool>,
        exit_code: i32,
        pid: Option<u32>,
        #[allow(dead_code)]
        kill_error: bool,
    }

    impl MockProcessHandle {
        fn new_running() -> Self {
            Self {
                running: StdMutex::new(true),
                exit_code: 0,
                pid: Some(12345),
                kill_error: false,
            }
        }

        fn new_exited(exit_code: i32) -> Self {
            Self {
                running: StdMutex::new(false),
                exit_code,
                pid: Some(12345),
                kill_error: false,
            }
        }
    }

    #[async_trait]
    impl ProcessHandle for MockProcessHandle {
        async fn try_wait(&mut self) -> crate::Result<Option<i32>> {
            if *self.running.lock().unwrap() {
                Ok(None)
            } else {
                Ok(Some(self.exit_code))
            }
        }

        async fn kill(&mut self) -> crate::Result<()> {
            if self.kill_error {
                Err(Phd2Error::Io(std::io::Error::other("Mock kill error")))
            } else {
                *self.running.lock().unwrap() = false;
                Ok(())
            }
        }

        async fn wait(&mut self) -> crate::Result<i32> {
            *self.running.lock().unwrap() = false;
            Ok(self.exit_code)
        }

        fn id(&self) -> Option<u32> {
            self.pid
        }
    }

    /// Mock process spawner
    struct MockProcessSpawner {
        spawn_results: StdMutex<Vec<std::result::Result<Box<dyn ProcessHandle>, String>>>,
        spawn_count: StdMutex<u32>,
        spawned_executables: StdMutex<Vec<String>>,
        spawned_envs: StdMutex<Vec<HashMap<String, String>>>,
    }

    impl MockProcessSpawner {
        fn new() -> Self {
            Self {
                spawn_results: StdMutex::new(Vec::new()),
                spawn_count: StdMutex::new(0),
                spawned_executables: StdMutex::new(Vec::new()),
                spawned_envs: StdMutex::new(Vec::new()),
            }
        }

        fn add_spawn_success(&self) {
            self.spawn_results
                .lock()
                .unwrap()
                .push(Ok(Box::new(MockProcessHandle::new_running())));
        }

        fn add_spawn_failure(&self, message: &str) {
            self.spawn_results
                .lock()
                .unwrap()
                .push(Err(message.to_string()));
        }

        fn add_process_exits_immediately(&self, exit_code: i32) {
            self.spawn_results
                .lock()
                .unwrap()
                .push(Ok(Box::new(MockProcessHandle::new_exited(exit_code))));
        }

        fn get_spawn_count(&self) -> u32 {
            *self.spawn_count.lock().unwrap()
        }

        fn get_spawned_executables(&self) -> Vec<String> {
            self.spawned_executables.lock().unwrap().clone()
        }

        fn get_spawned_envs(&self) -> Vec<HashMap<String, String>> {
            self.spawned_envs.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ProcessSpawner for MockProcessSpawner {
        async fn spawn(
            &self,
            executable: &Path,
            env: &HashMap<String, String>,
        ) -> crate::Result<Box<dyn ProcessHandle>> {
            *self.spawn_count.lock().unwrap() += 1;
            self.spawned_executables
                .lock()
                .unwrap()
                .push(executable.display().to_string());
            self.spawned_envs.lock().unwrap().push(env.clone());

            let mut results = self.spawn_results.lock().unwrap();
            if results.is_empty() {
                Err(Phd2Error::ProcessStartFailed(
                    "No mock spawn results available".to_string(),
                ))
            } else {
                let result = results.remove(0);
                match result {
                    Ok(handle) => Ok(handle),
                    Err(msg) => Err(Phd2Error::ProcessStartFailed(msg)),
                }
            }
        }
    }

    /// Mock connection factory for testing process manager
    struct MockConnectionFactory {
        can_connect_results: StdMutex<Vec<bool>>,
        default_can_connect: bool,
    }

    impl MockConnectionFactory {
        fn new() -> Self {
            Self {
                can_connect_results: StdMutex::new(Vec::new()),
                default_can_connect: false,
            }
        }

        fn set_can_connect(&self, can_connect: bool) {
            self.can_connect_results.lock().unwrap().push(can_connect);
        }

        fn always_connectable() -> Self {
            let mut factory = Self::new();
            factory.default_can_connect = true;
            factory
        }

        fn never_connectable() -> Self {
            Self::new()
        }
    }

    #[async_trait]
    impl ConnectionFactory for MockConnectionFactory {
        async fn connect(&self, _addr: &str, _timeout: Duration) -> crate::Result<ConnectionPair> {
            // This is not used by process manager, but required by the trait
            Err(Phd2Error::ConnectionFailed(
                "Not implemented for mock".to_string(),
            ))
        }

        async fn can_connect(&self, _addr: &str) -> bool {
            let mut results = self.can_connect_results.lock().unwrap();
            if results.is_empty() {
                self.default_can_connect
            } else {
                results.remove(0)
            }
        }
    }

    /// Path to a file that exists on disk, used as a stand-in executable path.
    /// The mock spawner never actually runs this, but `get_executable_path`
    /// checks that the file exists. Use the running test binary (always present
    /// in both Cargo's target/ dir and Bazel's test sandbox) rather than
    /// `CARGO_MANIFEST_DIR/Cargo.toml`, which Bazel does not stage at runtime.
    fn dummy_executable_path() -> std::path::PathBuf {
        std::env::current_exe().expect("current_exe should be available in tests")
    }

    fn create_test_config() -> Phd2Config {
        // Use a file that exists as the executable path.
        // The actual command won't be run because we're using a mock spawner.
        Phd2Config {
            host: "localhost".to_string(),
            port: 4400,
            executable_path: Some(dummy_executable_path()),
            connection_timeout: std::time::Duration::from_secs(1),
            ..Default::default()
        }
    }

    // ============================================================================
    // Basic process manager tests
    // ============================================================================

    #[tokio::test]
    async fn test_process_manager_creation() {
        let spawner = Arc::new(MockProcessSpawner::new());
        let factory = Arc::new(MockConnectionFactory::new());
        let config = create_test_config();

        let _manager = Phd2ProcessManager::with_spawner(config, spawner, factory);
        // Just verify creation succeeds
    }

    #[tokio::test]
    async fn test_has_managed_process_initially_false() {
        let spawner = Arc::new(MockProcessSpawner::new());
        let factory = Arc::new(MockConnectionFactory::new());
        let config = create_test_config();

        let manager = Phd2ProcessManager::with_spawner(config, spawner, factory);
        assert!(!manager.has_managed_process().await);
    }

    // ============================================================================
    // is_phd2_running tests
    // ============================================================================

    #[tokio::test]
    async fn test_is_phd2_running_when_connectable() {
        let spawner = Arc::new(MockProcessSpawner::new());
        let factory = Arc::new(MockConnectionFactory::always_connectable());
        let config = create_test_config();

        let manager = Phd2ProcessManager::with_spawner(config, spawner, factory);
        assert!(manager.is_phd2_running().await);
    }

    #[tokio::test]
    async fn test_is_phd2_running_when_not_connectable() {
        let spawner = Arc::new(MockProcessSpawner::new());
        let factory = Arc::new(MockConnectionFactory::never_connectable());
        let config = create_test_config();

        let manager = Phd2ProcessManager::with_spawner(config, spawner, factory);
        assert!(!manager.is_phd2_running().await);
    }

    // ============================================================================
    // start_phd2 tests
    // ============================================================================

    #[tokio::test]
    async fn test_start_phd2_when_already_running() {
        let spawner = Arc::new(MockProcessSpawner::new());
        let factory = Arc::new(MockConnectionFactory::always_connectable());
        let config = create_test_config();

        let manager = Phd2ProcessManager::with_spawner(config, spawner.clone(), factory);

        // Should return Ok without spawning since PHD2 is "already running"
        manager.start_phd2().await.unwrap();
        assert_eq!(spawner.get_spawn_count(), 0);
    }

    #[tokio::test]
    async fn test_start_phd2_spawn_failure() {
        let spawner = Arc::new(MockProcessSpawner::new());
        spawner.add_spawn_failure("Mock spawn failure");

        let factory = Arc::new(MockConnectionFactory::never_connectable());
        let config = create_test_config();

        let manager = Phd2ProcessManager::with_spawner(config, spawner, factory);

        let result = manager.start_phd2().await;
        assert!(matches!(result, Err(Phd2Error::ProcessStartFailed(_))));
    }

    #[tokio::test]
    async fn test_start_phd2_process_exits_prematurely() {
        let spawner = Arc::new(MockProcessSpawner::new());
        spawner.add_process_exits_immediately(1);

        let factory = Arc::new(MockConnectionFactory::never_connectable());
        let config = create_test_config();

        let manager = Phd2ProcessManager::with_spawner(config, spawner, factory);

        let result = manager.start_phd2().await;
        assert!(matches!(result, Err(Phd2Error::ProcessStartFailed(_))));
    }

    #[tokio::test]
    async fn test_start_phd2_passes_environment() {
        let spawner = Arc::new(MockProcessSpawner::new());
        spawner.add_spawn_success();

        let factory = Arc::new(MockConnectionFactory::new());
        // First check: not running, then after spawn: running
        factory.set_can_connect(false);
        factory.set_can_connect(true);

        let mut config = create_test_config();
        config
            .spawn_env
            .insert("DISPLAY".to_string(), ":0".to_string());
        config
            .spawn_env
            .insert("TEST_VAR".to_string(), "test_value".to_string());

        let manager = Phd2ProcessManager::with_spawner(config, spawner.clone(), factory);

        manager.start_phd2().await.unwrap();

        let envs = spawner.get_spawned_envs();
        assert_eq!(envs.len(), 1);
        assert_eq!(envs[0].get("DISPLAY"), Some(&":0".to_string()));
        assert_eq!(envs[0].get("TEST_VAR"), Some(&"test_value".to_string()));
    }

    #[tokio::test]
    async fn test_start_phd2_uses_configured_executable() {
        let spawner = Arc::new(MockProcessSpawner::new());
        spawner.add_spawn_success();

        let factory = Arc::new(MockConnectionFactory::new());
        factory.set_can_connect(false);
        factory.set_can_connect(true);

        let config = create_test_config();

        let manager = Phd2ProcessManager::with_spawner(config, spawner.clone(), factory);

        manager.start_phd2().await.unwrap();

        let executables = spawner.get_spawned_executables();
        assert_eq!(executables.len(), 1);
        // Uses the configured dummy executable path
        assert_eq!(
            executables[0],
            dummy_executable_path().display().to_string()
        );
    }

    #[tokio::test]
    async fn test_start_phd2_timeout_waiting_for_ready() {
        let spawner = Arc::new(MockProcessSpawner::new());
        spawner.add_spawn_success();

        // Never becomes connectable
        let factory = Arc::new(MockConnectionFactory::never_connectable());

        let mut config = create_test_config();
        config.connection_timeout = std::time::Duration::from_secs(1); // Short timeout for test

        let manager = Phd2ProcessManager::with_spawner(config, spawner, factory);

        let result = manager.start_phd2().await;
        assert!(matches!(result, Err(Phd2Error::Timeout(_))));
    }

    #[tokio::test]
    async fn test_start_phd2_sets_managed_process() {
        let spawner = Arc::new(MockProcessSpawner::new());
        spawner.add_spawn_success();

        let factory = Arc::new(MockConnectionFactory::new());
        factory.set_can_connect(false);
        factory.set_can_connect(true);

        let config = create_test_config();

        let manager = Phd2ProcessManager::with_spawner(config, spawner, factory);

        manager.start_phd2().await.unwrap();
        assert!(manager.has_managed_process().await);
    }

    #[tokio::test]
    async fn test_start_phd2_already_has_managed_process() {
        let spawner = Arc::new(MockProcessSpawner::new());
        spawner.add_spawn_success();
        spawner.add_spawn_success();

        let factory = Arc::new(MockConnectionFactory::new());
        // First start: not running -> running
        factory.set_can_connect(false);
        factory.set_can_connect(true);
        // Second start attempt: not running
        factory.set_can_connect(false);

        let config = create_test_config();

        let manager = Phd2ProcessManager::with_spawner(config, spawner.clone(), factory);

        // First start succeeds
        manager.start_phd2().await.unwrap();

        // Second start should fail because we already have a managed process
        let result = manager.start_phd2().await;
        assert!(matches!(result, Err(Phd2Error::ProcessAlreadyRunning)));

        // Only one spawn should have occurred
        assert_eq!(spawner.get_spawn_count(), 1);
    }

    // ============================================================================
    // stop_phd2 tests
    // ============================================================================

    #[tokio::test]
    async fn test_stop_phd2_no_managed_process() {
        let spawner = Arc::new(MockProcessSpawner::new());
        let factory = Arc::new(MockConnectionFactory::new());
        let config = create_test_config();

        let manager = Phd2ProcessManager::with_spawner(config, spawner, factory);

        // Should succeed even with no managed process
        manager.stop_phd2(None).await.unwrap();
    }

    #[tokio::test]
    async fn test_stop_phd2_without_client() {
        let spawner = Arc::new(MockProcessSpawner::new());
        spawner.add_spawn_success();

        let factory = Arc::new(MockConnectionFactory::new());
        factory.set_can_connect(false);
        factory.set_can_connect(true);

        let config = create_test_config();

        let manager = Phd2ProcessManager::with_spawner(config, spawner, factory);

        manager.start_phd2().await.unwrap();
        assert!(manager.has_managed_process().await);

        // Stop without client - should force kill
        manager.stop_phd2(None).await.unwrap();
        assert!(!manager.has_managed_process().await);
    }
}
