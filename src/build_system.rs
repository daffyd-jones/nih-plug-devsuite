use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;

#[derive(Debug, Clone)]
pub enum BuildMessage {
    Stdout(String),
    Stderr(String),
    Finished { success: bool },
}

#[derive(Debug, Clone, PartialEq)]
pub enum BuildStatus {
    Idle,
    Building,
    Success,
    Failed,
}

pub struct BuildSystem {
    pub status: BuildStatus,
    pub output_lines: Vec<BuildOutputLine>,
    pub receiver: Option<mpsc::Receiver<BuildMessage>>,
    /// Set to Some after a successful build — consumed by the app to trigger reload.
    pub artifact_ready: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct BuildOutputLine {
    pub text: String,
    pub is_error: bool,
}

impl BuildSystem {
    pub fn new() -> Self {
        Self {
            status: BuildStatus::Idle,
            output_lines: Vec::new(),
            receiver: None,
            artifact_ready: None,
        }
    }

    /// Start a build using `cargo nih-plug bundle <name> --release`.
    pub fn start_build(&mut self, project_path: &Path) {
        if self.status == BuildStatus::Building {
            return;
        }

        self.status = BuildStatus::Building;
        self.output_lines.clear();
        self.artifact_ready = None;
        self.output_lines.push(BuildOutputLine {
            text: format!("Bundling plugin at {}...", project_path.display()),
            is_error: false,
        });

        let (tx, rx) = mpsc::channel();
        self.receiver = Some(rx);

        let path = project_path.to_path_buf();

        thread::spawn(move || {
            let result = run_nih_plug_bundle(&path, &tx);
            let success = result.is_ok();
            let _ = tx.send(BuildMessage::Finished { success });
        });
    }

    pub fn poll(&mut self) {
        if let Some(ref receiver) = self.receiver {
            while let Ok(msg) = receiver.try_recv() {
                match msg {
                    BuildMessage::Stdout(line) => {
                        self.output_lines.push(BuildOutputLine {
                            text: line,
                            is_error: false,
                        });
                    }
                    BuildMessage::Stderr(line) => {
                        let is_error = line.contains("error")
                            || line.contains("Error")
                            || line.contains("cannot find");
                        self.output_lines.push(BuildOutputLine {
                            text: line,
                            is_error,
                        });
                    }
                    BuildMessage::Finished { success } => {
                        self.status = if success {
                            BuildStatus::Success
                        } else {
                            BuildStatus::Failed
                        };
                        self.output_lines.push(BuildOutputLine {
                            text: if success {
                                "✓ Bundle succeeded".to_string()
                            } else {
                                "✗ Bundle failed".to_string()
                            },
                            is_error: !success,
                        });
                    }
                }
            }
        }

        if self.status == BuildStatus::Success || self.status == BuildStatus::Failed {
            self.receiver = None;
        }
    }
}

fn run_nih_plug_bundle(project_path: &Path, tx: &mpsc::Sender<BuildMessage>) -> Result<(), String> {
    // Determine lib name
    let package_name = get_package_name(project_path)?;

    let home = std::env::var("HOME").unwrap_or_default();
    let cargo_bin = format!("{}/.cargo/bin", home);
    let current_path = std::env::var("PATH").unwrap_or_default();
    let new_path = if current_path.is_empty() {
        cargo_bin.clone()
    } else {
        format!("{}:{}", cargo_bin, current_path)
    };

    // let _ = tx.send(BuildMessage::Stdout(format!("Using PATH: {}", new_path)));

    let _ = tx.send(BuildMessage::Stdout(format!(
        "Running: cargo nih-plug bundle {} --release",
        package_name
    )));

    let _ = tx.send(BuildMessage::Stdout(format!(
        "Run path: {:?} ",
        project_path
    )));

    // let _ = tx.send(BuildMessage::Stdout(format!(
    //     "CARGO_MANIFEST_DIR = {:?}",
    //     std::env::var("CARGO_MANIFEST_DIR")
    // )));

    let mut child = Command::new("cargo")
        .arg("nih-plug")
        .arg("bundle")
        .arg(&package_name)
        .arg("--release")
        .current_dir(project_path)
        .env("PATH", &new_path)
        .env_remove("CARGO_MANIFEST_DIR")
        .env_remove("CARGO_PKG_NAME")
        .env_remove("CARGO_PKG_VERSION")
        .env_remove("CARGO_CRATE_NAME")
        .env_remove("CARGO_PRIMARY_PACKAGE")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            format!(
                "Failed to spawn 'cargo nih-plug bundle'. Is cargo-nih-plug installed? Error: {}",
                e
            )
        })?;

    if let Some(stdout) = child.stdout.take() {
        let tx_clone = tx.clone();
        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines().flatten() {
                let _ = tx_clone.send(BuildMessage::Stdout(line));
            }
        });
    }

    if let Some(stderr) = child.stderr.take() {
        let tx_clone = tx.clone();
        thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines().flatten() {
                let _ = tx_clone.send(BuildMessage::Stderr(line));
            }
        });
    }

    let exit_status = child
        .wait()
        .map_err(|e| format!("Failed to wait on cargo: {}", e))?;

    if exit_status.success() {
        Ok(())
    } else {
        Err("Bundle failed".to_string())
    }
}

/// Parse Cargo.toml for [lib] name, falling back to [package] name.
fn get_package_name(project_path: &Path) -> Result<String, String> {
    let cargo_toml = project_path.join("Cargo.toml");
    let content =
        std::fs::read_to_string(&cargo_toml).map_err(|e| format!("Can't read Cargo.toml: {e}"))?;

    // // Look for [lib] name
    // let mut in_lib = false;
    // for line in content.lines() {
    //     let t = line.trim();
    //     if t.starts_with('[') {
    //         in_lib = t == "[lib]";
    //         continue;
    //     }
    //     if in_lib && t.starts_with("name") {
    //         if let Some(val) = t.split('=').nth(1) {
    //             return Ok(val.trim().trim_matches('"').trim_matches('\'').to_string());
    //         }
    //     }
    // }

    // Fallback to package name
    let mut in_pkg = false;
    for line in content.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_pkg = t == "[package]";
            continue;
        }
        if in_pkg && t.starts_with("name") {
            if let Some(val) = t.split('=').nth(1) {
                return Ok(val.trim().trim_matches('"').trim_matches('\'').to_string());
            }
        }
    }

    Err("Could not determine lib name from Cargo.toml".into())
}
