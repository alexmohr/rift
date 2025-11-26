use tracing::{error, info, warn};

use super::super::Screen;
use crate::actor::app::{AppThreadHandle, WindowId};
use crate::actor::reactor::{DisplaySelector, FocusDisplaySelector, Reactor, WorkspaceSwitchState};
use crate::actor::stack_line::Event as StackLineEvent;
use crate::actor::wm_controller::WmEvent;
use crate::actor::{menu_bar, raise_manager};
use crate::common::collections::HashMap;
use crate::common::config::{self as config, Config};
use crate::common::log::{MetricsCommand, handle_command};
use crate::layout_engine::{EventResponse, LayoutCommand, LayoutEvent};
use crate::sys::screen::{SpaceId, order_visible_spaces_by_position};
use crate::sys::window_server::{self as window_server, WindowServerId};

pub struct CommandEventHandler;

impl CommandEventHandler {
    pub fn handle_command_layout(reactor: &mut Reactor, cmd: LayoutCommand) {
        info!(?cmd);
        let visible_spaces_input: Vec<(SpaceId, _)> = reactor
            .space_manager
            .screens
            .iter()
            .filter_map(|screen| {
                let space = reactor.space_manager.space_for_screen(screen)?;
                let center = screen.frame.mid();
                Some((space, center))
            })
            .collect();

        let mut visible_space_centers = HashMap::default();
        for (space, center) in &visible_spaces_input {
            visible_space_centers.insert(*space, *center);
        }

        let visible_spaces = order_visible_spaces_by_position(visible_spaces_input.iter().cloned());

        let is_workspace_switch = matches!(
            cmd,
            LayoutCommand::NextWorkspace(_)
                | LayoutCommand::PrevWorkspace(_)
                | LayoutCommand::SwitchToWorkspace(_)
                | LayoutCommand::SwitchToLastWorkspace
        );
        let workspace_space = if is_workspace_switch {
            let space = reactor.workspace_command_space();
            if let Some(space) = space {
                reactor.store_current_floating_positions(space);
            }
            space
        } else {
            None
        };
        if is_workspace_switch {
            reactor.workspace_switch_manager.workspace_switch_generation =
                reactor.workspace_switch_manager.workspace_switch_generation.wrapping_add(1);
            reactor.workspace_switch_manager.active_workspace_switch =
                Some(reactor.workspace_switch_manager.workspace_switch_generation);
        }

        let response = match &cmd {
            LayoutCommand::NextWorkspace(_)
            | LayoutCommand::PrevWorkspace(_)
            | LayoutCommand::SwitchToWorkspace(_)
            | LayoutCommand::CreateWorkspace
            | LayoutCommand::SwitchToLastWorkspace => {
                if let Some(space) = workspace_space {
                    reactor
                        .layout_manager
                        .layout_engine
                        .handle_virtual_workspace_command(space, &cmd)
                } else {
                    EventResponse::default()
                }
            }
            LayoutCommand::MoveWindowToWorkspace { .. } => {
                if let Some(space) = reactor.workspace_command_space() {
                    reactor
                        .layout_manager
                        .layout_engine
                        .handle_virtual_workspace_command(space, &cmd)
                } else {
                    EventResponse::default()
                }
            }
            LayoutCommand::MoveWorkspaceToMonitor { direction, wrap_around } => {
                Self::handle_move_workspace_to_monitor(reactor, *direction, *wrap_around);
                EventResponse::default()
            }
            LayoutCommand::MoveNodeToMonitor { direction, wrap_around } => {
                Self::handle_move_node_to_monitor(reactor, *direction, *wrap_around);
                EventResponse::default()
            }
            _ => reactor.layout_manager.layout_engine.handle_command(
                reactor.workspace_command_space(),
                &visible_spaces,
                &visible_space_centers,
                cmd,
            ),
        };

        reactor.workspace_switch_manager.workspace_switch_state = if is_workspace_switch {
            WorkspaceSwitchState::Active
        } else {
            WorkspaceSwitchState::Inactive
        };
        reactor.handle_layout_response(response, workspace_space);
    }

    pub fn handle_command_metrics(_reactor: &mut Reactor, cmd: MetricsCommand) {
        handle_command(cmd);
    }

