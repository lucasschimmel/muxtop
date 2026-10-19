// Layout & rendering for the TUI.

mod actions_menu;
pub mod chrome;
mod confirm;
mod containers;
mod filter_bar;
mod general;
pub mod glyphs;
pub(crate) mod gpu;
mod help;
mod inspector;
pub(crate) mod kube;
mod log_view;
pub(crate) mod network;
mod palette;
mod processes;
pub mod sanitize;
pub mod theme;
pub mod widgets;

use ratatui::Frame;

use crate::app::{AppState, Overlay, Tab};
use crate::terminal::Breakpoint;
use glyphs::Glyphs;
use theme::Theme;

/// Everything a view needs to draw itself.
///
/// Bundling these means a view can never accidentally resolve its own theme or
/// hardcode a glyph, which is how 0.4 ended up with two different sort arrows
/// and a Unicode flag that `general.rs` accepted and then ignored.
pub struct Render<'a> {
    pub app: &'a AppState,
    pub theme: &'a Theme,
    pub glyphs: &'a Glyphs,
    /// Layout class of the frame being drawn.
    ///
    /// Derived from the frame, not from `TermCaps`: a resize event and the
    /// frame that follows it are two different moments, and the layout must
    /// follow the surface it is actually painting on.
    pub breakpoint: Breakpoint,
}

impl Render<'_> {
    /// Text that ends in an ellipsis, in whichever glyph set is active.
    pub fn ellipsis(&self, text: &str) -> String {
        format!("{text}{}", self.glyphs.ellipsis)
    }

    /// `Process — nginx`, with a dash the terminal can actually render.
    pub fn titled(&self, kind: &str, name: &str) -> String {
        format!("{kind} {} {name}", self.glyphs.dash)
    }
}

/// Render the full application: chrome, content, overlays.
pub fn draw_root(frame: &mut Frame, app: &AppState) {
    let theme = Theme::with_kind(app.theme_kind, app.term_caps.color_support);
    let glyphs = Glyphs::new(app.term_caps.unicode);
    let area = frame.area();
    let r = Render {
        app,
        theme: &theme,
        glyphs: &glyphs,
        breakpoint: Breakpoint::from_width(area.width),
    };
    let (header_area, tabbar_area, content_area, status_area) = chrome::split(area);

    match tabbar_area {
        Some(tabs) => {
            chrome::draw_header(frame, header_area, &r);
            chrome::draw_tabbar(frame, tabs, &r);
        }
        // Too narrow for two rows of chrome: merge them.
        None => chrome::draw_compact_chrome(frame, header_area, &r),
    }

    draw_content(frame, content_area, &r);
    chrome::draw_statusbar(frame, status_area, &r);

    // Overlays. The confirm dialog outranks everything else: it is the only
    // one guarding a destructive action.
    match app.overlay {
        Overlay::None => {}
        Overlay::Palette | Overlay::Command => palette::draw_palette(frame, &r),
        Overlay::Help => help::draw_help(frame, &r),
        Overlay::Log => log_view::draw_log(frame, &r),
        Overlay::Actions => actions_menu::draw_actions(frame, &r),
        Overlay::Inspector => inspector::draw_inspector(frame, content_area, &r),
    }

    if app.confirm.is_some() {
        confirm::draw_confirm(frame, &r);
    }
}

