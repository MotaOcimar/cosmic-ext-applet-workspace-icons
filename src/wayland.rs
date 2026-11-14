// Copyright 2023 System76 <info@system76.com>
// SPDX-License-Identifier: GPL-3.0-only

use calloop::channel::*;
use cctk::{
    cosmic_protocols::{
        toplevel_info::v1::client::zcosmic_toplevel_handle_v1,
        toplevel_management::v1::client::zcosmic_toplevel_manager_v1,
    },
    sctk::{
        self,
        output::{OutputHandler, OutputState},
        reexports::{
            calloop,
            calloop_wayland_source::WaylandSource,
            client::{self as wayland_client},
            protocols::ext::{
                foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
                workspace::v1::client::ext_workspace_handle_v1::ExtWorkspaceHandleV1,
            },
        },
        registry::{ProvidesRegistryState, RegistryState},
        seat::{Capability, SeatHandler, SeatState},
    },
    toplevel_info::{ToplevelInfo, ToplevelInfoHandler, ToplevelInfoState},
    toplevel_management::{ToplevelManagerHandler, ToplevelManagerState},
    workspace::{Workspace, WorkspaceHandler, WorkspaceState},
};
use futures::{SinkExt, channel::mpsc, executor::block_on};
use std::os::{
    fd::{FromRawFd, RawFd},
    unix::net::UnixStream,
};
use wayland_client::{
    Connection, QueueHandle, WEnum,
    globals::registry_queue_init,
    protocol::{
        wl_output::{self, WlOutput},
        wl_seat::WlSeat,
    },
};

#[derive(Debug, Clone)]
pub enum WorkspaceEvent {
    Activate(ExtWorkspaceHandleV1),
    /// Focuses a window, restoring it first if it is minimized.
    ActivateToplevel(ExtForeignToplevelHandleV1),
    MinimizeToplevel(ExtForeignToplevelHandleV1),
}

#[derive(Debug, Clone)]
pub struct WorkspaceSnapshot {
    pub workspaces: Vec<Workspace>,
    pub toplevels: Vec<ToplevelInfo>,
    pub output: Option<WlOutput>,
}

pub fn spawn_workspaces(tx: mpsc::Sender<WorkspaceSnapshot>) -> SyncSender<WorkspaceEvent> {
    let (workspaces_tx, workspaces_rx) = calloop::channel::sync_channel(100);

    let socket = std::env::var("X_PRIVILEGED_WAYLAND_SOCKET")
        .ok()
        .and_then(|fd| {
            fd.parse::<RawFd>()
                .ok()
                .map(|fd| unsafe { UnixStream::from_raw_fd(fd) })
        });

    let conn = if let Some(socket) = socket {
        Connection::from_socket(socket)
    } else {
        Connection::connect_to_env()
    }
    .map_err(anyhow::Error::msg);

    if let Ok(conn) = conn {
        std::thread::spawn(move || {
            let configured_output = std::env::var("COSMIC_PANEL_OUTPUT")
                .ok()
                .unwrap_or_default();
            let mut event_loop = calloop::EventLoop::<State>::try_new().unwrap();
            let loop_handle = event_loop.handle();
            let (globals, event_queue) = registry_queue_init(&conn).unwrap();
            let qhandle = event_queue.handle();

            WaylandSource::new(conn, event_queue)
                .insert(loop_handle)
                .unwrap();

            let registry_state = RegistryState::new(&globals);
            let mut state = State {
                // Must be before `WorkspaceState`
                output_state: OutputState::new(&globals, &qhandle),
                seat_state: SeatState::new(&globals, &qhandle),
                configured_output,
                workspace_state: WorkspaceState::new(&registry_state, &qhandle),
                toplevel_info_state: ToplevelInfoState::new(&registry_state, &qhandle),
                // Optional: without it, icons still show but cannot focus windows.
                toplevel_manager_state: ToplevelManagerState::try_new(&registry_state, &qhandle),
                registry_state,
                expected_output: None,
                tx,
                running: true,
                have_workspaces: false,
            };
            let loop_handle = event_loop.handle();
            loop_handle
                .insert_source(workspaces_rx, |e, (), state| match e {
                    Event::Msg(WorkspaceEvent::Activate(handle)) => {
                        handle.activate();
                        state
                            .workspace_state
                            .workspace_manager()
                            .get()
                            .unwrap()
                            .commit();
                    }
                    Event::Msg(WorkspaceEvent::ActivateToplevel(handle)) => {
                        state.activate_toplevel(&handle);
                    }
                    Event::Msg(WorkspaceEvent::MinimizeToplevel(handle)) => {
                        state.minimize_toplevel(&handle);
                    }
                    Event::Closed => {
                        if let Ok(workspace_manager) =
                            state.workspace_state.workspace_manager().get()
                        {
                            for g in state.workspace_state.workspace_groups() {
                                g.handle.destroy();
                            }
                            workspace_manager.stop();
                        }
                    }
                })
                .unwrap();
            while state.running {
                event_loop.dispatch(None, &mut state).unwrap();
            }
        });
    } else {
        eprintln!("ENV variable WAYLAND_DISPLAY is missing. Exiting...");
        std::process::exit(1);
    }

    workspaces_tx
}

pub struct State {
    running: bool,
    tx: mpsc::Sender<WorkspaceSnapshot>,
    configured_output: String,
    expected_output: Option<WlOutput>,
    output_state: OutputState,
    seat_state: SeatState,
    registry_state: RegistryState,
    workspace_state: WorkspaceState,
    toplevel_info_state: ToplevelInfoState,
    toplevel_manager_state: Option<ToplevelManagerState>,
    have_workspaces: bool,
}

