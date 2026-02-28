use eframe::egui;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct FileBrowser {
    expanded_dirs: std::collections::HashSet<PathBuf>,
}

impl FileBrowser {
    pub fn new() -> Self {
        Self {
            expanded_dirs: std::collections::HashSet::new(),
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui, root: &Path) -> Option<PathBuf> {
        let mut clicked_file = None;

        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.label(
                egui::RichText::new(
                    root.file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("Project"),
                )
                .strong()
                .size(14.0),
            );
            ui.separator();
            self.show_directory(ui, root, &mut clicked_file);
        });

        clicked_file
    }

    fn show_directory(
        &mut self,
        ui: &mut egui::Ui,
        dir: &Path,
        clicked_file: &mut Option<PathBuf>,
    ) {
        let mut entries: Vec<_> = match std::fs::read_dir(dir) {
            Ok(entries) => entries.filter_map(|e| e.ok()).collect(),
            Err(_) => return,
        };

        entries.sort_by(|a, b| {
            let a_is_dir = a.file_type().map(|t| t.is_dir()).unwrap_or(false);
            let b_is_dir = b.file_type().map(|t| t.is_dir()).unwrap_or(false);
            match (a_is_dir, b_is_dir) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => a.file_name().cmp(&b.file_name()),
            }
        });

        for entry in entries {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();

            if name.starts_with('.') || name == "target" {
                continue;
            }

            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);

            if is_dir {
                let is_expanded = self.expanded_dirs.contains(&path);
                let icon = if is_expanded { "📂" } else { "📁" };
                let header = format!("{} {}", icon, name);

                let response = ui.selectable_label(false, &header);
                if response.clicked() {
                    if is_expanded {
                        self.expanded_dirs.remove(&path);
                    } else {
                        self.expanded_dirs.insert(path.clone());
                    }
                }

                if is_expanded {
                    ui.indent(&path, |ui| {
                        self.show_directory(ui, &path, clicked_file);
                    });
                }
            } else {
                let icon = file_icon(&name);
                let label = format!("{} {}", icon, name);

                if ui.selectable_label(false, &label).clicked() {
                    *clicked_file = Some(path);
                }
            }
        }
    }
}

fn file_icon(filename: &str) -> &'static str {
    if filename.ends_with(".rs") {
        "🦀"
    } else if filename.ends_with(".toml") {
        "⚙"
    } else if filename.ends_with(".md") {
        "📝"
    } else if filename.ends_with(".lock") {
        "🔒"
    } else {
        "📄"
    }
}
