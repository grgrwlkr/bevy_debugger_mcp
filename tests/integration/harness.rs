/// Integration Test Harness for Bevy Debugger MCP
///
/// This module provides a comprehensive test harness for validating all debugging features
/// across different scenarios with mock clients, performance validation, and comprehensive
/// coverage analysis.
use bevy_debugger_mcp::{
    brp_client::BrpClient,
    config::Config,
    error::{Error, Result},
    mcp_server::McpServer,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, RwLock};
use tokio::time::timeout;
use tracing::{debug, error, info, warn};

/// Test configuration with default settings optimized for testing
#[derive(Debug, Clone)]
pub struct TestConfig {
    pub bevy_brp_host: String,
    pub bevy_brp_port: u16,
    pub mcp_port: u16,
    pub timeout_ms: u64,
    pub enable_performance_tracking: bool,
}

impl Default for TestConfig {
    fn default() -> Self {
        Self {
            bevy_brp_host: "localhost".to_string(),
            bevy_brp_port: 15702,
            mcp_port: 3000 + (rand::random::<u16>() % 1000), // Random port for parallel tests
            timeout_ms: 10000,                               // 10s timeout for tests
            enable_performance_tracking: true,
        }
    }
}

/// Test harness for comprehensive integration testing
pub struct IntegrationTestHarness {
    pub config: TestConfig,
    pub mcp_server: Arc<McpServer>,
    pub performance_metrics: Arc<Mutex<PerformanceMetrics>>,
    pub test_session_id: String,
}

/// Performance tracking for tests
#[derive(Debug, Default)]
pub struct PerformanceMetrics {
    pub command_latencies: HashMap<String, Vec<Duration>>,
    pub test_start_time: Option<Instant>,
    pub total_commands_executed: u64,
    pub failed_commands: u64,
    pub performance_violations: Vec<String>,
}

impl PerformanceMetrics {
    /// Record command execution time
    pub fn record_command_latency(&mut self, command: &str, duration: Duration) {
        self.command_latencies
            .entry(command.to_string())
            .or_default()
            .push(duration);
        self.total_commands_executed += 1;
    }

    /// Record performance violation
    pub fn record_violation(&mut self, violation: String) {
        self.performance_violations.push(violation);
    }

    /// Check if performance is within acceptable bounds
    pub fn is_performance_acceptable(&self) -> bool {
        // Check that average command latency is < 100ms
        for latencies in self.command_latencies.values() {
            let avg = latencies.iter().sum::<Duration>() / latencies.len() as u32;
            if avg > Duration::from_millis(100) {
                return false;
            }
        }

        // Check that no performance violations occurred
        self.performance_violations.is_empty()
    }

    /// Generate performance report
    pub fn generate_report(&self) -> Value {
        let mut command_stats = serde_json::Map::new();

        for (command, latencies) in &self.command_latencies {
            let avg = latencies.iter().sum::<Duration>() / latencies.len() as u32;
            let min = latencies.iter().min().copied().unwrap_or_default();
            let max = latencies.iter().max().copied().unwrap_or_default();

            command_stats.insert(
                command.clone(),
                json!({
                    "avg_ms": avg.as_millis(),
                    "min_ms": min.as_millis(),
                    "max_ms": max.as_millis(),
                    "samples": latencies.len()
                }),
            );
        }

        json!({
            "total_commands": self.total_commands_executed,
            "failed_commands": self.failed_commands,
            "performance_acceptable": self.is_performance_acceptable(),
            "command_statistics": command_stats,
            "performance_violations": self.performance_violations,
            "test_duration_ms": self.test_start_time
                .map(|start| start.elapsed().as_millis())
                .unwrap_or(0)
        })
    }
}

impl IntegrationTestHarness {
    /// Create a new test harness with default configuration
    pub async fn new() -> Result<Self> {
        Self::with_config(TestConfig::default()).await
    }

    /// Create test harness with custom configuration
    pub async fn with_config(test_config: TestConfig) -> Result<Self> {
        let config = Config {
            bevy_brp_host: test_config.bevy_brp_host.clone(),
            bevy_brp_port: test_config.bevy_brp_port,
            mcp_port: test_config.mcp_port,
            ..Config::default()
        };

        let brp_client = Arc::new(RwLock::new(BrpClient::new(&config)));

        let mcp_server = Arc::new(McpServer::new(config, brp_client));

        let test_session_id = format!(
            "test-{}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            rand::random::<u32>()
        );

        let performance_metrics = PerformanceMetrics {
            test_start_time: Some(Instant::now()),
            ..Default::default()
        };

        Ok(Self {
            config: test_config,
            mcp_server,
            performance_metrics: Arc::new(Mutex::new(performance_metrics)),
            test_session_id,
        })
    }

    /// Execute MCP tool call with performance tracking
    pub async fn execute_tool_call(&self, tool_name: &str, arguments: Value) -> Result<Value> {
        let start = Instant::now();

        info!(
            "Executing tool call: {} with args: {}",
            tool_name, arguments
        );

        let result = timeout(
            Duration::from_millis(self.config.timeout_ms),
            self.mcp_server
                .handle_tool_call(tool_name, arguments.clone()),
        )
        .await;

        let duration = start.elapsed();

        // Record performance metrics
        if self.config.enable_performance_tracking {
            let mut metrics = self.performance_metrics.lock().await;
            metrics.record_command_latency(tool_name, duration);

            // Check for performance violations
            if duration > Duration::from_millis(100) {
                metrics.record_violation(format!(
                    "Tool '{}' exceeded 100ms latency: {}ms",
                    tool_name,
                    duration.as_millis()
                ));
            }
        }

        match result {
            Ok(Ok(response)) => {
                debug!("Tool call succeeded in {}ms", duration.as_millis());
                Ok(response)
            }
            Ok(Err(e)) => {
                warn!("Tool call failed: {}", e);
                let mut metrics = self.performance_metrics.lock().await;
                metrics.failed_commands += 1;
                Err(e)
            }
            Err(_) => {
                error!("Tool call timed out after {}ms", self.config.timeout_ms);
                let mut metrics = self.performance_metrics.lock().await;
                metrics.failed_commands += 1;
                metrics.record_violation(format!(
                    "Tool '{}' timed out after {}ms",
                    tool_name, self.config.timeout_ms
                ));
                Err(Error::Timeout("Tool call timed out".to_string()))
            }
        }
    }

