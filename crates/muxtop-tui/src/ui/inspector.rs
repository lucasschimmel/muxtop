// Inspector (`Enter`).
//
// A monitor needs a second layer. muxtop 0.4 had none: `Enter` was unbound, and
// everything the tables truncated — the full command line, the image digest,
// the pod's node, the exit code — was simply unreachable.
//
// Renders as a side panel when the terminal is wide enough and as a full
// overlay when it is not, because a 40%-wide panel on a 60-column ssh session
// is not a detail view, it is a column of hyphens.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{AppState, KubeSubview, Tab};
use crate::ui::Render;
use crate::ui::sanitize::scrub_ctrl;
use crate::ui::widgets::{meter, overlay};
use muxtop_core::gpu::GpuVendor;

/// Width of the key column inside the inspector.
const KEY_WIDTH: usize = 14;

pub fn draw_inspector(frame: &mut Frame, content_area: Rect, r: &Render<'_>) {
    let Some((title, rows)) = details(r) else {
        return;
    };

    let side_panel =
        r.breakpoint.allows_side_panel() && content_area.width >= 80 && content_area.height >= 6;

    let area = if side_panel {
        // Split the content area rather than the whole screen: the table stays
        // visible, which is the point of a side panel.
        let [_, right] = Layout::horizontal([Constraint::Percentage(58), Constraint::Fill(1)])
            .areas(content_area);
        right
    } else {
        overlay::centered_percent(86, 80, frame.area())
    };

    let inner = overlay::popup(frame, area, &title, r.theme, r.glyphs);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let [body, footer] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(inner);

    let lines: Vec<Line<'static>> = rows
        .into_iter()
        .flat_map(|row| render_row(row, body.width, r))
        .collect();

    let max_scroll = lines.len().saturating_sub(body.height as usize);
    let scroll = r.app.overlay_scroll.min(max_scroll);
    let visible: Vec<Line<'static>> = lines
        .into_iter()
        .skip(scroll)
        .take(body.height as usize)
        .collect();
    frame.render_widget(Paragraph::new(visible), body);

    let hint = Line::from(vec![
        Span::styled(" y ", r.theme.key()),
        Span::styled(" copy id   ", r.theme.key_desc()),
        Span::styled(" Esc ", r.theme.key()),
        Span::styled(" close ", r.theme.key_desc()),
    ]);
    frame.render_widget(Paragraph::new(hint), footer);
}

/// One row of the detail view.
enum Row {
    /// A key/value pair.
    Kv(String, String),
    /// A section heading.
    Section(String),
    /// Free text that may wrap over several lines (command lines, mostly).
    Wrapped(String, String),
    /// A labelled percentage meter.
    Meter(String, f64, String),
}

