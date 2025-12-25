use gpui::{App, Context, Window};
use terminal_view::terminal_panel::TerminalPanel;
use terminal::Terminal;
use workspace::Workspace;
use zed_actions::flutter::{HotReload, HotRestart, FlutterRun};

mod log_view;

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(hot_reload);
        workspace.register_action(hot_restart);
        workspace.register_action(flutter_run);
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
            target,
            "--vmservice-out-file=.dart_tool/flutter_url".to_string(),
        ],
        full_label: "Flutter Run".to_string(),
        label: "flutter run".to_string(),
        command_label: "flutter run".to_string(),
        show_command: true,
        use_new_terminal: true,
        allow_concurrent_runs: true,
        reveal: task::RevealStrategy::Always,
        cwd: cwd.map(|p| std::path::PathBuf::from(p)),
        ..Default::default()
    };

    panel.update(cx, |panel, cx| {
        panel.add_terminal_task(spawn, task::RevealStrategy::Always, window, cx).detach();
    });
}

fn hot_reload(workspace: &mut Workspace, _: &HotReload, _: &mut Window, cx: &mut Context<Workspace>) {
    send_to_flutter_terminals(workspace, "r", cx);
}

fn hot_restart(workspace: &mut Workspace, _: &HotRestart, _: &mut Window, cx: &mut Context<Workspace>) {
    send_to_flutter_terminals(workspace, "R", cx);
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
