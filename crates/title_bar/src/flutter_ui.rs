use gpui::{
    Action, Context, Entity, IntoElement, ParentElement, Render, Styled, Subscription, Task,
    Window, actions,
};
use project::Project;
use serde::Deserialize;
use ui::ContextMenu;
use ui::prelude::*;
use ui::{Button, ButtonStyle, IconButton, IconName, IconSize, PopoverMenu, Tooltip};
use util::ResultExt;
use zed_actions::flutter::{HotReload, HotRestart};

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct Device {
    id: String,
    name: String,
    target_platform: String,
    #[serde(default)]
    emulator: bool,
}

actions!(flutter_control, [SelectDevice]);

pub struct FlutterControls {
    project: Entity<Project>,
    devices: Vec<Device>,
    selected_device: Option<Device>,
    targets: Vec<String>,
    selected_target: Option<String>,
    worktree_root: Option<std::path::PathBuf>,
    is_loading_devices: bool,
    is_loading_targets: bool,
    fetch_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl FlutterControls {
    pub fn new(project: Entity<Project>, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            project: project.clone(),
            devices: Vec::new(),
            selected_device: None,
            targets: Vec::new(),
            selected_target: None,
            worktree_root: None,
            is_loading_devices: true,
            is_loading_targets: true,
            fetch_task: None,
            _subscriptions: Vec::new(),
        };
        this.refresh_devices(cx);
        this.refresh_targets(cx);
        this
    }

    fn refresh_devices(&mut self, cx: &mut Context<Self>) {
        self.is_loading_devices = true;
        self.fetch_task = Some(cx.spawn(async move |this, cx| {
            let output = cx
                .background_executor()
                .spawn(async move {
                    std::process::Command::new("flutter")
                        .args(["devices", "--machine"])
                        .output()
                })
                .await;

            this.update(cx, |controls, cx| {
                controls.is_loading_devices = false;
                if let Ok(ref output) = output {
                    if let Ok(json) = String::from_utf8(output.stdout.clone()) {
                        if let Ok(devices) = serde_json::from_str::<Vec<Device>>(&json) {
                            controls.devices = devices;
                            if controls.selected_device.is_none() {
                                controls.selected_device = controls.devices.first().cloned();
                            }
                        }
                    }
                }
                cx.notify();
            })
            .log_err();
        }));
    }

    fn refresh_targets(&mut self, cx: &mut Context<Self>) {
        let project = self.project.clone();
        self.is_loading_targets = true;

        cx.spawn(async move |this, cx| {
            let mut targets = Vec::new();
            let mut worktree_root = None;

            // Get worktree paths to scan
            let worktree_paths: Vec<_> = project
                .read_with(cx, |project, app| {
                    project
                        .worktrees(app)
                        .map(|wt| wt.read(app).abs_path().to_path_buf())
                        .collect()
                })
                .ok()
                .unwrap_or_default();

            for worktree_path in worktree_paths {
                // Store the first worktree root for computing absolute paths
                if worktree_root.is_none() {
                    worktree_root = Some(worktree_path.clone());
                }
                // Recursively find all pubspec.yaml files to locate Flutter packages
                find_dart_targets(&worktree_path, &worktree_path, &mut targets);
            }

            this.update(cx, |controls, cx| {
                controls.is_loading_targets = false;
                controls.worktree_root = worktree_root;
                if !targets.is_empty() {
                    targets.sort();
                    controls.targets = targets;
                    if controls.selected_target.is_none()
                        || !controls
                            .targets
                            .contains(controls.selected_target.as_ref().unwrap())
                    {
                        controls.selected_target = controls.targets.first().cloned();
                    }
                }
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }
}

fn find_dart_targets(
    base_path: &std::path::Path,
    current_path: &std::path::Path,
    targets: &mut Vec<String>,
) {
    // Recurse into subdirectories first (skip hidden dirs, build, .dart_tool)
    let Ok(entries) = std::fs::read_dir(current_path) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();

        if !path.is_dir() {
            continue;
        }

        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };

        if name.starts_with('.') || name == "build" || name == ".dart_tool" {
            continue;
        }

        find_dart_targets(base_path, &path, targets);
    }

    // Check if there's a pubspec.yaml here (indicating a Flutter/Dart package)
    let pubspec = current_path.join("pubspec.yaml");
    if !pubspec.exists() {
        return;
    }

    let lib_dir = current_path.join("lib");
    if !lib_dir.exists() {
        return;
    }

    let Ok(lib_entries) = std::fs::read_dir(&lib_dir) else {
        return;
    };

    for entry in lib_entries.flatten() {
        let path = entry.path();

        if !path.is_file() {
            continue;
        }

        let Some(filename) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };

        // Only include main*.dart files (main.dart, main_dev.dart, etc.)
        if !filename.starts_with("main") || !filename.ends_with(".dart") {
            continue;
        }

        let Ok(relative) = path.strip_prefix(base_path) else {
            continue;
        };
        let relative_str = relative.to_string_lossy().to_string();

        if targets.contains(&relative_str) {
            continue;
        }

        targets.push(relative_str);
    }
}

impl Render for FlutterControls {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let device_name = if self.is_loading_devices {
            "Loading...".to_string()
        } else {
            self.selected_device
                .as_ref()
                .map(|d| d.name.clone())
                .unwrap_or_else(|| "No Device".to_string())
        };
        let devices = self.devices.clone();

