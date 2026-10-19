// GPU tab — NVIDIA (NVML), AMD (amdgpu sysfs) and Apple Silicon (IOKit +
// IOReport), read-only.
//
// Ported onto the shared widget layer in 0.5.1. The domain rule that shaped the
// original view is preserved and is the reason several cells render a dash:
// unlike CPU or memory, *no* GPU metric is universally available, so a driver
// that cannot report a value must never be shown as reporting zero.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::Render;
use super::filter_bar;
use super::sanitize::scrub_ctrl;
use super::widgets::columns::{Align, Column, PRIO_ESSENTIAL, PRIO_HIGH, PRIO_LOW, PRIO_MEDIUM};
use super::widgets::empty::{self, EmptyState};
use super::widgets::meter;
use super::widgets::table::{self, Cell, Row, Spec};
use crate::app::{GpuSortField, GpuSubview};
use crate::ui::theme::Level;
use muxtop_core::gpu::{
    GpuBackend, GpuDeviceSnapshot, GpuProcessKind, GpuProcessSnapshot, GpuVendor, GpusSnapshot,
};
use muxtop_core::process::SortOrder;

const DEVICE_COLUMNS: [Column; 10] = [
    Column::fixed("#", 4, Align::Right, PRIO_ESSENTIAL),
    Column::flex("DEVICE", 18, PRIO_ESSENTIAL),
    Column::fixed("VENDOR", 9, Align::Left, PRIO_LOW),
    Column::fixed("UTIL", 7, Align::Right, PRIO_ESSENTIAL),
    Column::fixed("VRAM", 18, Align::Right, PRIO_HIGH),
    Column::fixed("TEMP", 7, Align::Right, PRIO_HIGH),
    Column::fixed("POWER", 12, Align::Right, PRIO_MEDIUM),
    Column::fixed("CLOCK", 9, Align::Right, PRIO_LOW),
    Column::fixed("FAN", 6, Align::Right, PRIO_LOW),
    Column::fixed("ENC/DEC", 10, Align::Right, PRIO_LOW),
];

/// Index of the memory column in [`DEVICE_COLUMNS`], whose header is the one
/// thing about the table that depends on the hardware — see [`memory_label`].
const DEVICE_MEM_COLUMN: usize = 4;

const PROC_COLUMNS: &[Column] = &[
    Column::fixed("PID", 8, Align::Right, PRIO_ESSENTIAL),
    Column::fixed("GPU", 5, Align::Right, PRIO_MEDIUM),
    Column::flex("PROCESS", 18, PRIO_ESSENTIAL),
    Column::fixed("TYPE", 10, Align::Left, PRIO_LOW),
    Column::fixed("VRAM", 10, Align::Right, PRIO_ESSENTIAL),
];

pub fn draw_gpu_tab(frame: &mut Frame, area: Rect, r: &Render<'_>) {
    let app = r.app;
    let Some(snap) = app.last_snapshot.as_ref().and_then(|s| s.gpu.as_ref()) else {
        let waiting = r.ellipsis("Probing for GPUs");
        empty::render(frame, area, &EmptyState::waiting(&waiting), r.theme);
        return;
    };

    if !snap.available {
        // The engine explains itself: no driver, no device, or a probe that was
        // deliberately skipped. A bare "no GPU" would not distinguish them.
        let detail = scrub_ctrl(&snap.detail).into_owned();
        let state = if detail.is_empty() {
            EmptyState::empty("No GPU detected", None)
        } else {
            EmptyState::empty("No GPU detected", Some(&detail))
        };
        empty::render(frame, area, &state, r.theme);
        return;
    }

    let filter_h = u16::from(app.filter_editing());
    let [summary_area, subtab_area, table_area, filter_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(filter_h),
    ])
    .areas(area);

    draw_summary(frame, summary_area, r, snap);
    draw_subtabs(frame, subtab_area, r, snap);

    match app.gpu_subview {
        GpuSubview::Devices => draw_devices(frame, table_area, r, snap),
        GpuSubview::Procs => draw_procs(frame, table_area, r, snap),
    }

    if filter_h > 0 {
        filter_bar::draw(frame, filter_area, r, "Filter GPUs");
    }
}

