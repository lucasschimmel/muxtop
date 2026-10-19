// Containers tab (Docker / Podman).

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::Render;
use super::filter_bar;
use super::sanitize::scrub_ctrl;
use super::widgets::columns::{Align, Column, PRIO_ESSENTIAL, PRIO_HIGH, PRIO_LOW, PRIO_MEDIUM};
use super::widgets::empty::{self, EmptyState};
use super::widgets::table::{self, Cell, Row, Spec};
use super::widgets::{badge, meter, spark};
use crate::app::ContainerSortField;
use crate::ui::theme::Level;
use muxtop_core::containers::{ContainerSnapshot, ContainerState, ContainersSnapshot, EngineKind};
use muxtop_core::process::SortOrder;

const COL_NAME: usize = 0;
const COL_CPU: usize = 3;
const COL_MEM: usize = 4;
const COL_RX: usize = 5;
const COL_TX: usize = 6;
const COL_UPTIME: usize = 7;

const COLUMNS: &[Column] = &[
    Column::fixed("NAME", 22, Align::Left, PRIO_ESSENTIAL),
    Column::flex("IMAGE", 16, PRIO_MEDIUM),
    Column::fixed("STATE", 13, Align::Left, PRIO_ESSENTIAL),
    Column::fixed("CPU%", 7, Align::Right, PRIO_HIGH),
    Column::fixed("MEMORY", 18, Align::Right, PRIO_HIGH),
    Column::fixed("RX/s", 10, Align::Right, PRIO_LOW),
    Column::fixed("TX/s", 10, Align::Right, PRIO_LOW),
    Column::fixed("UPTIME", 9, Align::Right, PRIO_LOW),
];

const GRAPH_HEIGHT: u16 = 4;

pub fn draw_containers_tab(frame: &mut Frame, area: Rect, r: &Render<'_>) {
    let app = r.app;
    let Some(snapshot) = app.last_snapshot.as_ref() else {
        let waiting = r.ellipsis("Waiting for data");
        empty::render(frame, area, &EmptyState::waiting(&waiting), r.theme);
        return;
    };

    // Three distinct states, three distinct explanations. This is the part of
    // 0.4 that was already right, so it is kept and generalised.
    let containers = match snapshot.containers.as_ref() {
        None => {
            empty::render(
                frame,
                area,
                &EmptyState::empty(
                    "No container engine configured",
                    Some("Start Docker or Podman and relaunch, or pass --docker-socket."),
                ),
                r.theme,
            );
            return;
        }
        Some(cs) if !cs.daemon_up => {
            empty::render(
                frame,
                area,
                &EmptyState::error(
                    "No container daemon detected",
                    "The socket is not answering.",
                    "Run `docker` or `podman system service`, and check your user is in the `docker` group.",
                ),
                r.theme,
            );
            return;
        }
        Some(cs) => cs,
    };

    let rows = app.sorted_filtered_containers();
    let filter_h = u16::from(app.filter_editing());
    let graph_h = if !rows.is_empty() && area.height >= 14 {
        GRAPH_HEIGHT
    } else {
        0
    };

    let [summary_area, table_area, graph_area, filter_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(graph_h),
        Constraint::Length(filter_h),
    ])
    .areas(area);

    draw_summary(frame, summary_area, r, containers);

    let now_ms = snapshot.timestamp_ms;
    let filtered = !app.containers_filter_input.is_empty();
    let spec = Spec {
        columns: COLUMNS,
        sort_col: sort_column(app.containers_sort_field),
        descending: matches!(app.containers_sort_order, SortOrder::Desc),
        total: rows.len(),
        selected: app.containers_selected,
        scroll: app.containers_scroll_offset,
        col_scroll: app.col_scroll,
        empty: if filtered {
            EmptyState::no_match("No matching containers")
        } else {
            EmptyState::empty(
                "No containers",
                Some("The daemon is up but has nothing running."),
            )
        },
    };

    table::draw(frame, table_area, r, &spec, |idx| match rows.get(idx) {
        Some(c) => container_row(c, now_ms, r),
        None => Row::new(Vec::new()),
    });

    if graph_h > 0 {
        draw_graph(frame, graph_area, r, rows);
    }
    if filter_h > 0 {
        filter_bar::draw(frame, filter_area, r, "Filter containers");
    }
}

fn sort_column(field: ContainerSortField) -> Option<usize> {
    Some(match field {
        ContainerSortField::Name => COL_NAME,
        ContainerSortField::Cpu => COL_CPU,
        ContainerSortField::Mem => COL_MEM,
        ContainerSortField::NetRx => COL_RX,
        ContainerSortField::NetTx => COL_TX,
        ContainerSortField::Uptime => COL_UPTIME,
    })
}

