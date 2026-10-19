use std::io::{self, Stdout};

use crossterm::{
    cursor,
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

use crate::error::TuiError;

// ---------------------------------------------------------------------------
// Terminal capability detection
// ---------------------------------------------------------------------------

/// Detected color support level.
///
/// muxtop runs everywhere from a kitty window with 24-bit color down to a bare
/// Linux virtual console over a serial link, so every level here is a level we
/// actually render for — see [`crate::ui::theme::Theme`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorSupport {
    /// No color at all. Hierarchy is carried by bold / dim / reverse only.
    /// Selected for `TERM=dumb`, an unset `TERM`, or a set `NO_COLOR`.
    NoColor,
    /// Basic 16-color ANSI (`TERM=linux`, `TERM=xterm`, serial consoles).
    Basic,
    /// 256-color mode (`TERM` contains `256color`, or a known 256-color term).
    Colors256,
    /// True color (24-bit) — `$COLORTERM` is `truecolor` / `24bit`, or the
    /// terminal is known to support it.
    TrueColor,
}

/// How wide the terminal is, in layout terms. Views branch on this instead of
/// comparing raw column counts in a dozen places.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Breakpoint {
    /// < 60 columns — phone-sized SSH client, split tmux pane.
    Xs,
    /// 60–99 columns — the classic 80×24.
    Sm,
    /// 100–139 columns.
    Md,
    /// >= 140 columns.
    Lg,
}

impl Breakpoint {
    /// Classify a width. Public because the renderer classifies the frame it
    /// is painting, which is more current than the last resize event.
    pub fn from_width(width: u16) -> Self {
        match width {
            0..=59 => Breakpoint::Xs,
            60..=99 => Breakpoint::Sm,
            100..=139 => Breakpoint::Md,
            _ => Breakpoint::Lg,
        }
    }

    /// Whether this breakpoint has room for a side-by-side inspector panel.
    /// Narrower terminals get a full-screen overlay instead.
    pub fn allows_side_panel(self) -> bool {
        self >= Breakpoint::Md
    }
}

/// User-facing overrides for capability detection, sourced from the CLI.
///
/// Detection is deliberately conservative, but a user who knows their terminal
/// (or is piping through something exotic) must always be able to force the
/// answer — that is what these are for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CapsOverrides {
    /// `--no-color`: force [`ColorSupport::NoColor`].
    pub no_color: bool,
    /// `--ascii`: force the ASCII glyph set even on a UTF-8 terminal.
    pub ascii: bool,
    /// `--no-mouse`: never enable mouse capture. Mouse capture takes the
    /// terminal's native text selection away, which some users would rather
    /// keep — and it is useless on a console that has no pointer at all.
    pub no_mouse: bool,
}

/// Terminal capabilities detected at startup.
#[derive(Debug, Clone)]
pub struct TermCaps {
    /// Color support level.
    pub color_support: ColorSupport,
    /// Whether the terminal can render the Unicode glyph set.
    pub unicode: bool,
    /// Whether mouse reporting should be enabled. Always additive: every
    /// mouse gesture in muxtop has a keyboard equivalent, so `false` here
    /// costs the user nothing but a convenience.
    pub mouse: bool,
    /// Terminal width at detection time.
    pub width: u16,
    /// Terminal height at detection time.
    pub height: u16,
}

impl Default for TermCaps {
    fn default() -> Self {
        Self {
            color_support: ColorSupport::TrueColor,
            unicode: true,
            mouse: true,
            width: 80,
            height: 24,
        }
    }
}

impl TermCaps {
    /// Whether the terminal is considered "small" (< 80 columns or < 24 rows).
    pub fn is_small(&self) -> bool {
        self.width < 80 || self.height < 24
    }

    /// Layout class for the current width.
    pub fn breakpoint(&self) -> Breakpoint {
        Breakpoint::from_width(self.width)
    }

    /// Record a resize so subsequent frames pick the right breakpoint.
    pub fn set_size(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
    }
}

/// Detect terminal capabilities from environment variables and terminal size.
pub fn detect_terminal_caps() -> TermCaps {
    detect_terminal_caps_with(CapsOverrides::default())
}