fn backend_label(backend: GpuBackend) -> &'static str {
    match backend {
        GpuBackend::Nvml => "NVML",
        GpuBackend::AmdSysfs => "amdgpu",
        GpuBackend::AppleIoReport => "IOReport",
    }
}

/// Header for the memory column.
///
/// `VRAM` is a lie on Apple Silicon: the GPU has no dedicated memory, it
/// addresses the same pool as the CPU. The column shows the same two numbers
/// either way — bytes held by the GPU against the total available to it — but
/// calling the whole of a Mac's RAM "VRAM" would misdescribe both of them.
/// The header follows the devices actually on screen, so a mixed host (only
/// possible off macOS, where no Apple GPU exists) keeps the discrete wording.
fn memory_label(snap: &GpusSnapshot) -> &'static str {
    let all_unified =
        !snap.devices.is_empty() && snap.devices.iter().all(|d| d.vendor == GpuVendor::Apple);
    if all_unified { "MEM" } else { "VRAM" }
}

/// [`DEVICE_COLUMNS`] with the memory header resolved for this snapshot.
fn device_columns(snap: &GpusSnapshot) -> [Column; DEVICE_COLUMNS.len()] {
    let mut columns = DEVICE_COLUMNS;
    columns[DEVICE_MEM_COLUMN].label = memory_label(snap);
    columns
}

fn vendor_label(vendor: GpuVendor) -> &'static str {
    match vendor {
        GpuVendor::Nvidia => "NVIDIA",
        GpuVendor::Amd => "AMD",
        GpuVendor::Intel => "Intel",
        GpuVendor::Apple => "Apple",
        GpuVendor::Unknown => "Unknown",
    }
}

