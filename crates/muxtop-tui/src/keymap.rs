// Keymap — the single source of truth for what every key does.
//
// muxtop 0.4 spread its bindings across a 300-line `match` in the event
// handler, a hand-written footer, a hand-written README table and a palette
// registry. They drifted: the README documented `+`/`-` for renice that no
// handler implemented, and half the Kube keys existed nowhere but the README.
//
// Here, one table drives dispatch, the help screen, the which-key panel and the
// footer hints. A binding that is not in this table does not exist, and one
// that is in it is automatically documented.

use crossterm::event::{KeyCode, KeyModifiers};

use crate::app::{GpuSubview, KubeSubview, Tab};
use muxtop_core::actions::Signal;

/// Everything the user can ask muxtop to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    // -- application --
    Quit,
    Help,
    Palette,
    CommandMode,
    TogglePause,
    Refresh,
    ToggleLog,

    // -- navigation --
    NextTab,
    PrevTab,
    GoTab(Tab),
    NextSubview,
    PrevSubview,

    // -- table --
    RowDown,
    RowUp,
    RowPageDown,
    RowPageUp,
    RowHalfDown,
    RowHalfUp,
    RowFirst,
    RowLast,
    ColLeft,
    ColRight,
    OpenInspector,
    ActionsMenu,
    CopySelection,

    // -- sort & filter --
    OpenFilter,
    Back,
    CycleSort,
    ReverseSort,

    // -- processes --
    ToggleTree,
    Renice(i32),
    Kill(Signal),

    // -- containers --
    ContainerStop,
    ContainerKill,
    ContainerRestart,
    ContainerExec,

    // -- kubernetes --
    KubeSubview(KubeSubview),
    ToggleKubeScope,

    // -- gpu --
    GpuSubview(GpuSubview),
}

impl Action {
    /// Whether the action mutates something outside muxtop and therefore
    /// cannot work against a remote host, where the server holds the sockets.
    pub fn is_local_only(self) -> bool {
        matches!(
            self,
            Action::Renice(_)
                | Action::Kill(_)
                | Action::ContainerStop
                | Action::ContainerKill
                | Action::ContainerRestart
                | Action::ContainerExec
                | Action::ToggleKubeScope
        )
    }
}

/// Where a binding applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Active on every tab.
    Global,
    /// Active on one tab only. A tab-scoped key does nothing elsewhere rather
    /// than silently acting on an invisible view — muxtop 0.4 let `t` and
    /// `F1`–`F5` re-sort the process table while the user watched the Network
    /// tab.
    Tab(Tab),
}

impl Scope {
    fn matches(self, tab: Tab) -> bool {
        match self {
            Scope::Global => true,
            Scope::Tab(t) => t == tab,
        }
    }
}

/// Help-screen grouping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Group {
    Navigation,
    Table,
    SortFilter,
    Actions,
    App,
}

impl Group {
    pub const ALL: &'static [Group] = &[
        Group::Navigation,
        Group::Table,
        Group::SortFilter,
        Group::Actions,
        Group::App,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Group::Navigation => "NAVIGATION",
            Group::Table => "TABLE",
            Group::SortFilter => "SORT & FILTER",
            Group::Actions => "ACTIONS",
            Group::App => "APPLICATION",
        }
    }
}

/// One key bound to one action.
#[derive(Debug, Clone, Copy)]
pub struct Binding {
    pub code: KeyCode,
    pub mods: KeyModifiers,
    pub action: Action,
    pub scope: Scope,
    pub group: Group,
    /// How the key is written for humans (`Ctrl+P`, `F9`, `↑`).
    pub display: &'static str,
    /// What the action does, in the imperative.
    pub label: &'static str,
    /// Whether this binding is worth offering in the footer's short hint list.
    pub hint: bool,
    /// Whether this is the canonical display for the action. Aliases (`↑` for
    /// `k`) set this to `false` so the help screen shows one row per action
    /// while still listing every key that triggers it.
    pub primary: bool,
}

const NONE: KeyModifiers = KeyModifiers::NONE;
const CTRL: KeyModifiers = KeyModifiers::CONTROL;
const ALT: KeyModifiers = KeyModifiers::ALT;

