use std::path::{Path, PathBuf};
use crate::templates::Templates;

pub struct ScaffoldOptions {
    /// The directory the project folder will be created inside
    pub parent_dir: PathBuf,
    /// Snake_case project name, used for folder name and Cargo package name
    pub project_name: String,
}

/// Converts "my_plugin" to "MyPlugin" for the struct name
fn to_pascal_case(s: &str) -> String {
    s.split('_')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                None => String::new(),
                Some(first) => first.to_uppercase().to_string() + chars.as_str(),
            }
        })
        .collect()
}

/// Simple template substitution — replaces {{key}} with value
fn substitute(template: &str, key: &str, value: &str) -> String {
    template.replace(&format!("{{{{{}}}}}", key), value)
}

pub fn scaffold_project(opts: &ScaffoldOptions) -> Result<PathBuf, String> {
    let project_name = sanitize_project_name(&opts.project_name);
    if project_name.is_empty() {
        return Err("Project name cannot be empty".to_string());
    }

    let plugin_name = to_pascal_case(&project_name);
    let project_dir = opts.parent_dir.join(&project_name);

    if project_dir.exists() {
        return Err(format!(
            "Directory already exists: {}",
            project_dir.display()
        ));
    }

    // Create directory structure
    let src_dir = project_dir.join("src");
    std::fs::create_dir_all(&src_dir)
        .map_err(|e| format!("Failed to create project directory: {}", e))?;

    // Write Cargo.toml
    let cargo_content = apply_substitutions(
        Templates::CARGO_TOML,
        &project_name,
        &plugin_name,
    );
    write_file(&project_dir.join("Cargo.toml"), &cargo_content)?;

    // Write src/lib.rs
    let lib_content = apply_substitutions(
        Templates::LIB_RS,
        &project_name,
        &plugin_name,
    );
    write_file(&src_dir.join("lib.rs"), &lib_content)?;

    // Write src/params.rs
    let params_content = apply_substitutions(
        Templates::PARAMS_RS,
        &project_name,
        &plugin_name,
    );
    write_file(&src_dir.join("params.rs"), &params_content)?;

    // Write src/editor.rs
    let editor_content = apply_substitutions(
        Templates::EDITOR_RS,
        &project_name,
        &plugin_name,
    );
    write_file(&src_dir.join("editor.rs"), &editor_content)?;

    Ok(project_dir)
}

fn apply_substitutions(template: &str, project_name: &str, plugin_name: &str) -> String {
    let s = substitute(template, "project_name", project_name);
    let s = substitute(&s, "PluginName", plugin_name);
    // plugin_name_str is the human-readable display name with spaces
    let display_name = project_name.replace('_', " ");
    let s = substitute(&s, "plugin_name_str", &display_name);
    s
}

fn write_file(path: &Path, content: &str) -> Result<(), String> {
    std::fs::write(path, content)
        .map_err(|e| format!("Failed to write {}: {}", path.display(), e))
}

fn sanitize_project_name(name: &str) -> String {
    name.trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '_' { c } else { '_' })
        .collect::<String>()
        // Collapse consecutive underscores
        .split('_')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("_")
}