        let target_name = if self.is_loading_targets {
            "Loading...".to_string()
        } else {
            self.selected_target
                .clone()
                .unwrap_or_else(|| "No Target".to_string())
        };

        let this = cx.entity().downgrade();
        h_flex()
            .gap_1()
            .child({
                let devices = devices.clone();
                let this = this.clone();
                PopoverMenu::new("device-picker")
                    .trigger(
                        Button::new("device-picker-trigger", device_name)
                            .style(ButtonStyle::Subtle),
                    )
                    .menu(move |window, cx| {
                        let devices = devices.clone();
                        let this = this.clone();
                        ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
                            if devices.is_empty() {
                                menu = menu.label("No devices found");
                            } else {
                                for device in devices.iter() {
                                    let device_clone = device.clone();
                                    let this = this.clone();
                                    menu = menu.entry(
                                        device.name.clone(),
                                        None,
                                        move |_window, cx| {
                                            let device = device_clone.clone();
                                            this.update(cx, |controls, cx| {
                                                controls.selected_device = Some(device);
                                                cx.notify();
                                            })
                                            .log_err();
                                        },
                                    );
                                }
                            }
                            menu
                        })
                        .into()
                    })
            })
            .child({
                let target_name = std::path::Path::new(&target_name)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map_or_else(|| target_name.clone(), |s| s.to_string());
                let targets = self.targets.clone();
                let this = this.clone();
                PopoverMenu::new("target-picker")
                    .trigger(
                        Button::new("target-picker-trigger", target_name)
                            .style(ButtonStyle::Subtle),
                    )
                    .menu(move |window, cx| {
                        let targets = targets.clone();
                        let this = this.clone();
                        ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
                            for target in targets.iter() {
                                let target_clone = target.clone();
                                let this = this.clone();
                                menu = menu.entry(target.clone(), None, move |_window, cx| {
                                    let target = target_clone.clone();
                                    this.update(cx, |controls, cx| {
                                        controls.selected_target = Some(target);
                                        cx.notify();
                                    })
                                    .log_err();
                                });
                            }
                            menu
                        })
                        .into()
                    })
            })
            .child({
                let selected_device = self.selected_device.clone();
                let selected_target = self.selected_target.clone();
                let worktree_root = self.worktree_root.clone();
                IconButton::new("flutter-run", IconName::PlayFilled)
                    .icon_size(IconSize::Small)
                    .tooltip(|window, cx| Tooltip::text("Run Flutter")(window, cx))
                    .on_click(move |_event, window, cx| {
                        let (cwd, relative_target) = selected_target
                            .as_ref()
                            .map(|target| {
                                let path = std::path::Path::new(target);
                                let lib_dir = path.parent();
                                let filename = path.file_name().and_then(|f| f.to_str());
                                let pkg_relative = lib_dir.and_then(|lib| lib.parent());
                                let abs_cwd = pkg_relative.and_then(|pkg| {
                                    worktree_root
                                        .as_ref()
                                        .map(|root| root.join(pkg).to_string_lossy().to_string())
                                });
                                let rel_target = filename.map(|f| format!("lib/{}", f));
                                (abs_cwd, rel_target)
                            })
                            .unwrap_or((None, None));

                        window.dispatch_action(
                            zed_actions::flutter::FlutterRun {
                                device_id: selected_device.as_ref().map(|d| d.id.clone()),
                                target: relative_target,
                                cwd,
                            }
                            .boxed_clone(),
                            cx,
                        );
                    })
            })
            .child(
                IconButton::new("hot-reload", IconName::BoltFilled)
                    .icon_size(IconSize::Small)
                    .tooltip(|window, cx| Tooltip::text("Hot Reload")(window, cx))
                    .on_click(|_, window, cx| {
                        window.dispatch_action(HotReload.boxed_clone(), cx);
                    }),
            )
            .child(
                IconButton::new("hot-restart", IconName::RotateCw)
                    .icon_size(IconSize::Small)
                    .tooltip(|window, cx| Tooltip::text("Hot Restart")(window, cx))
                    .on_click(|_, window, cx| {
                        window.dispatch_action(HotRestart.boxed_clone(), cx);
                    }),
            )
            // Divider to separate Flutter controls from other title bar elements
            .child(div().w_px().h_4().bg(cx.theme().colors().border))
    }
}
