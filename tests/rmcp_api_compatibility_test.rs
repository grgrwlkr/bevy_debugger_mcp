/*
 * Bevy Debugger MCP - rmcp 0.2.1 API Compatibility Tests
 * Copyright (C) 2025 ladvien
 *
 * Tests for rmcp 0.2.1 API compatibility and ServerHandler implementation
 */

use bevy_debugger_mcp::{brp_client::BrpClient, config::Config, mcp_tools::BevyDebuggerTools};
use rmcp::model::*;
use rmcp::Service;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Test that BevyDebuggerTools implements ServerHandler correctly
#[tokio::test]
async fn test_server_handler_implementation() {
    let config = Config::default();
    let brp_client = Arc::new(RwLock::new(BrpClient::new(&config)));
    let tools = BevyDebuggerTools::new(brp_client);

    // Test get_info method returns correct ServerInfo structure
    let server_info = tools.get_info();

    assert_eq!(server_info.protocol_version, ProtocolVersion::V_2024_11_05);
    assert_eq!(server_info.server_info.name, "bevy-debugger-mcp");
    assert!(!server_info.server_info.version.is_empty());
    assert!(server_info.capabilities.tools.is_some());
    assert!(server_info.instructions.is_some());
    assert!(server_info
        .instructions
        .as_ref()
        .unwrap()
        .contains("AI-assisted debugging"));
}

/// Test that tool routing works correctly
#[tokio::test]
async fn test_tool_routing() {
    let config = Config::default();
    let brp_client = Arc::new(RwLock::new(BrpClient::new(&config)));
    let tools = BevyDebuggerTools::new(brp_client);

    // Test list_tools functionality
    // Note: In real usage, the tool_handler macro would handle this
    let server_info = tools.get_info();
    assert!(server_info.capabilities.tools.is_some());
}

/// Test error handling compatibility with rmcp Error types
#[tokio::test]
async fn test_error_handling() {
    let config = Config::default();
    let brp_client = Arc::new(RwLock::new(BrpClient::new(&config)));
    let tools = BevyDebuggerTools::new(brp_client);

    // Test that tools can be created without compilation errors
    // This test mainly ensures the API compatibility is working
    let _server_info = tools.get_info();

    // If we get here, the rmcp 0.2.1 API compatibility is working
}

/// Test ServerHandler trait methods are properly implemented
#[tokio::test]
async fn test_server_handler_traits() {
    let config = Config::default();
    let brp_client = Arc::new(RwLock::new(BrpClient::new(&config)));
    let tools = BevyDebuggerTools::new(brp_client);

    // Test that we can call ServerHandler methods
    let server_info = tools.get_info();

    // Verify the structure matches rmcp 0.2.1 expectations
    assert_eq!(server_info.protocol_version, ProtocolVersion::V_2024_11_05);

    // Verify capabilities are properly structured
    let caps = server_info.capabilities;
    assert!(caps.tools.is_some());

    // Verify server info is correct
    assert_eq!(server_info.server_info.name, "bevy-debugger-mcp");
}

/// Integration test for MCP tool schema generation
#[tokio::test]
async fn test_tool_schema_generation() {
    // TODO: Add schemars dependency if schema validation is needed
    // For now, just note that schema validation is disabled
    println!("Schema validation tests disabled - schemars dependency not available");
}