    pub fn handle_config_updated(reactor: &mut Reactor, new_cfg: Config) {
        let old_keys = reactor.config_manager.config.keys.clone();

        reactor.config_manager.config = new_cfg;
        reactor
            .layout_manager
            .layout_engine
            .set_layout_settings(&reactor.config_manager.config.settings.layout);

        reactor
            .layout_manager
            .layout_engine
            .update_virtual_workspace_settings(&reactor.config_manager.config.virtual_workspaces);

        reactor
            .drag_manager
            .update_config(reactor.config_manager.config.settings.window_snapping);

        if let Some(tx) = &reactor.communication_manager.stack_line_tx {
            if let Err(e) = tx.try_send(StackLineEvent::ConfigUpdated(
                reactor.config_manager.config.clone(),
            )) {
                warn!("Failed to send config update to stack line: {}", e);
            }
        }

        if let Some(tx) = &reactor.menu_manager.menu_tx {
            if let Err(e) = tx.try_send(menu_bar::Event::ConfigUpdated(
                reactor.config_manager.config.clone(),
            )) {
                warn!("Failed to send config update to menu bar: {}", e);
            }
        }

        let _ = reactor.update_layout(false, true).unwrap_or_else(|e| {
            warn!("Layout update failed: {}", e);
            false
        });

        if old_keys != reactor.config_manager.config.keys {
            if let Some(wm) = &reactor.communication_manager.wm_sender {
                wm.send(WmEvent::ConfigUpdated(reactor.config_manager.config.clone()));
            }
        }
    }

    pub fn handle_command_reactor_debug(reactor: &mut Reactor) {
        for screen in &reactor.space_manager.screens {
            if let Some(space) = reactor.space_manager.space_for_screen(screen) {
                reactor.layout_manager.layout_engine.debug_tree_desc(space, "", true);
            }
        }
    }

    pub fn handle_command_reactor_serialize(reactor: &mut Reactor) {
        if let Ok(state) = reactor.serialize_state() {
            println!("{}", state);
        }
    }

    pub fn handle_command_reactor_save_and_exit(reactor: &mut Reactor) {
        match reactor.layout_manager.layout_engine.save(config::restore_file()) {
            Ok(()) => std::process::exit(0),
            Err(e) => {
                error!("Could not save layout: {e}");
                std::process::exit(3);
            }
        }
    }

    pub fn handle_command_reactor_switch_space(
        _reactor: &mut Reactor,
        dir: crate::layout_engine::Direction,
    ) {
        unsafe { window_server::switch_space(dir) }
    }

    pub fn handle_command_reactor_focus_window(
        reactor: &mut Reactor,
        window_id: WindowId,
        window_server_id: Option<WindowServerId>,
    ) {
        if reactor.window_manager.windows.contains_key(&window_id) {
            if let Some(space) =
                reactor.window_manager.windows.get(&window_id).and_then(|w| {
                    reactor.best_space_for_window(&w.frame_monotonic, w.window_server_id)
                })
            {
                reactor.send_layout_event(LayoutEvent::WindowFocused(space, window_id));
            }

            let mut app_handles: HashMap<i32, AppThreadHandle> = HashMap::default();
            if let Some(app) = reactor.app_manager.apps.get(&window_id.pid) {
                app_handles.insert(window_id.pid, app.handle.clone());
            }
            let request = raise_manager::Event::RaiseRequest(raise_manager::RaiseRequest {
                raise_windows: Vec::new(),
                focus_window: Some((window_id, None)),
                app_handles,
            });
            if let Err(e) = reactor.communication_manager.raise_manager_tx.try_send(request) {
                warn!("Failed to send raise request: {}", e);
            }
        } else if let Some(wsid) = window_server_id {
            if let Err(e) = window_server::make_key_window(window_id.pid, wsid) {
                warn!("Failed to make key window: {:?}", e);
            }
        }
    }

    pub fn handle_command_reactor_show_mission_control_all(reactor: &mut Reactor) {
        if let Some(wm) = reactor.communication_manager.wm_sender.as_ref() {
            let _ = wm.send(crate::actor::wm_controller::WmEvent::Command(
                crate::actor::wm_controller::WmCommand::Wm(
                    crate::actor::wm_controller::WmCmd::ShowMissionControlAll,
                ),
            ));
        }
    }

    pub fn handle_command_reactor_show_mission_control_current(reactor: &mut Reactor) {
        if let Some(wm) = reactor.communication_manager.wm_sender.as_ref() {
            let _ = wm.send(crate::actor::wm_controller::WmEvent::Command(
                crate::actor::wm_controller::WmCommand::Wm(
                    crate::actor::wm_controller::WmCmd::ShowMissionControlCurrent,
                ),
            ));
        }
    }