// Written by hand because `ToplevelManagerState` does not implement `Debug`.
impl std::fmt::Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("State")
            .field("running", &self.running)
            .field("configured_output", &self.configured_output)
            .field("expected_output", &self.expected_output)
            .field("workspace_state", &self.workspace_state)
            .field("toplevel_info_state", &self.toplevel_info_state)
            .field(
                "toplevel_manager",
                &self
                    .toplevel_manager_state
                    .as_ref()
                    .map(|state| &state.manager),
            )
            .field("have_workspaces", &self.have_workspaces)
            .finish_non_exhaustive()
    }
}

impl State {
    /// Returns the toplevel manager and the COSMIC handle of a window.
    fn managed_toplevel(
        &self,
        handle: &ExtForeignToplevelHandleV1,
    ) -> Option<(
        &zcosmic_toplevel_manager_v1::ZcosmicToplevelManagerV1,
        &ToplevelInfo,
        &zcosmic_toplevel_handle_v1::ZcosmicToplevelHandleV1,
    )> {
        let manager = &self.toplevel_manager_state.as_ref()?.manager;
        let info = self.toplevel_info_state.info(handle)?;
        let toplevel = info.cosmic_toplevel.as_ref()?;
        Some((manager, info, toplevel))
    }

    fn activate_toplevel(&self, handle: &ExtForeignToplevelHandleV1) {
        let Some((manager, info, toplevel)) = self.managed_toplevel(handle) else {
            tracing::warn!("cannot focus window: toplevel management unavailable");
            return;
        };
        let Some(seat) = self.seat_state.seats().next() else {
            tracing::warn!("cannot focus window: no seat");
            return;
        };
        if info
            .state
            .contains(&zcosmic_toplevel_handle_v1::State::Minimized)
        {
            manager.unset_minimized(toplevel);
        }
        manager.activate(toplevel, &seat);
    }

    fn minimize_toplevel(&self, handle: &ExtForeignToplevelHandleV1) {
        if let Some((manager, _, toplevel)) = self.managed_toplevel(handle) {
            manager.set_minimized(toplevel);
        } else {
            tracing::warn!("cannot minimize window: toplevel management unavailable");
        }
    }

    pub fn workspace_list(&self) -> Vec<Workspace> {
        self.workspace_state
            .workspace_groups()
            .filter(|g| {
                g.outputs
                    .iter()
                    .any(|o| Some(o) == self.expected_output.as_ref())
            })
            .flat_map(|g| {
                g.workspaces
                    .iter()
                    .filter_map(|handle| self.workspace_state.workspace_info(handle))
            })
            .cloned()
            .collect()
    }

    fn snapshot(&self) -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            workspaces: self.workspace_list(),
            toplevels: self.toplevel_info_state.toplevels().cloned().collect(),
            output: self.expected_output.clone(),
        }
    }

    fn send_snapshot(&mut self) {
        let snapshot = self.snapshot();
        let _ = block_on(self.tx.send(snapshot));
    }
}

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    sctk::registry_handlers![OutputState, SeatState,];
}

impl SeatHandler for State {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: WlSeat) {}

    fn new_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: WlSeat,
        _capability: Capability,
    ) {
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: WlSeat,
        _capability: Capability,
    ) {
    }

    fn remove_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: WlSeat) {}
}

impl ToplevelManagerHandler for State {
    fn toplevel_manager_state(&mut self) -> &mut ToplevelManagerState {
        // Manager events only arrive after the manager was bound.
        self.toplevel_manager_state
            .as_mut()
            .expect("toplevel manager event without a bound manager")
    }

    fn capabilities(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _capabilities: Vec<
            WEnum<zcosmic_toplevel_manager_v1::ZcosmicToplelevelManagementCapabilitiesV1>,
        >,
    ) {
    }
}

impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        let info = self.output_state.info(&output).unwrap();
        if info.name.as_deref() == Some(&self.configured_output) {
            self.expected_output = Some(output);
            if self.have_workspaces {
                self.send_snapshot();
            }
        }
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        if self.expected_output.as_ref() == Some(&output) {
            self.expected_output = None;
            self.send_snapshot();
        }
    }
}

impl WorkspaceHandler for State {
    fn workspace_state(&mut self) -> &mut WorkspaceState {
        &mut self.workspace_state
    }

    fn done(&mut self) {
        self.have_workspaces = true;
        self.send_snapshot();
    }
}

impl ToplevelInfoHandler for State {
    fn toplevel_info_state(&mut self) -> &mut ToplevelInfoState {
        &mut self.toplevel_info_state
    }

    fn new_toplevel(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _toplevel: &ExtForeignToplevelHandleV1,
    ) {
        self.send_snapshot();
    }

    fn update_toplevel(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _toplevel: &ExtForeignToplevelHandleV1,
    ) {
        self.send_snapshot();
    }

    fn toplevel_closed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        toplevel: &ExtForeignToplevelHandleV1,
    ) {
        // The toolkit invokes this callback immediately before removing the
        // closed toplevel from its state, so omit it from this snapshot.
        let mut snapshot = self.snapshot();
        snapshot
            .toplevels
            .retain(|info| info.foreign_toplevel != *toplevel);
        let _ = block_on(self.tx.send(snapshot));
    }
}

cctk::delegate_toplevel_info!(State);
cctk::delegate_toplevel_manager!(State);
cctk::delegate_workspace!(State);
sctk::delegate_output!(State);
sctk::delegate_registry!(State);
sctk::delegate_seat!(State);