/// Detect terminal capabilities, applying CLI overrides.
pub fn detect_terminal_caps_with(overrides: CapsOverrides) -> TermCaps {
    let env = |k: &str| std::env::var(k).unwrap_or_default();

    let term = env("TERM");
    let colorterm = env("COLORTERM");
    let term_program = env("TERM_PROGRAM");
    // NO_COLOR is honoured when set to any non-empty value, per
    // https://no-color.org — the value itself carries no meaning.
    let no_color_env = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
    // The locale is what actually decides whether the terminal decodes our
    // multi-byte glyphs. LC_ALL wins over LC_CTYPE, which wins over LANG.
    let locale = [env("LC_ALL"), env("LC_CTYPE"), env("LANG")]
        .into_iter()
        .find(|v| !v.is_empty())
        .unwrap_or_default();

    let color_support = if overrides.no_color || no_color_env {
        ColorSupport::NoColor
    } else {
        detect_color_support(&term, &colorterm, &term_program)
    };

    let unicode = !overrides.ascii && detect_unicode(&term, &locale);
    let mouse = !overrides.no_mouse && detect_mouse(&term);

    let (width, height) = crossterm::terminal::size().unwrap_or((80, 24));

    TermCaps {
        color_support,
        unicode,
        mouse,
        width,
        height,
    }
}

/// Terminals known to speak 24-bit color even when they forget to advertise
/// `$COLORTERM` (notably when reached over `ssh` or through `sudo -i`, which
/// commonly strip it while keeping `$TERM` or `$TERM_PROGRAM`).
const TRUECOLOR_TERMS: &[&str] = &[
    "alacritty",
    "contour",
    "foot",
    "ghostty",
    "kitty",
    "rio",
    "wezterm",
];

/// `$TERM_PROGRAM` values known to speak 24-bit color. macOS terminals in
/// particular set this and nothing else.
const TRUECOLOR_PROGRAMS: &[&str] = &["ghostty", "iterm.app", "wezterm", "vscode", "hyper"];

fn detect_color_support(term: &str, colorterm: &str, term_program: &str) -> ColorSupport {
    let term = term.to_lowercase();
    let colorterm = colorterm.to_lowercase();
    let term_program = term_program.to_lowercase();

    // A dumb or absent terminal gets no color escapes at all.
    if term == "dumb" || term.is_empty() {
        return ColorSupport::NoColor;
    }

    if colorterm == "truecolor" || colorterm == "24bit" {
        return ColorSupport::TrueColor;
    }
    if TRUECOLOR_TERMS.iter().any(|t| term.contains(t)) {
        return ColorSupport::TrueColor;
    }
    if TRUECOLOR_PROGRAMS.contains(&term_program.as_str()) {
        return ColorSupport::TrueColor;
    }

    if term.contains("256color") || term.contains("direct") {
        return ColorSupport::Colors256;
    }
    // Apple's Terminal.app reports `xterm-256color` (caught above); the bare
    // `nsterm` entries do not, but they are 256-capable.
    if term.starts_with("nsterm") {
        return ColorSupport::Colors256;
    }

    // `TERM=linux` (the kernel virtual console), `xterm`, `vt220`, `screen`
    // without a 256 suffix, serial consoles: 16 colors is the honest answer.
    ColorSupport::Basic
}

fn detect_unicode(term: &str, locale: &str) -> bool {
    let term = term.to_lowercase();

    if term == "dumb" || term.is_empty() {
        return false;
    }

    // The Linux kernel virtual console (`TERM=linux`) renders through a 256- or
    // 512-glyph font (Lat15/Lat2 by default) that has no braille, no block
    // fractions and no rounded box drawing. It will happily accept our UTF-8
    // and paint tofu. ASCII is the correct answer there — this is the exact
    // case of somebody at the physical console of an Ubuntu box, or on a
    // provider's KVM / serial-over-LAN console.
    if term == "linux" || term.starts_with("vt1") || term.starts_with("vt2") {
        return false;
    }

    let locale = locale.to_lowercase();
    if locale.contains("utf-8") || locale.contains("utf8") {
        return true;
    }

    // No UTF-8 in the locale. On Windows the console is switched to UTF-8 by
    // the runtime rather than by environment variables, and macOS terminals are
    // UTF-8 without ever setting LANG, so believing the (absent) locale there
    // would needlessly downgrade every default install.
    if cfg!(windows) || cfg!(target_os = "macos") {
        return true;
    }

    // On Linux/BSD an empty or POSIX/C locale genuinely means single-byte.
    // An `ssh` session with no locale forwarding lands here, and ASCII is what
    // actually renders correctly.
    false
}

