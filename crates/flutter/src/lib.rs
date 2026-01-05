use gpui::{App, AppContext as _, Context, Window};
use workspace::Workspace;
use zed_actions::flutter::{HotReload, HotRestart, FlutterRun, FlutterStop, OpenFlutterLogs};

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
        workspace.register_action(flutter_stop);
        workspace.register_action(open_flutter_logs);
    })
    .detach();
}

fn flutter_run(workspace: &mut Workspace, action: &FlutterRun, window: &mut Window, cx: &mut Context<Workspace>) {
    let device_id = action.device_id.clone().unwrap_or_else(|| "macos".to_string());
    let target = action.target.clone().unwrap_or_else(|| "lib/main.dart".to_string());
    let cwd = action.cwd.clone();

    if let Some(panel) = workspace.panel::<FlutterLogPanel>(cx) {
        panel.update(cx, |view, cx| {
            view.start_run(device_id, target, cwd, cx);
        });
        workspace.toggle_panel_focus::<FlutterLogPanel>(window, cx);
    }
}

fn hot_reload(workspace: &mut Workspace, _: &HotReload, _: &mut Window, cx: &mut Context<Workspace>) {
    if let Some(panel) = workspace.panel::<FlutterLogPanel>(cx) {
        panel.read(cx).send_input("r");
    }
}

fn hot_restart(workspace: &mut Workspace, _: &HotRestart, _window: &mut Window, cx: &mut Context<Workspace>) {
    if let Some(panel) = workspace.panel::<FlutterLogPanel>(cx) {
        panel.update(cx, |view, cx| {
            view.send_input("R");
            view.clear_logs(cx);
        });
    }
}

fn flutter_stop(workspace: &mut Workspace, _: &FlutterStop, _window: &mut Window, cx: &mut Context<Workspace>) {
    if let Some(panel) = workspace.panel::<FlutterLogPanel>(cx) {
        panel.update(cx, |view, cx| {
            view.stop_run(cx);
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