fn draw_summary(frame: &mut Frame, area: Rect, r: &Render<'_>, snap: &GpusSnapshot) {
    let backends: Vec<&str> = snap.backends.iter().map(|b| backend_label(*b)).collect();
    let line = Line::from(vec![
        Span::styled(" GPU ", r.theme.accent_fill()),
        Span::styled(format!("  {} device", snap.devices.len()), r.theme.body()),
        Span::styled(
            if snap.devices.len() == 1 { "" } else { "s" },
            r.theme.body(),
        ),
        Span::styled(
            format!(" {} {}", r.glyphs.sep, backends.join(" + ")),
            r.theme.dim(),
        ),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_subtabs(frame: &mut Frame, area: Rect, r: &Render<'_>, snap: &GpusSnapshot) {
    let counts = [
        (GpuSubview::Devices, "D", snap.devices.len()),
        (GpuSubview::Procs, "P", snap.processes.len()),
    ];
    let mut spans = Vec::with_capacity(counts.len() * 2);
    for (sv, key, count) in counts {
        let active = sv == r.app.gpu_subview;
        spans.push(Span::styled(
            format!(" {key} "),
            if active {
                r.theme.key()
            } else {
                r.theme.subtle()
            },
        ));
        spans.push(Span::styled(
            format!("{} {count}  ", sv.label()),
            if active {
                r.theme.accent()
            } else {
                r.theme.dim()
            },
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

// ---------------------------------------------------------------------------
// Devices
// ---------------------------------------------------------------------------

pub(crate) fn device_matches_filter(d: &GpuDeviceSnapshot, filter: &str) -> bool {
    filter.is_empty()
        || d.name.to_lowercase().contains(filter)
        || d.vendor.label().to_lowercase().contains(filter)
}

pub(crate) fn process_matches_filter(p: &GpuProcessSnapshot, filter: &str) -> bool {
    filter.is_empty()
        || p.name.to_lowercase().contains(filter)
        || p.pid.to_string().contains(filter)
}

/// Shared projection for rendering, bounds, inspection and clipboard identity.
pub(crate) fn visible_devices(app: &crate::app::AppState) -> Vec<&GpuDeviceSnapshot> {
    let Some(snap) = app.last_snapshot.as_ref().and_then(|s| s.gpu.as_ref()) else {
        return Vec::new();
    };
    let f = app.gpu_filter_input.to_lowercase();
    let mut rows: Vec<_> = snap
        .devices
        .iter()
        .filter(|d| device_matches_filter(d, &f))
        .collect();
    sort_devices(&mut rows, app.gpu_sort_field, app.gpu_sort_order);
    rows
}

pub(crate) fn visible_procs(app: &crate::app::AppState) -> Vec<&GpuProcessSnapshot> {
    let Some(snap) = app.last_snapshot.as_ref().and_then(|s| s.gpu.as_ref()) else {
        return Vec::new();
    };
    let f = app.gpu_filter_input.to_lowercase();
    let mut rows: Vec<_> = snap
        .processes
        .iter()
        .filter(|p| process_matches_filter(p, &f))
        .collect();
    sort_procs(&mut rows, app.gpu_sort_field, app.gpu_sort_order);
    rows
}

fn draw_devices(frame: &mut Frame, area: Rect, r: &Render<'_>, snap: &GpusSnapshot) {
    let app = r.app;
    let f = app.gpu_filter_input.to_lowercase();
    let devices = visible_devices(app);

    let columns = device_columns(snap);
    let spec = Spec {
        columns: &columns,
        sort_col: device_sort_column(app.gpu_sort_field),
        descending: matches!(app.gpu_sort_order, SortOrder::Desc),
        total: devices.len(),
        selected: app.gpu_selected,
        scroll: app.gpu_scroll_offset,
        col_scroll: app.col_scroll,
        empty: if f.is_empty() {
            EmptyState::empty("No GPU devices", None)
        } else {
            EmptyState::no_match("No matching GPUs")
        },
    };

    table::draw(frame, area, r, &spec, |idx| match devices.get(idx) {
        Some(d) => device_row(d, r),
        None => Row::new(Vec::new()),
    });
}

/// Format a power figure with just enough precision to stay true.
///
/// Whole watts are right for a discrete card, which idles in the tens and
/// peaks in the hundreds. They are wrong for an Apple Silicon GPU, which idles
/// under a tenth of a watt: rounding that to `0W` claims a powered-off block,
/// which is a different statement from a nearly-idle one and exactly the kind
/// of confusion the dashes elsewhere in this table exist to prevent.
pub(super) fn watts(w: f32) -> String {
    if w.abs() < 10.0 {
        format!("{w:.1}")
    } else {
        format!("{w:.0}")
    }
}

fn device_row(d: &GpuDeviceSnapshot, r: &Render<'_>) -> Row {
    let dash = || Cell::new(r.glyphs.none.to_string());

    // A dash means "this driver cannot report this", never "zero" — conflating
    // the two would make the tab lie about an idle GPU.
    let util = d.utilization_pct.map_or_else(dash, |p| {
        Cell::colored(format!("{p:.0}%"), r.theme.gauge_color(f64::from(p)))
    });

    let vram = match (d.mem_used_bytes, d.mem_total_bytes) {
        (Some(used), Some(total)) if total > 0 => {
            let pct = used as f64 / total as f64 * 100.0;
            Cell::colored(
                format!("{}/{}", meter::human_bytes(used), meter::human_bytes(total)),
                r.theme.gauge_color(pct),
            )
        }
        (Some(used), _) => Cell::new(meter::human_bytes(used)),
        _ => dash(),
    };

    let temp = d.temperature_c.map_or_else(dash, |t| {
        // Thermal thresholds are the vendor's, not ours: 83 C is where NVIDIA
        // starts throttling, and AMD is in the same neighbourhood.
        let level = if t >= 83.0 {
            r.theme.danger
        } else if t >= 70.0 {
            r.theme.warning
        } else {
            r.theme.success
        };
        Cell::colored(format!("{t:.0}C"), level)
    });

    let power = match (d.power_watts, d.power_limit_watts) {
        (Some(w), Some(limit)) => Cell::new(format!("{}/{}W", watts(w), watts(limit))),
        (Some(w), None) => Cell::new(format!("{}W", watts(w))),
        _ => dash(),
    };

    let clock = d
        .graphics_clock_mhz
        .map_or_else(dash, |c| Cell::new(format!("{c}MHz")));

    let fan = d
        .fan_pct
        .map_or_else(dash, |f| Cell::new(format!("{f:.0}%")));

    let encdec = match (d.encoder_pct, d.decoder_pct) {
        (Some(e), Some(dec)) => Cell::new(format!("{e:.0}/{dec:.0}%")),
        _ => dash(),
    };

    Row::new(vec![
        Cell::new(d.index.to_string()),
        Cell::new(scrub_ctrl(&d.name).into_owned()),
        Cell::new(vendor_label(d.vendor)),
        util,
        vram,
        temp,
        power,
        clock,
        fan,
        encdec,
    ])
}

fn device_sort_column(field: GpuSortField) -> Option<usize> {
    Some(match field {
        GpuSortField::DeviceIndex => 0,
        GpuSortField::DeviceName => 1,
        GpuSortField::DeviceUtil => 3,
        GpuSortField::DeviceMem => 4,
        GpuSortField::DeviceTemp => 5,
        GpuSortField::DevicePower => 6,
        _ => return None,
    })
}

fn f32_key(v: f32) -> i64 {
    (v * 100.0) as i64
}

/// Whether the device cannot report the metric being sorted on.
fn metric_missing(d: &GpuDeviceSnapshot, field: GpuSortField) -> bool {
    match field {
        GpuSortField::DeviceUtil => d.utilization_pct.is_none(),
        GpuSortField::DeviceMem => d.mem_used_bytes.is_none(),
        GpuSortField::DeviceTemp => d.temperature_c.is_none(),
        GpuSortField::DevicePower => d.power_watts.is_none(),
        // Index and name always exist.
        _ => false,
    }
}

fn sort_devices(devices: &mut [&GpuDeviceSnapshot], field: GpuSortField, order: SortOrder) {
    // Devices that cannot report the metric are parked at the end and stay
    // there whichever way the column is sorted. Folding them in as a very low
    // value would make an unreported temperature look like the coldest card —
    // exactly the confusion the dashes in the table exist to prevent.
    devices.sort_by_key(|d| metric_missing(d, field));
    let split = devices
        .iter()
        .position(|d| metric_missing(d, field))
        .unwrap_or(devices.len());
    let (reported, _unreported) = devices.split_at_mut(split);

    match field {
        GpuSortField::DeviceName => reported.sort_by(|a, b| a.name.cmp(&b.name)),
        GpuSortField::DeviceUtil => reported
            .sort_by_key(|d| std::cmp::Reverse(f32_key(d.utilization_pct.unwrap_or_default()))),
        GpuSortField::DeviceMem => {
            reported.sort_by_key(|d| std::cmp::Reverse(d.mem_used_bytes.unwrap_or_default()))
        }
        GpuSortField::DeviceTemp => reported
            .sort_by_key(|d| std::cmp::Reverse(f32_key(d.temperature_c.unwrap_or_default()))),
        GpuSortField::DevicePower => {
            reported.sort_by_key(|d| std::cmp::Reverse(f32_key(d.power_watts.unwrap_or_default())))
        }
        // Index is the natural order and the default.
        _ => reported.sort_by_key(|d| d.index),
    }
    // Index ascends by nature; every metric descends by nature.
    let ascending = matches!(order, SortOrder::Asc);
    let natural_ascending = matches!(field, GpuSortField::DeviceIndex | GpuSortField::DeviceName);
    if ascending != natural_ascending {
        reported.reverse();
    }
}

// ---------------------------------------------------------------------------
// Processes
// ---------------------------------------------------------------------------

fn draw_procs(frame: &mut Frame, area: Rect, r: &Render<'_>, snap: &GpusSnapshot) {
    let app = r.app;

    // AMD's sysfs interface exposes no per-process accounting. An empty table
    // there would read as "nothing is using the GPU", which is a different and
    // false statement.
    if snap.processes.is_empty() && !snap.devices.iter().any(|d| d.supports_process_stats) {
        empty::render(
            frame,
            area,
            &EmptyState::empty(
                "Per-process GPU usage is unavailable",
                Some(
                    "This backend exposes no per-process accounting: a driver limitation, not an idle GPU.",
                ),
            ),
            r.theme,
        );
        return;
    }

    let f = app.gpu_filter_input.to_lowercase();
    let procs = visible_procs(app);

    let spec = Spec {
        columns: PROC_COLUMNS,
        sort_col: proc_sort_column(app.gpu_sort_field),
        descending: matches!(app.gpu_sort_order, SortOrder::Desc),
        total: procs.len(),
        selected: app.gpu_selected,
        scroll: app.gpu_scroll_offset,
        col_scroll: app.col_scroll,
        empty: if f.is_empty() {
            EmptyState::empty("No processes are using the GPU", None)
        } else {
            EmptyState::no_match("No matching processes")
        },
    };

    table::draw(frame, area, r, &spec, |idx| match procs.get(idx) {
        Some(p) => proc_row(p, r),
        None => Row::new(Vec::new()),
    });
}

fn proc_row(p: &GpuProcessSnapshot, r: &Render<'_>) -> Row {
    let name = if p.name.is_empty() {
        // The PID vanished between the GPU query and the process-table lookup.
        r.glyphs.none.to_string()
    } else {
        scrub_ctrl(&p.name).into_owned()
    };
    let kind = match p.kind {
        GpuProcessKind::Compute => "compute",
        GpuProcessKind::Graphics => "graphics",
        GpuProcessKind::Both => "both",
        GpuProcessKind::Unknown => "unknown",
    };
    // `None` is routine on Windows: under WDDM the OS owns video memory, so
    // NVML cannot attribute it per process.
    let vram = p
        .mem_bytes
        .map_or_else(|| r.glyphs.none.to_string(), meter::human_bytes);

    Row::new(vec![
        Cell::new(p.pid.to_string()),
        Cell::new(p.device_index.to_string()),
        Cell::new(name),
        Cell::new(kind),
        Cell::new(vram),
    ])
}

fn proc_sort_column(field: GpuSortField) -> Option<usize> {
    Some(match field {
        GpuSortField::ProcPid => 0,
        GpuSortField::ProcDevice => 1,
        GpuSortField::ProcName => 2,
        GpuSortField::ProcMem => 4,
        _ => return None,
    })
}

fn sort_procs(procs: &mut [&GpuProcessSnapshot], field: GpuSortField, order: SortOrder) {
    // Same rule as devices: an unattributable allocation (routine under
    // Windows' WDDM) is parked at the end rather than sorted as "smallest".
    let missing =
        |p: &GpuProcessSnapshot| matches!(field, GpuSortField::ProcMem) && p.mem_bytes.is_none();
    procs.sort_by_key(|p| missing(p));
    let split = procs.iter().position(|p| missing(p)).unwrap_or(procs.len());
    let (reported, _unreported) = procs.split_at_mut(split);

    match field {
        GpuSortField::ProcPid => reported.sort_by_key(|p| p.pid),
        GpuSortField::ProcName => reported.sort_by(|a, b| a.name.cmp(&b.name)),
        GpuSortField::ProcDevice => reported.sort_by_key(|p| (p.device_index, p.pid)),
        _ => reported.sort_by_key(|p| std::cmp::Reverse(p.mem_bytes.unwrap_or_default())),
    }
    let ascending = matches!(order, SortOrder::Asc);
    let natural_ascending = !matches!(field, GpuSortField::ProcMem);
    if ascending != natural_ascending {
        reported.reverse();
    }
}

/// Severity of a device's utilisation, for the General tab's summary card.
pub fn device_level(d: &GpuDeviceSnapshot) -> Level {
    match d.utilization_pct {
        Some(p) if p >= 80.0 => Level::Error,
        Some(p) if p >= 50.0 => Level::Warning,
        Some(_) => Level::Success,
        None => Level::Neutral,
    }
}

/// One-line summary of a device, shared with the inspector and the dashboard.
pub fn device_summary(d: &GpuDeviceSnapshot, glyphs: &super::glyphs::Glyphs) -> String {
    let util = d
        .utilization_pct
        .map_or_else(|| glyphs.none.to_string(), |p| format!("{p:.0}%"));
    match (d.mem_used_bytes, d.mem_total_bytes) {
        (Some(used), Some(total)) => format!(
            "{util} {} {}/{}",
            glyphs.sep,
            meter::human_bytes(used),
            meter::human_bytes(total)
        ),
        _ => util,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{AppState, Tab};
    use crate::ui::test_support::*;

    fn device(index: u32, vendor: GpuVendor, full_metrics: bool) -> GpuDeviceSnapshot {
        GpuDeviceSnapshot {
            index,
            vendor,
            backend: if vendor == GpuVendor::Nvidia {
                GpuBackend::Nvml
            } else {
                GpuBackend::AmdSysfs
            },
            name: format!("Test GPU {index}"),
            bus_id: format!("0000:0{index}:00.0"),
            driver_version: Some("560.35".into()),
            utilization_pct: Some(64.0),
            mem_utilization_pct: Some(40.0),
            mem_used_bytes: Some(8 << 30),
            mem_total_bytes: Some(24 << 30),
            temperature_c: Some(71.0),
            power_watts: Some(220.0),
            power_limit_watts: Some(450.0),
            graphics_clock_mhz: Some(2520),
            memory_clock_mhz: Some(10501),
            fan_pct: full_metrics.then_some(48.0),
            encoder_pct: full_metrics.then_some(12.0),
            decoder_pct: full_metrics.then_some(3.0),
            supports_process_stats: full_metrics,
        }
    }

    fn gpu_app(available: bool, full_metrics: bool, procs: bool) -> AppState {
        let devices = if available {
            vec![device(
                0,
                if full_metrics {
                    GpuVendor::Nvidia
                } else {
                    GpuVendor::Amd
                },
                full_metrics,
            )]
        } else {
            Vec::new()
        };
        let processes = if procs {
            vec![GpuProcessSnapshot {
                pid: 4242,
                device_index: 0,
                name: "blender".into(),
                kind: GpuProcessKind::Compute,
                mem_bytes: Some(2 << 30),
            }]
        } else {
            Vec::new()
        };

        let mut snap = snapshot();
        snap.gpu = Some(GpusSnapshot {
            backends: if available {
                vec![if full_metrics {
                    GpuBackend::Nvml
                } else {
                    GpuBackend::AmdSysfs
                }]
            } else {
                Vec::new()
            },
            available,
            devices,
            processes,
            detail: if available {
                String::new()
            } else {
                "No NVIDIA driver and no amdgpu device found.".into()
            },
        });
        let mut app = AppState::new();
        app.tab = Tab::Gpu;
        app.apply_snapshot(snap);
        app
    }

    #[test]
    fn devices_view_lists_devices() {
        let app = gpu_app(true, true, false);
        let text = all_text(&render_with(&app, 160, 30));
        assert!(text.contains("DEVICE"));
        assert!(text.contains("Test GPU 0"));
        assert!(text.contains("NVIDIA"));
        assert!(text.contains("64%"));
    }

    #[test]
    fn vram_is_shown_against_capacity() {
        let app = gpu_app(true, true, false);
        let text = all_text(&render_with(&app, 180, 30));
        assert!(
            text.contains("8.0G/24.0G"),
            "VRAM must be shown against the card's total:\n{text}"
        );
    }

    #[test]
    fn unreported_metrics_render_as_a_dash_not_zero() {
        // The whole point of the GPU tab's data model: "unavailable" and
        // "idle" are different statements.
        let app = gpu_app(true, false, false);
        let text = all_text(&render_with(&app, 180, 30));
        assert!(
            text.contains('—'),
            "a metric the driver cannot report must not read as 0:\n{text}"
        );
    }

    #[test]
    fn absent_gpu_explains_why() {
        let app = gpu_app(false, false, false);
        let text = all_text(&render_with(&app, 100, 24));
        assert!(text.contains("No GPU detected"));
        assert!(
            text.contains("amdgpu"),
            "the engine's reason must reach the user:\n{text}"
        );
    }

    #[test]
    fn procs_view_lists_processes() {
        let mut app = gpu_app(true, true, true);
        app.switch_gpu_subview(GpuSubview::Procs);
        let text = all_text(&render_with(&app, 140, 30));
        assert!(text.contains("PROCESS"));
        assert!(text.contains("blender"));
        assert!(text.contains("4242"));
    }

    #[test]
    fn procs_view_distinguishes_no_support_from_no_processes() {
        // AMD reports no per-process accounting at all.
        let mut app = gpu_app(true, false, false);
        app.switch_gpu_subview(GpuSubview::Procs);
        let text = all_text(&render_with(&app, 140, 30));
        assert!(
            text.contains("no per-process accounting"),
            "an empty list would read as an idle GPU:\n{text}"
        );

        // NVIDIA with genuinely nothing running says something different.
        let mut app = gpu_app(true, true, false);
        app.switch_gpu_subview(GpuSubview::Procs);
        let text = all_text(&render_with(&app, 140, 30));
        assert!(text.contains("No processes are using the GPU"));
    }

    #[test]
    fn subtab_bar_shows_counts_and_keys() {
        let app = gpu_app(true, true, true);
        let text = all_text(&render_with(&app, 140, 30));
        assert!(text.contains("Devices 1"));
        assert!(text.contains("Procs 1"));
    }

    #[test]
    fn summary_names_the_backend() {
        let app = gpu_app(true, true, false);
        let text = all_text(&render_with(&app, 140, 30));
        assert!(text.contains("NVML"));
        assert!(text.contains("1 device"));
    }

    #[test]
    fn unreported_metrics_sort_last_in_both_directions() {
        // An unavailable metric is not a small one.
        let mut hot = device(0, GpuVendor::Nvidia, true);
        hot.temperature_c = Some(90.0);
        let mut unknown = device(1, GpuVendor::Amd, false);
        unknown.temperature_c = None;
        let mut devices = vec![&hot, &unknown];

        sort_devices(&mut devices, GpuSortField::DeviceTemp, SortOrder::Desc);
        assert_eq!(devices[0].index, 0, "the hottest card leads");
        sort_devices(&mut devices, GpuSortField::DeviceTemp, SortOrder::Asc);
        assert_eq!(
            devices[0].index, 0,
            "ascending must not promote an unreported temperature to coldest"
        );
    }

    // ---- Apple Silicon ----------------------------------------------------

    /// An Apple Silicon device as the v0.7 backend produces one: unified
    /// memory, no temperature, no fan, no power cap, no per-process stats.
    fn apple_device() -> GpuDeviceSnapshot {
        GpuDeviceSnapshot {
            index: 0,
            vendor: GpuVendor::Apple,
            backend: GpuBackend::AppleIoReport,
            name: "Apple M3 (8-core GPU)".into(),
            bus_id: String::new(),
            driver_version: Some("351.2".into()),
            utilization_pct: Some(62.0),
            mem_utilization_pct: None,
            mem_used_bytes: Some(2 << 30),
            mem_total_bytes: Some(8 << 30),
            temperature_c: None,
            power_watts: Some(2.9),
            power_limit_watts: None,
            graphics_clock_mhz: Some(874),
            memory_clock_mhz: None,
            fan_pct: None,
            encoder_pct: None,
            decoder_pct: None,
            supports_process_stats: false,
        }
    }

    fn apple_app() -> AppState {
        let mut snap = snapshot();
        snap.gpu = Some(GpusSnapshot {
            backends: vec![GpuBackend::AppleIoReport],
            available: true,
            devices: vec![apple_device()],
            processes: Vec::new(),
            detail: String::new(),
        });
        let mut app = AppState::new();
        app.tab = Tab::Gpu;
        app.apply_snapshot(snap);
        app
    }

    #[test]
    fn apple_memory_column_does_not_claim_to_be_vram() {
        // The GPU shares the machine's RAM with the CPU. Showing 2 GB of an
        // 8 GB pool under a "VRAM" header would describe neither number.
        let app = apple_app();
        let text = all_text(&render_with(&app, 180, 30));
        assert!(text.contains("MEM"), "expected a MEM header:\n{text}");
        assert!(
            !text.contains("VRAM"),
            "unified memory must not be labelled VRAM:\n{text}"
        );
        assert!(text.contains("2.0G/8.0G"), "{text}");
    }

    #[test]
    fn discrete_cards_keep_the_vram_header() {
        // The relabelling is conditional, not a rename: an NVIDIA card really
        // does have dedicated video memory.
        let app = gpu_app(true, true, false);
        let text = all_text(&render_with(&app, 180, 30));
        assert!(text.contains("VRAM"), "{text}");
    }

    #[test]
    fn sub_watt_power_does_not_round_away_to_zero() {
        // An idle Apple GPU draws under a tenth of a watt. `0W` would claim a
        // powered-off block, which is not what the counter said.
        assert_eq!(watts(0.08), "0.1");
        assert_eq!(watts(2.9), "2.9");
        // Discrete cards keep whole watts: `220/450W` is the useful reading,
        // and a decimal there is noise on a figure that moves by tens.
        assert_eq!(watts(220.0), "220");
        assert_eq!(watts(450.4), "450");
    }

    #[test]
    fn apple_renders_its_unavailable_metrics_as_dashes() {
        let app = apple_app();
        let text = all_text(&render_with(&app, 180, 30));
        assert!(
            text.contains('—'),
            "temperature, fan and enc/dec are unreportable on Apple Silicon \
             and must not read as zero:\n{text}"
        );
        assert!(text.contains("Apple"));
        assert!(text.contains("874MHz"), "{text}");
    }

    #[test]
    fn apple_procs_view_says_the_platform_cannot_answer() {
        let mut app = apple_app();
        app.switch_gpu_subview(GpuSubview::Procs);
        let text = all_text(&render_with(&app, 140, 30));
        assert!(
            text.contains("no per-process accounting"),
            "an empty list would read as an idle GPU:\n{text}"
        );
    }

    #[test]
    fn memory_label_follows_the_devices_on_screen() {
        let mut snap = GpusSnapshot::unavailable();
        assert_eq!(
            memory_label(&snap),
            "VRAM",
            "an empty list must not adopt the unified wording"
        );

        snap.devices.push(apple_device());
        assert_eq!(memory_label(&snap), "MEM");

        // A host with both would be reporting one device that has dedicated
        // memory, so the discrete wording wins.
        snap.devices.push(device(1, GpuVendor::Nvidia, true));
        assert_eq!(memory_label(&snap), "VRAM");
    }

    #[test]
    fn renders_under_every_profile_and_size() {
        for sv in [GpuSubview::Devices, GpuSubview::Procs] {
            let mut app = gpu_app(true, true, true);
            app.switch_gpu_subview(sv);
            for (w, h) in [(1u16, 1u16), (40, 8), (80, 24), (200, 50)] {
                let _ = render_with(&app, w, h);
            }
            for (color, unicode) in all_profiles() {
                let _ = render_caps(&mut app, 140, 30, color, unicode);
            }
        }
    }
}
