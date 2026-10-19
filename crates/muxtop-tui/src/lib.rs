pub mod app;
pub mod clipboard;
pub mod error;
pub mod event;
pub mod keymap;
pub mod notify;
pub mod terminal;
pub mod ui;

pub use app::{AppState, Command, ConfirmAction, Overlay, PaletteState, Tab};
pub use error::TuiError;
pub use event::{Event, EventHandler, TICK_RATE};
pub use keymap::{Action, Binding};
pub use notify::{Notifier, Toast};
pub use terminal::{
    Breakpoint, CapsOverrides, ColorSupport, TermCaps, TerminalGuard, Tui, detect_terminal_caps,
    detect_terminal_caps_with, init_terminal, install_panic_hook, restore_terminal,
};
pub use ui::theme::{Level, ThemeKind};

use std::net::SocketAddr;

use muxtop_core::process::SortField;
use muxtop_core::system::SystemSnapshot;
use tokio::sync::mpsc;

/// Whether the TUI is monitoring the local machine or a remote server.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum ConnectionMode {
    /// Collecting system data locally via sysinfo.
    #[default]
    Local,
    /// Receiving snapshots from a remote muxtop-server.
    Remote { hostname: String, addr: SocketAddr },
}

/// Remote data availability, displayed persistently alongside host identity.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum RemoteStatus {
    #[default]
    Waiting,
    Connected,
    Stale,
    AuthenticationFailed,
    ServerError(u16),
}

impl RemoteStatus {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Waiting => "connecting / stale",
            Self::Connected => "live",
            Self::Stale => "disconnected / stale",
            Self::AuthenticationFailed => "authentication failed / stale",
            Self::ServerError(_) => "server error / stale",
        }
    }
}

impl AppState {
    fn apply_remote_update(&mut self, update: muxtop_proto::RemoteUpdate) {
        match update {
            muxtop_proto::RemoteUpdate::Connection(event) => self.apply_connection_event(event),
            muxtop_proto::RemoteUpdate::Snapshot(snapshot) => {
                self.apply_transport_snapshot(snapshot)
            }
        }
    }

    fn apply_transport_snapshot(&mut self, snapshot: SystemSnapshot) {
        let accepted = !self.paused;
        self.apply_snapshot(snapshot);
        if accepted
            && self.is_remote()
            && matches!(
                self.remote_status,
                RemoteStatus::Waiting | RemoteStatus::Connected
            )
        {
            self.remote_status = RemoteStatus::Connected;
        }
    }

    /// Apply transport status independently of snapshot arrival and pause mode.
    pub fn apply_connection_event(&mut self, event: muxtop_proto::ConnectionEvent) {
        use muxtop_proto::ConnectionEvent;
        self.remote_status = match event {
            ConnectionEvent::Connected { hostname } => {
                if let ConnectionMode::Remote {
                    hostname: current, ..
                } = &mut self.connection_mode
                {
                    *current = hostname;
                }
                // Existing samples stay stale until a fresh snapshot arrives.
                RemoteStatus::Waiting
            }
            ConnectionEvent::Disconnected => {
                if matches!(
                    self.remote_status,
                    RemoteStatus::AuthenticationFailed | RemoteStatus::ServerError(_)
                ) {
                    self.remote_status.clone()
                } else {
                    RemoteStatus::Stale
                }
            }
            ConnectionEvent::ServerError { code: 401, .. } => RemoteStatus::AuthenticationFailed,
            ConnectionEvent::ServerError { code, .. } => RemoteStatus::ServerError(code),
        };
    }
}

/// Configuration passed from CLI arguments to the TUI.
#[derive(Debug, Clone)]
pub struct CliConfig {
    /// Initial process filter pattern (from `--filter`).
    pub filter: Option<String>,
    /// Initial sort field (from `--sort`).
    pub sort_field: SortField,
    /// Start in tree view mode (from `--tree`).
    pub tree_mode: bool,
    /// Local or remote connection mode.
    pub connection_mode: ConnectionMode,
    /// Colour scheme (from `--theme`).
    pub theme: ThemeKind,
    /// Terminal capability overrides (`--no-color`, `--ascii`, `--no-mouse`).
    pub caps: CapsOverrides,
}