    pub fn handle_command_reactor_dismiss_mission_control(reactor: &mut Reactor) {
        if let Some(wm) = reactor.communication_manager.wm_sender.as_ref() {
            let _ = wm.send(crate::actor::wm_controller::WmEvent::Command(
                crate::actor::wm_controller::WmCommand::Wm(
                    crate::actor::wm_controller::WmCmd::ShowMissionControlAll,
                ),
            ));
        } else {
            reactor.set_mission_control_active(false);
        }
    }

    fn focus_first_window_on_screen(reactor: &mut Reactor, screen: &Screen) -> bool {
        if let Some(space) = reactor.space_manager.space_for_screen(screen) {
            let focus_target = reactor.last_focused_window_in_space(space).or_else(|| {
                reactor
                    .layout_manager
                    .layout_engine
                    .windows_in_active_workspace(space)
                    .into_iter()
                    .next()
            });
            if let Some(window_id) = focus_target {
                reactor.send_layout_event(LayoutEvent::WindowFocused(space, window_id));
                return true;
            }
        }
        false
    }

    pub fn handle_command_reactor_move_mouse_to_display(
        reactor: &mut Reactor,
        selector: &DisplaySelector,
    ) {
        let target_screen = match selector {
            DisplaySelector::Index(idx) => reactor.space_manager.screens.get(*idx).cloned(),
            DisplaySelector::Uuid(uuid) => {
                reactor.space_manager.screens.iter().find(|s| s.display_uuid == *uuid).cloned()
            }
        };

        if let Some(screen) = target_screen {
            let center = screen.frame.mid();
            if let Some(event_tap_tx) = reactor.communication_manager.event_tap_tx.as_ref() {
                event_tap_tx.send(crate::actor::event_tap::Request::Warp(center));
            }
            let _ = Self::focus_first_window_on_screen(reactor, &screen);
        }
    }

    pub fn handle_command_reactor_focus_display(
        reactor: &mut Reactor,
        selector: &FocusDisplaySelector,
    ) {
        let screen = match reactor.screen_for_focus_selector(selector).cloned() {
            Some(s) => s,
            None => return,
        };

        if Self::focus_first_window_on_screen(reactor, &screen) {
            return;
        }

        if let Some(event_tap_tx) = reactor.communication_manager.event_tap_tx.as_ref() {
            event_tap_tx.send(crate::actor::event_tap::Request::Warp(screen.frame.mid()));
        }
    }

    pub fn handle_command_reactor_close_window(
        reactor: &mut Reactor,
        window_server_id: Option<WindowServerId>,
    ) {
        let target = window_server_id
            .and_then(|wsid| reactor.window_manager.window_ids.get(&wsid).copied())
            .or_else(|| reactor.main_window());
        if let Some(wid) = target {
            reactor.request_close_window(wid);
        } else {
            warn!("Close window command ignored because no window is tracked");
        }
    }

    fn handle_move_workspace_to_monitor(
        reactor: &mut Reactor,
        direction: crate::layout_engine::MonitorDirection,
        wrap_around: bool,
    ) {
        use crate::layout_engine::MonitorDirection;

        let Some(current_space) = reactor.workspace_command_space() else {
            warn!("Cannot move workspace: no current space");
            return;
        };

        let Some(current_screen) = reactor.space_manager.screen_by_space(current_space) else {
            warn!("Cannot move workspace: no screen for current space");
            return;
        };

        let target_screen = match direction {
            MonitorDirection::Left => {
                Self::find_adjacent_screen(reactor, current_screen, crate::layout_engine::Direction::Left, wrap_around)
            }
            MonitorDirection::Right => {
                Self::find_adjacent_screen(reactor, current_screen, crate::layout_engine::Direction::Right, wrap_around)
            }
            MonitorDirection::Next => {
                Self::find_next_screen(reactor, current_screen, wrap_around)
            }
        };

        let Some(target_screen) = target_screen else {
            info!("No target monitor found in direction {:?}", direction);
            return;
        };

        let Some(target_space) = reactor.space_manager.space_for_screen(target_screen) else {
            warn!("Target screen has no space");
            return;
        };

        // Check if the workspace already exists on the target display
        if target_space == current_space {
            info!("Workspace is already on the target monitor");
            return;
        }

        // Get all windows in the current active workspace
        let windows_to_move = reactor
            .layout_manager
            .layout_engine
            .windows_in_active_workspace(current_space);

        if windows_to_move.is_empty() {
            info!("No windows to move in current workspace");
            return;
        }

        info!("Moving {} windows from space {:?} to {:?}", windows_to_move.len(), current_space, target_space);

        // Move each window to the target space
        for window_id in windows_to_move {
            if let Some(window_state) = reactor.window_manager.windows.get(&window_id) {
                if let Some(wsid) = window_state.window_server_id {
                    if let Err(e) = window_server::move_window_to_space(wsid, target_space.get()) {
                        warn!("Failed to move window {:?} to target space: {:?}", window_id, e);
                    }
                }
            }
        }

        // Warp mouse to the target display
        if let Some(event_tap_tx) = reactor.communication_manager.event_tap_tx.as_ref() {
            event_tap_tx.send(crate::actor::event_tap::Request::Warp(target_screen.frame.mid()));
        }
    }