macro_rules! b {
    ($code:expr, $mods:expr, $action:expr, $scope:expr, $group:expr, $display:literal, $label:literal, $hint:expr, $primary:expr) => {
        Binding {
            code: $code,
            mods: $mods,
            action: $action,
            scope: $scope,
            group: $group,
            display: $display,
            label: $label,
            hint: $hint,
            primary: $primary,
        }
    };
}

/// The complete keymap.
///
/// Order matters: the first binding whose key and scope match wins, so
/// tab-scoped entries are listed before the global ones they shadow.
pub const BINDINGS: &[Binding] = &[
    // ---------------- tab-scoped: Processes ----------------
    b!(
        KeyCode::Char('t'),
        NONE,
        Action::ToggleTree,
        Scope::Tab(Tab::Processes),
        Group::Table,
        "t",
        "Tree view",
        true,
        true
    ),
    b!(
        KeyCode::F(5),
        NONE,
        Action::ToggleTree,
        Scope::Tab(Tab::Processes),
        Group::Table,
        "F5",
        "Tree view",
        false,
        false
    ),
    b!(
        KeyCode::F(7),
        NONE,
        Action::Renice(1),
        Scope::Tab(Tab::Processes),
        Group::Actions,
        "F7",
        "Lower priority (nice +1)",
        false,
        true
    ),
    b!(
        KeyCode::Char('-'),
        NONE,
        Action::Renice(1),
        Scope::Tab(Tab::Processes),
        Group::Actions,
        "-",
        "Lower priority (nice +1)",
        false,
        false
    ),
    b!(
        KeyCode::F(8),
        NONE,
        Action::Renice(-1),
        Scope::Tab(Tab::Processes),
        Group::Actions,
        "F8",
        "Raise priority (nice -1)",
        false,
        true
    ),
    b!(
        KeyCode::Char('+'),
        NONE,
        Action::Renice(-1),
        Scope::Tab(Tab::Processes),
        Group::Actions,
        "+",
        "Raise priority (nice -1)",
        false,
        false
    ),
    b!(
        KeyCode::F(9),
        NONE,
        Action::Kill(Signal::Term),
        Scope::Tab(Tab::Processes),
        Group::Actions,
        "F9",
        "Kill process (SIGTERM)",
        true,
        true
    ),
    b!(
        KeyCode::F(10),
        NONE,
        Action::Kill(Signal::Kill),
        Scope::Tab(Tab::Processes),
        Group::Actions,
        "F10",
        "Force kill (SIGKILL)",
        false,
        true
    ),
    // ---------------- tab-scoped: Containers ----------------
    b!(
        KeyCode::F(9),
        NONE,
        Action::ContainerStop,
        Scope::Tab(Tab::Containers),
        Group::Actions,
        "F9",
        "Stop container",
        true,
        true
    ),
    b!(
        KeyCode::F(10),
        NONE,
        Action::ContainerKill,
        Scope::Tab(Tab::Containers),
        Group::Actions,
        "F10",
        "Kill container",
        false,
        true
    ),
    b!(
        KeyCode::F(11),
        NONE,
        Action::ContainerRestart,
        Scope::Tab(Tab::Containers),
        Group::Actions,
        "F11",
        "Restart container",
        false,
        true
    ),
    b!(
        KeyCode::Char('e'),
        NONE,
        Action::ContainerExec,
        Scope::Tab(Tab::Containers),
        Group::Actions,
        "e",
        "Open container shell (PTY)",
        true,
        true
    ),
    // ---------------- tab-scoped: Kubernetes ----------------
    b!(
        KeyCode::Char('P'),
        NONE,
        Action::KubeSubview(KubeSubview::Pods),
        Scope::Tab(Tab::Kube),
        Group::Navigation,
        "P",
        "Pods sub-view",
        true,
        true
    ),
    b!(
        KeyCode::Char('N'),
        NONE,
        Action::KubeSubview(KubeSubview::Nodes),
        Scope::Tab(Tab::Kube),
        Group::Navigation,
        "N",
        "Nodes sub-view",
        false,
        true
    ),
    b!(
        KeyCode::Char('D'),
        NONE,
        Action::KubeSubview(KubeSubview::Deployments),
        Scope::Tab(Tab::Kube),
        Group::Navigation,
        "D",
        "Deployments sub-view",
        false,
        true
    ),
    b!(
        KeyCode::Char('A'),
        NONE,
        Action::ToggleKubeScope,
        Scope::Tab(Tab::Kube),
        Group::Actions,
        "A",
        "Toggle namespace scope",
        true,
        true
    ),
    // ---------------- tab-scoped: GPU ----------------
    b!(
        KeyCode::Char('D'),
        NONE,
        Action::GpuSubview(GpuSubview::Devices),
        Scope::Tab(Tab::Gpu),
        Group::Navigation,
        "D",
        "Devices sub-view",
        true,
        true
    ),
    b!(
        KeyCode::Char('P'),
        NONE,
        Action::GpuSubview(GpuSubview::Procs),
        Scope::Tab(Tab::Gpu),
        Group::Navigation,
        "P",
        "Processes sub-view",
        false,
        true
    ),
    // ---------------- global: application ----------------
    b!(
        KeyCode::Char('q'),
        NONE,
        Action::Quit,
        Scope::Global,
        Group::App,
        "q",
        "Quit",
        true,
        true
    ),
    b!(
        KeyCode::Char('c'),
        CTRL,
        Action::Quit,
        Scope::Global,
        Group::App,
        "Ctrl+C",
        "Quit",
        false,
        false
    ),
    b!(
        KeyCode::Char('?'),
        NONE,
        Action::Help,
        Scope::Global,
        Group::App,
        "?",
        "Help",
        true,
        true
    ),
    b!(
        KeyCode::F(1),
        NONE,
        Action::Help,
        Scope::Global,
        Group::App,
        "F1",
        "Help",
        false,
        false
    ),
    b!(
        KeyCode::Char('p'),
        CTRL,
        Action::Palette,
        Scope::Global,
        Group::App,
        "Ctrl+P",
        "Command palette",
        true,
        true
    ),
    b!(
        KeyCode::Char('k'),
        CTRL,
        Action::Palette,
        Scope::Global,
        Group::App,
        "Ctrl+K",
        "Command palette",
        false,
        false
    ),
    b!(
        KeyCode::Char(':'),
        NONE,
        Action::CommandMode,
        Scope::Global,
        Group::App,
        ":",
        "Command mode",
        false,
        true
    ),
    b!(
        KeyCode::Char(' '),
        NONE,
        Action::TogglePause,
        Scope::Global,
        Group::App,
        "Space",
        "Pause / resume refresh",
        true,
        true
    ),
    b!(
        KeyCode::Char('r'),
        NONE,
        Action::Refresh,
        Scope::Global,
        Group::App,
        "r",
        "Refresh now",
        false,
        true
    ),
    b!(
        KeyCode::Char('l'),
        CTRL,
        Action::ToggleLog,
        Scope::Global,
        Group::App,
        "Ctrl+L",
        "Message log",
        false,
        true
    ),
    // ---------------- global: navigation ----------------
    b!(
        KeyCode::Tab,
        NONE,
        Action::NextTab,
        Scope::Global,
        Group::Navigation,
        "Tab",
        "Next tab",
        true,
        true
    ),
    b!(
        KeyCode::BackTab,
        NONE,
        Action::PrevTab,
        Scope::Global,
        Group::Navigation,
        "Shift+Tab",
        "Previous tab",
        false,
        true
    ),
    b!(
        KeyCode::Char('1'),
        ALT,
        Action::GoTab(Tab::General),
        Scope::Global,
        Group::Navigation,
        "Alt+1",
        "General tab",
        false,
        true
    ),
    b!(
        KeyCode::Char('2'),
        ALT,
        Action::GoTab(Tab::Processes),
        Scope::Global,
        Group::Navigation,
        "Alt+2",
        "Processes tab",
        false,
        true
    ),
    b!(
        KeyCode::Char('3'),
        ALT,
        Action::GoTab(Tab::Network),
        Scope::Global,
        Group::Navigation,
        "Alt+3",
        "Network tab",
        false,
        true
    ),
    b!(
        KeyCode::Char('4'),
        ALT,
        Action::GoTab(Tab::Containers),
        Scope::Global,
        Group::Navigation,
        "Alt+4",
        "Containers tab",
        false,
        true
    ),
    b!(
        KeyCode::Char('5'),
        ALT,
        Action::GoTab(Tab::Kube),
        Scope::Global,
        Group::Navigation,
        "Alt+5",
        "Kubernetes tab",
        false,
        true
    ),
    b!(
        KeyCode::Char('6'),
        ALT,
        Action::GoTab(Tab::Gpu),
        Scope::Global,
        Group::Navigation,
        "Alt+6",
        "GPU tab",
        false,
        true
    ),
    b!(
        KeyCode::Char(']'),
        NONE,
        Action::NextSubview,
        Scope::Global,
        Group::Navigation,
        "]",
        "Next sub-view",
        false,
        true
    ),
    b!(
        KeyCode::Char('['),
        NONE,
        Action::PrevSubview,
        Scope::Global,
        Group::Navigation,
        "[",
        "Previous sub-view",
        false,
        true
    ),
    // ---------------- global: table ----------------
    b!(
        KeyCode::Char('j'),
        NONE,
        Action::RowDown,
        Scope::Global,
        Group::Table,
        "j",
        "Move down",
        false,
        true
    ),
    b!(
        KeyCode::Down,
        NONE,
        Action::RowDown,
        Scope::Global,
        Group::Table,
        "↓",
        "Move down",
        false,
        false
    ),
    b!(
        KeyCode::Char('k'),
        NONE,
        Action::RowUp,
        Scope::Global,
        Group::Table,
        "k",
        "Move up",
        false,
        true
    ),
    b!(
        KeyCode::Up,
        NONE,
        Action::RowUp,
        Scope::Global,
        Group::Table,
        "↑",
        "Move up",
        false,
        false
    ),
    b!(
        KeyCode::PageDown,
        NONE,
        Action::RowPageDown,
        Scope::Global,
        Group::Table,
        "PgDn",
        "Page down",
        false,
        true
    ),
    b!(
        KeyCode::PageUp,
        NONE,
        Action::RowPageUp,
        Scope::Global,
        Group::Table,
        "PgUp",
        "Page up",
        false,
        true
    ),
    b!(
        KeyCode::Char('d'),
        CTRL,
        Action::RowHalfDown,
        Scope::Global,
        Group::Table,
        "Ctrl+D",
        "Half page down",
        false,
        true
    ),
    b!(
        KeyCode::Char('u'),
        CTRL,
        Action::RowHalfUp,
        Scope::Global,
        Group::Table,
        "Ctrl+U",
        "Half page up",
        false,
        true
    ),
    b!(
        KeyCode::Char('g'),
        NONE,
        Action::RowFirst,
        Scope::Global,
        Group::Table,
        "g",
        "First row",
        false,
        true
    ),
    b!(
        KeyCode::Home,
        NONE,
        Action::RowFirst,
        Scope::Global,
        Group::Table,
        "Home",
        "First row",
        false,
        false
    ),
    b!(
        KeyCode::Char('G'),
        NONE,
        Action::RowLast,
        Scope::Global,
        Group::Table,
        "G",
        "Last row",
        false,
        true
    ),
    b!(
        KeyCode::End,
        NONE,
        Action::RowLast,
        Scope::Global,
        Group::Table,
        "End",
        "Last row",
        false,
        false
    ),
    // Horizontal arrows scroll columns. In 0.4 they switched tabs, which put
    // "change screen" and "change row" on the same arrow cluster.
    b!(
        KeyCode::Char('h'),
        NONE,
        Action::ColLeft,
        Scope::Global,
        Group::Table,
        "h",
        "Scroll columns left",
        false,
        true
    ),
    b!(
        KeyCode::Left,
        NONE,
        Action::ColLeft,
        Scope::Global,
        Group::Table,
        "←",
        "Scroll columns left",
        false,
        false
    ),
    b!(
        KeyCode::Char('l'),
        NONE,
        Action::ColRight,
        Scope::Global,
        Group::Table,
        "l",
        "Scroll columns right",
        false,
        true
    ),
    b!(
        KeyCode::Right,
        NONE,
        Action::ColRight,
        Scope::Global,
        Group::Table,
        "→",
        "Scroll columns right",
        false,
        false
    ),
    b!(
        KeyCode::Enter,
        NONE,
        Action::OpenInspector,
        Scope::Global,
        Group::Table,
        "Enter",
        "Inspect selected row",
        true,
        true
    ),
    b!(
        KeyCode::Char('i'),
        NONE,
        Action::OpenInspector,
        Scope::Global,
        Group::Table,
        "i",
        "Inspect selected row",
        false,
        false
    ),
    b!(
        KeyCode::Char('x'),
        NONE,
        Action::ActionsMenu,
        Scope::Global,
        Group::Actions,
        "x",
        "Actions menu",
        true,
        true
    ),
    b!(
        KeyCode::Char('y'),
        NONE,
        Action::CopySelection,
        Scope::Global,
        Group::Table,
        "y",
        "Copy row identifier",
        false,
        true
    ),
    // ---------------- global: sort & filter ----------------
    b!(
        KeyCode::Char('/'),
        NONE,
        Action::OpenFilter,
        Scope::Global,
        Group::SortFilter,
        "/",
        "Filter",
        true,
        true
    ),
    b!(
        KeyCode::F(4),
        NONE,
        Action::OpenFilter,
        Scope::Global,
        Group::SortFilter,
        "F4",
        "Filter",
        false,
        false
    ),
    b!(
        KeyCode::F(3),
        NONE,
        Action::OpenFilter,
        Scope::Global,
        Group::SortFilter,
        "F3",
        "Search",
        false,
        false
    ),
    b!(
        KeyCode::Char('s'),
        NONE,
        Action::CycleSort,
        Scope::Global,
        Group::SortFilter,
        "s",
        "Cycle sort column",
        true,
        true
    ),
    b!(
        KeyCode::F(6),
        NONE,
        Action::CycleSort,
        Scope::Global,
        Group::SortFilter,
        "F6",
        "Cycle sort column",
        false,
        false
    ),
    b!(
        KeyCode::Char('S'),
        NONE,
        Action::ReverseSort,
        Scope::Global,
        Group::SortFilter,
        "S",
        "Reverse sort order",
        false,
        true
    ),
    b!(
        KeyCode::Char('I'),
        NONE,
        Action::ReverseSort,
        Scope::Global,
        Group::SortFilter,
        "I",
        "Reverse sort order",
        false,
        false
    ),
    b!(
        KeyCode::Esc,
        NONE,
        Action::Back,
        Scope::Global,
        Group::SortFilter,
        "Esc",
        "Back / clear filter",
        false,
        true
    ),
];