impl Default for CliConfig {
    fn default() -> Self {
        Self {
            filter: None,
            sort_field: SortField::Cpu,
            tree_mode: false,
            connection_mode: ConnectionMode::default(),
            theme: ThemeKind::default(),
            caps: CapsOverrides::default(),
        }
    }
}

/// Run the TUI event loop. Blocks until the user quits.
/// The TerminalGuard ensures the terminal is restored on any exit path
/// (normal return, error propagation via ?, or panic unwind).
///
/// `container_engine` is shared with the Collector so that Stop/Kill/Restart
/// actions dispatched from the TUI hit the same daemon. Pass `None` to
/// disable container actions (the UI surfaces a "not configured" status
/// message instead of spawning work).
///
/// `cluster_engine` is shared the same way so the `A` key can rescope the
/// Kube tab between a single namespace and all namespaces. Pass `None` in
/// remote mode — the server owns the kubeconfig there — or when `--no-kube`
/// is set.
pub fn run(
    rx: mpsc::Receiver<SystemSnapshot>,
    config: CliConfig,
    container_engine: Option<
        std::sync::Arc<dyn muxtop_core::container_engine::ContainerEngine + Send + Sync>,
    >,
    cluster_engine: Option<
        std::sync::Arc<dyn muxtop_core::cluster_engine::ClusterEngine + Send + Sync>,
    >,
) -> Result<(), TuiError> {
    run_with_connection_events(rx, config, container_engine, cluster_engine, None)
}

/// Run with transport events, retaining `run` for existing local callers.
pub fn run_with_connection_events(
    rx: mpsc::Receiver<SystemSnapshot>,
    config: CliConfig,
    container_engine: Option<
        std::sync::Arc<dyn muxtop_core::container_engine::ContainerEngine + Send + Sync>,
    >,
    cluster_engine: Option<
        std::sync::Arc<dyn muxtop_core::cluster_engine::ClusterEngine + Send + Sync>,
    >,
    mut connection_rx: Option<mpsc::Receiver<muxtop_proto::RemoteUpdate>>,
) -> Result<(), TuiError> {
    install_panic_hook();
    // Capabilities are detected before the terminal is put into raw mode so the
    // mouse decision is known by the time we would enable capture.
    let term_caps = detect_terminal_caps_with(config.caps);
    let mut guard = init_terminal(term_caps.mouse)?;
    let mut app = app::AppState::with_config(config, term_caps);
    if let Some(engine) = container_engine {
        app.set_container_engine(engine);
    }
    if let Some(engine) = cluster_engine {
        app.set_cluster_engine(engine);
    }
    let mut handler = EventHandler::new(rx);

    while app.running() {
        // Drain any container-action outcomes produced by spawned tokio
        // tasks (Stop/Kill/Restart) so they surface as status messages.
        // `pump_action_results` flips `needs_redraw` if anything arrived.
        app.pump_action_results();

        // PERF-H1: event-driven render. We only repaint when something the
        // UI actually shows has changed: a new event from the user / OS, a
        // new snapshot, an async action outcome, or an expired status
        // message. Idle ticks at 60 Hz no longer pay for a full draw.
        let mut should_draw = app.take_needs_redraw();
        if let Some(rx) = &mut connection_rx {
            while let Ok(event) = rx.try_recv() {
                app.apply_remote_update(event);
                should_draw = true;
            }
        }

        match handler.poll_event()? {
            Event::Key(key) => {
                app.handle_key_event(key);
                should_draw = true;
            }
            Event::Mouse(mouse) => {
                app.handle_mouse_event(mouse);
                should_draw = true;
            }
            Event::Snapshot(snap) => {
                app.apply_transport_snapshot(snap);
                should_draw = true;
            }
            Event::Resize(width, height) => {
                // The layout picks its breakpoint from these, so they have to
                // be current before the next frame is composed.
                app.term_caps.set_size(width, height);
                should_draw = true;
            }
            Event::Tick => {
                should_draw |= handle_idle_tick_at(&mut app, std::time::Instant::now());
            }
        }

        if should_draw {
            app.tick();
            guard.terminal.draw(|frame| ui::draw_root(frame, &app))?;
        }
    }

    // Explicit restore for clean exit (TerminalGuard Drop is the safety net).
    let mouse_enabled = guard.mouse_enabled();
    restore_terminal(&mut guard.terminal, mouse_enabled)?;
    Ok(())
}