fn detect_mouse(term: &str) -> bool {
    let term = term.to_lowercase();
    // No pointer to report from, or no protocol to report it with.
    !(term.is_empty() || term == "dumb" || term == "linux" || term.starts_with("vt"))
}

/// Type alias for our terminal backend.
pub type Tui = Terminal<CrosstermBackend<Stdout>>;

/// RAII guard that restores the terminal on drop (normal exit, error, or unwind).
pub struct TerminalGuard {
    pub terminal: Tui,
    mouse_enabled: bool,
}

impl TerminalGuard {
    /// Access the underlying terminal.
    pub fn terminal_mut(&mut self) -> &mut Tui {
        &mut self.terminal
    }

    /// Whether mouse capture was actually turned on.
    pub fn mouse_enabled(&self) -> bool {
        self.mouse_enabled
    }

    /// Give the shell the normal screen while keeping raw input (Ctrl+C must
    /// reach its PTY, not signal muxtop). Resume even when the session fails.
    #[cfg(unix)]
    pub(crate) fn with_shell<R>(&mut self, session: impl FnOnce() -> R) -> Result<R, TuiError> {
        use crossterm::event::{DisableBracketedPaste, DisableFocusChange};
        use crossterm::style::{Attribute, ResetColor, SetAttribute};
        discard_pending_input()?;
        execute!(
            self.terminal.backend_mut(),
            DisableMouseCapture,
            LeaveAlternateScreen,
            cursor::Show
        )?;
        let result = session();
        discard_pending_input()?;
        // The shell may have changed emulator modes or entered its own alternate
        // screen. Reset those modes before ratatui redraws the complete frame.
        enable_raw_mode()?;
        execute!(
            self.terminal.backend_mut(),
            DisableMouseCapture,
            DisableBracketedPaste,
            DisableFocusChange,
            ResetColor,
            SetAttribute(Attribute::Reset),
            LeaveAlternateScreen,
            EnterAlternateScreen,
            cursor::Hide
        )?;
        if self.mouse_enabled {
            execute!(self.terminal.backend_mut(), EnableMouseCapture)?;
        }
        self.terminal.autoresize()?;
        self.terminal.clear()?;
        Ok(result)
    }
}

/// A key read can prefetch a burst into crossterm's parser. At a terminal-owner
/// boundary, discard complete queued events as well as kernel typeahead: shell
/// input must never be replayed as TUI actions. Users wait for the shell prompt.
#[cfg(unix)]
fn discard_pending_input() -> Result<(), TuiError> {
    use std::os::fd::AsRawFd;
    let tty = std::fs::OpenOptions::new().read(true).open("/dev/tty")?;
    // SAFETY: tty owns a live terminal fd; TCIFLUSH only discards pending input.
    if unsafe { libc::tcflush(tty.as_raw_fd(), libc::TCIFLUSH) } != 0 {
        return Err(TuiError::Terminal(io::Error::last_os_error()));
    }
    const MAX_QUEUED_EVENTS: usize = 4096;
    for _ in 0..MAX_QUEUED_EVENTS {
        if !crossterm::event::poll(std::time::Duration::ZERO)? {
            return Ok(());
        }
        let _ = crossterm::event::read()?;
    }
    Err(TuiError::Terminal(io::Error::other(
        "terminal input did not settle during shell handoff",
    )))
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = restore_terminal(&mut self.terminal, self.mouse_enabled);
    }
}