/// Severity of a container state — what turns a plain string into a badge.
fn state_level(state: ContainerState) -> Level {
    match state {
        ContainerState::Running => Level::Success,
        ContainerState::Paused | ContainerState::Created => Level::Info,
        ContainerState::Restarting | ContainerState::Removing => Level::Warning,
        ContainerState::Exited | ContainerState::Dead => Level::Error,
    }
}

fn state_label(state: ContainerState) -> &'static str {
    match state {
        ContainerState::Created => "created",
        ContainerState::Running => "running",
        ContainerState::Paused => "paused",
        ContainerState::Restarting => "restarting",
        ContainerState::Exited => "exited",
        ContainerState::Dead => "dead",
        ContainerState::Removing => "removing",
    }
}

fn container_row(c: &ContainerSnapshot, now_ms: u64, r: &Render<'_>) -> Row {
    let level = state_level(c.state);
    let running = c.state == ContainerState::Running;
    let theme = r.theme;

    // Memory as used/limit: a bare figure says nothing about how close to the
    // cgroup ceiling a container is.
    let memory = if c.mem_limit_bytes > 0 {
        format!(
            "{}/{}",
            meter::human_bytes(c.mem_used_bytes),
            meter::human_bytes(c.mem_limit_bytes)
        )
    } else {
        meter::human_bytes(c.mem_used_bytes)
    };

    let dash = r.glyphs.none.to_string();
    Row::new(vec![
        Cell::new(scrub_ctrl(&c.name).into_owned()),
        Cell::new(scrub_ctrl(&c.image).into_owned()),
        Cell::colored(
            format!(
                "{} {}",
                badge::marker(level, r.glyphs),
                state_label(c.state)
            ),
            theme.level_color(level),
        ),
        if running {
            Cell::colored(
                format!("{:.1}", c.cpu_pct),
                theme.gauge_color(f64::from(c.cpu_pct)),
            )
        } else {
            Cell::new(dash.clone())
        },
        if running {
            Cell::new(memory)
        } else {
            Cell::new(dash.clone())
        },
        if running {
            Cell::new(meter::human_bytes(c.net_rx_bytes))
        } else {
            Cell::new(dash.clone())
        },
        if running {
            Cell::new(meter::human_bytes(c.net_tx_bytes))
        } else {
            Cell::new(dash.clone())
        },
        if running {
            Cell::new(uptime(c.started_at_ms, now_ms).unwrap_or_else(|| dash.clone()))
        } else {
            Cell::new(dash)
        },
    ])
}

/// Compact uptime from a start timestamp.
fn uptime(started_at_ms: u64, now_ms: u64) -> Option<String> {
    if started_at_ms == 0 {
        return None;
    }
    Some(super::inspector::format_age(
        now_ms.saturating_sub(started_at_ms) / 1000,
    ))
}