/// Advance deferred input without forcing redraws during otherwise idle ticks.
pub(crate) fn handle_idle_tick_at(app: &mut AppState, now: std::time::Instant) -> bool {
    app.flush_filter_at(now);
    let filter_changed = app.take_needs_redraw();
    app.status_message_just_expired() || filter_changed
}

#[cfg(test)]
mod tests {
    use super::*;

    // @generated by:gauntlet run:muxtop-audit-fixes
    #[test]
    fn t_ac_04_reconnect_fifo_cannot_relabel_queued_previous_sample_live() {
        use muxtop_proto::{ConnectionEvent, RemoteUpdate};
        let mut app = AppState::with_config(
            CliConfig {
                connection_mode: ConnectionMode::Remote {
                    hostname: "host".into(),
                    addr: "127.0.0.1:1".parse().unwrap(),
                },
                ..CliConfig::default()
            },
            TermCaps::default(),
        );
        let (tx, mut rx) = mpsc::channel(8);
        for update in [
            RemoteUpdate::Connection(ConnectionEvent::Connected {
                hostname: "old".into(),
            }),
            RemoteUpdate::Snapshot(ui::test_support::snapshot()),
            RemoteUpdate::Connection(ConnectionEvent::Disconnected),
            RemoteUpdate::Connection(ConnectionEvent::Connected {
                hostname: "new".into(),
            }),
        ] {
            tx.try_send(update).unwrap();
        }
        while let Ok(update) = rx.try_recv() {
            app.apply_remote_update(update);
        }
        assert!(app.last_snapshot.is_some());
        assert_eq!(
            app.remote_status,
            RemoteStatus::Waiting,
            "old samples precede the new session boundary"
        );
        app.apply_remote_update(RemoteUpdate::Snapshot(ui::test_support::snapshot()));
        assert_eq!(app.remote_status, RemoteStatus::Connected);
    }

    // @generated by:gauntlet run:muxtop-audit-fixes
    #[test]
    fn t_ac_04_remote_status_marks_disconnect_authentication_and_recovery() {
        use muxtop_proto::ConnectionEvent;
        let mut app = AppState::with_config(
            CliConfig {
                connection_mode: ConnectionMode::Remote {
                    hostname: "requested".into(),
                    addr: "127.0.0.1:1".parse().unwrap(),
                },
                ..CliConfig::default()
            },
            TermCaps::default(),
        );
        app.apply_connection_event(ConnectionEvent::Connected {
            hostname: "actual".into(),
        });
        assert_eq!(app.remote_status, RemoteStatus::Waiting);
        app.apply_transport_snapshot(ui::test_support::snapshot());
        assert_eq!(app.remote_status, RemoteStatus::Connected);
        app.apply_connection_event(ConnectionEvent::Disconnected);
        let rendered = ui::test_support::render_with(&app, 200, 40);
        assert!(ui::test_support::contains(
            &rendered,
            "disconnected / stale"
        ));
        app.apply_transport_snapshot(ui::test_support::snapshot());
        assert_eq!(
            app.remote_status,
            RemoteStatus::Stale,
            "queued old samples cannot revive a lost connection"
        );
        app.apply_connection_event(ConnectionEvent::ServerError {
            code: 401,
            message: "unauthorized".into(),
        });
        app.apply_connection_event(ConnectionEvent::Disconnected);
        assert_eq!(app.remote_status, RemoteStatus::AuthenticationFailed);
        app.apply_connection_event(ConnectionEvent::Connected {
            hostname: "recovered".into(),
        });
        app.paused = true;
        app.apply_transport_snapshot(ui::test_support::snapshot());
        assert_eq!(app.remote_status, RemoteStatus::Waiting);
        app.paused = false;
        app.apply_transport_snapshot(ui::test_support::snapshot());
        assert_eq!(app.remote_status, RemoteStatus::Connected);
        assert!(
            matches!(&app.connection_mode, ConnectionMode::Remote { hostname, .. } if hostname == "recovered")
        );
    }

