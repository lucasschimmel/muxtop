// Application chrome: the header line, the tab bar and the status bar.
//
// 0.4 spent three rows at the top of the screen — 12.5% of an 80×24 terminal —
// showing an application name, a version number and five tab labels. The same
// two rows now carry the host, the connection, live vitals, per-tab counts and
// the clock, and the bottom row shows the state the user previously had to
// remember: sort column, active filter and match count, cursor position.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{AppState, Tab};
use crate::keymap;
use crate::ui::Render;
use crate::ui::sanitize::scrub_ctrl;
use crate::ui::widgets::{meter, scrollbar};

/// Width below which the header and tab bar are merged into a single row.
const COMPACT_WIDTH: u16 = 60;

/// Rows of chrome above the content area for a given width.
pub fn header_rows(width: u16) -> u16 {
    if width < COMPACT_WIDTH { 1 } else { 2 }
}

// ---------------------------------------------------------------------------
// Header
// ---------------------------------------------------------------------------

/// The top line: identity, connection, host, uptime, global vitals, clock.
pub fn draw_header(frame: &mut Frame, area: Rect, r: &Render<'_>) {
    let app = r.app;
    let theme = r.theme;
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(12);

    spans.push(Span::styled(" muxtop ", theme.accent_fill()));
    spans.push(Span::styled(
        format!(" v{} ", env!("CARGO_PKG_VERSION")),
        theme.dim().bg(theme.header_bg),
    ));

    spans.extend(connection_spans(r));

    if let Some(snapshot) = app.last_snapshot.as_ref() {
        spans.push(Span::styled(
            format!("  up {} ", format_uptime(snapshot.load.uptime_secs)),
            theme.dim().bg(theme.header_bg),
        ));

        // Global vitals, so the numbers that matter are on screen whatever tab
        // is open. Only drawn when there is room for them to be readable.
        if area.width >= 90 {
            let cpu = f64::from(snapshot.cpu.global_usage);
            let mem = percent(snapshot.memory.used, snapshot.memory.total);
            spans.push(Span::styled(" CPU ", theme.dim().bg(theme.header_bg)));
            spans.extend(meter::inline(cpu, 8, theme, r.glyphs));
            spans.push(Span::styled(
                format!(" {cpu:>3.0}% "),
                theme.dim().bg(theme.header_bg),
            ));
            spans.push(Span::styled(" MEM ", theme.dim().bg(theme.header_bg)));
            spans.extend(meter::inline(mem, 8, theme, r.glyphs));
            spans.push(Span::styled(
                format!(" {mem:>3.0}% "),
                theme.dim().bg(theme.header_bg),
            ));
        }
    }

    if app.paused {
        spans.push(Span::styled(
            " PAUSED ",
            theme.level_fill(crate::ui::theme::Level::Warning),
        ));
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(theme.chrome()),
        area,
    );
}

