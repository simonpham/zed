use gpui::{App, AppContext as _, Context, Window};
use terminal_view::terminal_panel::TerminalPanel;
use terminal::Terminal;
use workspace::Workspace;
use zed_actions::flutter::{HotReload, HotRestart, FlutterRun, OpenFlutterLogs};

mod log_view;
mod vm_service;

pub use log_view::FlutterLogPanel;

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        if let Some(window) = window {
            let panel = cx.new(|cx| FlutterLogPanel::new(workspace, window, cx));
            workspace.add_panel(panel, window, cx);
        }

        workspace.register_action(hot_reload);
        workspace.register_action(hot_restart);
        workspace.register_action(flutter_run);
        workspace.register_action(open_flutter_logs);
    })
    .detach();
}

fn flutter_run(workspace: &mut Workspace, action: &FlutterRun, window: &mut Window, cx: &mut Context<Workspace>) {
    let device_id = action.device_id.clone().unwrap_or_else(|| "macos".to_string());
    let target = action.target.clone().unwrap_or_else(|| "lib/main.dart".to_string());
    let cwd = action.cwd.clone();

    let Some(panel) = workspace.panel::<TerminalPanel>(cx) else {
        return;
    };

    let spawn = task::SpawnInTerminal {
        command: Some("flutter".to_string()),
        args: vec![
            "run".to_string(),
            "-d".to_string(),
            device_id,
            "-t".to_string(),
            target.clone(),
            "--vmservice-out-file=.dart_tool/flutter_url".to_string(),
        ],
        full_label: "Flutter Run".to_string(),
        label: "flutter run".to_string(),
        command_label: "flutter run".to_string(),
        show_command: true,
        use_new_terminal: true,
        allow_concurrent_runs: true,
        reveal: task::RevealStrategy::Always,
        cwd: cwd.as_ref().map(|p| std::path::PathBuf::from(p)),
        ..Default::default()
    };

    // Resolve project root to find .dart_tool
    let mut search_path = None;
    if let Some(cwd) = &cwd {
        search_path = Some(std::path::PathBuf::from(cwd));
    } else if let Some(first_worktree) = workspace.worktrees(cx).next() {
        // Attempt to find pubspec.yaml starting from target
        // If target is relative, it is relative to worktree root usually, or cwd if set (handled above)
        let worktree_root = first_worktree.read(cx).abs_path();
        let target_path = std::path::PathBuf::from(&target);
        let full_target_path = if target_path.is_absolute() {
             target_path
        } else {
             worktree_root.join(target_path)
        };

        // Walk up from target to find pubspec.yaml
        let mut current = full_target_path.parent();
        while let Some(path) = current {
            if path.join("pubspec.yaml").exists() {
                search_path = Some(path.to_path_buf());
                break;
            }
            // Stop if we hit the worktree root to avoid going too far up system
            if path == worktree_root.as_ref() {
                 if path.join("pubspec.yaml").exists() {
                     search_path = Some(path.to_path_buf());
                 }
                 break;
            }
            current = path.parent();
        }
    }

    panel.update(cx, |panel, cx| {
        panel.add_terminal_task(spawn, task::RevealStrategy::Always, window, cx).detach();
    });

    if let Some(panel) = workspace.panel::<FlutterLogPanel>(cx) {
        panel.update(cx, |view, cx| {
            if let Some(path) = search_path {
                view.set_search_path(path.clone(), cx);
            }
            view.connect(cx);
        });
        workspace.toggle_panel_focus::<FlutterLogPanel>(window, cx);
    }
}

fn hot_reload(workspace: &mut Workspace, _: &HotReload, _: &mut Window, cx: &mut Context<Workspace>) {
    send_to_flutter_terminals(workspace, "r", cx);
}

fn hot_restart(workspace: &mut Workspace, _: &HotRestart, _window: &mut Window, cx: &mut Context<Workspace>) {
    send_to_flutter_terminals(workspace, "R", cx);
    if let Some(panel) = workspace.panel::<FlutterLogPanel>(cx) {
        panel.update(cx, |view, cx| {
            view.clear_logs(cx);
        });
    }
}

fn open_flutter_logs(workspace: &mut Workspace, _: &OpenFlutterLogs, window: &mut Window, cx: &mut Context<Workspace>) {
    if let Some(panel) = workspace.panel::<FlutterLogPanel>(cx) {
        panel.update(cx, |view, cx| {
            view.connect(cx);
        });
        workspace.toggle_panel_focus::<FlutterLogPanel>(window, cx);
    }
}

fn send_to_flutter_terminals(
    workspace: &mut Workspace,
    text: &str,
    cx: &mut Context<Workspace>,
) {
    let Some(panel) = workspace.panel::<TerminalPanel>(cx) else {
        return;
    };
    
    let terminals = panel.read(cx).terminals(cx);

    for terminal in terminals {
        let terminal: gpui::Entity<Terminal> = terminal;
        terminal.update(cx, |terminal, _cx| {
            // Check terminal title for "flutter" (task titles contain "Flutter Run" or "flutter run")
            let title = terminal.title(false);
            let is_flutter = title.to_lowercase().contains("flutter");
            
            // Fallback: also check process info
            let is_flutter = is_flutter || terminal.process_info().map_or(false, |info| {
                info.current.as_ref().map_or(false, |process| {
                    process.name == "flutter" || 
                    process.argv.iter().any(|arg| arg.contains("flutter"))
                })
            });

            if is_flutter {
                terminal.input(text.to_string().into_bytes());
            }
        });
    }
}
