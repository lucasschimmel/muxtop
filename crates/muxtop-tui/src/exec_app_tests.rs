// Acceptance trace: T-AC-01, T-AC-02 (test names use Rust-compatible t_ac_ identifiers).
// @generated run:muxtop-exec-pty — spec-first behavior tests.
use async_trait::async_trait;
use muxtop_core::container_engine::{ContainerEngine, EngineError};
use muxtop_core::containers::{ContainerSnapshot, ContainerState, ContainersSnapshot, EngineKind};
use std::sync::Arc;

struct Engine;
#[async_trait]
impl ContainerEngine for Engine {
    async fn list_and_stats(&self) -> Result<Vec<ContainerSnapshot>, EngineError> {
        unreachable!("UI must only queue exec")
    }
    async fn stop(&self, _: &str, _: Option<u64>) -> Result<(), EngineError> {
        unreachable!()
    }
    async fn kill(&self, _: &str) -> Result<(), EngineError> {
        unreachable!()
    }
    async fn restart(&self, _: &str) -> Result<(), EngineError> {
        unreachable!()
    }
    fn kind(&self) -> EngineKind {
        EngineKind::Docker
    }
}
fn container(name: &str, cpu_pct: f32, state: ContainerState) -> ContainerSnapshot {
    ContainerSnapshot {
        id: name.into(),
        id_full: format!("{name}-{}", "f".repeat(64)),
        name: name.into(),
        image: "test:latest".into(),
        state,
        status_text: String::new(),
        cpu_pct,
        mem_used_bytes: 0,
        mem_limit_bytes: 0,
        net_rx_bytes: 0,
        net_tx_bytes: 0,
        block_read_bytes: 0,
        block_write_bytes: 0,
        started_at_ms: 0,
    }
}
fn app() -> crate::app::AppState {
    use crate::app::{AppState, ContainerSortField, Tab};
    use muxtop_core::network::NetworkSnapshot;
    use muxtop_core::system::{CpuSnapshot, LoadSnapshot, MemorySnapshot, SystemSnapshot};
    let mut app = AppState::new();
    app.tab = Tab::Containers;
    app.set_container_engine(Arc::new(Engine));
    app.containers_sort_field = ContainerSortField::Cpu;
    app.containers_sort_order = muxtop_core::process::SortOrder::Desc;
    app.apply_snapshot(SystemSnapshot {
        cpu: CpuSnapshot {
            global_usage: 0.0,
            cores: vec![],
        },
        memory: MemorySnapshot {
            total: 0,
            used: 0,
            available: 0,
            swap_total: 0,
            swap_used: 0,
        },
        load: LoadSnapshot {
            one: 0.0,
            five: 0.0,
            fifteen: 0.0,
            uptime_secs: 0,
        },
        processes: vec![],
        networks: NetworkSnapshot {
            interfaces: vec![],
            total_rx: 0,
            total_tx: 0,
        },
        containers: Some(Arc::new(ContainersSnapshot {
            engine: EngineKind::Docker,
            daemon_up: true,
            containers: vec![
                container("hidden", 99.0, ContainerState::Running),
                container("pick-low", 1.0, ContainerState::Running),
                container("pick-high", 20.0, ContainerState::Running),
                container("stopped", 0.0, ContainerState::Exited),
            ],
        })),
        kube: None,
        gpu: None,
        timestamp_ms: 0,
    });
    app.set_filter("pick");
    app
}
fn key(app: &mut crate::app::AppState, code: crossterm::event::KeyCode) {
    app.handle_key_event(crossterm::event::KeyEvent::new(
        code,
        crossterm::event::KeyModifiers::NONE,
    ));
}
fn invoke(app: &mut crate::app::AppState, entry: usize) {
    use crossterm::event::KeyCode;
    match entry {
        0 => key(app, KeyCode::Char('e')),
        1 => {
            let index = app
                .tab_actions()
                .iter()
                .position(|(_, _, action)| *action == crate::keymap::Action::ContainerExec)
                .unwrap();
            key(app, KeyCode::Char('x'));
            app.actions_selected = index;
            key(app, KeyCode::Enter);
        }
        _ => {
            app.handle_key_event(crossterm::event::KeyEvent::new(
                KeyCode::Char('p'),
                crossterm::event::KeyModifiers::CONTROL,
            ));
            for character in "exec".chars() {
                key(app, KeyCode::Char(character));
            }
            key(app, KeyCode::Enter);
        }
    }
}

#[test]
#[cfg(unix)]
fn t_ac_01_key_menu_and_palette_queue_the_selected_sorted_filtered_full_id() {
    for (row, name) in [(0, "pick-high"), (1, "pick-low")] {
        for entry in 0..3 {
            let mut app = app();
            app.containers_selected = row;
            let expected = format!("{name}-{}", "f".repeat(64));
            invoke(&mut app, entry);
            assert_eq!(
                app.pending_exec.as_deref(),
                Some(expected.as_str()),
                "row {row}, entry {entry}"
            );
        }
    }
}

#[test]
fn t_ac_01_stopped_missing_engine_and_empty_selection_report_without_exec() {
    for guard in 0..3 {
        for entry in 0..3 {
            let mut app = app();
            match guard {
                0 => app.set_filter("stopped"),
                1 => app.container_engine = None,
                _ => app.set_filter("no matching container"),
            }
            invoke(&mut app, entry);
            assert!(app.pending_exec.is_none(), "guard {guard}, entry {entry}");
            assert!(
                app.active_status().is_some(),
                "guard {guard}, entry {entry}"
            );
        }
    }
}

#[test]
fn t_ac_02_remote_blocks_direct_key_menu_and_palette_exec() {
    let mut app = app();
    app.connection_mode = crate::ConnectionMode::Remote {
        hostname: "fixture".into(),
        addr: "127.0.0.1:12345".parse().unwrap(),
    };
    key(&mut app, crossterm::event::KeyCode::Char('e'));
    assert!(app.pending_exec.is_none());
    assert!(app.active_status().is_some());
    assert!(
        !app.tab_actions()
            .iter()
            .any(|(_, _, action)| *action == crate::keymap::Action::ContainerExec)
    );
    app.execute_invocation(crate::app::Command::ExecContainer, None);
    assert!(app.pending_exec.is_none());
    app.handle_key_event(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('p'),
        crossterm::event::KeyModifiers::CONTROL,
    ));
    assert!(
        !app.palette
            .filtered
            .iter()
            .any(|(command, _)| *command == crate::app::Command::ExecContainer)
    );
}