/// Render the content area based on the active tab.
fn draw_content(frame: &mut Frame, area: ratatui::layout::Rect, r: &Render<'_>) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    match r.app.tab {
        Tab::General => general::draw_general_tab(frame, area, r),
        Tab::Processes => processes::draw_processes_tab(frame, area, r),
        Tab::Network => network::draw_network_tab(frame, area, r),
        Tab::Containers => containers::draw_containers_tab(frame, area, r),
        Tab::Kube => kube::draw_kube_tab(frame, area, r),
        Tab::Gpu => gpu::draw_gpu_tab(frame, area, r),
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use crate::app::AppState;
    use crate::terminal::{ColorSupport, TermCaps};
    use ratatui::{Terminal, backend::TestBackend};

    /// Render an app state at a given size and return the buffer.
    pub fn render_with(app: &AppState, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| super::draw_root(frame, app)).unwrap();
        terminal.backend().buffer().clone()
    }

    /// Render under an explicit capability profile — the whole point of the
    /// degradation work is that all of them keep working.
    pub fn render_caps(
        app: &mut AppState,
        width: u16,
        height: u16,
        color: ColorSupport,
        unicode: bool,
    ) -> ratatui::buffer::Buffer {
        app.term_caps = TermCaps {
            color_support: color,
            unicode,
            mouse: false,
            width,
            height,
        };
        render_with(app, width, height)
    }

    pub fn line_text(buf: &ratatui::buffer::Buffer, row: u16) -> String {
        let width = buf.area.width;
        (0..width)
            .map(|col| buf.cell((col, row)).map(|c| c.symbol()).unwrap_or(" "))
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    pub fn contains(buf: &ratatui::buffer::Buffer, needle: &str) -> bool {
        (0..buf.area.height).any(|row| line_text(buf, row).contains(needle))
    }

    pub fn all_text(buf: &ratatui::buffer::Buffer) -> String {
        (0..buf.area.height)
            .map(|row| line_text(buf, row))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A snapshot with enough in it that every tab has something to draw.
    pub fn snapshot() -> muxtop_core::system::SystemSnapshot {
        use muxtop_core::network::{NetworkInterfaceSnapshot, NetworkSnapshot};
        use muxtop_core::process::ProcessInfo;
        use muxtop_core::system::*;

        let cores = (0..4)
            .map(|i| CoreSnapshot {
                name: format!("cpu{i}"),
                usage: 10.0 * (i as f32 + 1.0),
                frequency: 3600,
            })
            .collect();

        let processes = (0..40)
            .map(|i| ProcessInfo {
                pid: 1000 + i,
                parent_pid: (i > 0).then_some(1000),
                name: format!("proc{i}"),
                command: format!("/usr/bin/proc{i} --serve"),
                user: "lucas".to_string(),
                cpu_percent: 40.0 - i as f32,
                memory_bytes: 1_000_000 * u64::from(i + 1),
                memory_percent: 0.5 * (i as f32 + 1.0),
                status: if i % 3 == 0 { "Running" } else { "Sleeping" }.to_string(),
            })
            .collect();

        SystemSnapshot {
            cpu: CpuSnapshot {
                global_usage: 42.5,
                cores,
            },
            memory: MemorySnapshot {
                total: 16_000_000_000,
                used: 8_000_000_000,
                available: 8_000_000_000,
                swap_total: 4_000_000_000,
                swap_used: 1_000_000_000,
            },
            load: LoadSnapshot {
                one: 2.31,
                five: 1.87,
                fifteen: 1.42,
                uptime_secs: 90_061,
            },
            processes,
            networks: NetworkSnapshot {
                interfaces: vec![
                    NetworkInterfaceSnapshot {
                        name: "eth0".to_string(),
                        bytes_rx: 1_000_000,
                        bytes_tx: 500_000,
                        packets_rx: 1000,
                        packets_tx: 500,
                        errors_rx: 0,
                        errors_tx: 0,
                        mac_address: "00:11:22:33:44:55".to_string(),
                        is_up: true,
                    },
                    NetworkInterfaceSnapshot {
                        name: "lo".to_string(),
                        bytes_rx: 100,
                        bytes_tx: 100,
                        packets_rx: 10,
                        packets_tx: 10,
                        errors_rx: 0,
                        errors_tx: 0,
                        mac_address: "00:00:00:00:00:00".to_string(),
                        is_up: true,
                    },
                ],
                total_rx: 1_000_100,
                total_tx: 500_100,
            },
            containers: None,
            kube: None,
            gpu: None,
            timestamp_ms: 1_700_000_000_000,
        }
    }

    /// An app with data loaded, ready to render.
    pub fn app_with_data() -> AppState {
        let mut app = AppState::new();
        app.apply_snapshot(snapshot());
        app
    }

    pub fn all_profiles() -> Vec<(ColorSupport, bool)> {
        vec![
            (ColorSupport::TrueColor, true),
            (ColorSupport::Colors256, true),
            (ColorSupport::Basic, false),
            (ColorSupport::NoColor, false),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use crate::terminal::ColorSupport;

    // -- Layout --

    #[test]
    fn root_renders_at_the_classic_terminal_size() {
        let app = app_with_data();
        let buf = render_with(&app, 80, 24);
        assert!(!line_text(&buf, 0).is_empty(), "header must not be empty");
        assert!(
            !line_text(&buf, 23).is_empty(),
            "status bar must not be empty"
        );
    }

    #[test]
    fn root_never_panics_at_any_size() {
        let app = app_with_data();
        for (w, h) in [
            (1, 1),
            (2, 2),
            (10, 5),
            (40, 6),
            (60, 20),
            (80, 2),
            (80, 24),
            (200, 60),
            (400, 100),
        ] {
            let _ = render_with(&app, w, h);
        }
    }

    #[test]
    fn root_never_panics_on_any_tab_without_data() {
        // The first frames arrive before any snapshot does.
        let mut app = AppState::new();
        for &tab in Tab::ALL {
            app.tab = tab;
            for (w, h) in [(1, 1), (40, 10), (80, 24), (200, 50)] {
                let _ = render_with(&app, w, h);
            }
        }
    }

    #[test]
    fn root_renders_under_every_capability_profile() {
        // Degradation is the feature: a Linux console over serial and a kitty
        // window must both produce a usable frame.
        for &tab in Tab::ALL {
            for (color, unicode) in all_profiles() {
                let mut app = app_with_data();
                app.tab = tab;
                let _ = render_caps(&mut app, 80, 24, color, unicode);
            }
        }
    }

    #[test]
    fn ascii_profile_emits_no_multibyte_characters() {
        // A terminal we decided is ASCII-only must never receive UTF-8, or it
        // paints tofu where the table should be.
        for &tab in Tab::ALL {
            let mut app = app_with_data();
            app.tab = tab;
            let buf = render_caps(&mut app, 100, 30, ColorSupport::Basic, false);
            let text = all_text(&buf);
            assert!(
                text.is_ascii(),
                "non-ASCII output on {tab:?} in ASCII mode:\n{text}"
            );
        }
    }

    #[test]
    fn ascii_profile_stays_ascii_in_every_overlay() {
        for overlay in [
            Overlay::Palette,
            Overlay::Help,
            Overlay::Log,
            Overlay::Actions,
            Overlay::Inspector,
        ] {
            let mut app = app_with_data();
            app.tab = Tab::Processes;
            app.overlay = overlay;
            let buf = render_caps(&mut app, 100, 30, ColorSupport::Basic, false);
            let text = all_text(&buf);
            assert!(text.is_ascii(), "non-ASCII in {overlay:?}:\n{text}");
        }
    }

    // -- Header --

    #[test]
    fn header_shows_identity_and_version() {
        let app = app_with_data();
        let buf = render_with(&app, 100, 24);
        let header = line_text(&buf, 0);
        assert!(header.contains("muxtop"));
        assert!(header.contains(&format!("v{}", env!("CARGO_PKG_VERSION"))));
    }

    #[test]
    fn header_shows_the_paused_state() {
        let mut app = app_with_data();
        assert!(!contains(&render_with(&app, 100, 24), "PAUSED"));
        app.paused = true;
        assert!(
            contains(&render_with(&app, 100, 24), "PAUSED"),
            "a frozen view must say so"
        );
    }

    #[test]
    fn header_marks_a_remote_connection() {
        let mut app = app_with_data();
        app.connection_mode = crate::ConnectionMode::Remote {
            hostname: "prod-01".to_string(),
            addr: "10.0.0.1:4242".parse().unwrap(),
        };
        let buf = render_with(&app, 120, 24);
        assert!(contains(&buf, "prod-01"));
        assert!(
            contains(&buf, "read-only"),
            "remote mode must announce that actions are unavailable"
        );
    }

    /// SEC-02: the hostname arrives in the server's `Welcome` frame, and the
    /// chrome that renders it stays on screen for the whole session — so it
    /// needs the same guard the table cells got in v0.3.1.
    #[test]
    fn header_scrubs_a_hostile_remote_hostname() {
        let mut app = app_with_data();
        app.connection_mode = crate::ConnectionMode::Remote {
            hostname: "prod\x1b]0;pwned\x07-01".to_string(),
            addr: "10.0.0.1:4242".parse().unwrap(),
        };
        let text = all_text(&render_with(&app, 120, 24));

        assert!(
            !text.contains('\x1b'),
            "ESC survived into the chrome:\n{text}"
        );
        assert!(
            !text.contains('\x07'),
            "BEL survived into the chrome:\n{text}"
        );
        assert!(
            text.contains("prod?"),
            "the printable part of the hostname should still render:\n{text}"
        );
    }

    // -- Tab bar --

    #[test]
    fn tabbar_lists_every_tab() {
        let app = app_with_data();
        let buf = render_with(&app, 120, 24);
        let row = line_text(&buf, 1);
        for &tab in Tab::ALL {
            assert!(row.contains(tab.label()), "missing tab {tab:?} in `{row}`");
        }
    }

    #[test]
    fn tabbar_shows_live_counts() {
        let app = app_with_data();
        let buf = render_with(&app, 120, 24);
        let row = line_text(&buf, 1);
        // 40 processes and 2 interfaces are in the fixture.
        assert!(row.contains("40"), "process count missing from `{row}`");
    }

    #[test]
    fn tabbar_has_no_placeholder_tabs() {
        // 0.4 advertised a hardcoded `GPU [soon]` entry that did nothing.
        let app = app_with_data();
        let buf = render_with(&app, 120, 24);
        assert!(!contains(&buf, "[soon]"));
    }

    // -- Status bar --

    #[test]
    fn statusbar_shows_sort_state() {
        let mut app = app_with_data();
        app.tab = Tab::Processes;
        let buf = render_with(&app, 120, 24);
        let status = line_text(&buf, 23);
        assert!(
            status.contains("sort cpu"),
            "the active sort must be visible: `{status}`"
        );
    }

    #[test]
    fn statusbar_shows_filter_and_match_count() {
        let mut app = app_with_data();
        app.tab = Tab::Processes;
        app.set_filter("proc1");
        let buf = render_with(&app, 120, 24);
        let status = line_text(&buf, 23);
        assert!(
            status.contains("filter \"proc1\""),
            "an active filter must be visible: `{status}`"
        );
    }

    #[test]
    fn statusbar_shows_position() {
        let mut app = app_with_data();
        app.tab = Tab::Processes;
        app.selected = 6;
        let buf = render_with(&app, 120, 24);
        assert!(
            line_text(&buf, 23).contains("7/40"),
            "cursor position must be visible"
        );
    }

    #[test]
    fn statusbar_shows_a_toast_with_its_severity() {
        let mut app = app_with_data();
        app.notify(theme::Level::Error, "Kill failed: permission denied");
        let buf = render_with(&app, 120, 24);
        assert!(contains(&buf, "Kill failed"));
    }

    #[test]
    fn statusbar_hides_local_only_hints_in_remote_mode() {
        let mut app = app_with_data();
        app.tab = Tab::Processes;
        app.connection_mode = crate::ConnectionMode::Remote {
            hostname: "prod".to_string(),
            addr: "10.0.0.1:4242".parse().unwrap(),
        };
        let status = line_text(&render_with(&app, 200, 24), 23);
        assert!(
            !status.contains("SIGTERM"),
            "must not offer a kill we cannot perform: `{status}`"
        );
    }

    #[test]
    fn statusbar_never_overflows_its_row() {
        // The 0.4 footer was a fixed list that silently ran off an 80-column
        // terminal; segments now stop at the edge.
        for width in [40u16, 60, 80, 100, 200] {
            let mut app = app_with_data();
            app.tab = Tab::Containers;
            let buf = render_with(&app, width, 24);
            let status = line_text(&buf, 23);
            assert!(
                status.chars().count() <= width as usize,
                "status bar overflowed at width {width}"
            );
        }
    }

    // -- Content dispatch --

    #[test]
    fn content_differs_between_tabs() {
        let mut app = app_with_data();
        app.tab = Tab::General;
        let general = all_text(&render_with(&app, 100, 30));
        app.tab = Tab::Processes;
        let processes = all_text(&render_with(&app, 100, 30));
        assert_ne!(general, processes);
    }

    #[test]
    fn compact_terminals_get_one_row_of_chrome() {
        let app = app_with_data();
        let buf = render_with(&app, 50, 20);
        let first = line_text(&buf, 0);
        assert!(first.contains("muxtop"));
        // The active tab is named on the same row as the brand.
        assert!(first.contains("General"));
    }
}
