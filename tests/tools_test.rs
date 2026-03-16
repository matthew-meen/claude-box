use anyhow::Result;
use serde_json::json;
use std::fs;
use tempfile::TempDir;

use claude_box::tools::{prepare_mcp_config_with_home, resolve_binaries};

// ---------------------------------------------------------------------------
// Test 1: prepare_mcp_config with empty allow_tools copies the full config
// ---------------------------------------------------------------------------
#[test]
fn test_prepare_mcp_config_empty_allowlist_keeps_all_servers() -> Result<()> {
    let home = TempDir::new()?;
    let claude_dir = home.path().join(".claude");
    fs::create_dir_all(&claude_dir)?;

    let settings = json!({
        "mcpServers": {
            "server-a": {},
            "server-b": {},
            "server-c": {}
        }
    });
    fs::write(
        claude_dir.join("settings.json"),
        serde_json::to_string_pretty(&settings)?,
    )?;

    let result = prepare_mcp_config_with_home(&[], home.path())?;
    let (_path, value) = result.expect("should return Some");

    let servers = value["mcpServers"].as_object().expect("mcpServers object");
    assert_eq!(servers.len(), 3, "all 3 servers should be present");
    assert!(servers.contains_key("server-a"));
    assert!(servers.contains_key("server-b"));
    assert!(servers.contains_key("server-c"));
    Ok(())
}

// ---------------------------------------------------------------------------
// Test 2: prepare_mcp_config with allow list filters out unlisted servers
// ---------------------------------------------------------------------------
#[test]
fn test_prepare_mcp_config_allowlist_filters_servers() -> Result<()> {
    let home = TempDir::new()?;
    let claude_dir = home.path().join(".claude");
    fs::create_dir_all(&claude_dir)?;

    let settings = json!({
        "mcpServers": {
            "keep-me": { "command": "foo" },
            "remove-a": { "command": "bar" },
            "remove-b": { "command": "baz" }
        }
    });
    fs::write(
        claude_dir.join("settings.json"),
        serde_json::to_string_pretty(&settings)?,
    )?;

    let allow = vec!["keep-me".to_string()];
    let result = prepare_mcp_config_with_home(&allow, home.path())?;
    let (_path, value) = result.expect("should return Some");

    let servers = value["mcpServers"].as_object().expect("mcpServers object");
    assert_eq!(servers.len(), 1, "only the allowed server should remain");
    assert!(servers.contains_key("keep-me"));
    assert!(!servers.contains_key("remove-a"));
    assert!(!servers.contains_key("remove-b"));
    Ok(())
}

// ---------------------------------------------------------------------------
// Test 3: prepare_mcp_config with no settings file returns None
// ---------------------------------------------------------------------------
#[test]
fn test_prepare_mcp_config_no_settings_returns_none() -> Result<()> {
    let home = TempDir::new()?;
    let result = prepare_mcp_config_with_home(&[], home.path())?;
    assert!(result.is_none(), "should return None when no settings file exists");
    Ok(())
}

// ---------------------------------------------------------------------------
// Test 4: resolve_binaries with /bin/sh succeeds and returns source dir
// ---------------------------------------------------------------------------
#[test]
fn test_resolve_binaries_sh_succeeds() -> Result<()> {
    let shares = resolve_binaries(&["sh".to_string()])?;
    assert_eq!(shares.len(), 1, "sh should produce one dir share");
    let share = &shares[0];
    assert!(
        share.host_dir.exists(),
        "host_dir should exist: {}",
        share.host_dir.display()
    );
    assert!(
        share.names.contains(&"sh".to_string()),
        "share should list 'sh' as an allowed name"
    );
    assert_eq!(share.tag, "claude-box-bin-0");
    Ok(())
}

// ---------------------------------------------------------------------------
// Test 5: resolve_binaries with a nonexistent binary returns an error
// ---------------------------------------------------------------------------
#[test]
fn test_resolve_binaries_missing_binary_returns_error() {
    let result = resolve_binaries(&["nonexistent_binary_xyz_abc".to_string()]);
    assert!(result.is_err(), "missing binary should produce an error");
    let err = result.unwrap_err();
    assert!(
        err.to_string().contains("not found on host PATH"),
        "error message should mention 'not found on host PATH', got: {err}"
    );
}

// ---------------------------------------------------------------------------
// Test 6: resolve_binaries with empty list returns empty vec
// ---------------------------------------------------------------------------
#[test]
fn test_resolve_binaries_empty_list_is_empty() -> Result<()> {
    let shares = resolve_binaries(&[])?;
    assert!(shares.is_empty(), "empty allow list should return empty vec");
    Ok(())
}

// ---------------------------------------------------------------------------
// Test 7: two binaries from the same directory share one entry
// ---------------------------------------------------------------------------
#[test]
fn test_resolve_binaries_same_dir_grouped() -> Result<()> {
    // sh and ls are both typically in /bin on macOS
    let shares = resolve_binaries(&["sh".to_string(), "ls".to_string()])?;
    // Both should be in the same directory share if they resolve to the same parent
    let total_names: usize = shares.iter().map(|s| s.names.len()).sum();
    assert_eq!(total_names, 2, "both binaries should be tracked");
    // Tags should be unique
    let tags: std::collections::HashSet<&str> = shares.iter().map(|s| s.tag.as_str()).collect();
    assert_eq!(tags.len(), shares.len(), "all tags must be unique");
    Ok(())
}