fn render_row(row: Row, width: u16, r: &Render<'_>) -> Vec<Line<'static>> {
    match row {
        Row::Section(title) => vec![
            Line::from(""),
            Line::from(Span::styled(title, r.theme.accent())),
        ],
        Row::Kv(k, v) => vec![Line::from(vec![
            Span::styled(format!("{k:<KEY_WIDTH$}"), r.theme.dim()),
            Span::styled(v, r.theme.body()),
        ])],
        Row::Meter(k, pct, label) => {
            let bar_w = width.saturating_sub(KEY_WIDTH as u16 + label.len() as u16 + 2);
            let mut spans = vec![Span::styled(format!("{k:<KEY_WIDTH$}"), r.theme.dim())];
            spans.extend(meter::inline(pct, bar_w, r.theme, r.glyphs));
            spans.push(Span::styled(format!(" {label}"), r.theme.body()));
            vec![Line::from(spans)]
        }
        Row::Wrapped(k, v) => {
            // Wrap on the value column so a 300-character command line is
            // readable instead of being cut at the panel edge.
            let avail = (width as usize).saturating_sub(KEY_WIDTH).max(8);
            let chars: Vec<char> = v.chars().collect();
            chars
                .chunks(avail)
                .enumerate()
                .map(|(i, chunk)| {
                    let key = if i == 0 { k.clone() } else { String::new() };
                    Line::from(vec![
                        Span::styled(format!("{key:<KEY_WIDTH$}"), r.theme.dim()),
                        Span::styled(chunk.iter().collect::<String>(), r.theme.body()),
                    ])
                })
                .collect()
        }
    }
}

/// Build the detail rows for whatever is selected on the active tab.
fn details(r: &Render<'_>) -> Option<(String, Vec<Row>)> {
    let app = r.app;
    match app.tab {
        Tab::General | Tab::Processes => process_details(app, r),
        Tab::Network => network_details(app, r),
        Tab::Containers => container_details(app, r),
        Tab::Kube => kube_details(app, r),
        Tab::Gpu => gpu_details(app, r),
    }
}

/// Detail rows for the selected GPU device or process.
///
/// Every optional metric renders the "unavailable" dash rather than a zero:
/// on GPUs those are genuinely different statements, and the inspector is
/// where a user goes to find out which one they are looking at.
fn gpu_details(app: &AppState, r: &Render<'_>) -> Option<(String, Vec<Row>)> {
    use crate::app::GpuSubview;

    let dash = || r.glyphs.none.to_string();

    match app.gpu_subview {
        GpuSubview::Devices => {
            let devices = super::gpu::visible_devices(app);
            let d = devices.get(app.gpu_selected)?;
            let mut rows = vec![
                Row::Kv("Index".into(), d.index.to_string()),
                // Apple Silicon's GPU is on the SoC die and has no PCI bus, so
                // the backend reports no id at all. An empty value renders as
                // a dash like every other unreportable field, instead of as a
                // blank that reads like a rendering bug.
                Row::Kv(
                    "Bus".into(),
                    if d.bus_id.is_empty() {
                        dash()
                    } else {
                        scrub_ctrl(&d.bus_id).into_owned()
                    },
                ),
                Row::Kv(
                    "Driver".into(),
                    d.driver_version
                        .as_deref()
                        .map_or_else(dash, |v| scrub_ctrl(v).into_owned()),
                ),
                Row::Section("LOAD".into()),
            ];
            if let Some(p) = d.utilization_pct {
                rows.push(Row::Meter("GPU".into(), f64::from(p), format!("{p:.0}%")));
            } else {
                rows.push(Row::Kv("GPU".into(), dash()));
            }
            // Apple Silicon has no dedicated video memory: the total here is
            // the machine's whole unified pool, which the CPU is competing
            // for. Labelling it "VRAM" would misdescribe both numbers.
            let memory_label = if d.vendor == GpuVendor::Apple {
                "Unified memory"
            } else {
                "VRAM"
            };
            match (d.mem_used_bytes, d.mem_total_bytes) {
                (Some(used), Some(total)) if total > 0 => rows.push(Row::Meter(
                    memory_label.into(),
                    used as f64 / total as f64 * 100.0,
                    format!(
                        "{} / {}",
                        meter::human_bytes(used),
                        meter::human_bytes(total)
                    ),
                )),
                _ => rows.push(Row::Kv(memory_label.into(), dash())),
            }
            rows.push(Row::Section("PHYSICAL".into()));
            rows.push(Row::Kv(
                "Temperature".into(),
                d.temperature_c.map_or_else(dash, |t| format!("{t:.0} C")),
            ));
            rows.push(Row::Kv(
                "Power".into(),
                match (d.power_watts, d.power_limit_watts) {
                    (Some(w), Some(l)) => {
                        format!("{} W / {} W", super::gpu::watts(w), super::gpu::watts(l))
                    }
                    (Some(w), None) => format!("{} W", super::gpu::watts(w)),
                    _ => dash(),
                },
            ));
            rows.push(Row::Kv(
                "Fan".into(),
                d.fan_pct.map_or_else(dash, |f| format!("{f:.0}%")),
            ));
            rows.push(Row::Kv(
                "Graphics clock".into(),
                d.graphics_clock_mhz
                    .map_or_else(dash, |c| format!("{c} MHz")),
            ));
            rows.push(Row::Kv(
                "Memory clock".into(),
                d.memory_clock_mhz.map_or_else(dash, |c| format!("{c} MHz")),
            ));
            if !d.supports_process_stats {
                rows.push(Row::Section("NOTE".into()));
                rows.push(Row::Wrapped(
                    "".into(),
                    "This backend reports no per-process usage.".into(),
                ));
            }
            Some((r.titled("GPU", &scrub_ctrl(&d.name)), rows))
        }
        GpuSubview::Procs => {
            let procs = super::gpu::visible_procs(app);
            let p = procs.get(app.gpu_selected)?;
            let rows = vec![
                Row::Kv("PID".into(), p.pid.to_string()),
                Row::Kv("Device".into(), p.device_index.to_string()),
                Row::Kv("Type".into(), format!("{:?}", p.kind)),
                Row::Kv(
                    "VRAM".into(),
                    p.mem_bytes.map_or_else(dash, meter::human_bytes),
                ),
            ];
            let name = if p.name.is_empty() {
                dash()
            } else {
                scrub_ctrl(&p.name).into_owned()
            };
            Some((r.titled("GPU process", &name), rows))
        }
    }
}

fn process_details(app: &AppState, r: &Render<'_>) -> Option<(String, Vec<Row>)> {
    let p = app.selected_process()?;
    // Process `comm` and `cmdline` are attacker-controlled by any local user
    // able to spawn a process, and land in a Span verbatim.
    let name = scrub_ctrl(&p.name).into_owned();
    let command = scrub_ctrl(&p.command).into_owned();
    let user = scrub_ctrl(&p.user).into_owned();

    let rows = vec![
        Row::Kv("PID".into(), p.pid.to_string()),
        Row::Kv(
            "Parent".into(),
            p.parent_pid
                .map_or_else(|| r.glyphs.none.to_string(), |v| v.to_string()),
        ),
        Row::Kv("User".into(), user),
        Row::Kv("Status".into(), p.status.clone()),
        Row::Section("RESOURCES".into()),
        Row::Meter(
            "CPU".into(),
            f64::from(p.cpu_percent),
            format!("{:.1}%", p.cpu_percent),
        ),
        Row::Meter(
            "Memory".into(),
            f64::from(p.memory_percent),
            format!(
                "{} ({:.1}%)",
                meter::human_bytes(p.memory_bytes),
                p.memory_percent
            ),
        ),
        Row::Section("COMMAND".into()),
        Row::Wrapped("".into(), command),
    ];
    Some((r.titled("Process", &name), rows))
}

fn network_details(app: &AppState, r: &Render<'_>) -> Option<(String, Vec<Row>)> {
    let interfaces = app.visible_interfaces();
    let i = interfaces.get(app.net_selected)?;
    let rows = vec![
        Row::Kv(
            "State".into(),
            if i.is_up { "up" } else { "down" }.to_string(),
        ),
        Row::Kv("MAC".into(), scrub_ctrl(&i.mac_address).into_owned()),
        Row::Section("TOTALS".into()),
        Row::Kv("Received".into(), meter::human_bytes(i.bytes_rx)),
        Row::Kv("Sent".into(), meter::human_bytes(i.bytes_tx)),
        Row::Kv("Packets rx".into(), i.packets_rx.to_string()),
        Row::Kv("Packets tx".into(), i.packets_tx.to_string()),
        Row::Section("ERRORS".into()),
        Row::Kv("Receive".into(), i.errors_rx.to_string()),
        Row::Kv("Transmit".into(), i.errors_tx.to_string()),
    ];
    Some((r.titled("Interface", &scrub_ctrl(&i.name)), rows))
}

fn container_details(app: &AppState, r: &Render<'_>) -> Option<(String, Vec<Row>)> {
    let c = app
        .sorted_filtered_containers()
        .get(app.containers_selected)?;
    let mem_pct = if c.mem_limit_bytes > 0 {
        c.mem_used_bytes as f64 / c.mem_limit_bytes as f64 * 100.0
    } else {
        0.0
    };
    let mem_label = if c.mem_limit_bytes > 0 {
        format!(
            "{} / {}",
            meter::human_bytes(c.mem_used_bytes),
            meter::human_bytes(c.mem_limit_bytes)
        )
    } else {
        format!("{} (no limit)", meter::human_bytes(c.mem_used_bytes))
    };

    let rows = vec![
        Row::Kv("ID".into(), c.id.clone()),
        Row::Kv("State".into(), scrub_ctrl(&c.status_text).into_owned()),
        Row::Wrapped("Image".into(), scrub_ctrl(&c.image).into_owned()),
        Row::Section("RESOURCES".into()),
        Row::Meter(
            "CPU".into(),
            f64::from(c.cpu_pct),
            format!("{:.1}%", c.cpu_pct),
        ),
        Row::Meter("Memory".into(), mem_pct, mem_label),
        Row::Section("IO".into()),
        Row::Kv("Net rx".into(), meter::human_bytes(c.net_rx_bytes)),
        Row::Kv("Net tx".into(), meter::human_bytes(c.net_tx_bytes)),
        Row::Kv("Block read".into(), meter::human_bytes(c.block_read_bytes)),
        Row::Kv(
            "Block write".into(),
            meter::human_bytes(c.block_write_bytes),
        ),
    ];
    Some((r.titled("Container", &scrub_ctrl(&c.name)), rows))
}

fn kube_details(app: &AppState, r: &Render<'_>) -> Option<(String, Vec<Row>)> {
    let snap = app.last_snapshot.as_ref()?.kube.as_ref()?;
    let &idx = app.kube_view_cache.get(app.kube_selected)?;

    match app.kube_subview {
        KubeSubview::Pods => {
            let p = snap.pods.get(idx)?;
            let rows = vec![
                Row::Kv("Namespace".into(), scrub_ctrl(&p.namespace).into_owned()),
                Row::Kv("Phase".into(), format!("{:?}", p.phase)),
                Row::Kv("Ready".into(), format!("{}/{}", p.ready.0, p.ready.1)),
                Row::Kv("Restarts".into(), p.restarts.to_string()),
                Row::Kv("Node".into(), scrub_ctrl(&p.node).into_owned()),
                Row::Kv("QoS".into(), format!("{:?}", p.qos)),
                Row::Kv("Age".into(), format_age(p.age_seconds)),
                Row::Section("METRICS".into()),
                Row::Kv(
                    "CPU".into(),
                    p.cpu_millis.map_or_else(
                        || format!("{} (no metrics-server)", r.glyphs.none),
                        |m| format!("{m}m"),
                    ),
                ),
                Row::Kv(
                    "Memory".into(),
                    p.mem_bytes.map_or_else(
                        || format!("{} (no metrics-server)", r.glyphs.none),
                        meter::human_bytes,
                    ),
                ),
            ];
            Some((r.titled("Pod", &scrub_ctrl(&p.name)), rows))
        }
        KubeSubview::Nodes => {
            let n = snap.nodes.get(idx)?;
            let rows = vec![
                Row::Kv("Status".into(), format!("{:?}", n.status)),
                Row::Kv("Roles".into(), n.roles.join(", ")),
                Row::Kv(
                    "Kubelet".into(),
                    scrub_ctrl(&n.kubelet_version).into_owned(),
                ),
                Row::Kv("Age".into(), format_age(n.age_seconds)),
                Row::Section("CAPACITY".into()),
                Row::Kv(
                    "Pods".into(),
                    format!("{} / {}", n.pod_count, n.pod_capacity),
                ),
                Row::Kv(
                    "CPU".into(),
                    format!(
                        "{}m allocatable of {}m",
                        n.cpu_allocatable_millis, n.cpu_capacity_millis
                    ),
                ),
                Row::Kv(
                    "Memory".into(),
                    format!(
                        "{} allocatable of {}",
                        meter::human_bytes(n.mem_allocatable_bytes),
                        meter::human_bytes(n.mem_capacity_bytes)
                    ),
                ),
            ];
            Some((r.titled("Node", &scrub_ctrl(&n.name)), rows))
        }
        KubeSubview::Deployments => {
            let d = snap.deployments.get(idx)?;
            let rows = vec![
                Row::Kv("Namespace".into(), scrub_ctrl(&d.namespace).into_owned()),
                Row::Kv(
                    "Ready".into(),
                    format!("{}/{}", d.replicas_ready, d.replicas_desired),
                ),
                Row::Kv("Up to date".into(), d.replicas_uptodate.to_string()),
                Row::Kv("Available".into(), d.replicas_available.to_string()),
                Row::Kv("Strategy".into(), format!("{:?}", d.strategy)),
                Row::Kv("Age".into(), format_age(d.age_seconds)),
            ];
            Some((r.titled("Deployment", &scrub_ctrl(&d.name)), rows))
        }
    }
}

/// Kubernetes-style compact age: `3d`, `4h`, `12m`, `45s`.
pub fn format_age(secs: u64) -> String {
    match secs {
        s if s >= 86_400 => format!("{}d", s / 86_400),
        s if s >= 3600 => format!("{}h", s / 3600),
        s if s >= 60 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Overlay;

    use crate::ui::test_support::*;

    fn inspect(tab: Tab, w: u16, h: u16) -> String {
        let mut app = app_with_data();
        app.tab = tab;
        app.overlay = Overlay::Inspector;
        all_text(&render_with(&app, w, h))
    }

    #[test]
    fn process_inspector_shows_the_full_command() {
        let text = inspect(Tab::Processes, 120, 30);
        assert!(text.contains("PID"));
        assert!(
            text.contains("--serve"),
            "the inspector exists to show what the table truncates:\n{text}"
        );
    }

    #[test]
    fn network_inspector_shows_totals_and_errors() {
        let text = inspect(Tab::Network, 120, 30);
        assert!(text.contains("eth0"));
        assert!(text.contains("ERRORS"));
        assert!(text.contains("MAC"));
    }

    #[test]
    fn inspector_is_a_side_panel_on_wide_terminals() {
        // The table must stay visible beside it.
        let mut app = app_with_data();
        app.tab = Tab::Processes;
        app.overlay = Overlay::Inspector;
        let buf = render_with(&app, 160, 30);
        let text = all_text(&buf);
        assert!(text.contains("PID"), "inspector missing");
        assert!(
            text.contains("proc0"),
            "the table should still be visible next to the panel"
        );
    }

    #[test]
    fn inspector_takes_over_on_narrow_terminals() {
        let text = inspect(Tab::Processes, 62, 24);
        assert!(text.contains("Process"));
    }

    #[test]
    fn inspector_draws_nothing_when_there_is_no_selection() {
        let mut app = crate::app::AppState::new();
        app.overlay = Overlay::Inspector;
        // No snapshot: nothing is selected, and nothing must be drawn or panic.
        let text = all_text(&render_with(&app, 100, 30));
        assert!(!text.contains("Process —"));
    }

    #[test]
    fn inspector_scrubs_control_characters() {
        use muxtop_core::process::ProcessInfo;
        let mut snap = snapshot();
        snap.processes[0] = ProcessInfo {
            pid: 6,
            parent_pid: None,
            name: "evil\x1b[31m".to_string(),
            command: "/bin/evil\x07 --flag".to_string(),
            user: "root".to_string(),
            cpu_percent: 99.0,
            memory_bytes: 1,
            memory_percent: 0.1,
            status: "Running".to_string(),
        };
        let mut app = crate::app::AppState::new();
        app.apply_snapshot(snap);
        app.tab = Tab::Processes;
        app.overlay = Overlay::Inspector;
        let text = all_text(&render_with(&app, 120, 30));
        assert!(!text.contains('\x1b'), "escape sequence reached the screen");
        assert!(!text.contains('\x07'));
    }

    #[test]
    fn inspector_survives_every_size_and_profile() {
        for &tab in Tab::ALL {
            let mut app = app_with_data();
            app.tab = tab;
            app.overlay = Overlay::Inspector;
            for (w, h) in [(1u16, 1u16), (20, 6), (62, 24), (100, 30), (200, 60)] {
                let _ = render_with(&app, w, h);
            }
            for (color, unicode) in all_profiles() {
                let _ = render_caps(&mut app, 100, 30, color, unicode);
            }
        }
    }

    #[test]
    fn age_formats_compactly() {
        assert_eq!(format_age(5), "5s");
        assert_eq!(format_age(90), "1m");
        assert_eq!(format_age(7200), "2h");
        assert_eq!(format_age(200_000), "2d");
    }
    // @generated by:gauntlet run:muxtop-audit-fixes
    #[test]
    fn audit_network_inspector_matches_sorted_filtered_selection() {
        use crate::app::{Command, NetworkSortField};
        use muxtop_core::process::SortOrder;
        let mut app = app_with_data();
        app.tab = Tab::Network;
        app.net_sort_field = NetworkSortField::Name;
        app.net_sort_order = SortOrder::Desc;
        app.overlay = Overlay::Inspector;
        let text = all_text(&render_with(&app, 62, 24));
        assert!(text.contains("lo"));
        assert!(!text.contains("eth0"));
        assert_eq!(app.selected_identifier().as_deref(), Some("lo"));
        app.execute_invocation(Command::ToggleSortOrder, None);
        assert_eq!(app.selected_identifier().as_deref(), Some("eth0"));
        app.set_filter("lo");
        let text = all_text(&render_with(&app, 62, 24));
        assert!(text.contains("lo"));
        assert_eq!(app.selected_identifier().as_deref(), Some("lo"));
    }
}
