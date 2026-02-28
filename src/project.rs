use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectConfig {
    pub name: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct OpenFile {
    pub path: PathBuf,
    pub content: String,
    pub modified: bool,
}

#[derive(Debug)]
pub struct Project {
    pub config: ProjectConfig,
    pub open_files: Vec<OpenFile>,
    pub active_file_index: Option<usize>,
}

impl Project {
    pub fn open(path: PathBuf) -> Result<Self, String> {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("untitled")
            .to_string();

        if !path.exists() {
            return Err(format!("Path does not exist: {}", path.display()));
        }

        Ok(Self {
            config: ProjectConfig { name, path },
            open_files: Vec::new(),
            active_file_index: None,
        })
    }

    pub fn open_file(&mut self, path: &Path) -> Result<(), String> {
        // Check if already open
        if let Some(idx) = self.open_files.iter().position(|f| f.path == path) {
            self.active_file_index = Some(idx);
            return Ok(());
        }

        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;

        self.open_files.push(OpenFile {
            path: path.to_path_buf(),
            content,
            modified: false,
        });

        self.active_file_index = Some(self.open_files.len() - 1);
        Ok(())
    }

    pub fn save_active_file(&mut self) -> Result<(), String> {
        if let Some(idx) = self.active_file_index {
            let file = &mut self.open_files[idx];
            std::fs::write(&file.path, &file.content)
                .map_err(|e| format!("Failed to save {}: {}", file.path.display(), e))?;
            file.modified = false;
        }
        Ok(())
    }

    pub fn save_all_files(&mut self) -> Result<(), String> {
        for file in &mut self.open_files {
            if file.modified {
                std::fs::write(&file.path, &file.content)
                    .map_err(|e| format!("Failed to save {}: {}", file.path.display(), e))?;
                file.modified = false;
            }
        }
        Ok(())
    }

    pub fn close_file(&mut self, index: usize) {
        if index < self.open_files.len() {
            self.open_files.remove(index);
            self.active_file_index = match self.active_file_index {
                Some(active) if active == index => {
                    if self.open_files.is_empty() {
                        None
                    } else {
                        Some(active.min(self.open_files.len() - 1))
                    }
                }
                Some(active) if active > index => Some(active - 1),
                other => other,
            };
        }
    }

    pub fn active_file(&self) -> Option<&OpenFile> {
        self.active_file_index.map(|idx| &self.open_files[idx])
    }

    pub fn active_file_mut(&mut self) -> Option<&mut OpenFile> {
        self.active_file_index.map(|idx| &mut self.open_files[idx])
    }
}