/// Initialize the terminal: raw mode, alternate screen, and mouse capture only
/// when the terminal actually has a pointer and the user has not opted out.
///
/// On partial failure, cleans up any state already set.
pub fn init_terminal(mouse: bool) -> Result<TerminalGuard, TuiError> {
    enable_raw_mode()?;

    let mut stdout = io::stdout();
    if let Err(e) = execute!(stdout, EnterAlternateScreen) {
        let _ = disable_raw_mode();
        return Err(TuiError::Terminal(e));
    }
    // Mouse capture is best-effort: a terminal that refuses it must not stop
    // muxtop from starting, because nothing in the UI depends on it.
    let mouse_enabled = mouse && execute!(stdout, EnableMouseCapture).is_ok();

    let backend = CrosstermBackend::new(stdout);
    match Terminal::new(backend) {
        Ok(terminal) => Ok(TerminalGuard {
            terminal,
            mouse_enabled,
        }),
        Err(e) => {
            let _ = restore_stdout(mouse_enabled);
            let _ = disable_raw_mode();
            Err(TuiError::Terminal(e))
        }
    }
}

/// Restore the terminal to its original state.
pub fn restore_terminal(terminal: &mut Tui, mouse_enabled: bool) -> Result<(), TuiError> {
    disable_raw_mode()?;
    if mouse_enabled {
        execute!(terminal.backend_mut(), DisableMouseCapture)?;
    }
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

fn restore_stdout(mouse_enabled: bool) -> io::Result<()> {
    let mut stdout = io::stdout();
    if mouse_enabled {
        let _ = execute!(stdout, DisableMouseCapture);
    }
    execute!(stdout, LeaveAlternateScreen)
}

/// Install a panic hook that restores the terminal before printing the panic message.
/// Must be called BEFORE init_terminal().
pub fn install_panic_hook() {
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = disable_raw_mode();
        // Unconditionally disabling mouse capture is harmless when it was
        // never enabled, and leaving it on would wreck the user's shell.
        let _ = execute!(
            io::stdout(),
            DisableMouseCapture,
            LeaveAlternateScreen,
            cursor::Show
        );
        original_hook(panic_info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_panic_hook_install() {
        install_panic_hook();
    }

    #[test]
    fn test_restore_is_idempotent() {
        let _ = disable_raw_mode();
    }

    #[test]
    fn test_terminal_guard_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<TerminalGuard>();
    }

    // ---- Color support detection ----

    #[test]
    fn test_detect_truecolor() {
        assert_eq!(
            detect_color_support("xterm-256color", "truecolor", ""),
            ColorSupport::TrueColor
        );
    }

    #[test]
    fn test_detect_truecolor_24bit() {
        assert_eq!(
            detect_color_support("xterm", "24bit", ""),
            ColorSupport::TrueColor
        );
    }

    #[test]
    fn test_detect_truecolor_known_term() {
        assert_eq!(
            detect_color_support("xterm-kitty", "", ""),
            ColorSupport::TrueColor
        );
        assert_eq!(
            detect_color_support("alacritty", "", ""),
            ColorSupport::TrueColor
        );
    }

    #[test]
    fn test_detect_truecolor_known_program() {
        // iTerm2 over ssh commonly loses COLORTERM but keeps TERM_PROGRAM.
        assert_eq!(
            detect_color_support("xterm", "", "iTerm.app"),
            ColorSupport::TrueColor
        );
    }

    #[test]
    fn test_detect_256color() {
        assert_eq!(
            detect_color_support("xterm-256color", "", ""),
            ColorSupport::Colors256
        );
        assert_eq!(
            detect_color_support("screen-256color", "", ""),
            ColorSupport::Colors256
        );
        assert_eq!(
            detect_color_support("tmux-256color", "", ""),
            ColorSupport::Colors256
        );
    }

    #[test]
    fn test_detect_basic_color() {
        assert_eq!(detect_color_support("xterm", "", ""), ColorSupport::Basic);
        // The Linux virtual console: 16 colors, no more.
        assert_eq!(detect_color_support("linux", "", ""), ColorSupport::Basic);
        assert_eq!(detect_color_support("vt220", "", ""), ColorSupport::Basic);
    }

    #[test]
    fn test_detect_no_color_dumb() {
        assert_eq!(detect_color_support("dumb", "", ""), ColorSupport::NoColor);
    }

    #[test]
    fn test_detect_no_color_empty() {
        assert_eq!(detect_color_support("", "", ""), ColorSupport::NoColor);
    }

    #[test]
    fn test_no_color_override_wins_over_truecolor() {
        let caps = detect_terminal_caps_with(CapsOverrides {
            no_color: true,
            ..Default::default()
        });
        assert_eq!(caps.color_support, ColorSupport::NoColor);
    }

    // ---- Unicode detection ----

    #[test]
    fn test_detect_unicode_utf8_locale() {
        assert!(detect_unicode("xterm-256color", "en_US.UTF-8"));
        assert!(detect_unicode("xterm-256color", "fr_FR.utf8"));
    }

    #[test]
    fn test_detect_unicode_dumb_term() {
        assert!(!detect_unicode("dumb", "en_US.UTF-8"));
    }

    #[test]
    fn test_detect_unicode_linux_console_is_ascii() {
        // The kernel console font has no braille and no block fractions, so a
        // UTF-8 locale is not enough to make our glyphs render.
        assert!(!detect_unicode("linux", "en_US.UTF-8"));
        assert!(!detect_unicode("vt220", "en_US.UTF-8"));
    }

    #[test]
    #[cfg(not(any(windows, target_os = "macos")))]
    fn test_detect_unicode_posix_locale_is_ascii_on_unix() {
        assert!(!detect_unicode("xterm-256color", "C"));
        assert!(!detect_unicode("xterm-256color", ""));
    }

    #[test]
    #[cfg(any(windows, target_os = "macos"))]
    fn test_detect_unicode_assumed_on_windows_and_macos() {
        // Neither platform expresses its UTF-8-ness through the locale.
        assert!(detect_unicode("xterm-256color", ""));
    }

    #[test]
    fn test_ascii_override_wins() {
        let caps = detect_terminal_caps_with(CapsOverrides {
            ascii: true,
            ..Default::default()
        });
        assert!(!caps.unicode);
    }

    // ---- Mouse detection ----

    #[test]
    fn test_mouse_disabled_on_console_and_dumb() {
        assert!(!detect_mouse("linux"));
        assert!(!detect_mouse("dumb"));
        assert!(!detect_mouse("vt100"));
        assert!(!detect_mouse(""));
    }

    #[test]
    fn test_mouse_enabled_on_normal_terminals() {
        assert!(detect_mouse("xterm-256color"));
        assert!(detect_mouse("tmux-256color"));
    }

    #[test]
    fn test_no_mouse_override_wins() {
        let caps = detect_terminal_caps_with(CapsOverrides {
            no_mouse: true,
            ..Default::default()
        });
        assert!(!caps.mouse);
    }

    // ---- TermCaps ----

    #[test]
    fn test_term_caps_default() {
        let caps = TermCaps::default();
        assert_eq!(caps.color_support, ColorSupport::TrueColor);
        assert!(caps.unicode);
        assert!(caps.mouse);
        assert_eq!(caps.width, 80);
        assert_eq!(caps.height, 24);
    }

    #[test]
    fn test_term_caps_is_small() {
        let mut caps = TermCaps::default();
        assert!(!caps.is_small());
        caps.width = 79;
        assert!(caps.is_small());
        caps.width = 80;
        caps.height = 23;
        assert!(caps.is_small());
    }

    #[test]
    fn test_breakpoints() {
        assert_eq!(Breakpoint::from_width(40), Breakpoint::Xs);
        assert_eq!(Breakpoint::from_width(59), Breakpoint::Xs);
        assert_eq!(Breakpoint::from_width(60), Breakpoint::Sm);
        assert_eq!(Breakpoint::from_width(80), Breakpoint::Sm);
        assert_eq!(Breakpoint::from_width(100), Breakpoint::Md);
        assert_eq!(Breakpoint::from_width(139), Breakpoint::Md);
        assert_eq!(Breakpoint::from_width(140), Breakpoint::Lg);
        assert_eq!(Breakpoint::from_width(400), Breakpoint::Lg);
    }

    #[test]
    fn test_breakpoint_side_panel() {
        assert!(!Breakpoint::Xs.allows_side_panel());
        assert!(!Breakpoint::Sm.allows_side_panel());
        assert!(Breakpoint::Md.allows_side_panel());
        assert!(Breakpoint::Lg.allows_side_panel());
    }

    #[test]
    fn test_set_size_updates_breakpoint() {
        let mut caps = TermCaps::default();
        assert_eq!(caps.breakpoint(), Breakpoint::Sm);
        caps.set_size(160, 50);
        assert_eq!(caps.breakpoint(), Breakpoint::Lg);
    }
}
