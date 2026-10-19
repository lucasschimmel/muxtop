// Network tab.

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
use crate::app::NetworkSortField;
use crate::ui::theme::Level;
use muxtop_core::network::NetworkInterfaceSnapshot;
use muxtop_core::process::SortOrder;

const COL_NAME: usize = 0;
const COL_RX: usize = 2;
const COL_TX: usize = 3;
const COL_TOTAL_RX: usize = 4;
const COL_TOTAL_TX: usize = 5;
const COL_ERRORS: usize = 6;

const COLUMNS: &[Column] = &[
    Column::flex("INTERFACE", 12, PRIO_ESSENTIAL),
    Column::fixed("S", 2, Align::Left, PRIO_LOW),
    Column::fixed("RX/s", 12, Align::Right, PRIO_ESSENTIAL),
    Column::fixed("TX/s", 12, Align::Right, PRIO_ESSENTIAL),
    Column::fixed("TOTAL RX", 10, Align::Right, PRIO_MEDIUM),
    Column::fixed("TOTAL TX", 10, Align::Right, PRIO_MEDIUM),
    Column::fixed("ERR", 7, Align::Right, PRIO_HIGH),
];

/// Rows reserved for the traffic graph under the table.
const GRAPH_HEIGHT: u16 = 4;

pub fn draw_network_tab(frame: &mut Frame, area: Rect, r: &Render<'_>) {
    let app = r.app;
    if app.last_snapshot.is_none() {
        let waiting = r.ellipsis("Waiting for data");
        empty::render(frame, area, &EmptyState::waiting(&waiting), r.theme);
        return;
    }

    let interfaces = app.visible_interfaces();
    let filter_h = u16::from(app.filter_editing());
    // The graph needs a selected interface and enough vertical room to be worth
    // the space it costs.
    let graph_h = if !interfaces.is_empty() && area.height >= 12 {
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

    draw_summary(frame, summary_area, r, &interfaces);

    let filtered = !app.net_filter_input.is_empty();
    let spec = Spec {
        columns: COLUMNS,
        sort_col: sort_column(app.net_sort_field),
        descending: matches!(app.net_sort_order, SortOrder::Desc),
        total: interfaces.len(),
        selected: app.net_selected,
        scroll: app.net_scroll_offset,
        col_scroll: app.col_scroll,
        empty: if filtered {
            EmptyState::no_match("No matching interfaces")
        } else {
            EmptyState::empty(
                "No network interfaces",
                Some("Nothing is reporting traffic."),
            )
        },
    };

    table::draw(frame, table_area, r, &spec, |idx| {
        match interfaces.get(idx) {
            Some(iface) => interface_row(iface, r),
            None => Row::new(Vec::new()),
        }
    });

    if graph_h > 0 {
        draw_graph(frame, graph_area, r, &interfaces);
    }
    if filter_h > 0 {
        filter_bar::draw(frame, filter_area, r, "Filter interfaces");
    }
}

fn sort_column(field: NetworkSortField) -> Option<usize> {
    Some(match field {
        NetworkSortField::Name => COL_NAME,
        NetworkSortField::RxRate => COL_RX,
        NetworkSortField::TxRate => COL_TX,
        NetworkSortField::TotalRx => COL_TOTAL_RX,
        NetworkSortField::TotalTx => COL_TOTAL_TX,
        NetworkSortField::Errors => COL_ERRORS,
    })
}

/// Filter + sort the interfaces exactly as the table shows them.
pub(crate) fn sort_interfaces(app: &crate::app::AppState, rows: &mut [NetworkInterfaceSnapshot]) {
    let history = &app.network_history;

    match app.net_sort_field {
        NetworkSortField::Name => rows.sort_by(|a, b| a.name.cmp(&b.name)),
        NetworkSortField::RxRate => rows.sort_by(|a, b| {
            history
                .bandwidth_rx(&b.name)
                .partial_cmp(&history.bandwidth_rx(&a.name))
                .unwrap_or(std::cmp::Ordering::Equal)
        }),
        NetworkSortField::TxRate => rows.sort_by(|a, b| {
            history
                .bandwidth_tx(&b.name)
                .partial_cmp(&history.bandwidth_tx(&a.name))
                .unwrap_or(std::cmp::Ordering::Equal)
        }),
        NetworkSortField::TotalRx => rows.sort_by_key(|i| std::cmp::Reverse(i.bytes_rx)),
        NetworkSortField::TotalTx => rows.sort_by_key(|i| std::cmp::Reverse(i.bytes_tx)),
        NetworkSortField::Errors => {
            rows.sort_by_key(|i| std::cmp::Reverse(i.errors_rx + i.errors_tx))
        }
    }
    // `Name` sorts ascending by nature; the others descend by nature.
    let ascending = matches!(app.net_sort_order, SortOrder::Asc);
    let natural_ascending = matches!(app.net_sort_field, NetworkSortField::Name);
    if ascending != natural_ascending {
        rows.reverse();
    }
}

fn interface_row(iface: &NetworkInterfaceSnapshot, r: &Render<'_>) -> Row {
    let history = &r.app.network_history;
    let rx = history.bandwidth_rx(&iface.name) as u64;
    let tx = history.bandwidth_tx(&iface.name) as u64;
    let errors = iface.errors_rx + iface.errors_tx;
    let level = if iface.is_up {
        Level::Success
    } else {
        Level::Neutral
    };

    Row::new(vec![
        Cell::new(scrub_ctrl(&iface.name).into_owned()),
        Cell::colored(badge::marker(level, r.glyphs), r.theme.level_color(level)),
        Cell::new(format!("{} {}", r.glyphs.arrow_down, meter::human_rate(rx))),
        Cell::new(format!("{} {}", r.glyphs.arrow_up, meter::human_rate(tx))),
        Cell::new(meter::human_bytes(iface.bytes_rx)),
        Cell::new(meter::human_bytes(iface.bytes_tx)),
        // Errors are the reason this column exists; a non-zero value must never
        // look like every other number on the row.
        if errors > 0 {
            Cell::colored(errors.to_string(), r.theme.danger)
        } else {
            Cell::new("0")
        },
    ])
}

fn draw_summary(
    frame: &mut Frame,
    area: Rect,
    r: &Render<'_>,
    interfaces: &[NetworkInterfaceSnapshot],
) {
    let history = &r.app.network_history;
    let rx: f64 = interfaces
        .iter()
        .map(|i| history.bandwidth_rx(&i.name))
        .sum();
    let tx: f64 = interfaces
        .iter()
        .map(|i| history.bandwidth_tx(&i.name))
        .sum();
    let up = interfaces.iter().filter(|i| i.is_up).count();

    let line = Line::from(vec![
        Span::styled(" Network ", r.theme.accent_fill()),
        Span::styled(
            format!("  Interfaces {up}/{}", interfaces.len()),
            r.theme.dim(),
        ),
        Span::styled(
            format!(
                "   Total {} {}   {} {}",
                r.glyphs.arrow_down,
                meter::human_rate(rx as u64),
                r.glyphs.arrow_up,
                meter::human_rate(tx as u64)
            ),
            r.theme.body(),
        ),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

/// Traffic history for the selected interface.
fn draw_graph(
    frame: &mut Frame,
    area: Rect,
    r: &Render<'_>,
    interfaces: &[NetworkInterfaceSnapshot],
) {
    let Some(iface) = interfaces.get(r.app.net_selected) else {
        return;
    };
    let width = area.width.saturating_sub(14);
    if width == 0 {
        return;
    }
    let history = &r.app.network_history;
    let rx = history.sparkline_rx(&iface.name, width as usize);
    let tx = history.sparkline_tx(&iface.name, width as usize);
    // One scale for both series, so the two lines are comparable rather than
    // each being normalised to its own maximum.
    let scale = rx
        .iter()
        .chain(tx.iter())
        .copied()
        .max()
        .unwrap_or(1)
        .max(1);

    let mut lines = vec![Line::from(Span::styled(
        format!(" {} ", scrub_ctrl(&iface.name)),
        r.theme.accent(),
    ))];
    for (arrow, data, color) in [
        (r.glyphs.arrow_down, rx, r.theme.success),
        (r.glyphs.arrow_up, tx, r.theme.info),
    ] {
        let mut spans = vec![Span::styled(format!(" {arrow} "), r.theme.dim())];
        spans.extend(
            spark::line(
                &data,
                width,
                Some(scale),
                spark::Tint::Flat,
                color,
                r.theme,
                r.glyphs,
            )
            .spans,
        );
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(Span::styled(
        format!(" peak {}", meter::human_rate(scale)),
        r.theme.subtle(),
    )));
    frame.render_widget(Paragraph::new(lines), area);
}

#[cfg(test)]
mod tests {
    use crate::app::{AppState, Tab};
    use crate::ui::test_support::*;

    fn app() -> AppState {
        let mut app = app_with_data();
        app.tab = Tab::Network;
        app
    }

    #[test]
    fn table_lists_interfaces_with_rates() {
        let text = all_text(&render_with(&app(), 120, 24));
        assert!(text.contains("INTERFACE"));
        assert!(text.contains("RX/s"));
        assert!(text.contains("TX/s"));
        assert!(text.contains("eth0"));
        assert!(text.contains("lo"));
    }

    #[test]
    fn summary_shows_totals_and_interface_count() {
        let text = all_text(&render_with(&app(), 120, 24));
        assert!(text.contains("Interfaces"));
        assert!(text.contains("Total"));
    }

    #[test]
    fn graph_shows_the_selected_interface() {
        let text = all_text(&render_with(&app(), 120, 30));
        assert!(text.contains("peak"), "traffic graph missing:\n{text}");
    }

    #[test]
    fn graph_is_dropped_on_short_terminals_rather_than_squashed() {
        let text = all_text(&render_with(&app(), 120, 11));
        assert!(!text.contains("peak"));
        // The table still renders.
        assert!(text.contains("eth0"));
    }

    #[test]
    fn errors_are_shown() {
        let mut snap = snapshot();
        snap.networks.interfaces[0].errors_rx = 12;
        let mut app = AppState::new();
        app.tab = Tab::Network;
        app.apply_snapshot(snap);
        let text = all_text(&render_with(&app, 120, 24));
        assert!(text.contains("12"));
    }

    #[test]
    fn empty_filter_result_explains_itself() {
        let mut app = app();
        app.set_filter("zzzz");
        let text = all_text(&render_with(&app, 100, 24));
        assert!(text.contains("No matching interfaces"));
    }

    #[test]
    fn renders_under_every_profile_and_size() {
        let mut app = app();
        for (w, h) in [(1u16, 1u16), (40, 8), (80, 24), (200, 50)] {
            let _ = render_with(&app, w, h);
        }
        for (color, unicode) in all_profiles() {
            let _ = render_caps(&mut app, 100, 30, color, unicode);
        }
    }
}