    fn handle_move_node_to_monitor(
        reactor: &mut Reactor,
        direction: crate::layout_engine::MonitorDirection,
        wrap_around: bool,
    ) {
        use crate::layout_engine::MonitorDirection;

        let Some(current_window) = reactor.main_window() else {
            warn!("Cannot move node: no active window");
            return;
        };

        let Some(current_space) = reactor.workspace_command_space() else {
            warn!("Cannot move node: no current space");
            return;
        };

        let Some(current_screen) = reactor.space_manager.screen_by_space(current_space) else {
            warn!("Cannot move node: no screen for current space");
            return;
        };

        let target_screen = match direction {
            MonitorDirection::Left => {
                Self::find_adjacent_screen(reactor, current_screen, crate::layout_engine::Direction::Left, wrap_around)
            }
            MonitorDirection::Right => {
                Self::find_adjacent_screen(reactor, current_screen, crate::layout_engine::Direction::Right, wrap_around)
            }
            MonitorDirection::Next => {
                Self::find_next_screen(reactor, current_screen, wrap_around)
            }
        };

        let Some(target_screen) = target_screen else {
            info!("No target monitor found in direction {:?}", direction);
            return;
        };

        let Some(target_space) = reactor.space_manager.space_for_screen(target_screen) else {
            warn!("Target screen has no space");
            return;
        };

        if target_space == current_space {
            info!("Window is already on the target monitor");
            return;
        }

        // Move the window to the target space
        if let Some(window_state) = reactor.window_manager.windows.get(&current_window) {
            if let Some(wsid) = window_state.window_server_id {
                if let Err(e) = window_server::move_window_to_space(wsid, target_space.get()) {
                    warn!("Failed to move window {:?} to target space: {:?}", current_window, e);
                } else {
                    info!("Moved window {:?} from space {:?} to {:?}", current_window, current_space, target_space);
                }
            }
        }
    }

    fn find_adjacent_screen<'a>(
        reactor: &'a Reactor,
        current_screen: &Screen,
        direction: crate::layout_engine::Direction,
        wrap_around: bool,
    ) -> Option<&'a Screen> {
        reactor.screen_for_focus_direction(direction).or_else(|| {
            if wrap_around {
                // If wrap-around is enabled and no screen found, go to opposite end
                let opposite_dir = direction.opposite();
                let mut furthest: Option<&Screen> = None;
                let origin = current_screen.frame.mid();

                for screen in &reactor.space_manager.screens {
                    if screen.screen_id == current_screen.screen_id {
                        continue;
                    }
                    let center = screen.frame.mid();
                    let delta = match opposite_dir {
                        crate::layout_engine::Direction::Left => origin.x - center.x,
                        crate::layout_engine::Direction::Right => center.x - origin.x,
                        crate::layout_engine::Direction::Up => center.y - origin.y,
                        crate::layout_engine::Direction::Down => origin.y - center.y,
                    };

                    if furthest.is_none() || delta > 0.0 {
                        furthest = Some(screen);
                    }
                }
                furthest
            } else {
                None
            }
        })
    }

    fn find_next_screen<'a>(
        reactor: &'a Reactor,
        current_screen: &Screen,
        wrap_around: bool,
    ) -> Option<&'a Screen> {
        let screens = &reactor.space_manager.screens;
        if screens.len() <= 1 {
            return None;
        }

        let current_idx = screens.iter().position(|s| s.screen_id == current_screen.screen_id)?;
        let next_idx = (current_idx + 1) % screens.len();

        if next_idx == current_idx {
            return None;
        }

        if !wrap_around && next_idx == 0 && current_idx > 0 {
            // We wrapped around but wrap_around is false
            return None;
        }

        screens.get(next_idx)
    }
}