/// Resolve a key press to an action for the active tab.
///
/// `SHIFT` is ignored for character keys because the character itself already
/// carries the case, and terminals disagree about whether to report it.
pub fn resolve(code: KeyCode, mods: KeyModifiers, tab: Tab) -> Option<Action> {
    let mods = normalise(code, mods);
    BINDINGS
        .iter()
        .find(|b| b.code == code && b.mods == mods && b.scope.matches(tab))
        .map(|b| b.action)
}

fn normalise(code: KeyCode, mods: KeyModifiers) -> KeyModifiers {
    let mut m = mods;
    if matches!(code, KeyCode::Char(_)) {
        m.remove(KeyModifiers::SHIFT);
    }
    // Terminals that report the numeric keypad add NUM_LOCK / KEYPAD bits we
    // never bind against.
    m.intersection(KeyModifiers::CONTROL | KeyModifiers::ALT)
}

/// Bindings that apply on `tab`, tab-scoped ones first.
pub fn for_tab(tab: Tab) -> impl Iterator<Item = &'static Binding> {
    BINDINGS.iter().filter(move |b| b.scope.matches(tab))
}

/// One row per action for the help screen: the action's label plus every key
/// that triggers it, canonical key first.
pub fn help_rows(tab: Tab, scope: Scope) -> Vec<(String, &'static str, Group)> {
    let mut rows: Vec<(String, &'static str, Group)> = Vec::new();
    let mut seen: Vec<Action> = Vec::new();

    for b in BINDINGS
        .iter()
        .filter(|b| b.scope == scope && b.scope.matches(tab))
    {
        if seen.contains(&b.action) {
            continue;
        }
        seen.push(b.action);
        let keys: Vec<&str> = BINDINGS
            .iter()
            .filter(|o| o.action == b.action && o.scope == b.scope)
            .map(|o| o.display)
            .collect();
        rows.push((keys.join(" "), b.label, b.group));
    }
    rows
}

/// The short hint list for the status bar, most useful first.
pub fn hints(tab: Tab, remote: bool) -> Vec<(&'static str, &'static str)> {
    let mut out: Vec<(&'static str, &'static str)> = Vec::new();
    // Tab-scoped hints first: they are the ones the user cannot guess.
    for b in BINDINGS
        .iter()
        .filter(|b| b.hint && matches!(b.scope, Scope::Tab(t) if t == tab))
    {
        if remote && b.action.is_local_only() {
            continue;
        }
        out.push((b.display, b.label));
    }
    for b in BINDINGS
        .iter()
        .filter(|b| b.hint && b.scope == Scope::Global && b.primary)
    {
        if remote && b.action.is_local_only() {
            continue;
        }
        out.push((b.display, b.label));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABS: &[Tab] = Tab::ALL;

    #[test]
    fn no_key_is_bound_twice_in_the_same_scope() {
        for (i, a) in BINDINGS.iter().enumerate() {
            for b in &BINDINGS[i + 1..] {
                if a.code == b.code && a.mods == b.mods && a.scope == b.scope {
                    panic!(
                        "{:?}+{:?} is bound twice in {:?}: {} and {}",
                        a.code, a.mods, a.scope, a.label, b.label
                    );
                }
            }
        }
    }

    #[test]
    fn every_key_resolves_to_exactly_one_action_per_tab() {
        for &tab in TABS {
            for b in BINDINGS {
                if !b.scope.matches(tab) {
                    continue;
                }
                let resolved = resolve(b.code, b.mods, tab);
                assert!(
                    resolved.is_some(),
                    "{} does not resolve on {tab:?}",
                    b.display
                );
            }
        }
    }

    #[test]
    fn tab_scoped_bindings_shadow_global_ones_only_on_their_tab() {
        // `t` toggles the tree on Processes and does nothing anywhere else.
        assert_eq!(
            resolve(KeyCode::Char('t'), NONE, Tab::Processes),
            Some(Action::ToggleTree)
        );
        assert_eq!(resolve(KeyCode::Char('t'), NONE, Tab::Network), None);
        assert_eq!(resolve(KeyCode::Char('t'), NONE, Tab::General), None);
    }

    #[test]
    fn function_keys_do_not_leak_across_tabs() {
        // Regression: in 0.4, F9 killed the selected *process* from any tab and
        // F1–F5 silently re-sorted the process table from the Network tab.
        assert_eq!(
            resolve(KeyCode::F(9), NONE, Tab::Processes),
            Some(Action::Kill(Signal::Term))
        );
        assert_eq!(
            resolve(KeyCode::F(9), NONE, Tab::Containers),
            Some(Action::ContainerStop)
        );
        assert_eq!(resolve(KeyCode::F(9), NONE, Tab::Network), None);
        assert_eq!(resolve(KeyCode::F(11), NONE, Tab::Processes), None);
    }

    #[test]
    fn f1_is_help_as_every_other_monitor_binds_it() {
        for &tab in TABS {
            assert_eq!(resolve(KeyCode::F(1), NONE, tab), Some(Action::Help));
        }
    }

    #[test]
    fn horizontal_arrows_no_longer_switch_tabs() {
        for &tab in TABS {
            assert_eq!(resolve(KeyCode::Left, NONE, tab), Some(Action::ColLeft));
            assert_eq!(resolve(KeyCode::Right, NONE, tab), Some(Action::ColRight));
        }
        assert_eq!(
            resolve(KeyCode::Tab, NONE, Tab::General),
            Some(Action::NextTab)
        );
    }

    #[test]
    fn renice_keys_documented_in_the_readme_actually_exist() {
        // The 0.4 README documented `+` / `-` that no handler implemented.
        assert_eq!(
            resolve(KeyCode::Char('+'), NONE, Tab::Processes),
            Some(Action::Renice(-1))
        );
        assert_eq!(
            resolve(KeyCode::Char('-'), NONE, Tab::Processes),
            Some(Action::Renice(1))
        );
        assert_eq!(
            resolve(KeyCode::F(7), NONE, Tab::Processes),
            Some(Action::Renice(1))
        );
    }

    #[test]
    fn kube_keys_are_scoped_to_the_kube_tab() {
        assert_eq!(
            resolve(KeyCode::Char('A'), NONE, Tab::Kube),
            Some(Action::ToggleKubeScope)
        );
        assert_eq!(resolve(KeyCode::Char('A'), NONE, Tab::Processes), None);
        assert_eq!(
            resolve(KeyCode::Char('P'), NONE, Tab::Kube),
            Some(Action::KubeSubview(KubeSubview::Pods))
        );
    }

    #[test]
    fn shift_is_ignored_for_character_keys() {
        // Terminals disagree about reporting SHIFT alongside an uppercase char.
        assert_eq!(
            resolve(KeyCode::Char('S'), KeyModifiers::SHIFT, Tab::Processes),
            Some(Action::ReverseSort)
        );
        assert_eq!(
            resolve(KeyCode::Char('G'), KeyModifiers::SHIFT, Tab::Processes),
            Some(Action::RowLast)
        );
    }

    #[test]
    fn control_and_alt_stay_significant() {
        assert_eq!(
            resolve(KeyCode::Char('p'), CTRL, Tab::General),
            Some(Action::Palette)
        );
        // Lowercase `p` with no modifier is not the palette.
        assert_ne!(
            resolve(KeyCode::Char('p'), NONE, Tab::General),
            Some(Action::Palette)
        );
        assert_eq!(
            resolve(KeyCode::Char('1'), ALT, Tab::Kube),
            Some(Action::GoTab(Tab::General))
        );
    }

    #[test]
    fn unknown_modifier_bits_do_not_block_a_match() {
        // Some terminals set KEYPAD / NUM_LOCK bits we never bind against.
        let noisy = KeyModifiers::CONTROL | KeyModifiers::SHIFT;
        assert_eq!(
            resolve(KeyCode::Char('p'), noisy, Tab::General),
            Some(Action::Palette)
        );
    }

    #[test]
    fn ctrl_c_quits_from_every_tab() {
        for &tab in TABS {
            assert_eq!(resolve(KeyCode::Char('c'), CTRL, tab), Some(Action::Quit));
        }
    }

    #[test]
    fn every_binding_is_documented() {
        for b in BINDINGS {
            assert!(!b.label.is_empty(), "{:?} has no label", b.code);
            assert!(!b.display.is_empty(), "{:?} has no display form", b.code);
        }
    }

    #[test]
    fn help_lists_every_action_exactly_once_per_scope() {
        for &tab in TABS {
            let rows = help_rows(tab, Scope::Global);
            let labels: Vec<&str> = rows.iter().map(|(_, l, _)| *l).collect();
            for (i, a) in labels.iter().enumerate() {
                assert!(
                    !labels[i + 1..].contains(a),
                    "help lists `{a}` twice on {tab:?}"
                );
            }
        }
    }

    #[test]
    fn help_merges_aliases_into_one_row() {
        let rows = help_rows(Tab::General, Scope::Global);
        let (keys, _, _) = rows
            .iter()
            .find(|(_, label, _)| *label == "Move down")
            .expect("Move down must be documented");
        assert!(keys.contains('j'), "aliases should be merged: {keys}");
        assert!(keys.contains('↓'), "aliases should be merged: {keys}");
    }

    #[test]
    fn help_includes_tab_specific_bindings() {
        let rows = help_rows(Tab::Kube, Scope::Tab(Tab::Kube));
        assert!(rows.iter().any(|(_, l, _)| l.contains("namespace scope")));
        let rows = help_rows(Tab::Processes, Scope::Tab(Tab::Processes));
        assert!(rows.iter().any(|(_, l, _)| l.contains("SIGTERM")));
    }

    #[test]
    fn hints_hide_local_only_actions_in_remote_mode() {
        let local = hints(Tab::Processes, false);
        let remote = hints(Tab::Processes, true);
        assert!(local.iter().any(|(_, l)| l.contains("SIGTERM")));
        assert!(
            !remote.iter().any(|(_, l)| l.contains("SIGTERM")),
            "kill must not be advertised against a remote host"
        );
        // Navigation hints survive in both modes.
        assert!(remote.iter().any(|(k, _)| *k == "Tab"));
    }

    #[test]
    fn hints_lead_with_tab_specific_keys() {
        let h = hints(Tab::Kube, false);
        assert_eq!(
            h.first().map(|(k, _)| *k),
            Some("P"),
            "the keys a user cannot guess come first"
        );
    }

    #[test]
    fn local_only_actions_are_marked() {
        assert!(Action::Kill(Signal::Kill).is_local_only());
        assert!(Action::Renice(1).is_local_only());
        assert!(Action::ContainerRestart.is_local_only());
        assert!(!Action::NextTab.is_local_only());
        assert!(!Action::OpenFilter.is_local_only());
    }
}