/// Connection indicator: local, remote, and where.
fn connection_spans(r: &Render<'_>) -> Vec<Span<'static>> {
    let theme = r.theme;
    let g = r.glyphs;
    match &r.app.connection_mode {
        crate::ConnectionMode::Local => {
            let host = muxtop_core::system::host_name().unwrap_or("local");
            vec![
                Span::styled(
                    format!("  {} ", g.conn_local),
                    theme
                        .level_style(crate::ui::theme::Level::Success)
                        .bg(theme.header_bg),
                ),
                Span::styled(host.to_string(), theme.chrome()),
            ]
        }
        crate::ConnectionMode::Remote { hostname, addr } => vec![
            Span::styled(
                format!("  {} ", g.conn_remote),
                ratatui::style::Style::default()
                    .fg(theme.accent_secondary)
                    .bg(theme.header_bg),
            ),
            // The hostname arrives in the server's `Welcome` frame. A hostile
            // or compromised server — or merely a host whose $HOSTNAME a local
            // user controls — would otherwise own a line of chrome that stays
            // on screen for the whole session. Same guard the table cells got
            // in v0.3.1 (MED-S5).
            Span::styled(
                format!("{}:{} ", scrub_ctrl(hostname), addr.port()),
                ratatui::style::Style::default()
                    .fg(theme.accent_secondary)
                    .bg(theme.header_bg),
            ),
            Span::styled("read-only ", theme.subtle().bg(theme.header_bg)),
            Span::styled(
                format!("{} ", r.app.remote_status.label()),
                theme
                    .level_style(if r.app.remote_status == crate::RemoteStatus::Connected {
                        crate::Level::Success
                    } else {
                        crate::Level::Warning
                    })
                    .bg(theme.header_bg),
            ),
        ],
    }
}

// ---------------------------------------------------------------------------
// Tab bar
// ---------------------------------------------------------------------------

/// The tab row: `¹General  ²Processes 342  ³Network 4 …`.
///
/// Counts are live, and a tab whose data source is unavailable renders dim
/// rather than pretending to be ready. The 0.4 hardcoded `GPU [soon]`
/// placeholder is gone: a tab either exists or it does not.
pub fn draw_tabbar(frame: &mut Frame, area: Rect, r: &Render<'_>) {
    let app = r.app;
    let theme = r.theme;
    let compact = area.width < 100;

    let mut spans: Vec<Span<'static>> = Vec::with_capacity(Tab::ALL.len() * 3);
    for (i, &tab) in Tab::ALL.iter().enumerate() {
        let active = tab == app.tab;
        let available = tab_available(app, tab);

        let style = match (active, available) {
            (true, _) => theme.accent().bg(theme.header_bg),
            (false, true) => theme.dim().bg(theme.header_bg),
            (false, false) => theme.subtle().bg(theme.header_bg),
        };

        spans.push(Span::styled(
            if active {
                format!(" {}", tab_number(i + 1, r.glyphs))
            } else {
                format!("  {}", tab_number(i + 1, r.glyphs))
            },
            theme.subtle().bg(theme.header_bg),
        ));
        spans.push(Span::styled(tab.label().to_string(), style));

        // The count is the point of the tab bar: it tells you whether a tab is
        // worth switching to before you switch to it.
        if !compact && let Some(count) = tab_count(app, tab) {
            spans.push(Span::styled(
                format!(" {count}"),
                if active {
                    theme.strong().bg(theme.header_bg)
                } else {
                    theme.subtle().bg(theme.header_bg)
                },
            ));
        }
        spans.push(Span::styled(" ", theme.chrome()));
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(theme.chrome()),
        area,
    );
}

/// Whether a tab has a working data source behind it.
fn tab_available(app: &AppState, tab: Tab) -> bool {
    match tab {
        Tab::Containers => app
            .last_snapshot
            .as_ref()
            .and_then(|s| s.containers.as_ref())
            .is_some_and(|c| c.daemon_up),
        Tab::Kube => app
            .last_snapshot
            .as_ref()
            .and_then(|s| s.kube.as_ref())
            .is_some_and(|k| k.reachable),
        Tab::Gpu => app
            .last_snapshot
            .as_ref()
            .and_then(|s| s.gpu.as_ref())
            .is_some_and(|g| g.available),
        _ => true,
    }
}

fn tab_count(app: &AppState, tab: Tab) -> Option<usize> {
    // No snapshot yet: a count of zero would read as "nothing to see here"
    // rather than "not measured yet".
    app.last_snapshot.as_ref()?;
    match tab {
        Tab::General => None,
        Tab::Processes => Some(app.process_count()),
        Tab::Network => Some(app.net_interface_count()),
        Tab::Containers => tab_available(app, tab).then(|| app.containers_count()),
        Tab::Kube => tab_available(app, tab).then(|| app.kube_count()),
        Tab::Gpu => tab_available(app, tab).then(|| app.gpu_count()),
    }
}

/// The Alt+N hint next to a tab label.
///
/// Superscript digits are outside the glyph range of the Linux console
/// font, so ASCII terminals get a plain digit instead.
fn tab_number(n: usize, glyphs: &crate::ui::glyphs::Glyphs) -> String {
    const SUP: [&str; 9] = ["¹", "²", "³", "⁴", "⁵", "⁶", "⁷", "⁸", "⁹"];
    if !glyphs.unicode {
        return n.to_string();
    }
    SUP.get(n - 1)
        .map_or_else(|| n.to_string(), |s| (*s).to_string())
}

/// One-row chrome for very narrow terminals: brand, active tab, count.
pub fn draw_compact_chrome(frame: &mut Frame, area: Rect, r: &Render<'_>) {
    let theme = r.theme;
    let mut spans = vec![Span::styled(" muxtop ", theme.accent_fill())];
    spans.push(Span::styled(
        format!(" {} ", r.app.tab.label()),
        theme.accent().bg(theme.header_bg),
    ));
    if let Some(count) = tab_count(r.app, r.app.tab) {
        spans.push(Span::styled(
            format!("{count} "),
            theme.dim().bg(theme.header_bg),
        ));
    }
    if r.app.paused {
        spans.push(Span::styled(
            " PAUSED ",
            theme.level_fill(crate::ui::theme::Level::Warning),
        ));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(theme.chrome()),
        area,
    );
}

// ---------------------------------------------------------------------------
// Status bar
// ---------------------------------------------------------------------------

/// The bottom row: a toast if one is pending, otherwise the state segments
/// followed by as many contextual hints as fit.
pub fn draw_statusbar(frame: &mut Frame, area: Rect, r: &Render<'_>) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let app = r.app;
    let theme = r.theme;

    // A message the user has not seen yet outranks the hints.
    if let Some(toast) = app.active_status() {
        let extra = app.notifier.active().len().saturating_sub(1);
        let mut spans = vec![Span::styled(
            format!(" {} ", toast.text),
            theme.level_fill(toast.level),
        )];
        if extra > 0 {
            spans.push(Span::styled(
                format!(" +{extra} more (Ctrl+L) "),
                theme.dim().bg(theme.header_bg),
            ));
        }
        frame.render_widget(
            Paragraph::new(Line::from(spans)).style(theme.chrome()),
            area,
        );
        return;
    }

    let mut spans: Vec<Span<'static>> = Vec::with_capacity(16);
    let mut used = 0usize;
    let push = |spans: &mut Vec<Span<'static>>, used: &mut usize, span: Span<'static>| -> bool {
        let len = span.content.chars().count();
        if *used + len > area.width as usize {
            return false;
        }
        *used += len;
        spans.push(span);
        true
    };

    // -- state segments: what the user would otherwise have to remember --
    for segment in state_segments(r) {
        let len = segment.chars().count();
        if used + len + 3 > area.width as usize {
            break;
        }
        push(
            &mut spans,
            &mut used,
            Span::styled(format!(" {segment} "), theme.dim().bg(theme.header_bg)),
        );
        push(
            &mut spans,
            &mut used,
            Span::styled(r.glyphs.sep.to_string(), theme.subtle().bg(theme.header_bg)),
        );
    }

    // -- hints: whatever room is left, most useful first --
    for (key, label) in keymap::hints(app.tab, app.is_remote()) {
        if !push(
            &mut spans,
            &mut used,
            Span::styled(format!(" {key} "), theme.key()),
        ) {
            break;
        }
        if !push(
            &mut spans,
            &mut used,
            Span::styled(format!("{label} "), theme.key_desc()),
        ) {
            break;
        }
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(theme.chrome()),
        area,
    );
}

/// The state chips, most important first.
fn state_segments(r: &Render<'_>) -> Vec<String> {
    let app = r.app;
    let mut out = Vec::with_capacity(4);

    if app.paused {
        out.push("PAUSED".to_string());
    }

    // Sort: which column, which direction.
    let arrow = if sort_descending(app) {
        r.glyphs.sort_desc
    } else {
        r.glyphs.sort_asc
    };
    out.push(format!("sort {}{arrow}", sort_label(app)));

    // Filter: the text *and* how much it kept, because an empty table and a
    // filter that matches nothing look identical without the count.
    let filter = app.filter_text();
    if !filter.is_empty() {
        out.push(format!("filter \"{filter}\" {}", app.item_count()));
    }

    // Position in the list.
    let (selected, _) = selection_of(app);
    if let Some(pos) = scrollbar::position_label(selected, app.item_count()) {
        out.push(pos);
    }
    out
}

fn selection_of(app: &AppState) -> (usize, usize) {
    match app.tab {
        Tab::Network => (app.net_selected, app.net_scroll_offset),
        Tab::Containers => (app.containers_selected, app.containers_scroll_offset),
        Tab::Kube => (app.kube_selected, app.kube_scroll_offset),
        Tab::Gpu => (app.gpu_selected, app.gpu_scroll_offset),
        _ => (app.selected, app.scroll_offset),
    }
}

fn sort_descending(app: &AppState) -> bool {
    use muxtop_core::process::SortOrder;
    let order = match app.tab {
        Tab::Network => app.net_sort_order,
        Tab::Containers => app.containers_sort_order,
        Tab::Kube => app.kube_sort_order,
        Tab::Gpu => app.gpu_sort_order,
        _ => app.sort_order,
    };
    matches!(order, SortOrder::Desc)
}

fn sort_label(app: &AppState) -> &'static str {
    use crate::app::{ContainerSortField, GpuSortField, KubeSortField, NetworkSortField};
    use muxtop_core::process::SortField;
    match app.tab {
        Tab::Network => match app.net_sort_field {
            NetworkSortField::Name => "name",
            NetworkSortField::RxRate => "rx",
            NetworkSortField::TxRate => "tx",
            NetworkSortField::TotalRx => "total rx",
            NetworkSortField::TotalTx => "total tx",
            NetworkSortField::Errors => "errors",
        },
        Tab::Containers => match app.containers_sort_field {
            ContainerSortField::Name => "name",
            ContainerSortField::Cpu => "cpu",
            ContainerSortField::Mem => "mem",
            ContainerSortField::NetRx => "rx",
            ContainerSortField::NetTx => "tx",
            ContainerSortField::Uptime => "uptime",
        },
        Tab::Gpu => match app.gpu_sort_field {
            GpuSortField::DeviceIndex => "index",
            GpuSortField::DeviceName | GpuSortField::ProcName => "name",
            GpuSortField::DeviceUtil => "util",
            GpuSortField::DeviceMem | GpuSortField::ProcMem => "vram",
            GpuSortField::DeviceTemp => "temp",
            GpuSortField::DevicePower => "power",
            GpuSortField::ProcPid => "pid",
            GpuSortField::ProcDevice => "device",
        },
        Tab::Kube => match app.kube_sort_field {
            KubeSortField::PodName | KubeSortField::NodeName | KubeSortField::DeployName => "name",
            KubeSortField::PodCpu | KubeSortField::NodeCpuPct => "cpu",
            KubeSortField::PodMem | KubeSortField::NodeMemPct => "mem",
            KubeSortField::PodRestarts => "restarts",
            KubeSortField::PodAge | KubeSortField::NodeAge | KubeSortField::DeployAge => "age",
            KubeSortField::PodPhase => "phase",
            KubeSortField::NodePodCount => "pods",
            KubeSortField::DeployNamespace => "namespace",
            KubeSortField::DeployReadyRatio => "ready",
        },
        _ => match app.sort_field {
            SortField::Cpu => "cpu",
            SortField::Mem => "mem",
            SortField::Pid => "pid",
            SortField::Name => "name",
            SortField::User => "user",
        },
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn percent(used: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        (used as f64 / total as f64 * 100.0).clamp(0.0, 100.0)
    }
}

/// `3d 4h 12m`, dropping the leading units that are zero.
pub fn format_uptime(secs: u64) -> String {
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3600;
    let mins = (secs % 3600) / 60;
    if days > 0 {
        format!("{days}d {hours}h {mins}m")
    } else if hours > 0 {
        format!("{hours}h {mins}m")
    } else {
        format!("{mins}m")
    }
}

/// Split an area into header rows, content, and the status bar.
pub fn split(area: Rect) -> (Rect, Option<Rect>, Rect, Rect) {
    if header_rows(area.width) == 1 {
        let [chrome, content, status] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(1),
        ])
        .areas(area);
        (chrome, None, content, status)
    } else {
        let [header, tabs, content, status] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(1),
        ])
        .areas(area);
        (header, Some(tabs), content, status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uptime_drops_leading_zero_units() {
        assert_eq!(format_uptime(0), "0m");
        assert_eq!(format_uptime(90), "1m");
        assert_eq!(format_uptime(7200), "2h 0m");
        assert_eq!(format_uptime(90_061), "1d 1h 1m");
    }

    #[test]
    fn tab_numbers_are_one_cell_and_degrade_to_ascii() {
        let unicode = crate::ui::glyphs::Glyphs::new(true);
        let ascii = crate::ui::glyphs::Glyphs::new(false);
        for i in 1..=Tab::ALL.len() {
            assert_eq!(tab_number(i, &unicode).chars().count(), 1);
            let a = tab_number(i, &ascii);
            assert!(a.is_ascii(), "tab number leaked a superscript: {a}");
        }
        assert_eq!(tab_number(42, &unicode), "42");
    }

    #[test]
    fn percent_handles_a_zero_total() {
        assert_eq!(percent(0, 0), 0.0);
        assert_eq!(percent(1, 2), 50.0);
        // A used figure above total (possible across a snapshot boundary)
        // clamps instead of overflowing a meter.
        assert_eq!(percent(3, 2), 100.0);
    }

    #[test]
    fn chrome_collapses_to_one_row_when_narrow() {
        assert_eq!(header_rows(40), 1);
        assert_eq!(header_rows(59), 1);
        assert_eq!(header_rows(60), 2);
        assert_eq!(header_rows(200), 2);
    }

    #[test]
    fn split_reserves_a_status_row_at_every_width() {
        for width in [40u16, 80, 200] {
            let (_, _, content, status) = split(Rect::new(0, 0, width, 24));
            assert_eq!(status.height, 1);
            assert!(content.height > 0);
        }
    }

    #[test]
    fn split_survives_a_terminal_with_no_room_for_content() {
        // Two rows cannot hold header + tabs + content + status. Whatever the
        // solver gives up, the regions must stay inside the area and the call
        // must not panic — muxtop has to start in a two-row tmux pane.
        for h in [0u16, 1, 2, 3] {
            let area = Rect::new(0, 0, 80, h);
            let (header, tabs, content, status) = split(area);
            for r in [header, content, status].into_iter().chain(tabs) {
                assert!(
                    r.y + r.height <= area.y + area.height,
                    "region {r:?} escaped a {h}-row terminal"
                );
            }
        }
    }
}