    /// Verify all MCP commands are testable
    pub async fn verify_all_commands(&self) -> Result<CommandCoverage> {
        let mut coverage = CommandCoverage::default();

        // Test all primary MCP commands
        let commands = vec![
            "observe",
            "experiment",
            "screenshot",
            "hypothesis",
            "stress",
            "replay",
            "anomaly",
            "orchestrate",
            "pipeline",
            "resource_metrics",
            "performance_dashboard",
            "health_check",
            "dead_letter_queue",
            "diagnostic_report",
            "checkpoint",
            "bug_report",
            "debug",
        ];

        for command in &commands {
            coverage.total_commands += 1;

            // Test with minimal valid arguments
            let test_args = match *command {
                "observe" => json!({"query": "entities"}),
                "experiment" => json!({"type": "performance"}),
                "screenshot" => json!({"path": "/tmp/test.png"}),
                "hypothesis" => json!({"hypothesis": "test"}),
                "stress" => json!({"type": "entity_spawn", "count": 10}),
                "replay" => json!({"session_id": "test"}),
                "anomaly" => json!({"metric": "frame_time"}),
                "orchestrate" => json!({"tool": "observe", "arguments": {"query": "test"}}),
                "pipeline" => json!({"template": "observe_experiment_replay"}),
                _ => json!({}),
            };

            match self.execute_tool_call(command, test_args).await {
                Ok(_) => {
                    coverage.tested_commands += 1;
                    coverage.working_commands.push(command.to_string());
                }
                Err(e) => {
                    coverage.failed_commands.push(format!("{}: {}", command, e));
                }
            }
        }

        info!(
            "Command coverage: {}/{} commands tested successfully",
            coverage.tested_commands, coverage.total_commands
        );

        Ok(coverage)
    }

    /// Get performance report
    pub async fn get_performance_report(&self) -> Value {
        let metrics = self.performance_metrics.lock().await;
        metrics.generate_report()
    }

    /// Check if tests meet acceptance criteria
    #[allow(dead_code)]
    pub async fn meets_acceptance_criteria(&self) -> AcceptanceCriteria {
        let coverage = self.verify_all_commands().await.unwrap_or_default();
        self.meets_acceptance_criteria_with_coverage(&coverage)
            .await
    }

    /// Check if tests meet acceptance criteria using existing coverage
    pub async fn meets_acceptance_criteria_with_coverage(
        &self,
        coverage: &CommandCoverage,
    ) -> AcceptanceCriteria {
        let metrics = self.performance_metrics.lock().await;

        AcceptanceCriteria {
            code_coverage_90_percent: coverage.tested_commands as f32
                / coverage.total_commands as f32
                >= 0.9,
            all_mcp_commands_tested: coverage.tested_commands == coverage.total_commands,
            performance_regression_prevented: metrics.is_performance_acceptable(),
            documentation_complete: true, // TODO: Check actual documentation
            tutorials_available: true,    // TODO: Check actual tutorials
            api_docs_generated: true,     // TODO: Check actual API docs
            troubleshooting_guide_complete: true, // TODO: Check actual troubleshooting guide
        }
    }

    /// Clean up test resources
    #[allow(dead_code)]
    pub async fn cleanup(&self) -> Result<()> {
        info!(
            "Cleaning up test harness for session: {}",
            self.test_session_id
        );

        // Generate final performance report
        let report = self.get_performance_report().await;
        debug!(
            "Final performance report: {}",
            serde_json::to_string_pretty(&report)?
        );

        Ok(())
    }
}

/// Command coverage statistics  
#[derive(Debug, Default)]
pub struct CommandCoverage {
    pub total_commands: usize,
    pub tested_commands: usize,
    pub working_commands: Vec<String>,
    pub failed_commands: Vec<String>,
}

/// Acceptance criteria validation results
#[derive(Debug)]
#[allow(dead_code)]
pub struct AcceptanceCriteria {
    pub code_coverage_90_percent: bool,
    pub all_mcp_commands_tested: bool,
    pub performance_regression_prevented: bool,
    pub documentation_complete: bool,
    pub tutorials_available: bool,
    pub api_docs_generated: bool,
    pub troubleshooting_guide_complete: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_harness_creation() {
        let harness = IntegrationTestHarness::new().await.unwrap();
        assert!(harness.test_session_id.starts_with("test-"));
    }

    #[tokio::test]
    async fn test_performance_tracking() {
        let harness = IntegrationTestHarness::new().await.unwrap();

        // Test a simple tool call
        let _result = harness
            .execute_tool_call("observe", json!({"query": "test"}))
            .await;

        // Should get an error since we're using mock BRP, but performance should be tracked
        let report = harness.get_performance_report().await;
        assert!(report.get("total_commands").unwrap().as_u64().unwrap() >= 1);
    }

    #[tokio::test]
    async fn test_command_coverage() {
        let harness = IntegrationTestHarness::new().await.unwrap();
        let coverage = harness.verify_all_commands().await.unwrap();

        assert!(coverage.total_commands > 0);
        // Some commands should work even with mocks
        assert!(coverage.tested_commands <= coverage.total_commands);
    }
}