    #[test]
    fn test_cli_config_default() {
        let config = CliConfig::default();
        assert!(config.filter.is_none());
        assert!(matches!(config.sort_field, SortField::Cpu));
        assert!(!config.tree_mode);
        assert_eq!(config.connection_mode, ConnectionMode::Local);
    }

    #[test]
    fn test_cli_config_with_filter() {
        let config = CliConfig {
            filter: Some("firefox".to_string()),
            ..Default::default()
        };
        assert_eq!(config.filter.as_deref(), Some("firefox"));
    }

    #[test]
    fn test_cli_config_with_sort() {
        let config = CliConfig {
            sort_field: SortField::Mem,
            ..Default::default()
        };
        assert!(matches!(config.sort_field, SortField::Mem));
    }

    #[test]
    fn test_cli_config_with_tree() {
        let config = CliConfig {
            tree_mode: true,
            ..Default::default()
        };
        assert!(config.tree_mode);
    }

    #[test]
    fn test_cli_config_clone() {
        let config = CliConfig {
            filter: Some("test".to_string()),
            sort_field: SortField::Pid,
            tree_mode: true,
            ..Default::default()
        };
        let cloned = config.clone();
        assert_eq!(cloned.filter, config.filter);
        assert!(matches!(cloned.sort_field, SortField::Pid));
        assert!(cloned.tree_mode);
    }

    #[test]
    fn test_cli_config_debug() {
        let config = CliConfig::default();
        assert!(!format!("{config:?}").is_empty());
    }

    #[test]
    fn test_connection_mode_default_is_local() {
        assert_eq!(ConnectionMode::default(), ConnectionMode::Local);
    }

    #[test]
    fn test_connection_mode_remote_variant() {
        let addr: std::net::SocketAddr = "127.0.0.1:4242".parse().unwrap();
        let mode = ConnectionMode::Remote {
            hostname: "prod-01".to_string(),
            addr,
        };
        assert!(matches!(mode, ConnectionMode::Remote { .. }));
        if let ConnectionMode::Remote { hostname, addr } = &mode {
            assert_eq!(hostname, "prod-01");
            assert_eq!(addr.port(), 4242);
        }
    }

    #[test]
    fn test_connection_mode_equality() {
        let addr: std::net::SocketAddr = "10.0.0.1:4242".parse().unwrap();
        let a = ConnectionMode::Remote {
            hostname: "host".to_string(),
            addr,
        };
        let b = ConnectionMode::Remote {
            hostname: "host".to_string(),
            addr,
        };
        assert_eq!(a, b);
        assert_ne!(ConnectionMode::Local, a);
    }

    #[test]
    fn test_cli_config_with_remote_mode() {
        let addr: std::net::SocketAddr = "192.168.1.1:4242".parse().unwrap();
        let config = CliConfig {
            connection_mode: ConnectionMode::Remote {
                hostname: "server".to_string(),
                addr,
            },
            ..Default::default()
        };
        assert!(matches!(
            config.connection_mode,
            ConnectionMode::Remote { .. }
        ));
    }
}
