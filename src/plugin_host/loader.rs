#![allow(unsafe_code)]

use clack_host::prelude::*;
use std::path::{Path, PathBuf};

pub struct PluginBinary {
    pub entry: PluginEntry,
    pub plugin_id: String,
    pub plugin_name: String,
    pub path: PathBuf,
}

/// Load a .clap binary and find the first plugin in it.
pub fn load_clap_binary(clap_path: &Path) -> Result<PluginBinary, String> {
    if !clap_path.exists() {
        return Err(format!("CLAP file not found: {}", clap_path.display()));
    }

    let entry = unsafe { PluginEntry::load(clap_path) }
        .map_err(|e| format!("Failed to load CLAP entry: {e}"))?;

    let factory = entry
        .get_plugin_factory()
        .ok_or_else(|| "CLAP file has no plugin factory".to_string())?;

    let mut first_plugin = None;
    for descriptor in factory.plugin_descriptors() {
        let id = descriptor
            .id()
            .and_then(|id| id.to_str().ok())
            .map(|s| s.to_string());
        let name = descriptor.name().map(|n| n.to_string_lossy().to_string());

        if let Some(id) = id {
            first_plugin = Some((id, name.unwrap_or_else(|| "Unknown Plugin".to_string())));
            break;
        }
    }

    let (plugin_id, plugin_name) =
        first_plugin.ok_or_else(|| "No plugins found in CLAP file".to_string())?;

    Ok(PluginBinary {
        entry,
        plugin_id,
        plugin_name,
        path: clap_path.to_path_buf(),
    })
}

/// Find the .clap bundle produced by `cargo nih-plug bundle` in a project directory.
///
/// nih-plug puts bundles under `target/bundled/<plugin_name>.clap`.
/// On Linux the .clap is a directory containing the shared library.
/// On Windows/macOS it's a single file or app bundle.
pub fn find_clap_bundle(project_path: &Path) -> Result<PathBuf, String> {
    let bundled_dir = project_path.join("target").join("bundled");

    if !bundled_dir.exists() {
        return Err(format!(
            "No target/bundled directory found at {}. Run 'cargo nih-plug bundle' first.",
            bundled_dir.display()
        ));
    }

    // Look for any .clap file/directory in bundled/
    let mut clap_files: Vec<PathBuf> = Vec::new();

    let entries =
        std::fs::read_dir(&bundled_dir).map_err(|e| format!("Failed to read bundled dir: {e}"))?;

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().map_or(false, |ext| ext == "clap") {
            clap_files.push(path);
        }
    }

    if clap_files.is_empty() {
        return Err(format!("No .clap files found in {}", bundled_dir.display()));
    }

    if clap_files.len() > 1 {
        eprintln!(
            "[loader] Warning: multiple .clap files found, using first: {}",
            clap_files[0].display()
        );
    }

    let clap_path = clap_files.into_iter().next().unwrap();

    // On Linux, .clap is a directory — the actual .so is inside
    if clap_path.is_dir() {
        println!("[loader] Found .clap directory: {}", clap_path.display());
        
        // Look for the .so inside
        let inner_entries = std::fs::read_dir(&clap_path)
            .map_err(|e| format!("Failed to read .clap directory: {e}"))?;

        for entry in inner_entries.flatten() {
            let p = entry.path();
            if p.extension().map_or(false, |e| e == "so") {
                println!("[loader] Using .so file: {}", p.display());
                return Ok(p);
            }
        }

        // Some nih-plug versions put the .so at the top level with .clap extension directly
        // Try loading the directory path itself (clack may handle it)
        println!("[loader] No .so found inside, using directory path: {}", clap_path.display());
        return Ok(clap_path);
    }

    println!("[loader] Using .clap file: {}", clap_path.display());
    Ok(clap_path)
}

/// Get the plugin library name from Cargo.toml in the project directory.
/// Parses [lib] name or falls back to [package] name with hyphens → underscores.
pub fn get_plugin_lib_name(project_path: &Path) -> Result<String, String> {
    let cargo_toml_path = project_path.join("Cargo.toml");
    let content = std::fs::read_to_string(&cargo_toml_path)
        .map_err(|e| format!("Failed to read Cargo.toml: {e}"))?;

    // Simple parsing — look for [lib] name = "..."
    let mut in_lib_section = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_lib_section = trimmed == "[lib]";
            continue;
        }
        if in_lib_section && trimmed.starts_with("name") {
            if let Some(val) = trimmed.split('=').nth(1) {
                let name = val.trim().trim_matches('"').trim_matches('\'');
                return Ok(name.to_string());
            }
        }
    }

    // Fallback: package name
    let mut in_package_section = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_package_section = trimmed == "[package]";
            continue;
        }
        if in_package_section && trimmed.starts_with("name") {
            if let Some(val) = trimmed.split('=').nth(1) {
                let name = val.trim().trim_matches('"').trim_matches('\'');
                return Ok(name.replace('-', "_"));
            }
        }
    }

    Err("Could not determine plugin name from Cargo.toml".into())
}