fn draw_summary(frame: &mut Frame, area: Rect, r: &Render<'_>, cs: &ContainersSnapshot) {
    let total = cs.containers.len();
    let running = cs
        .containers
        .iter()
        .filter(|c| c.state == ContainerState::Running)
        .count();
    let engine = match cs.engine {
        EngineKind::Docker => "Docker",
        EngineKind::Podman => "Podman",
        EngineKind::Unknown => "Engine",
    };

    let line = Line::from(vec![
        Span::styled(format!(" {engine} "), r.theme.accent_fill()),
        Span::styled(format!("  {running} running"), r.theme.body()),
        Span::styled(format!(" {} {} total", r.glyphs.sep, total), r.theme.dim()),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

/// CPU and receive history for the selected container.
fn draw_graph(frame: &mut Frame, area: Rect, r: &Render<'_>, rows: &[ContainerSnapshot]) {
    let Some(c) = rows.get(r.app.containers_selected) else {
        return;
    };
    let width = area.width.saturating_sub(10);
    if width == 0 {
        return;
    }

    let cpu = r.app.container_cpu_history(&c.id);
    let rx = r.app.container_rx_deltas(&c.id);

    let mut lines = vec![Line::from(Span::styled(
        format!(" {} ", scrub_ctrl(&c.name)),
        r.theme.accent(),
    ))];

    let mut cpu_line = vec![Span::styled(" CPU ", r.theme.dim())];
    cpu_line.extend(spark::line_percent(&cpu, width, r.theme, r.glyphs).spans);
    lines.push(Line::from(cpu_line));

    let mut rx_line = vec![Span::styled(
        format!(" {}   ", r.glyphs.arrow_down),
        r.theme.dim(),
    )];
    rx_line.extend(
        spark::line(
            &rx,
            width,
            None,
            spark::Tint::Flat,
            r.theme.success,
            r.theme,
            r.glyphs,
        )
        .spans,
    );
    lines.push(Line::from(rx_line));

    frame.render_widget(Paragraph::new(lines), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{AppState, Tab};
    use crate::ui::test_support::*;

    fn snapshot_with_containers(daemon_up: bool, states: &[ContainerState]) -> AppState {
        let containers = states
            .iter()
            .enumerate()
            .map(|(i, &state)| ContainerSnapshot {
                id: format!("abc{i:09}"),
                id_full: format!("abc{i:061}"),
                name: format!("service-{i}"),
                image: format!("ghcr.io/acme/service:{i}.0"),
                state,
                status_text: format!("{state:?}"),
                cpu_pct: 5.0 * (i as f32 + 1.0),
                mem_used_bytes: 128 * 1024 * 1024,
                mem_limit_bytes: 512 * 1024 * 1024,
                net_rx_bytes: 1_200_000,
                net_tx_bytes: 340_000,
                block_read_bytes: 0,
                block_write_bytes: 0,
                started_at_ms: 1_699_999_000_000,
            })
            .collect();

        let mut snap = snapshot();
        snap.containers = Some(std::sync::Arc::new(ContainersSnapshot {
            engine: EngineKind::Docker,
            daemon_up,
            containers,
        }));
        let mut app = AppState::new();
        app.tab = Tab::Containers;
        app.apply_snapshot(snap);
        app
    }

    #[test]
    fn table_lists_containers() {
        let app = snapshot_with_containers(true, &[ContainerState::Running]);
        let text = all_text(&render_with(&app, 140, 30));
        assert!(text.contains("NAME"));
        assert!(text.contains("service-0"));
        assert!(text.contains("running"));
    }

    #[test]
    fn memory_is_shown_against_its_limit() {
        let app = snapshot_with_containers(true, &[ContainerState::Running]);
        let text = all_text(&render_with(&app, 160, 30));
        assert!(
            text.contains("128M/512M"),
            "memory must be shown against the cgroup limit:\n{text}"
        );
    }

    #[test]
    fn stopped_containers_show_no_fake_metrics() {
        // 0.4 printed 0.0% CPU for an exited container, which reads as "idle"
        // rather than "not running".
        let app = snapshot_with_containers(true, &[ContainerState::Exited]);
        let text = all_text(&render_with(&app, 160, 30));
        assert!(text.contains("exited"));
        assert!(
            text.contains('—'),
            "unmeasurable values must render as a dash:\n{text}"
        );
    }

    #[test]
    fn engine_and_counts_are_summarised() {
        let app =
            snapshot_with_containers(true, &[ContainerState::Running, ContainerState::Exited]);
        let text = all_text(&render_with(&app, 140, 30));
        assert!(text.contains("Docker"));
        assert!(text.contains("1 running"));
        assert!(text.contains("2 total"));
    }

    #[test]
    fn no_daemon_state_says_what_to_do() {
        let app = snapshot_with_containers(false, &[]);
        let text = all_text(&render_with(&app, 100, 24));
        assert!(text.contains("No container daemon detected"));
        assert!(text.contains("podman system service"));
    }

    #[test]
    fn no_engine_state_is_distinct_from_a_dead_daemon() {
        let mut app = AppState::new();
        app.tab = Tab::Containers;
        app.apply_snapshot(snapshot()); // containers: None
        let text = all_text(&render_with(&app, 100, 24));
        assert!(text.contains("No container engine configured"));
    }

    #[test]
    fn renders_under_every_profile_and_size() {
        let mut app =
            snapshot_with_containers(true, &[ContainerState::Running, ContainerState::Dead]);
        for (w, h) in [(1u16, 1u16), (40, 8), (80, 24), (200, 50)] {
            let _ = render_with(&app, w, h);
        }
        for (color, unicode) in all_profiles() {
            let _ = render_caps(&mut app, 120, 30, color, unicode);
        }
    }

    #[test]
    fn uptime_is_compact() {
        assert_eq!(uptime(0, 1000), None);
        assert_eq!(uptime(2000, 1000).as_deref(), Some("0s"));
        assert_eq!(uptime(0, 0), None);
        assert_eq!(
            uptime(1_000_000, 1_000_000 + 7200 * 1000).as_deref(),
            Some("2h")
        );
    }
}
