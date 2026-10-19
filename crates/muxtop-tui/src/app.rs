// Application state machine.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use nucleo::pattern::{Atom, AtomKind, CaseMatching, Normalization};
use nucleo::{Config, Matcher, Utf32Str};
use tokio_util::sync::CancellationToken;

use muxtop_core::actions::{self, Signal};
use muxtop_core::network::NetworkHistory;
use muxtop_core::process::{
    ProcessInfo, SortField, SortOrder, build_process_tree, filter_processes, flatten_tree,
    sort_processes,
};
use muxtop_core::system::SystemSnapshot;

use crate::keymap::{self, Action};
use crate::notify::Notifier;
use crate::terminal::TermCaps;
use crate::ui::sanitize::scrub_ctrl;
use crate::ui::theme::{Level, ThemeKind};
use crate::ui::widgets;
use crate::{CliConfig, ConnectionMode};

// ---------------------------------------------------------------------------
// Modes
// ---------------------------------------------------------------------------

/// Which overlay, if any, currently owns the keyboard.
///
/// muxtop 0.4 tracked this as a handful of independent booleans, so "what does
/// Esc do right now" had no single answer. One enum means one answer, and it is
/// what makes `Esc` a predictable step backwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Overlay {
    #[default]
    None,
    /// Fuzzy command palette (`Ctrl+P`).
    Palette,
    /// Typed command line with arguments (`:`).
    Command,
    /// Keymap reference (`?`).
    Help,
    /// Contextual actions for the selected row (`x`).
    Actions,
    /// Detail pane for the selected row (`Enter`).
    Inspector,
    /// Message history (`Ctrl+L`).
    Log,
}

impl Overlay {
    /// Whether this overlay captures text input, which decides how printable
    /// characters are routed.
    pub fn is_text_input(self) -> bool {
        matches!(self, Overlay::Palette | Overlay::Command)
    }
}

// ---------------------------------------------------------------------------
// Confirm dialog
// ---------------------------------------------------------------------------

/// A pending action that requires user confirmation (y/n).
#[derive(Debug, Clone)]
pub enum ConfirmAction {
    /// Kill a process with the given signal.
    Kill {
        pid: u32,
        name: String,
        signal: Signal,
    },
    /// Change the nice value of a process.
    Renice { pid: u32, name: String, delta: i32 },
    /// Graceful container shutdown (SIGTERM with grace period).
    StopContainer { id: String, name: String },
    /// Force container kill (SIGKILL).
    KillContainer { id: String, name: String },
    /// Restart the container.
    RestartContainer { id: String, name: String },
}

impl ConfirmAction {
    /// Human-readable description for the confirmation dialog.
    ///
    /// MED-S5: process `comm` and container names are attacker-controlled and
    /// end up in a `Span` rendered verbatim by `ui::confirm`. They are scrubbed
    /// here — the table renderers scrub at their own call sites, but this
    /// prompt is built outside them.
    pub fn prompt(&self) -> String {
        match self {
            ConfirmAction::Kill { pid, name, signal } => {
                let sig_name = match signal {
                    Signal::Kill => "SIGKILL",
                    Signal::Term => "SIGTERM",
                };
                let name = scrub_ctrl(name);
                format!("Send {sig_name} to {name} (PID {pid})?  [y/n]")
            }
            ConfirmAction::Renice { pid, name, delta } => {
                let direction = if *delta > 0 {
                    "lower priority (+1)"
                } else {
                    "higher priority (-1)"
                };
                let name = scrub_ctrl(name);
                format!("Renice {name} (PID {pid}) to {direction}?  [y/n]")
            }
            ConfirmAction::StopContainer { id, name } => {
                let name = scrub_ctrl(name);
                format!("Stop container {name} ({})?  [y/n]", short_id(id))
            }
            ConfirmAction::KillContainer { id, name } => {
                let name = scrub_ctrl(name);
                format!("Kill container {name} ({})?  [y/n]", short_id(id))
            }
            ConfirmAction::RestartContainer { id, name } => {
                let name = scrub_ctrl(name);
                format!("Restart container {name} ({})?  [y/n]", short_id(id))
            }
        }
    }
}

fn short_id(id: &str) -> String {
    id.chars().take(12).collect()
}

// ---------------------------------------------------------------------------
// Command registry
// ---------------------------------------------------------------------------

/// All available commands in the palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Quit,
    ToggleTreeView,
    SortByCpu,
    SortByMem,
    SortByPid,
    SortByName,
    SortByUser,
    ToggleSortOrder,
    CycleSort,
    SwitchToGeneral,
    SwitchToProcesses,
    OpenFilter,
    NextTab,
    PrevTab,
    KillProcess,
    ForceKillProcess,
    NiceDown,
    NiceUp,
    ClearFilter,
    SwitchToNetwork,
    SortNetByRx,
    SortNetByTx,
    SortNetByName,
    SortNetByErrors,
    SwitchToContainers,
    SortContainersByCpu,
    SortContainersByMem,
    SortContainersByName,
    SortContainersByNetRx,
    StopContainer,
    KillContainer,
    RestartContainer,
    ExecContainer,
    SwitchToKube,
    SwitchToGpu,
    // -- added in 0.5.1: everything the keymap can do is reachable here too --
    ShowHelp,
    TogglePause,
    RefreshNow,
    ShowLog,
    InspectRow,
    CopyRowId,
    KubePods,
    KubeNodes,
    KubeDeployments,
    KubeToggleScope,
    // -- verb commands: only offered once their verb has been typed --
    /// `kill <name>` — the argument form the README has advertised since 0.3.
    KillNamed,
    /// `stop <name>`
    StopNamed,
    /// `restart <name>`
    RestartNamed,
    /// `sort <field>`
    SortBy,
    /// `filter <text>`
    FilterBy,
    /// `theme <name>`
    SetTheme,
    /// `tab <name>`
    GoToTab,
}

impl Command {
    pub const ALL: &[Command] = &[
        Command::Quit,
        Command::ToggleTreeView,
        Command::SortByCpu,
        Command::SortByMem,
        Command::SortByPid,
        Command::SortByName,
        Command::SortByUser,
        Command::ToggleSortOrder,
        Command::CycleSort,
        Command::SwitchToGeneral,
        Command::SwitchToProcesses,
        Command::OpenFilter,
        Command::NextTab,
        Command::PrevTab,
        Command::KillProcess,
        Command::ForceKillProcess,
        Command::NiceDown,
        Command::NiceUp,
        Command::ClearFilter,
        Command::SwitchToNetwork,
        Command::SortNetByRx,
        Command::SortNetByTx,
        Command::SortNetByName,
        Command::SortNetByErrors,
        Command::SwitchToContainers,
        Command::SortContainersByCpu,
        Command::SortContainersByMem,
        Command::SortContainersByName,
        Command::SortContainersByNetRx,
        Command::StopContainer,
        Command::KillContainer,
        Command::RestartContainer,
        Command::ExecContainer,
        Command::SwitchToKube,
        Command::SwitchToGpu,
        Command::ShowHelp,
        Command::TogglePause,
        Command::RefreshNow,
        Command::ShowLog,
        Command::InspectRow,
        Command::CopyRowId,
        Command::KubePods,
        Command::KubeNodes,
        Command::KubeDeployments,
        Command::KubeToggleScope,
        Command::KillNamed,
        Command::StopNamed,
        Command::RestartNamed,
        Command::SortBy,
        Command::FilterBy,
        Command::SetTheme,
        Command::GoToTab,
    ];

    /// The verb that introduces this command's argument form, if it has one.
    ///
    /// `kill firefox`, `stop nginx`, `restart postgres`, `sort mem`,
    /// `filter ngin`, `theme mono`, `tab kube`.
    pub fn verb(self) -> Option<&'static str> {
        match self {
            Command::KillNamed => Some("kill"),
            Command::StopNamed => Some("stop"),
            Command::RestartNamed => Some("restart"),
            Command::SortBy => Some("sort"),
            Command::FilterBy => Some("filter"),
            Command::SetTheme => Some("theme"),
            Command::GoToTab => Some("tab"),
            _ => None,
        }
    }

    /// Whether this command is meaningless without an argument, and so must be
    /// hidden from the palette until its verb has been typed.
    pub fn is_verb_only(self) -> bool {
        self.verb().is_some()
    }

    /// Which tab a command belongs to, for context-aware ranking. `None` means
    /// it is useful everywhere.
    pub fn home_tab(self) -> Option<Tab> {
        match self {
            Command::ToggleTreeView
            | Command::SortByCpu
            | Command::SortByMem
            | Command::SortByPid
            | Command::SortByName
            | Command::SortByUser
            | Command::KillProcess
            | Command::ForceKillProcess
            | Command::KillNamed
            | Command::NiceDown
            | Command::NiceUp => Some(Tab::Processes),
            Command::SortNetByRx
            | Command::SortNetByTx
            | Command::SortNetByName
            | Command::SortNetByErrors => Some(Tab::Network),
            Command::SortContainersByCpu
            | Command::SortContainersByMem
            | Command::SortContainersByName
            | Command::SortContainersByNetRx
            | Command::StopContainer
            | Command::KillContainer
            | Command::RestartContainer
            | Command::ExecContainer
            | Command::StopNamed
            | Command::RestartNamed => Some(Tab::Containers),
            Command::KubePods
            | Command::KubeNodes
            | Command::KubeDeployments
            | Command::KubeToggleScope => Some(Tab::Kube),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Command::Quit => "Quit",
            Command::ToggleTreeView => "Toggle tree view",
            Command::SortByCpu => "Sort by CPU",
            Command::SortByMem => "Sort by Memory",
            Command::SortByPid => "Sort by PID",
            Command::SortByName => "Sort by Name",
            Command::SortByUser => "Sort by User",
            Command::ToggleSortOrder => "Toggle sort order",
            Command::CycleSort => "Cycle sort field",
            Command::SwitchToGeneral => "Switch to General tab",
            Command::SwitchToProcesses => "Switch to Processes tab",
            Command::OpenFilter => "Open filter",
            Command::NextTab => "Next tab",
            Command::PrevTab => "Previous tab",
            Command::KillProcess => "Kill process (SIGTERM)",
            Command::ForceKillProcess => "Force kill (SIGKILL)",
            Command::NiceDown => "Lower priority (+1)",
            Command::NiceUp => "Raise priority (-1)",
            Command::ClearFilter => "Clear filter",
            Command::SwitchToNetwork => "Switch to Network tab",
            Command::SortNetByRx => "Sort network by RX",
            Command::SortNetByTx => "Sort network by TX",
            Command::SortNetByName => "Sort network by name",
            Command::SortNetByErrors => "Sort network by errors",
            Command::SwitchToContainers => "Switch to Containers tab",
            Command::SortContainersByCpu => "Sort containers by CPU",
            Command::SortContainersByMem => "Sort containers by memory",
            Command::SortContainersByName => "Sort containers by name",
            Command::SortContainersByNetRx => "Sort containers by network RX",
            Command::StopContainer => "Stop container (SIGTERM)",
            Command::KillContainer => "Kill container (SIGKILL)",
            Command::RestartContainer => "Restart container",
            Command::ExecContainer => "Open container shell (PTY)",
            Command::SwitchToKube => "Switch to Kubernetes tab",
            Command::SwitchToGpu => "Switch to GPU tab",
            Command::ShowHelp => "Help — keyboard reference",
            Command::TogglePause => "Pause / resume refresh",
            Command::RefreshNow => "Refresh now",
            Command::ShowLog => "Message log",
            Command::InspectRow => "Inspect selected row",
            Command::CopyRowId => "Copy row identifier",
            Command::KubePods => "Kube: Pods sub-view",
            Command::KubeNodes => "Kube: Nodes sub-view",
            Command::KubeDeployments => "Kube: Deployments sub-view",
            Command::KubeToggleScope => "Kube: toggle namespace scope",
            Command::KillNamed => "Kill process by name",
            Command::StopNamed => "Stop container by name",
            Command::RestartNamed => "Restart container by name",
            Command::SortBy => "Sort by field",
            Command::FilterBy => "Filter by text",
            Command::SetTheme => "Switch theme",
            Command::GoToTab => "Switch tab by name",
        }
    }

    pub fn shortcut(self) -> &'static str {
        match self {
            Command::Quit => "q",
            Command::ToggleTreeView => "t",
            Command::SortByCpu => "F3",
            Command::SortByMem => "F4",
            Command::SortByPid => "F1",
            Command::SortByName => "F2",
            Command::SortByUser => "F5",
            Command::ToggleSortOrder => "S",
            Command::CycleSort => "s",
            Command::SwitchToGeneral => "Alt+1",
            Command::SwitchToProcesses => "Alt+2",
            Command::OpenFilter => "/",
            Command::NextTab => "Tab",
            Command::PrevTab => "Shift+Tab",
            Command::KillProcess => "F9",
            Command::ForceKillProcess => "F10",
            Command::NiceDown => "F7",
            Command::NiceUp => "F8",
            Command::ClearFilter => "Esc",
            Command::SwitchToNetwork => "Alt+3",
            Command::SortNetByRx => "",
            Command::SortNetByTx => "",
            Command::SortNetByName => "",
            Command::SortNetByErrors => "",
            Command::SwitchToContainers => "Alt+4",
            Command::SortContainersByCpu => "",
            Command::SortContainersByMem => "",
            Command::SortContainersByName => "",
            Command::SortContainersByNetRx => "",
            Command::StopContainer => "F9",
            Command::KillContainer => "F10",
            Command::RestartContainer => "F11",
            Command::ExecContainer => "e",
            Command::SwitchToKube => "Alt+5",
            Command::SwitchToGpu => "Alt+6",
            Command::ShowHelp => "?",
            Command::TogglePause => "Space",
            Command::RefreshNow => "r",
            Command::ShowLog => "Ctrl+L",
            Command::InspectRow => "Enter",
            Command::CopyRowId => "y",
            Command::KubePods => "P",
            Command::KubeNodes => "N",
            Command::KubeDeployments => "D",
            Command::KubeToggleScope => "A",
            Command::KillNamed => "kill <name>",
            Command::StopNamed => "stop <name>",
            Command::RestartNamed => "restart <name>",
            Command::SortBy => "sort <field>",
            Command::FilterBy => "filter <text>",
            Command::SetTheme => "theme <name>",
            Command::GoToTab => "tab <name>",
        }
    }

    /// The search haystack: label + shortcut combined for better fuzzy matching.
    ///
    /// Kept around for callers that want a freshly-allocated string (e.g.
    /// debug logs); the hot palette filter path uses [`Command::search_texts`]
    /// to avoid the per-keystroke allocation cost.
    fn search_text(self) -> String {
        let aliases = if self == Command::ExecContainer {
            " exec docker podman"
        } else {
            ""
        };
        format!("{} {}{aliases}", self.label(), self.shortcut())
    }

    /// Pre-built haystack for every command in [`Command::ALL`], indexed by
    /// position. Built once on first access; PERF-M3 avoids the
    /// `format!("{} {}", label, shortcut)` allocation that previously fired
    /// for every command on every palette keystroke.
    pub fn search_texts() -> &'static [String] {
        use std::sync::OnceLock;
        static SEARCH_TEXTS: OnceLock<Vec<String>> = OnceLock::new();
        SEARCH_TEXTS.get_or_init(|| Self::ALL.iter().map(|c| c.search_text()).collect())
    }
}

// ---------------------------------------------------------------------------
// Palette state
// ---------------------------------------------------------------------------

/// State for the command palette overlay.
pub struct PaletteState {
    pub input: String,
    pub selected: usize,
    /// Filtered commands with match scores (higher = better).
    pub filtered: Vec<(Command, Option<u16>)>,
    /// Argument parsed out of a `verb rest` input, e.g. `firefox` in
    /// `kill firefox`. `None` when the input is a plain fuzzy query.
    pub arg: Option<String>,
    /// Commands run this session, most recent first. Session-only: muxtop
    /// writes no state to disk, and that is a feature.
    history: Vec<Command>,
    /// Reusable nucleo matcher (PERF-M3). Created lazily on the first call to
    /// [`Self::refilter_excluding`] with a non-empty input. Constructing a
    /// `Matcher` allocates several internal scratch buffers, so reusing the
    /// instance across keystrokes is a measurable win in the palette hot loop.
    matcher: Option<Matcher>,
}

impl Default for PaletteState {
    fn default() -> Self {
        Self::new()
    }
}

impl PaletteState {
    pub fn new() -> Self {
        let filtered = Command::ALL
            .iter()
            .copied()
            .filter(|c| !c.is_verb_only())
            .map(|cmd| (cmd, None))
            .collect();
        Self {
            input: String::new(),
            selected: 0,
            filtered,
            arg: None,
            history: Vec::new(),
            matcher: None,
        }
    }

    /// Record an executed command so it floats to the top next time.
    pub fn remember(&mut self, cmd: Command) {
        self.history.retain(|&c| c != cmd);
        self.history.insert(0, cmd);
        self.history.truncate(Self::HISTORY_LEN);
    }

    /// How many recently-used commands are promoted on an empty query.
    const HISTORY_LEN: usize = 5;

    /// Split `verb rest` into a command and its argument.
    ///
    /// Returns `None` when the input is not an argument form, in which case it
    /// is treated as a fuzzy query over command labels.
    fn parse_verb(input: &str) -> Option<(Command, String)> {
        let (head, rest) = input.split_once(char::is_whitespace)?;
        let head = head.to_lowercase();
        let cmd = Command::ALL
            .iter()
            .copied()
            .find(|c| c.verb() == Some(head.as_str()))?;
        Some((cmd, rest.trim().to_string()))
    }

    /// Recompute filtered results using nucleo fuzzy matching.
    /// `excluded` contains commands to hide (e.g. kill/renice in remote mode).
    pub fn refilter_excluding(&mut self, excluded: &[Command]) {
        self.refilter_ctx(excluded, None);
    }

    /// Recompute filtered results, ranking the active tab's commands first.
    ///
    /// Context matters more than raw fuzzy score here: "sort by memory" while
    /// looking at the Network tab almost certainly means the network table.
    pub fn refilter_ctx(&mut self, excluded: &[Command], tab: Option<Tab>) {
        let is_available = |cmd: &Command| !excluded.contains(cmd);

        // An argument form collapses the list to the single command it names,
        // so `kill firefox` cannot be confused with "Kill container".
        if let Some((cmd, arg)) = Self::parse_verb(&self.input)
            && is_available(&cmd)
        {
            self.arg = Some(arg);
            self.filtered.clear();
            self.filtered.push((cmd, Some(u16::MAX)));
            self.selected = 0;
            return;
        }
        self.arg = None;

        if self.input.is_empty() {
            self.filtered.clear();
            let mut candidates: Vec<Command> = Command::ALL
                .iter()
                .copied()
                .filter(|c| !c.is_verb_only())
                .filter(is_available)
                .collect();
            // Recently used first, then this tab's commands, then the rest.
            candidates.sort_by_key(|c| {
                let recency = self
                    .history
                    .iter()
                    .position(|h| h == c)
                    .unwrap_or(Self::HISTORY_LEN);
                let context = match (c.home_tab(), tab) {
                    (Some(home), Some(active)) if home == active => 0,
                    (None, _) => 1,
                    _ => 2,
                };
                (recency, context)
            });
            self.filtered
                .extend(candidates.into_iter().map(|cmd| (cmd, None)));
        } else {
            // PERF-M3: reuse the matcher across keystrokes — building one
            // allocates several scratch tables that we'd otherwise throw away.
            let matcher = self
                .matcher
                .get_or_insert_with(|| Matcher::new(Config::DEFAULT));
            let atom = Atom::new(
                &self.input,
                CaseMatching::Ignore,
                Normalization::Smart,
                AtomKind::Fuzzy,
                false,
            );

            let haystacks = Command::search_texts();
            let mut scored: Vec<(Command, u16)> = Command::ALL
                .iter()
                .copied()
                .enumerate()
                .filter(|(_, cmd)| !cmd.is_verb_only())
                .filter(|(_, cmd)| is_available(cmd))
                .filter_map(|(idx, cmd)| {
                    // PERF-M3: lift the cached haystack instead of `format!`'ing
                    // a fresh `String` every keystroke.
                    let haystack = &haystacks[idx];
                    let mut buf = Vec::new();
                    let haystack_utf32 = Utf32Str::new(haystack, &mut buf);
                    atom.score(haystack_utf32, matcher).map(|score| {
                        // Commands belonging to the tab in front of the user
                        // win ties. The bonus is deliberately small so it
                        // reorders equals without overriding a clear match.
                        let bonus = match (cmd.home_tab(), tab) {
                            (Some(home), Some(active)) if home == active => 24,
                            (None, _) => 8,
                            _ => 0,
                        };
                        (cmd, score.saturating_add(bonus))
                    })
                })
                .collect();

            scored.sort_by_key(|b| std::cmp::Reverse(b.1));
            self.filtered.clear();
            self.filtered
                .extend(scored.into_iter().map(|(cmd, s)| (cmd, Some(s))));
        }

        // Clamp selection
        if self.filtered.is_empty() {
            self.selected = 0;
        } else {
            self.selected = self.selected.min(self.filtered.len() - 1);
        }
    }

    /// Recompute filtered results (no exclusions).
    pub fn refilter(&mut self) {
        self.refilter_excluding(&[]);
    }
}

/// Tab identifiers for TUI views.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    #[default]
    General,
    Processes,
    Network,
    Containers,
    Kube,
    Gpu,
}

impl std::fmt::Display for Tab {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

impl Tab {
    pub const ALL: &[Tab] = &[
        Tab::General,
        Tab::Processes,
        Tab::Network,
        Tab::Containers,
        Tab::Kube,
        Tab::Gpu,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Tab::General => "General",
            Tab::Processes => "Processes",
            Tab::Network => "Network",
            Tab::Containers => "Containers",
            Tab::Kube => "Kube",
            Tab::Gpu => "GPU",
        }
    }

    pub fn next(self) -> Tab {
        let idx = Self::ALL.iter().position(|&t| t == self).unwrap_or(0);
        Self::ALL[(idx + 1) % Self::ALL.len()]
    }

    pub fn prev(self) -> Tab {
        let idx = Self::ALL.iter().position(|&t| t == self).unwrap_or(0);
        Self::ALL[(idx + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

/// Sort field for the Network tab.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum NetworkSortField {
    Name,
    #[default]
    RxRate,
    TxRate,
    TotalRx,
    TotalTx,
    Errors,
}

/// Sort field for the Containers tab.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum ContainerSortField {
    Name,
    #[default]
    Cpu,
    Mem,
    NetRx,
    NetTx,
    Uptime,
}

/// Active sub-view of the Kubernetes tab.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum KubeSubview {
    #[default]
    Pods,
    Nodes,
    Deployments,
}

impl KubeSubview {
    pub fn label(self) -> &'static str {
        match self {
            KubeSubview::Pods => "Pods",
            KubeSubview::Nodes => "Nodes",
            KubeSubview::Deployments => "Deployments",
        }
    }
}

/// Single sort-field enum unioning the three Kube sub-views. Stored as one
/// value on `AppState`; switching sub-view resets it to the sub-view's
/// natural default. The cycling helper `next_kube_sort_field` only walks
/// the variants that belong to the current sub-view, so an out-of-domain
/// value (e.g. `PodCpu` while on the Nodes view) recovers to the default
/// of the active sub-view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KubeSortField {
    // Pods
    PodName,
    PodCpu,
    PodMem,
    PodRestarts,
    PodAge,
    PodPhase,
    // Nodes
    NodeName,
    NodeCpuPct,
    NodeMemPct,
    NodePodCount,
    NodeAge,
    // Deployments
    DeployNamespace,
    DeployName,
    DeployReadyRatio,
    DeployAge,
}

impl KubeSortField {
    /// Default sort field for each sub-view.
    pub fn default_for(sv: KubeSubview) -> Self {
        match sv {
            KubeSubview::Pods => KubeSortField::PodCpu,
            KubeSubview::Nodes => KubeSortField::NodeCpuPct,
            KubeSubview::Deployments => KubeSortField::DeployName,
        }
    }
}

/// Active sub-view of the GPU tab.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum GpuSubview {
    #[default]
    Devices,
    Procs,
}

impl GpuSubview {
    pub fn label(self) -> &'static str {
        match self {
            GpuSubview::Devices => "Devices",
            GpuSubview::Procs => "Procs",
        }
    }
}

/// Single sort-field enum unioning the two GPU sub-views, following the
/// [`KubeSortField`] pattern: one value on `AppState`, reset to the
/// sub-view's natural default on switch, and cycled only within the active
/// sub-view's domain by `next_gpu_sort_field`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuSortField {
    // Devices
    DeviceIndex,
    DeviceName,
    DeviceUtil,
    DeviceMem,
    DeviceTemp,
    DevicePower,
    // Procs
    ProcPid,
    ProcName,
    ProcMem,
    ProcDevice,
}

impl GpuSortField {
    /// Default sort field for each sub-view.
    ///
    /// Devices default to **index** rather than utilisation: `nvidia-smi`
    /// numbers GPUs and users refer to them that way ("GPU 1 is the busy
    /// one"), so a list that reorders itself under load would break the
    /// mental map. Procs default to memory, where the biggest consumer is
    /// what the user came to find.
    pub fn default_for(sv: GpuSubview) -> Self {
        match sv {
            GpuSubview::Devices => GpuSortField::DeviceIndex,
            GpuSubview::Procs => GpuSortField::ProcMem,
        }
    }
}

/// Debounce window for filter-input keystroke bursts (PERF-H3).
///
/// Held off recomputes coalesce a short typing burst into a single pipeline
/// run while keeping the UI feeling responsive. 50 ms is short enough that
/// the user perceives the filter as "live" but long enough to absorb the
/// 60 Hz keyboard repeat rate on most terminals.
const FILTER_DEBOUNCE: Duration = Duration::from_millis(50);

/// Full application state for the TUI.
pub struct AppState {
    pub tab: Tab,
    pub sort_field: SortField,
    pub sort_order: SortOrder,
    pub filter_input: String,
    pub filter_active: bool,
    pub tree_mode: bool,
    pub selected: usize,
    pub scroll_offset: usize,
    /// Which overlay owns the keyboard, if any.
    pub overlay: Overlay,
    pub palette: PaletteState,
    /// Pending confirm dialog (kill/renice).
    pub confirm: Option<ConfirmAction>,
    /// Toast stack and message log.
    pub notifier: Notifier,
    /// Detected terminal capabilities.
    pub term_caps: TermCaps,
    /// Active colour scheme.
    pub theme_kind: ThemeKind,
    /// When true, incoming snapshots are ignored so the user can read a
    /// fast-moving table. Collection keeps running; only the view is frozen.
    pub paused: bool,
    /// Horizontal column-scroll offset for wide tables (`h` / `l`).
    pub col_scroll: usize,
    /// Selected entry in the contextual actions menu.
    pub actions_selected: usize,
    /// Scroll offset inside the help / log / inspector overlays.
    pub overlay_scroll: usize,
    /// Frame counter, used only to animate spinners.
    pub tick: usize,
    running: bool,
    pub last_snapshot: Option<SystemSnapshot>,
    /// Derived: sorted + filtered process list.
    pub visible_processes: Vec<ProcessInfo>,
    /// Derived: flattened tree (process, depth) pairs.
    pub visible_tree: Vec<(ProcessInfo, usize)>,
    /// Network history for bandwidth and sparkline calculations.
    pub network_history: NetworkHistory,
    /// Selected interface index (Network tab).
    pub net_selected: usize,
    /// Scroll offset (Network tab).
    pub net_scroll_offset: usize,
    /// Sort field for the Network tab.
    pub net_sort_field: NetworkSortField,
    /// Sort order for the Network tab.
    pub net_sort_order: SortOrder,
    /// Filter input for the Network tab.
    pub net_filter_input: String,
    /// Filter active flag for the Network tab.
    pub net_filter_active: bool,
    /// Selected container index (Containers tab).
    pub containers_selected: usize,
    /// Scroll offset (Containers tab).
    pub containers_scroll_offset: usize,
    /// Sort field for the Containers tab.
    pub kube_subview: KubeSubview,
    pub kube_sort_field: KubeSortField,
    pub kube_sort_order: SortOrder,
    pub kube_filter_input: String,
    pub kube_filter_active: bool,
    pub kube_selected: usize,
    pub kube_scroll_offset: usize,
    /// Filtered + sorted indices into the active Kube sub-view's list
    /// (PERF-03), refreshed by `recompute_kube_view` whenever the snapshot,
    /// sub-view, filter or sort changes. Stores indices rather than clones so
    /// the cache cost tracks row count, not row size.
    pub kube_view_cache: Vec<usize>,
    /// GPU tab state. Sorting and filtering are applied at render time, as on
    /// the Kube tab — a host has single-digit GPUs and rarely more than a few
    /// dozen GPU processes, so the per-frame projection is microseconds and
    /// not worth a cache.
    pub gpu_subview: GpuSubview,
    pub gpu_sort_field: GpuSortField,
    pub gpu_sort_order: SortOrder,
    pub gpu_filter_input: String,
    pub gpu_filter_active: bool,
    pub gpu_selected: usize,
    pub gpu_scroll_offset: usize,
    pub containers_sort_field: ContainerSortField,
    /// Sort order for the Containers tab.
    pub containers_sort_order: SortOrder,
    /// Filter input for the Containers tab (matches id / name / image).
    pub containers_filter_input: String,
    /// Filter active flag for the Containers tab.
    pub containers_filter_active: bool,
    /// Per-container CPU % history (60 samples each, capped by CONTAINER_HISTORY_LEN).
    container_cpu_hist: std::collections::HashMap<String, std::collections::VecDeque<f32>>,
    /// Per-container cumulative RX (bytes). Deltas are computed on the fly.
    container_rx_last: std::collections::HashMap<String, u64>,
    /// Per-container RX deltas (bytes/tick), capped.
    container_rx_hist: std::collections::HashMap<String, std::collections::VecDeque<u64>>,
    /// Cached sorted+filtered container rows (PERF-H4). Refreshed in
    /// `recompute_containers_view` whenever the snapshot, sort field, sort
    /// order, or filter input changes. Render call sites borrow this slice
    /// instead of recomputing the projection three times per frame.
    sorted_filtered_containers_cache: Vec<muxtop_core::containers::ContainerSnapshot>,
    /// Optional container engine for executing Stop/Kill/Restart actions.
    ///
    /// Shared (cheaply cloneable `Arc`) between AppState and the Collector's
    /// container loop. `None` means container actions surface a "not
    /// configured" status message rather than spawning a task.
    /// Full daemon ID of the shell request consumed exclusively by the TUI loop.
    pub(crate) pending_exec: Option<String>,
    pub container_engine:
        Option<std::sync::Arc<dyn muxtop_core::container_engine::ContainerEngine + Send + Sync>>,
    /// Local-mode cluster engine, shared with the Collector. `None` in remote
    /// mode (the server owns the kubeconfig) and when `--no-kube` is set —
    /// the `A` scope toggle surfaces a status message instead.
    pub cluster_engine:
        Option<std::sync::Arc<dyn muxtop_core::cluster_engine::ClusterEngine + Send + Sync>>,
    /// Channel sender for container action outcomes. Spawned tokio tasks
    /// send their status messages here; the TUI main loop drains them via
    /// `pump_action_results`.
    action_tx: tokio::sync::mpsc::UnboundedSender<(Level, String)>,
    /// Matching receiver. Lives on AppState so call sites stay simple.
    action_rx: tokio::sync::mpsc::UnboundedReceiver<(Level, String)>,
    /// Whether monitoring local machine or remote server.
    pub connection_mode: ConnectionMode,
    pub remote_status: crate::RemoteStatus,
    /// Render-coalescing flag (PERF-H1). Set by any state-mutating handler;
    /// the main loop reads + clears it via `take_needs_redraw` each
    /// iteration. `Event::Tick` no longer triggers a draw on its own.
    needs_redraw: bool,
    /// Debounce timer for the process filter (PERF-H3). Recorded on every
    /// keystroke that mutates the filter input; `recompute_visible` only
    /// re-runs when at least [`FILTER_DEBOUNCE`] has elapsed since the last
    /// keystroke (or the user hits Enter).
    last_filter_change: Option<Instant>,
    /// Cancellation token shared with spawned container action tasks
    /// (PERF-L3). Triggered when the TUI quits so in-flight Stop/Kill/Restart
    /// futures abort instead of running detached past TUI shutdown.
    shutdown_token: CancellationToken,
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

impl AppState {
    pub fn new() -> Self {
        let (action_tx, action_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            tab: Tab::default(),
            sort_field: SortField::Cpu,
            sort_order: SortOrder::Desc,
            filter_input: String::new(),
            filter_active: false,
            tree_mode: false,
            selected: 0,
            scroll_offset: 0,
            overlay: Overlay::None,
            palette: PaletteState::new(),
            confirm: None,
            notifier: Notifier::new(),
            term_caps: TermCaps::default(),
            theme_kind: ThemeKind::default(),
            paused: false,
            col_scroll: 0,
            actions_selected: 0,
            overlay_scroll: 0,
            tick: 0,
            running: true,
            last_snapshot: None,
            visible_processes: Vec::new(),
            visible_tree: Vec::new(),
            network_history: NetworkHistory::new(60),
            net_selected: 0,
            net_scroll_offset: 0,
            net_sort_field: NetworkSortField::default(),
            net_sort_order: SortOrder::Desc,
            net_filter_input: String::new(),
            net_filter_active: false,
            containers_selected: 0,
            containers_scroll_offset: 0,
            kube_subview: KubeSubview::default(),
            kube_sort_field: KubeSortField::default_for(KubeSubview::default()),
            kube_sort_order: SortOrder::Desc,
            kube_filter_input: String::new(),
            kube_filter_active: false,
            kube_selected: 0,
            kube_scroll_offset: 0,
            kube_view_cache: Vec::new(),
            gpu_subview: GpuSubview::default(),
            gpu_sort_field: GpuSortField::default_for(GpuSubview::default()),
            gpu_sort_order: SortOrder::Desc,
            gpu_filter_input: String::new(),
            gpu_filter_active: false,
            gpu_selected: 0,
            gpu_scroll_offset: 0,
            containers_sort_field: ContainerSortField::default(),
            containers_sort_order: SortOrder::Desc,
            containers_filter_input: String::new(),
            containers_filter_active: false,
            container_cpu_hist: std::collections::HashMap::new(),
            container_rx_last: std::collections::HashMap::new(),
            container_rx_hist: std::collections::HashMap::new(),
            sorted_filtered_containers_cache: Vec::new(),
            container_engine: None,
            pending_exec: None,
            cluster_engine: None,
            action_tx,
            action_rx,
            connection_mode: ConnectionMode::default(),
            remote_status: crate::RemoteStatus::Waiting,
            // PERF-H1: start with one redraw scheduled so the first frame is
            // painted before any event arrives.
            needs_redraw: true,
            last_filter_change: None,
            shutdown_token: CancellationToken::new(),
        }
    }

    /// Create AppState from CLI configuration and detected terminal capabilities.
    pub fn with_config(config: CliConfig, term_caps: TermCaps) -> Self {
        Self {
            sort_field: config.sort_field,
            tree_mode: config.tree_mode,
            filter_input: config.filter.unwrap_or_default(),
            term_caps,
            theme_kind: config.theme,
            connection_mode: config.connection_mode,
            ..Self::new()
        }
    }

    /// Returns true if currently in remote monitoring mode.
    pub fn is_remote(&self) -> bool {
        matches!(self.connection_mode, ConnectionMode::Remote { .. })
    }

    /// The newest visible message, if any.
    pub fn active_status(&self) -> Option<&crate::notify::Toast> {
        self.notifier.latest()
    }

    /// Post a message with an explicit severity.
    ///
    /// Severity is declared by the caller rather than sniffed out of the text:
    /// 0.4 tested the message for the substring "failed", so rewording an error
    /// silently painted it on the success colour.
    pub fn notify(&mut self, level: Level, msg: impl Into<String>) {
        self.notifier.push(level, msg);
        self.needs_redraw = true;
    }

    /// Post an error.
    fn set_error(&mut self, msg: String) {
        self.notify(Level::Error, msg);
    }

    /// Drop expired toasts, reporting whether anything changed.
    ///
    /// Used by the event-driven render path (PERF-H1) to schedule exactly one
    /// repaint when a toast disappears, instead of spinning at 60 Hz to notice.
    pub fn status_message_just_expired(&mut self) -> bool {
        self.notifier.expire()
    }

    /// Time until the next toast expires, so the event loop can size its poll
    /// timeout instead of waking up for nothing.
    pub fn next_status_deadline(&self) -> Option<Duration> {
        self.notifier.next_deadline()
    }

    /// Advance the animation counter (spinners). Cheap and wrapping.
    pub fn tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
        self.flush_pending_filter();
    }

    /// Flush pending input on idle events, even while collection is paused.
    pub(crate) fn flush_pending_filter(&mut self) {
        self.flush_filter_at(Instant::now());
    }

    pub(crate) fn flush_filter_at(&mut self, now: Instant) {
        if self
            .last_filter_change
            .is_some_and(|last| now.saturating_duration_since(last) >= FILTER_DEBOUNCE)
        {
            self.last_filter_change = None;
            self.recompute_visible();
            self.mark_dirty();
        }
    }

    pub fn running(&self) -> bool {
        self.running
    }

    pub fn quit(&mut self) {
        self.running = false;
        // PERF-L3: signal in-flight container action tasks to abort instead of
        // running detached past TUI shutdown. The token is also cloned into
        // every spawned task at submission time.
        self.shutdown_token.cancel();
    }

    /// Mark the TUI as needing a redraw on the next main-loop iteration
    /// (PERF-H1). Called by every state-mutating handler so the event loop
    /// can skip `terminal.draw` on idle ticks.
    fn mark_dirty(&mut self) {
        self.needs_redraw = true;
    }

    /// Take + clear the redraw flag. Used by the main loop to decide whether
    /// to call `terminal.draw` for this iteration.
    pub fn take_needs_redraw(&mut self) -> bool {
        std::mem::replace(&mut self.needs_redraw, false)
    }

    /// Cancellation token cloned into every container action spawn so
    /// in-flight Stop/Kill/Restart futures abort on TUI shutdown (PERF-L3).
    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown_token.clone()
    }

    /// Number of visible processes (respects tree_mode).
    pub fn process_count(&self) -> usize {
        if self.tree_mode {
            self.visible_tree.len()
        } else {
            self.visible_processes.len()
        }
    }

    /// Number of visible network interfaces (respects filter).
    pub fn net_interface_count(&self) -> usize {
        let Some(ref snapshot) = self.last_snapshot else {
            return 0;
        };
        if self.net_filter_input.is_empty() {
            snapshot.networks.interfaces.len()
        } else {
            let filter = self.net_filter_input.to_lowercase();
            snapshot
                .networks
                .interfaces
                .iter()
                .filter(|i| i.name.to_lowercase().contains(&filter))
                .count()
        }
    }

    /// Number of visible items in the current tab.
    pub fn item_count(&self) -> usize {
        match self.tab {
            Tab::Network => self.net_interface_count(),
            Tab::Containers => self.containers_count(),
            Tab::Kube => self.kube_count(),
            Tab::Gpu => self.gpu_count(),
            _ => self.process_count(),
        }
    }

    /// Reference to the selected/scroll_offset for the current tab.
    fn selected_mut(&mut self) -> (&mut usize, &mut usize) {
        match self.tab {
            Tab::Network => (&mut self.net_selected, &mut self.net_scroll_offset),
            Tab::Containers => (
                &mut self.containers_selected,
                &mut self.containers_scroll_offset,
            ),
            Tab::Kube => (&mut self.kube_selected, &mut self.kube_scroll_offset),
            Tab::Gpu => (&mut self.gpu_selected, &mut self.gpu_scroll_offset),
            _ => (&mut self.selected, &mut self.scroll_offset),
        }
    }

    /// Count of items rendered in the active Kube sub-view AFTER the filter
    /// is applied. Used by the j/k / scroll bounds.
    /// PERF-03: reads the cache. This runs on every navigation keypress, and
    /// re-filtering the whole pod list — lowercasing two fields per row — to
    /// answer "how many rows are there" was the same mistake PERF-M5 fixed
    /// for the Network and Containers tabs.
    pub fn kube_count(&self) -> usize {
        self.kube_view_cache.len()
    }

    /// Rebuild [`Self::kube_view_cache`] from the current snapshot and view
    /// state. Cheap to call; every mutation that can reorder or resize the
    /// Kube table must call it, or the table and the selection bounds drift
    /// apart.
    fn recompute_kube_view(&mut self) {
        let indices = match self.last_snapshot.as_ref().and_then(|s| s.kube.as_ref()) {
            Some(snap) => crate::ui::kube::compute_view_indices(
                snap,
                self.kube_subview,
                &self.kube_filter_input,
                self.kube_sort_field,
                self.kube_sort_order,
            ),
            None => Vec::new(),
        };
        self.kube_view_cache = indices;
    }

    /// Count of rows rendered in the active GPU sub-view AFTER the filter is
    /// applied. Used by the j/k / scroll bounds.
    ///
    /// The filter matches the same fields the sub-view renders: device name
    /// and vendor for Devices, process name and PID for Procs. Matching on
    /// PID matters — a user hunting a runaway job usually has the PID from
    /// the Processes tab, not the name.
    pub fn gpu_count(&self) -> usize {
        let Some(snap) = self.last_snapshot.as_ref().and_then(|s| s.gpu.as_ref()) else {
            return 0;
        };
        let filter = self.gpu_filter_input.to_lowercase();
        match self.gpu_subview {
            GpuSubview::Devices => snap
                .devices
                .iter()
                .filter(|d| crate::ui::gpu::device_matches_filter(d, &filter))
                .count(),
            GpuSubview::Procs => snap
                .processes
                .iter()
                .filter(|p| crate::ui::gpu::process_matches_filter(p, &filter))
                .count(),
        }
    }

    /// Switch the GPU tab's active sub-view, resetting sort + filter +
    /// selection so the user starts fresh in the new view. Mirrors
    /// [`Self::switch_kube_subview`].
    pub fn switch_gpu_subview(&mut self, sv: GpuSubview) {
        self.gpu_subview = sv;
        self.gpu_sort_field = GpuSortField::default_for(sv);
        self.gpu_sort_order = SortOrder::Desc;
        self.gpu_filter_input.clear();
        self.gpu_filter_active = false;
        self.gpu_selected = 0;
        self.gpu_scroll_offset = 0;
    }

    /// Switch the Kube tab's active sub-view, resetting sort + filter +
    /// selection so the user starts fresh in the new view.
    pub fn switch_kube_subview(&mut self, sv: KubeSubview) {
        self.kube_subview = sv;
        self.kube_sort_field = KubeSortField::default_for(sv);
        self.kube_sort_order = SortOrder::Desc;
        self.kube_filter_input.clear();
        self.kube_filter_active = false;
        self.kube_selected = 0;
        self.kube_scroll_offset = 0;
        // The indices now point into a different list entirely.
        self.recompute_kube_view();
    }

    /// Returns the currently selected process, if any.
    pub fn selected_process(&self) -> Option<&ProcessInfo> {
        if self.tree_mode {
            self.visible_tree.get(self.selected).map(|(p, _)| p)
        } else {
            self.visible_processes.get(self.selected)
        }
    }

    /// Attach a concrete `ContainerEngine` for use by Stop/Kill/Restart.
    ///
    /// Called once by the binary after the engine is built in `main.rs`;
    /// idempotent on None (kept as None).
    pub fn set_container_engine(
        &mut self,
        engine: std::sync::Arc<dyn muxtop_core::container_engine::ContainerEngine + Send + Sync>,
    ) {
        self.container_engine = Some(engine);
    }

    /// Attach a concrete `ClusterEngine` so `A` can rescope it at runtime.
    ///
    /// Local mode only — in remote mode the engine lives on the server and
    /// this stays `None`, mirroring how container actions are gated.
    pub fn set_cluster_engine(
        &mut self,
        engine: std::sync::Arc<dyn muxtop_core::cluster_engine::ClusterEngine + Send + Sync>,
    ) {
        self.cluster_engine = Some(engine);
    }

    /// `A` — flip the Kube tab between the configured namespace and
    /// all-namespaces.
    ///
    /// The engine decides which namespace to land on (it knows
    /// `--kube-namespace` and the kubeconfig default; the snapshot alone does
    /// not), so this only dispatches and reports. The new scope reaches the
    /// UI on the next poll tick — up to 5 s later — and the engine clears its
    /// pod/deployment caches immediately so no out-of-scope row survives the
    /// switch.
    pub fn toggle_kube_scope(&mut self) {
        let Some(engine) = self.cluster_engine.clone() else {
            self.notify(
                Level::Warning,
                "Namespace scope is local-only (unavailable in remote mode / --no-kube)",
            );
            return;
        };
        let tx = self.action_tx.clone();
        let token = self.shutdown_token.clone();
        tokio::spawn(async move {
            let msg = tokio::select! {
                biased;
                _ = token.cancelled() => return,
                scope = engine.toggle_scope() => match scope.namespace() {
                    Some(ns) => format!("Kube scope: namespace {ns}"),
                    None => "Kube scope: all namespaces".to_string(),
                },
            };
            let _ = tx.send((Level::Info, msg));
        });
        // The row set is about to change out from under the cursor.
        self.kube_selected = 0;
        self.kube_scroll_offset = 0;
        self.needs_redraw = true;
    }

    /// Selected container row (respects filter + sort), if any. Used by the
    /// action request methods below.
    pub fn selected_container(&self) -> Option<muxtop_core::containers::ContainerSnapshot> {
        let snapshot = self.last_snapshot.as_ref()?;
        let cs = snapshot.containers.as_ref()?;
        if !cs.daemon_up {
            return None;
        }
        // PERF-H4: read the cache populated by `recompute_containers_view`
        // (refreshed in `apply_snapshot` and on sort/filter changes) instead
        // of recomputing the projection on every call.
        self.sorted_filtered_containers_cache
            .get(self.containers_selected)
            .cloned()
    }

    /// Borrow the cached sorted+filtered container list (PERF-H4). Empty when
    /// the daemon is down or no snapshot has arrived yet.
    pub fn sorted_filtered_containers(&self) -> &[muxtop_core::containers::ContainerSnapshot] {
        &self.sorted_filtered_containers_cache
    }

    /// Drain outcomes from spawned container action tasks, surfacing them as
    /// status messages. Called by the TUI main loop every tick.
    pub fn pump_action_results(&mut self) {
        let mut had_any = false;
        while let Ok((level, msg)) = self.action_rx.try_recv() {
            self.notify(level, msg);
            had_any = true;
        }
        // A new status message changes what `draw_root` paints, so coalesce
        // the redraw here rather than relying on the next event.
        if had_any {
            self.mark_dirty();
        }
    }

    /// Shared sort+filter pipeline used by both the UI and action layer.
    /// Now refreshes [`Self::sorted_filtered_containers_cache`] in place
    /// instead of returning an owned `Vec`.
    fn recompute_containers_view(&mut self) {
        use muxtop_core::containers::ContainerSnapshot;
        self.sorted_filtered_containers_cache.clear();

        let Some(snapshot) = self.last_snapshot.as_ref() else {
            return;
        };
        let Some(cs) = snapshot.containers.as_ref() else {
            return;
        };
        if !cs.daemon_up {
            return;
        }

        let mut rows: Vec<ContainerSnapshot> = if self.containers_filter_input.is_empty() {
            cs.containers.clone()
        } else {
            let f = self.containers_filter_input.to_lowercase();
            cs.containers
                .iter()
                .filter(|c| {
                    c.name.to_lowercase().contains(&f)
                        || c.image.to_lowercase().contains(&f)
                        || c.id.to_lowercase().contains(&f)
                })
                .cloned()
                .collect()
        };
        match self.containers_sort_field {
            ContainerSortField::Name => rows.sort_by(|a, b| a.name.cmp(&b.name)),
            ContainerSortField::Cpu => rows.sort_by(|a, b| {
                b.cpu_pct
                    .partial_cmp(&a.cpu_pct)
                    .unwrap_or(std::cmp::Ordering::Equal)
            }),
            ContainerSortField::Mem => rows.sort_by_key(|c| std::cmp::Reverse(c.mem_used_bytes)),
            ContainerSortField::NetRx => rows.sort_by_key(|c| std::cmp::Reverse(c.net_rx_bytes)),
            ContainerSortField::NetTx => rows.sort_by_key(|c| std::cmp::Reverse(c.net_tx_bytes)),
            ContainerSortField::Uptime => rows.sort_by_key(|c| c.started_at_ms),
        }
        let is_asc = matches!(self.containers_sort_order, SortOrder::Asc);
        let is_name = matches!(self.containers_sort_field, ContainerSortField::Name);
        let is_uptime = matches!(self.containers_sort_field, ContainerSortField::Uptime);
        let default_asc = is_name || is_uptime;
        if is_asc != default_asc {
            rows.reverse();
        }
        self.sorted_filtered_containers_cache = rows;
    }

    fn request_container_action(&mut self, build: impl FnOnce(String, String) -> ConfirmAction) {
        if self.is_remote() {
            self.notify(Level::Warning, "Actions are disabled in remote mode");
            return;
        }
        if let Some(c) = self.selected_container() {
            // LOW-S3 / Slice C follow-up: pass the FULL 64-char container ID
            // to the engine so calls survive collisions on the 12-char short
            // prefix. The confirm prompt still renders the short prefix via
            // `short_id` for readability.
            self.confirm = Some(build(c.id_full.clone(), c.name.clone()));
        }
    }

    /// Queue a local shell against the visible row, never its shortened ID.
    pub fn request_container_exec(&mut self) {
        if self.is_remote() {
            self.notify(Level::Warning, "Actions are disabled in remote mode");
            return;
        }
        if !cfg!(unix) {
            self.notify(Level::Warning, "Interactive exec requires Linux or macOS");
            return;
        }
        if self.tab != Tab::Containers {
            self.notify(
                Level::Warning,
                "Select a running container on the Containers tab first",
            );
            return;
        }
        let Some(container) = self.selected_container() else {
            self.notify(Level::Warning, "Select a running container first");
            return;
        };
        if container.state != muxtop_core::containers::ContainerState::Running {
            self.notify(
                Level::Warning,
                "Interactive exec requires a running container",
            );
            return;
        }
        if self.container_engine.is_none() {
            self.notify(Level::Error, "No container engine is configured");
            return;
        }
        self.pending_exec = Some(container.id_full);
    }

    #[cfg(unix)]
    pub(crate) fn exec_cancellation(&self) -> CancellationToken {
        self.shutdown_token.clone()
    }

    /// F9 — graceful stop (SIGTERM + grace).
    pub fn request_container_stop(&mut self) {
        self.request_container_action(|id, name| ConfirmAction::StopContainer { id, name });
    }

    /// F10 — force kill (SIGKILL).
    pub fn request_container_kill(&mut self) {
        self.request_container_action(|id, name| ConfirmAction::KillContainer { id, name });
    }

    /// F11 — restart.
    pub fn request_container_restart(&mut self) {
        self.request_container_action(|id, name| ConfirmAction::RestartContainer { id, name });
    }

    /// Execute a confirmed container action by spawning a tokio task.
    ///
    /// PERF-L3: each spawn carries a clone of the AppState's
    /// [`CancellationToken`] and races the engine future against the token
    /// in `tokio::select!`. On TUI shutdown the future is dropped within the
    /// next scheduler tick rather than running detached.
    fn execute_container_action(&mut self, action: ConfirmAction) {
        let Some(engine) = self.container_engine.clone() else {
            self.notify(Level::Error, "No container engine is configured");
            return;
        };
        let tx = self.action_tx.clone();
        let token = self.shutdown_token.clone();
        match action {
            ConfirmAction::StopContainer { id, name } => {
                tokio::spawn(async move {
                    let outcome = tokio::select! {
                        biased;
                        _ = token.cancelled() => return,
                        result = engine.stop(&id, Some(10)) => match result {
                            Ok(()) => (Level::Success, format!("Container {name} stopped")),
                            Err(e) => (Level::Error, format!("Failed to stop {name}: {e}")),
                        },
                    };
                    let _ = tx.send(outcome);
                });
            }
            ConfirmAction::KillContainer { id, name } => {
                tokio::spawn(async move {
                    let outcome = tokio::select! {
                        biased;
                        _ = token.cancelled() => return,
                        result = engine.kill(&id) => match result {
                            Ok(()) => (Level::Success, format!("Container {name} killed")),
                            Err(e) => (Level::Error, format!("Failed to kill {name}: {e}")),
                        },
                    };
                    let _ = tx.send(outcome);
                });
            }
            ConfirmAction::RestartContainer { id, name } => {
                tokio::spawn(async move {
                    let outcome = tokio::select! {
                        biased;
                        _ = token.cancelled() => return,
                        result = engine.restart(&id) => match result {
                            Ok(()) => (Level::Success, format!("Container {name} restarted")),
                            Err(e) => (Level::Error, format!("Failed to restart {name}: {e}")),
                        },
                    };
                    let _ = tx.send(outcome);
                });
            }
            _ => unreachable!("execute_container_action only handles container variants"),
        }
    }

    /// Update the snapshot and recompute derived views.
    ///
    /// Ignored while paused: `Space` freezes the view so a fast-moving table
    /// can actually be read. Collection keeps running in the background, so
    /// resuming shows the present rather than replaying a backlog.
    pub fn apply_snapshot(&mut self, snapshot: SystemSnapshot) {
        if self.paused {
            return;
        }
        self.network_history.push(snapshot.networks.clone());
        if let Some(cs) = snapshot.containers.as_ref() {
            self.push_container_history(cs);
        }
        self.last_snapshot = Some(snapshot);
        self.recompute_visible();
        // PERF-H4: refresh the cached sorted+filtered container view exactly
        // once per snapshot; render and action paths read this slice.
        self.recompute_containers_view();
        // PERF-03: same contract for the Kube tab — the cached view must
        // never outlive the snapshot it indexes into.
        self.recompute_kube_view();
        self.clamp_view_selections();
        // PERF-H1: a fresh snapshot always changes what we'd paint.
        self.mark_dirty();
    }

    /// Snapshot replacement can shrink any list, including inactive tabs.
    fn clamp_view_selections(&mut self) {
        let counts = [
            self.process_count(),
            self.net_interface_count(),
            self.containers_count(),
            self.kube_count(),
            self.gpu_count(),
        ];
        let positions = [
            (&mut self.selected, &mut self.scroll_offset),
            (&mut self.net_selected, &mut self.net_scroll_offset),
            (
                &mut self.containers_selected,
                &mut self.containers_scroll_offset,
            ),
            (&mut self.kube_selected, &mut self.kube_scroll_offset),
            (&mut self.gpu_selected, &mut self.gpu_scroll_offset),
        ];
        for ((selected, offset), count) in positions.into_iter().zip(counts) {
            *selected = widgets::clamp_selection(*selected, count);
            *offset = widgets::clamp_selection(*offset, count);
        }
    }

    /// Per-container sparkline history cap — 60 samples, enough to fill the
    /// widest terminals while keeping memory tiny.
    const CONTAINER_HISTORY_LEN: usize = 60;

    /// Update CPU% + RX-delta rings for every container in `cs`. Containers
    /// that have disappeared since the last snapshot get their histories
    /// dropped to avoid unbounded growth.
    fn push_container_history(&mut self, cs: &muxtop_core::containers::ContainersSnapshot) {
        use std::collections::{HashMap, VecDeque};
        let mut seen = std::collections::HashSet::<String>::with_capacity(cs.containers.len());

        for c in &cs.containers {
            seen.insert(c.id.clone());

            // CPU ring.
            let cpu_ring = self
                .container_cpu_hist
                .entry(c.id.clone())
                .or_insert_with(|| VecDeque::with_capacity(Self::CONTAINER_HISTORY_LEN));
            if cpu_ring.len() >= Self::CONTAINER_HISTORY_LEN {
                cpu_ring.pop_front();
            }
            cpu_ring.push_back(c.cpu_pct);

            // RX delta vs last cumulative value.
            let last = self.container_rx_last.get(&c.id).copied();
            let delta = match last {
                Some(prev) => c.net_rx_bytes.saturating_sub(prev),
                None => 0,
            };
            self.container_rx_last.insert(c.id.clone(), c.net_rx_bytes);

            let rx_ring = self
                .container_rx_hist
                .entry(c.id.clone())
                .or_insert_with(|| VecDeque::with_capacity(Self::CONTAINER_HISTORY_LEN));
            if rx_ring.len() >= Self::CONTAINER_HISTORY_LEN {
                rx_ring.pop_front();
            }
            rx_ring.push_back(delta);
        }

        // Drop entries for containers that no longer exist.
        fn drop_missing<V>(map: &mut HashMap<String, V>, seen: &std::collections::HashSet<String>) {
            map.retain(|k, _| seen.contains(k));
        }
        drop_missing(&mut self.container_cpu_hist, &seen);
        drop_missing(&mut self.container_rx_hist, &seen);
        drop_missing(&mut self.container_rx_last, &seen);
    }

    /// CPU% history slice for the given container id. Empty when unknown.
    pub fn container_cpu_history(&self, id: &str) -> Vec<f32> {
        self.container_cpu_hist
            .get(id)
            .map(|r| r.iter().copied().collect())
            .unwrap_or_default()
    }

    /// RX delta history (bytes per tick) for the given container id.
    pub fn container_rx_deltas(&self, id: &str) -> Vec<u64> {
        self.container_rx_hist
            .get(id)
            .map(|r| r.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Visible container count (respects filter).
    pub fn containers_count(&self) -> usize {
        let Some(ref snapshot) = self.last_snapshot else {
            return 0;
        };
        let Some(ref cs) = snapshot.containers else {
            return 0;
        };
        if self.containers_filter_input.is_empty() {
            cs.containers.len()
        } else {
            let f = self.containers_filter_input.to_lowercase();
            cs.containers
                .iter()
                .filter(|c| {
                    c.name.to_lowercase().contains(&f)
                        || c.image.to_lowercase().contains(&f)
                        || c.id.to_lowercase().contains(&f)
                })
                .count()
        }
    }

    /// Recompute visible_processes and visible_tree from last_snapshot.
    pub fn recompute_visible(&mut self) {
        let Some(ref snapshot) = self.last_snapshot else {
            self.visible_processes.clear();
            self.visible_tree.clear();
            return;
        };

        // Filter
        let filtered = filter_processes(&snapshot.processes, &self.filter_input);

        // Tree — only build when tree_mode is active (G-09: skip when off).
        // G-07: tree is built from filtered list, not raw snapshot.
        // PERF-H3: reuse the `filtered` Vec we just produced instead of
        // re-running `filter_processes` (which previously cost an entire
        // O(N) lower-case + contains scan a second time per recompute).
        if self.tree_mode {
            let tree = build_process_tree(&filtered);
            self.visible_tree = flatten_tree(&tree);
        } else {
            self.visible_tree.clear();
        }

        // Sort (consumes `filtered` last so the tree above can borrow it).
        let mut sorted = filtered;
        sort_processes(&mut sorted, self.sort_field, self.sort_order);
        self.visible_processes = sorted;

        // Clamp selection and scroll_offset (G-06).
        let count = self.process_count();
        if count > 0 {
            self.selected = self.selected.min(count - 1);
            self.scroll_offset = self.scroll_offset.min(count - 1);
        } else {
            self.selected = 0;
            self.scroll_offset = 0;
        }
    }

    /// Re-run [`Self::recompute_visible`] iff at least [`FILTER_DEBOUNCE`]
    /// has elapsed since the last filter keystroke (PERF-H3). Bursts of fast
    /// typing therefore coalesce into one pipeline run instead of N.
    fn recompute_visible_debounced(&mut self) {
        let now = Instant::now();
        let should_recompute = self
            .last_filter_change
            .is_none_or(|last| now.duration_since(last) >= FILTER_DEBOUNCE);
        self.last_filter_change = Some(now);
        if should_recompute {
            self.recompute_visible();
        }
    }

    // -----------------------------------------------------------------
    // Input routing
    // -----------------------------------------------------------------

    /// Route a key press to whatever currently owns the keyboard.
    ///
    /// The order here *is* the modality model: confirm dialog, then overlay,
    /// then filter editing, then the keymap. 0.4 spread the same decision over
    /// four independent booleans, which is why `Esc` behaved differently
    /// depending on which one happened to be set.
    pub fn handle_key_event(&mut self, key: KeyEvent) {
        self.notifier.expire();

        // Ctrl+C quits from every mode, unconditionally. Nothing below may
        // capture it — a user who cannot leave is a user we have trapped.
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.quit();
            return;
        }

        if self.confirm.is_some() {
            self.handle_confirm_key(key);
            return;
        }

        match self.overlay {
            Overlay::Palette | Overlay::Command => {
                self.handle_palette_key(key);
                return;
            }
            Overlay::Help | Overlay::Log | Overlay::Inspector => {
                self.handle_scrollable_overlay_key(key);
                return;
            }
            Overlay::Actions => {
                self.handle_actions_key(key);
                return;
            }
            Overlay::None => {}
        }

        if self.filter_editing() {
            self.handle_filter_key(key);
            return;
        }

        if let Some(action) = keymap::resolve(key.code, key.modifiers, self.tab) {
            self.dispatch(action);
        }
        self.mark_dirty();
    }

    /// Perform an action, whatever asked for it: a key, the palette, the
    /// actions menu or a mouse click. One implementation, one behaviour.
    pub fn dispatch(&mut self, action: Action) {
        // A remote server owns its own processes and sockets; muxtop is a
        // viewer there. Refuse loudly rather than silently doing nothing.
        if action.is_local_only() && self.is_remote() {
            self.notify(Level::Warning, "Actions are disabled in remote mode");
            return;
        }

        match action {
            // -- application --
            Action::Quit => self.quit(),
            Action::Help => self.open_overlay(Overlay::Help),
            Action::Palette => self.open_palette(),
            Action::CommandMode => self.open_command_line(),
            Action::ToggleLog => self.open_overlay(Overlay::Log),
            Action::TogglePause => {
                self.paused = !self.paused;
                let msg = if self.paused {
                    "Paused — press Space to resume"
                } else {
                    "Resumed"
                };
                self.notify(Level::Info, msg);
            }
            Action::Refresh => {
                // Collection runs on its own schedule; the useful thing a
                // manual refresh can do is un-freeze a paused view.
                if self.paused {
                    self.paused = false;
                    self.notify(Level::Info, "Resumed");
                } else {
                    self.notify(Level::Info, "Refreshing on the next collector tick");
                }
            }

            // -- navigation --
            Action::NextTab => self.go_to_tab(self.tab.next()),
            Action::PrevTab => self.go_to_tab(self.tab.prev()),
            Action::GoTab(tab) => self.go_to_tab(tab),
            Action::NextSubview => self.cycle_subview(1),
            Action::PrevSubview => self.cycle_subview(-1),

            // -- table --
            Action::RowDown => self.move_selection(1),
            Action::RowUp => self.move_selection(-1),
            Action::RowPageDown => self.move_selection(Self::PAGE as isize),
            Action::RowPageUp => self.move_selection(-(Self::PAGE as isize)),
            Action::RowHalfDown => self.move_selection(Self::PAGE as isize / 2),
            Action::RowHalfUp => self.move_selection(-(Self::PAGE as isize) / 2),
            Action::RowFirst => self.select_index(0),
            Action::RowLast => self.select_index(self.item_count().saturating_sub(1)),
            Action::ColLeft => self.col_scroll = self.col_scroll.saturating_sub(1),
            Action::ColRight => self.col_scroll = (self.col_scroll + 1).min(Self::MAX_COL_SCROLL),
            Action::OpenInspector => {
                if self.item_count() == 0 {
                    self.notify(Level::Info, "Nothing selected");
                } else {
                    self.open_overlay(Overlay::Inspector);
                }
            }
            Action::ActionsMenu => {
                if self.tab_actions().is_empty() {
                    self.notify(Level::Info, "No actions available on this tab");
                } else {
                    self.actions_selected = 0;
                    self.open_overlay(Overlay::Actions);
                }
            }
            Action::CopySelection => self.copy_selection(),

            // -- sort & filter --
            Action::OpenFilter => self.set_filter_editing(true),
            Action::Back => self.go_back(),
            Action::CycleSort => self.cycle_sort(),
            Action::ReverseSort => self.reverse_sort(),

            // -- processes --
            Action::ToggleTree => {
                self.tree_mode = !self.tree_mode;
                self.selected = 0;
                self.scroll_offset = 0;
                self.recompute_visible();
            }
            Action::Renice(delta) => self.request_renice(delta),
            Action::Kill(signal) => self.request_kill(signal),

            // -- containers --
            Action::ContainerStop => self.request_container_stop(),
            Action::ContainerKill => self.request_container_kill(),
            Action::ContainerRestart => self.request_container_restart(),
            Action::ContainerExec => self.request_container_exec(),

            // -- kubernetes --
            Action::KubeSubview(sv) => self.switch_kube_subview(sv),
            Action::ToggleKubeScope => self.toggle_kube_scope(),

            // -- gpu --
            Action::GpuSubview(sv) => self.switch_gpu_subview(sv),
        }
    }

    /// Rows moved by a page key.
    const PAGE: usize = 20;
    /// Upper bound on horizontal column scrolling, so `l` cannot walk a table
    /// off the edge of the world.
    const MAX_COL_SCROLL: usize = 8;

    /// Switch tabs, resetting the per-view transient state that would be
    /// meaningless in the new context.
    fn go_to_tab(&mut self, tab: Tab) {
        if self.tab == tab {
            return;
        }
        self.tab = tab;
        self.col_scroll = 0;
        // Leaving a tab abandons a half-typed filter on it, but keeps the
        // filter itself: coming back should find the view as it was left.
        self.filter_active = false;
        self.net_filter_active = false;
        self.containers_filter_active = false;
        self.kube_filter_active = false;
    }

    /// `[` / `]` — walk the active tab's sub-views. Only Kubernetes has any
    /// today; the action exists so future tabs inherit the binding for free.
    fn cycle_subview(&mut self, delta: isize) {
        match self.tab {
            Tab::Kube => {
                const ORDER: [KubeSubview; 3] = [
                    KubeSubview::Pods,
                    KubeSubview::Nodes,
                    KubeSubview::Deployments,
                ];
                let idx = ORDER
                    .iter()
                    .position(|&s| s == self.kube_subview)
                    .unwrap_or(0) as isize;
                let next = (idx + delta).rem_euclid(ORDER.len() as isize) as usize;
                self.switch_kube_subview(ORDER[next]);
            }
            Tab::Gpu => {
                const ORDER: [GpuSubview; 2] = [GpuSubview::Devices, GpuSubview::Procs];
                let idx = ORDER
                    .iter()
                    .position(|&s| s == self.gpu_subview)
                    .unwrap_or(0) as isize;
                let next = (idx + delta).rem_euclid(ORDER.len() as isize) as usize;
                self.switch_gpu_subview(ORDER[next]);
            }
            _ => {}
        }
    }

    /// Move the cursor by `delta` rows, clamped to the list.
    fn move_selection(&mut self, delta: isize) {
        let count = self.item_count();
        if count == 0 {
            return;
        }
        let (sel, _) = self.selected_mut();
        let next = (*sel as isize + delta).clamp(0, count as isize - 1);
        *sel = next as usize;
    }

    fn select_index(&mut self, index: usize) {
        let count = self.item_count();
        if count == 0 {
            return;
        }
        let (sel, _) = self.selected_mut();
        *sel = index.min(count - 1);
    }

    /// `Esc` — one step back, every time.
    ///
    /// Overlay, then toasts, then filter editing, then the filter itself. It
    /// never quits: a monitor that exits on a stray Esc is a monitor that
    /// loses your place.
    fn go_back(&mut self) {
        if self.overlay != Overlay::None {
            self.close_overlay();
        } else if !self.notifier.is_empty() {
            self.notifier.dismiss_all();
        } else if self.filter_editing() {
            self.set_filter_editing(false);
        } else if !self.filter_text().is_empty() {
            self.clear_filter();
        }
    }

    // -----------------------------------------------------------------
    // Filters — one implementation for all four tabs
    // -----------------------------------------------------------------

    /// Whether the active tab's filter input has the keyboard.
    pub fn filter_editing(&self) -> bool {
        match self.tab {
            Tab::Gpu => self.gpu_filter_active,
            Tab::Network => self.net_filter_active,
            Tab::Containers => self.containers_filter_active,
            Tab::Kube => self.kube_filter_active,
            _ => self.filter_active,
        }
    }

    /// The active tab's filter text.
    pub fn filter_text(&self) -> &str {
        match self.tab {
            Tab::Gpu => &self.gpu_filter_input,
            Tab::Network => &self.net_filter_input,
            Tab::Containers => &self.containers_filter_input,
            Tab::Kube => &self.kube_filter_input,
            _ => &self.filter_input,
        }
    }

    fn set_filter_editing(&mut self, on: bool) {
        match self.tab {
            Tab::Gpu => self.gpu_filter_active = on,
            Tab::Network => self.net_filter_active = on,
            Tab::Containers => self.containers_filter_active = on,
            Tab::Kube => self.kube_filter_active = on,
            _ => self.filter_active = on,
        }
    }

    fn filter_text_mut(&mut self) -> &mut String {
        match self.tab {
            Tab::Gpu => &mut self.gpu_filter_input,
            Tab::Network => &mut self.net_filter_input,
            Tab::Containers => &mut self.containers_filter_input,
            Tab::Kube => &mut self.kube_filter_input,
            _ => &mut self.filter_input,
        }
    }

    /// Clear the active tab's filter.
    ///
    /// 0.4's palette command had no `Tab::Kube` arm, so "Clear filter" on the
    /// Kube tab silently cleared the *process* filter instead. Routing every
    /// caller through one method is what makes that class of bug impossible.
    pub fn clear_filter(&mut self) {
        self.filter_text_mut().clear();
        self.after_filter_change(true);
    }

    /// Replace the active tab's filter wholesale (used by `filter <text>`).
    pub fn set_filter(&mut self, text: impl Into<String>) {
        *self.filter_text_mut() = text.into();
        self.after_filter_change(true);
    }

    /// Re-run whatever the active tab derives from its filter.
    fn after_filter_change(&mut self, immediate: bool) {
        match self.tab {
            Tab::Containers => self.recompute_containers_view(),
            Tab::Kube => self.recompute_kube_view(),
            Tab::Gpu | Tab::Network => {}
            _ => {
                if immediate {
                    self.last_filter_change = None;
                    self.recompute_visible();
                } else {
                    self.recompute_visible_debounced();
                }
            }
        }
        // A filter change can shrink the list under the cursor.
        let count = self.item_count();
        let (sel, off) = self.selected_mut();
        *sel = widgets::clamp_selection(*sel, count);
        *off = 0;
        self.mark_dirty();
    }

    /// Maximum number of characters accepted in a filter input.
    const MAX_FILTER_LEN: usize = 256;

    /// Handle keys while a filter input has the keyboard.
    fn handle_filter_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                // First Esc leaves the input; the filter stays applied. A
                // second Esc (handled by `go_back`) clears it.
                self.set_filter_editing(false);
                self.after_filter_change(true);
            }
            KeyCode::Enter => {
                self.set_filter_editing(false);
                self.after_filter_change(true);
            }
            KeyCode::Backspace => {
                self.filter_text_mut().pop();
                self.after_filter_change(false);
            }
            // A control chord is a command we do not have here, not text.
            KeyCode::Char(_) if key.modifiers.contains(KeyModifiers::CONTROL) => {}
            KeyCode::Char(c) if self.filter_text().len() < Self::MAX_FILTER_LEN => {
                self.filter_text_mut().push(c);
                self.after_filter_change(false);
            }
            _ => {}
        }
        self.mark_dirty();
    }

    // -----------------------------------------------------------------
    // Sorting
    // -----------------------------------------------------------------

    fn cycle_sort(&mut self) {
        match self.tab {
            Tab::Network => self.net_sort_field = next_net_sort_field(self.net_sort_field),
            Tab::Containers => {
                self.containers_sort_field = next_container_sort_field(self.containers_sort_field);
                self.recompute_containers_view();
            }
            Tab::Kube => {
                self.kube_sort_field =
                    next_kube_sort_field(self.kube_sort_field, self.kube_subview);
                self.kube_selected = 0;
                self.kube_scroll_offset = 0;
                self.recompute_kube_view();
            }
            Tab::Gpu => {
                self.gpu_sort_field = next_gpu_sort_field(self.gpu_sort_field, self.gpu_subview);
                self.gpu_selected = 0;
                self.gpu_scroll_offset = 0;
            }
            _ => {
                self.sort_field = next_sort_field(self.sort_field);
                self.recompute_visible();
            }
        }
    }

    fn reverse_sort(&mut self) {
        let flip = |o: SortOrder| match o {
            SortOrder::Asc => SortOrder::Desc,
            SortOrder::Desc => SortOrder::Asc,
        };
        match self.tab {
            Tab::Network => self.net_sort_order = flip(self.net_sort_order),
            Tab::Containers => {
                self.containers_sort_order = flip(self.containers_sort_order);
                self.recompute_containers_view();
            }
            Tab::Kube => {
                self.kube_sort_order = flip(self.kube_sort_order);
                self.recompute_kube_view();
            }
            Tab::Gpu => self.gpu_sort_order = flip(self.gpu_sort_order),
            _ => {
                self.sort_order = flip(self.sort_order);
                self.recompute_visible();
            }
        }
    }

    // -----------------------------------------------------------------
    // Row identity — inspector, clipboard, argument commands
    // -----------------------------------------------------------------

    /// A short identifier for the selected row: PID, interface name, container
    /// id or Kubernetes object name, depending on the tab.
    pub fn selected_identifier(&self) -> Option<String> {
        match self.tab {
            Tab::Network => self
                .visible_interfaces()
                .get(self.net_selected)
                .map(|i| i.name.clone()),
            Tab::Containers => self
                .sorted_filtered_containers()
                .get(self.containers_selected)
                .map(|c| c.id_full.clone()),
            Tab::Kube => self.selected_kube_name(),
            Tab::Gpu => self.selected_gpu_name(),
            _ => self.selected_process().map(|p| p.pid.to_string()),
        }
    }

    /// Network interfaces in the same filtered and sorted order as the table.
    pub fn visible_interfaces(&self) -> Vec<muxtop_core::network::NetworkInterfaceSnapshot> {
        let Some(snapshot) = self.last_snapshot.as_ref() else {
            return Vec::new();
        };
        let filter = self.net_filter_input.to_lowercase();
        let mut rows: Vec<_> = snapshot
            .networks
            .interfaces
            .iter()
            .filter(|i| filter.is_empty() || i.name.to_lowercase().contains(&filter))
            .cloned()
            .collect();
        crate::ui::network::sort_interfaces(self, &mut rows);
        rows
    }

    /// Name of the selected Kubernetes object, whichever sub-view is active.
    ///
    /// Reads the same cache the table renders from. The previous version
    /// re-derived the filtered list here and took `nth(kube_selected)` from
    /// it — but `kube_selected` indexes the *sorted* order, so with any sort
    /// active this returned a different object than the one highlighted on
    /// screen. Sharing one projection makes that impossible rather than
    /// merely fixed.
    fn selected_kube_name(&self) -> Option<String> {
        let snap = self.last_snapshot.as_ref()?.kube.as_ref()?;
        let &idx = self.kube_view_cache.get(self.kube_selected)?;
        match self.kube_subview {
            KubeSubview::Pods => snap.pods.get(idx).map(|p| p.name.clone()),
            KubeSubview::Nodes => snap.nodes.get(idx).map(|n| n.name.clone()),
            KubeSubview::Deployments => snap.deployments.get(idx).map(|d| d.name.clone()),
        }
    }

    /// Identifier of the selected GPU row: the device's name, or the PID of
    /// the process holding VRAM.
    fn selected_gpu_name(&self) -> Option<String> {
        match self.gpu_subview {
            GpuSubview::Devices => crate::ui::gpu::visible_devices(self)
                .get(self.gpu_selected)
                .map(|d| d.name.clone()),
            GpuSubview::Procs => crate::ui::gpu::visible_procs(self)
                .get(self.gpu_selected)
                .map(|p| p.pid.to_string()),
        }
    }

    /// `y` — copy the selected row's identifier to the system clipboard.
    ///
    /// Uses OSC 52, which travels over ssh and through tmux, because muxtop's
    /// whole point is being useful on a machine you are not sitting at.
    fn copy_selection(&mut self) {
        let Some(id) = self.selected_identifier() else {
            self.notify(Level::Info, "Nothing selected");
            return;
        };
        match crate::clipboard::copy(&id) {
            Ok(()) => self.notify(Level::Success, format!("Copied {id}")),
            Err(e) => self.notify(Level::Error, format!("Clipboard unavailable: {e}")),
        }
    }

    // -----------------------------------------------------------------
    // Overlays
    // -----------------------------------------------------------------

    fn open_overlay(&mut self, overlay: Overlay) {
        self.overlay = overlay;
        self.overlay_scroll = 0;
        self.mark_dirty();
    }

    /// Close whatever overlay is open.
    pub fn close_overlay(&mut self) {
        self.overlay = Overlay::None;
        self.overlay_scroll = 0;
        self.palette.input.clear();
        self.palette.selected = 0;
        self.palette.arg = None;
        self.mark_dirty();
    }

    /// Whether the command palette is on screen.
    pub fn show_palette(&self) -> bool {
        matches!(self.overlay, Overlay::Palette | Overlay::Command)
    }

    /// Whether the palette is in typed-command mode rather than fuzzy mode.
    pub fn command_mode(&self) -> bool {
        self.overlay == Overlay::Command
    }

    /// Keys shared by the read-only, scrollable overlays.
    fn handle_scrollable_overlay_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.close_overlay(),
            KeyCode::Char('?') if self.overlay == Overlay::Help => self.close_overlay(),
            KeyCode::Enter if self.overlay == Overlay::Inspector => self.close_overlay(),
            KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.close_overlay()
            }
            KeyCode::Down | KeyCode::Char('j') => self.overlay_scroll += 1,
            KeyCode::Up | KeyCode::Char('k') => {
                self.overlay_scroll = self.overlay_scroll.saturating_sub(1)
            }
            KeyCode::PageDown => self.overlay_scroll += Self::PAGE,
            KeyCode::PageUp => self.overlay_scroll = self.overlay_scroll.saturating_sub(Self::PAGE),
            KeyCode::Home | KeyCode::Char('g') => self.overlay_scroll = 0,
            KeyCode::Char('y') if self.overlay == Overlay::Inspector => self.copy_selection(),
            _ => {}
        }
        self.mark_dirty();
    }

    /// The actions offered by `x` on the active tab, in keymap order.
    ///
    /// Derived from the keymap, so an action added there appears here without
    /// anybody remembering to add it.
    pub fn tab_actions(&self) -> Vec<(&'static str, &'static str, Action)> {
        keymap::for_tab(self.tab)
            // Tab-scoped only: `x` itself is a global action, and a menu that
            // offers to open itself is noise.
            .filter(|b| matches!(b.scope, keymap::Scope::Tab(t) if t == self.tab))
            .filter(|b| b.group == keymap::Group::Actions && b.primary)
            .filter(|b| !(self.is_remote() && b.action.is_local_only()))
            .map(|b| (b.display, b.label, b.action))
            .collect()
    }

    fn handle_actions_key(&mut self, key: KeyEvent) {
        let actions = self.tab_actions();
        match key.code {
            KeyCode::Esc | KeyCode::Char('x') => self.close_overlay(),
            KeyCode::Down | KeyCode::Char('j') => {
                self.actions_selected =
                    widgets::clamp_selection(self.actions_selected + 1, actions.len());
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.actions_selected = self.actions_selected.saturating_sub(1);
            }
            KeyCode::Enter => {
                if let Some(&(_, _, action)) = actions.get(self.actions_selected) {
                    self.close_overlay();
                    self.dispatch(action);
                }
            }
            _ => {}
        }
        self.mark_dirty();
    }

    // -----------------------------------------------------------------
    // Confirmation
    // -----------------------------------------------------------------

    /// Request to kill the currently selected process (opens confirm dialog).
    fn request_kill(&mut self, signal: Signal) {
        match self.selected_process() {
            Some(proc) => {
                self.confirm = Some(ConfirmAction::Kill {
                    pid: proc.pid,
                    name: proc.name.clone(),
                    signal,
                });
            }
            None => self.notify(Level::Info, "No process selected"),
        }
    }

    /// Request to renice the currently selected process (opens confirm dialog).
    fn request_renice(&mut self, delta: i32) {
        match self.selected_process() {
            Some(proc) => {
                self.confirm = Some(ConfirmAction::Renice {
                    pid: proc.pid,
                    name: proc.name.clone(),
                    delta,
                });
            }
            None => self.notify(Level::Info, "No process selected"),
        }
    }

    /// Handle keys while the confirm dialog is up.
    ///
    /// Cancel is the default: `Esc`, `n` and any unbound key leave the dialog
    /// standing rather than doing something irreversible.
    fn handle_confirm_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                if let Some(action) = self.confirm.take() {
                    self.execute_confirm(action);
                }
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.confirm = None;
            }
            _ => {} // Block all other keys
        }
        self.mark_dirty();
    }

    /// Execute a confirmed action and report the outcome.
    fn execute_confirm(&mut self, action: ConfirmAction) {
        match action {
            ConfirmAction::Kill { pid, name, signal } => {
                let sig_name = match signal {
                    Signal::Kill => "SIGKILL",
                    Signal::Term => "SIGTERM",
                };
                match actions::kill_process(pid, signal) {
                    Ok(()) => self.notify(
                        Level::Success,
                        format!("Sent {sig_name} to {name} (PID {pid})"),
                    ),
                    Err(e) => self.set_error(format!("Kill failed: {e}")),
                }
            }
            ConfirmAction::Renice { pid, name, delta } => {
                let current = actions::get_process_priority(pid).unwrap_or(0);
                let new_nice = current + delta;
                match actions::renice_process(pid, new_nice) {
                    Ok(()) => self.notify(
                        Level::Success,
                        format!("Reniced {name} (PID {pid}) to nice={new_nice}"),
                    ),
                    Err(e) => self.set_error(format!("Renice failed: {e}")),
                }
            }
            action @ (ConfirmAction::StopContainer { .. }
            | ConfirmAction::KillContainer { .. }
            | ConfirmAction::RestartContainer { .. }) => {
                self.execute_container_action(action);
            }
        }
    }

    // -----------------------------------------------------------------
    // Command palette
    // -----------------------------------------------------------------

    /// Commands that cannot work against a remote host.
    const REMOTE_BLOCKED_COMMANDS: &[Command] = &[
        Command::KillProcess,
        Command::ForceKillProcess,
        Command::KillNamed,
        Command::NiceDown,
        Command::NiceUp,
        Command::StopContainer,
        Command::KillContainer,
        Command::RestartContainer,
        Command::ExecContainer,
        Command::StopNamed,
        Command::RestartNamed,
        Command::KubeToggleScope,
    ];

    /// Returns the list of commands to exclude from the palette (empty in local mode).
    fn excluded_commands(&self) -> &[Command] {
        if self.is_remote() {
            Self::REMOTE_BLOCKED_COMMANDS
        } else {
            &[]
        }
    }

    /// Open the fuzzy command palette.
    fn open_palette(&mut self) {
        self.overlay = Overlay::Palette;
        self.palette.input.clear();
        self.palette.selected = 0;
        self.refilter_palette();
        self.mark_dirty();
    }

    /// Open the typed command line (`:`), which is the same registry with the
    /// argument forms reachable.
    fn open_command_line(&mut self) {
        self.overlay = Overlay::Command;
        self.palette.input.clear();
        self.palette.selected = 0;
        self.refilter_palette();
        self.mark_dirty();
    }

    fn refilter_palette(&mut self) {
        let excluded = self.excluded_commands().to_vec();
        let tab = self.tab;
        self.palette.refilter_ctx(&excluded, Some(tab));
    }

    /// Handle keys while the palette or command line is open.
    fn handle_palette_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.close_overlay(),
            // Both palette keys toggle it shut again.
            KeyCode::Char('p' | 'k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.close_overlay()
            }
            // Any other control chord is a command we do not have, not text:
            // inserting its letter into the query would be surprising.
            KeyCode::Char(_) if key.modifiers.contains(KeyModifiers::CONTROL) => {}
            KeyCode::Enter => {
                if let Some(&(cmd, _)) = self.palette.filtered.get(self.palette.selected) {
                    let arg = self.palette.arg.clone();
                    self.palette.remember(cmd);
                    self.close_overlay();
                    self.execute_invocation(cmd, arg);
                }
            }
            KeyCode::Down => {
                self.palette.selected = widgets::clamp_selection(
                    self.palette.selected + 1,
                    self.palette.filtered.len(),
                );
            }
            KeyCode::Up => {
                self.palette.selected = self.palette.selected.saturating_sub(1);
            }
            KeyCode::Backspace => {
                self.palette.input.pop();
                self.refilter_palette();
            }
            KeyCode::Char(c) if self.palette.input.len() < Self::MAX_FILTER_LEN => {
                self.palette.input.push(c);
                self.refilter_palette();
            }
            _ => {}
        }
        self.mark_dirty();
    }

    /// Execute a palette command with its optional argument.
    pub fn execute_invocation(&mut self, cmd: Command, arg: Option<String>) {
        match (cmd, arg) {
            (Command::KillNamed, Some(name)) => self.kill_named(&name),
            (Command::StopNamed, Some(name)) => {
                self.container_named(&name, |id, name| ConfirmAction::StopContainer { id, name })
            }
            (Command::RestartNamed, Some(name)) => self.container_named(&name, |id, name| {
                ConfirmAction::RestartContainer { id, name }
            }),
            (Command::SortBy, Some(field)) => self.sort_by_name(&field),
            (Command::FilterBy, Some(text)) => self.set_filter(text),
            (Command::SetTheme, Some(name)) => self.set_theme(&name),
            (Command::GoToTab, Some(name)) => self.go_to_tab_named(&name),
            (cmd, _) if cmd.is_verb_only() => {
                self.notify(Level::Info, format!("Usage: {}", cmd.shortcut()));
            }
            (cmd, _) => self.execute_command(cmd),
        }
    }

    /// `kill <name>` — target the busiest process whose name matches.
    ///
    /// Ambiguity is resolved by the confirmation dialog, which names the exact
    /// target, rather than by guessing silently.
    fn kill_named(&mut self, needle: &str) {
        let needle_lower = needle.to_lowercase();
        let matches: Vec<&ProcessInfo> = self
            .visible_processes
            .iter()
            .filter(|p| {
                p.name.to_lowercase().contains(&needle_lower)
                    || p.command.to_lowercase().contains(&needle_lower)
            })
            .collect();

        match matches.first() {
            None => self.notify(Level::Warning, format!("No process matches `{needle}`")),
            Some(target) => {
                let (pid, name) = (target.pid, target.name.clone());
                let extra = matches.len();
                if extra > 1 {
                    self.notify(
                        Level::Info,
                        format!("{extra} processes match `{needle}` — targeting the busiest"),
                    );
                }
                self.confirm = Some(ConfirmAction::Kill {
                    pid,
                    name,
                    signal: Signal::Term,
                });
            }
        }
    }

    /// `stop <name>` / `restart <name>` — same contract for containers.
    fn container_named(
        &mut self,
        needle: &str,
        build: impl FnOnce(String, String) -> ConfirmAction,
    ) {
        if self.is_remote() {
            self.notify(Level::Warning, "Actions are disabled in remote mode");
            return;
        }
        let needle_lower = needle.to_lowercase();
        let found = self
            .sorted_filtered_containers()
            .iter()
            .find(|c| {
                c.name.to_lowercase().contains(&needle_lower)
                    || c.image.to_lowercase().contains(&needle_lower)
                    || c.id.to_lowercase().starts_with(&needle_lower)
            })
            .map(|c| (c.id_full.clone(), c.name.clone()));

        match found {
            Some((id, name)) => self.confirm = Some(build(id, name)),
            None => self.notify(Level::Warning, format!("No container matches `{needle}`")),
        }
    }

    /// `sort <field>` — resolved against the active tab's own columns.
    fn sort_by_name(&mut self, field: &str) {
        let f = field.trim().to_lowercase();
        let ok = match self.tab {
            Tab::Network => {
                let set = match f.as_str() {
                    "name" | "interface" => Some(NetworkSortField::Name),
                    "rx" | "down" => Some(NetworkSortField::RxRate),
                    "tx" | "up" => Some(NetworkSortField::TxRate),
                    "errors" | "err" => Some(NetworkSortField::Errors),
                    _ => None,
                };
                if let Some(s) = set {
                    self.net_sort_field = s;
                }
                set.is_some()
            }
            Tab::Containers => {
                let set = match f.as_str() {
                    "name" => Some(ContainerSortField::Name),
                    "cpu" => Some(ContainerSortField::Cpu),
                    "mem" | "memory" => Some(ContainerSortField::Mem),
                    "rx" | "net" => Some(ContainerSortField::NetRx),
                    "tx" => Some(ContainerSortField::NetTx),
                    "uptime" | "age" => Some(ContainerSortField::Uptime),
                    _ => None,
                };
                if let Some(s) = set {
                    self.containers_sort_field = s;
                    self.recompute_containers_view();
                }
                set.is_some()
            }
            Tab::Kube => {
                let set = named_kube_sort(&f, self.kube_subview);
                if let Some(sort) = set {
                    self.kube_sort_field = sort;
                    self.recompute_kube_view();
                }
                set.is_some()
            }
            Tab::Gpu => {
                let set = named_gpu_sort(&f, self.gpu_subview);
                if let Some(sort) = set {
                    self.gpu_sort_field = sort;
                }
                set.is_some()
            }
            _ => {
                let set = match f.as_str() {
                    "cpu" => Some(SortField::Cpu),
                    "mem" | "memory" => Some(SortField::Mem),
                    "pid" => Some(SortField::Pid),
                    "name" | "command" => Some(SortField::Name),
                    "user" => Some(SortField::User),
                    _ => None,
                };
                if let Some(s) = set {
                    self.sort_field = s;
                    self.recompute_visible();
                }
                set.is_some()
            }
        };
        if ok {
            self.notify(Level::Info, format!("Sorted by {f}"));
        } else {
            self.notify(
                Level::Warning,
                format!("`{f}` is not a sortable column on this tab"),
            );
        }
    }

    /// `theme <name>`.
    fn set_theme(&mut self, name: &str) {
        match name.parse::<ThemeKind>() {
            Ok(kind) => {
                self.theme_kind = kind;
                self.notify(Level::Success, format!("Theme: {kind}"));
            }
            Err(e) => self.notify(Level::Warning, e),
        }
    }

    /// `tab <name>`.
    fn go_to_tab_named(&mut self, name: &str) {
        let n = name.trim().to_lowercase();
        let found = Tab::ALL
            .iter()
            .copied()
            .find(|t| t.label().to_lowercase().starts_with(&n));
        match found {
            Some(tab) => self.go_to_tab(tab),
            None => self.notify(Level::Warning, format!("No tab named `{name}`")),
        }
    }

    /// Execute an argument-free command from the palette.
    fn execute_command(&mut self, cmd: Command) {
        // Block local-only actions in remote mode.
        if self.is_remote() && Self::REMOTE_BLOCKED_COMMANDS.contains(&cmd) {
            self.notify(Level::Warning, "Actions are disabled in remote mode");
            return;
        }

        match cmd {
            Command::Quit => self.dispatch(Action::Quit),
            Command::ToggleTreeView => {
                // Reachable from any tab through the palette, but it only means
                // something on the process table — go there first.
                self.go_to_tab(Tab::Processes);
                self.dispatch(Action::ToggleTree);
            }
            Command::SortByCpu => self.set_process_sort(SortField::Cpu),
            Command::SortByMem => self.set_process_sort(SortField::Mem),
            Command::SortByPid => self.set_process_sort(SortField::Pid),
            Command::SortByName => self.set_process_sort(SortField::Name),
            Command::SortByUser => self.set_process_sort(SortField::User),
            Command::ToggleSortOrder => self.reverse_sort(),
            Command::CycleSort => self.cycle_sort(),
            Command::SwitchToGeneral => self.go_to_tab(Tab::General),
            Command::SwitchToProcesses => self.go_to_tab(Tab::Processes),
            Command::SwitchToNetwork => self.go_to_tab(Tab::Network),
            Command::SwitchToContainers => self.go_to_tab(Tab::Containers),
            Command::SwitchToKube => self.go_to_tab(Tab::Kube),
            Command::OpenFilter => self.set_filter_editing(true),
            Command::ClearFilter => self.clear_filter(),
            Command::NextTab => self.go_to_tab(self.tab.next()),
            Command::PrevTab => self.go_to_tab(self.tab.prev()),
            Command::KillProcess => self.request_kill(Signal::Term),
            Command::ForceKillProcess => self.request_kill(Signal::Kill),
            Command::NiceDown => self.request_renice(1),
            Command::NiceUp => self.request_renice(-1),
            Command::SortNetByRx => self.net_sort_field = NetworkSortField::RxRate,
            Command::SortNetByTx => self.net_sort_field = NetworkSortField::TxRate,
            Command::SortNetByName => self.net_sort_field = NetworkSortField::Name,
            Command::SortNetByErrors => self.net_sort_field = NetworkSortField::Errors,
            Command::SortContainersByCpu => self.set_container_sort(ContainerSortField::Cpu),
            Command::SortContainersByMem => self.set_container_sort(ContainerSortField::Mem),
            Command::SortContainersByName => self.set_container_sort(ContainerSortField::Name),
            Command::SortContainersByNetRx => self.set_container_sort(ContainerSortField::NetRx),
            Command::StopContainer => self.request_container_stop(),
            Command::KillContainer => self.request_container_kill(),
            Command::RestartContainer => self.request_container_restart(),
            Command::ExecContainer => self.request_container_exec(),
            Command::ShowHelp => self.open_overlay(Overlay::Help),
            Command::TogglePause => self.dispatch(Action::TogglePause),
            Command::RefreshNow => self.dispatch(Action::Refresh),
            Command::ShowLog => self.open_overlay(Overlay::Log),
            Command::InspectRow => self.dispatch(Action::OpenInspector),
            Command::CopyRowId => self.copy_selection(),
            Command::KubePods => self.kube_command(KubeSubview::Pods),
            Command::KubeNodes => self.kube_command(KubeSubview::Nodes),
            Command::KubeDeployments => self.kube_command(KubeSubview::Deployments),
            Command::KubeToggleScope => {
                self.go_to_tab(Tab::Kube);
                self.toggle_kube_scope();
            }
            // Verb forms are handled by `execute_invocation`; reaching here
            // means the user picked one without typing an argument.
            Command::KillNamed
            | Command::StopNamed
            | Command::RestartNamed
            | Command::SortBy
            | Command::FilterBy
            | Command::SetTheme
            | Command::GoToTab => {
                self.notify(Level::Info, format!("Usage: {}", cmd.shortcut()));
            }
            Command::SwitchToGpu => {
                self.tab = Tab::Gpu;
            }
        }
    }

    fn set_process_sort(&mut self, field: SortField) {
        self.sort_field = field;
        self.recompute_visible();
    }

    fn set_container_sort(&mut self, field: ContainerSortField) {
        self.containers_sort_field = field;
        self.recompute_containers_view();
    }

    fn kube_command(&mut self, sv: KubeSubview) {
        self.go_to_tab(Tab::Kube);
        self.switch_kube_subview(sv);
    }

    // -----------------------------------------------------------------
    // Mouse
    // -----------------------------------------------------------------

    /// Handle a mouse event.
    ///
    /// Every gesture here has a keyboard equivalent — muxtop has to be usable
    /// on a bare console with no pointer, and on a terminal where the user has
    /// turned mouse capture off to keep native text selection.
    ///
    /// Two bugs are fixed relative to 0.4: the wheel moved the *process* scroll
    /// offset regardless of the active tab, and it moved the offset without the
    /// selection, so the very next frame snapped the view back and the wheel
    /// appeared dead.
    pub fn handle_mouse_event(&mut self, mouse: MouseEvent) {
        if self.overlay != Overlay::None {
            match mouse.kind {
                MouseEventKind::ScrollDown => self.overlay_scroll += Self::WHEEL_ROWS,
                MouseEventKind::ScrollUp => {
                    self.overlay_scroll = self.overlay_scroll.saturating_sub(Self::WHEEL_ROWS)
                }
                _ => return,
            }
            self.mark_dirty();
            return;
        }

        match mouse.kind {
            MouseEventKind::ScrollDown => self.scroll_rows(Self::WHEEL_ROWS as isize),
            MouseEventKind::ScrollUp => self.scroll_rows(-(Self::WHEEL_ROWS as isize)),
            _ => return,
        }
        self.mark_dirty();
    }

    /// Rows travelled per wheel notch.
    const WHEEL_ROWS: usize = 3;

    /// Scroll the active tab's table, carrying the selection along so the
    /// viewport actually stays where the user put it.
    fn scroll_rows(&mut self, delta: isize) {
        let count = self.item_count();
        if count == 0 {
            return;
        }
        let (sel, off) = self.selected_mut();
        let new_off = (*off as isize + delta).max(0) as usize;
        *off = new_off.min(count.saturating_sub(1));
        // Keep the cursor inside the window we just scrolled to.
        *sel = (*sel).max(*off).min(count - 1);
    }
}

/// Resolve names only within the active sub-view's sortable columns.
fn named_kube_sort(field: &str, view: KubeSubview) -> Option<KubeSortField> {
    use KubeSortField::*;
    Some(match (view, field) {
        (KubeSubview::Pods, "name") => PodName,
        (KubeSubview::Pods, "cpu") => PodCpu,
        (KubeSubview::Pods, "mem" | "memory") => PodMem,
        (KubeSubview::Pods, "restarts") => PodRestarts,
        (KubeSubview::Pods, "age") => PodAge,
        (KubeSubview::Pods, "phase" | "status") => PodPhase,
        (KubeSubview::Nodes, "name") => NodeName,
        (KubeSubview::Nodes, "cpu") => NodeCpuPct,
        (KubeSubview::Nodes, "mem" | "memory") => NodeMemPct,
        (KubeSubview::Nodes, "pods") => NodePodCount,
        (KubeSubview::Nodes, "age") => NodeAge,
        (KubeSubview::Deployments, "namespace" | "ns") => DeployNamespace,
        (KubeSubview::Deployments, "name") => DeployName,
        (KubeSubview::Deployments, "ready") => DeployReadyRatio,
        (KubeSubview::Deployments, "age") => DeployAge,
        _ => return None,
    })
}

fn named_gpu_sort(field: &str, view: GpuSubview) -> Option<GpuSortField> {
    use GpuSortField::*;
    Some(match (view, field) {
        (GpuSubview::Devices, "index") => DeviceIndex,
        (GpuSubview::Devices, "name") => DeviceName,
        (GpuSubview::Devices, "gpu" | "util" | "utilization") => DeviceUtil,
        (GpuSubview::Devices, "mem" | "memory" | "vram") => DeviceMem,
        (GpuSubview::Devices, "temp" | "temperature") => DeviceTemp,
        (GpuSubview::Devices, "power") => DevicePower,
        (GpuSubview::Procs, "pid") => ProcPid,
        (GpuSubview::Procs, "name" | "command") => ProcName,
        (GpuSubview::Procs, "mem" | "memory" | "vram") => ProcMem,
        (GpuSubview::Procs, "device" | "index") => ProcDevice,
        _ => return None,
    })
}

/// Cycle to the next sort field.
fn next_sort_field(field: SortField) -> SortField {
    match field {
        SortField::Cpu => SortField::Mem,
        SortField::Mem => SortField::Pid,
        SortField::Pid => SortField::Name,
        SortField::Name => SortField::User,
        SortField::User => SortField::Cpu,
    }
}

/// Cycle to the next network sort field.
fn next_net_sort_field(field: NetworkSortField) -> NetworkSortField {
    match field {
        NetworkSortField::RxRate => NetworkSortField::TxRate,
        NetworkSortField::TxRate => NetworkSortField::Name,
        NetworkSortField::Name => NetworkSortField::Errors,
        NetworkSortField::Errors => NetworkSortField::TotalRx,
        NetworkSortField::TotalRx => NetworkSortField::TotalTx,
        NetworkSortField::TotalTx => NetworkSortField::RxRate,
    }
}

/// Cycle to the next GPU sort field, scoped to the active sub-view.
///
/// Like [`next_kube_sort_field`], an out-of-domain value (a Devices field
/// while the Procs sub-view is active) recovers to the active sub-view's
/// default rather than getting stuck.
fn next_gpu_sort_field(field: GpuSortField, sv: GpuSubview) -> GpuSortField {
    match sv {
        GpuSubview::Devices => match field {
            GpuSortField::DeviceIndex => GpuSortField::DeviceUtil,
            GpuSortField::DeviceUtil => GpuSortField::DeviceMem,
            GpuSortField::DeviceMem => GpuSortField::DeviceTemp,
            GpuSortField::DeviceTemp => GpuSortField::DevicePower,
            GpuSortField::DevicePower => GpuSortField::DeviceName,
            GpuSortField::DeviceName => GpuSortField::DeviceIndex,
            _ => GpuSortField::default_for(sv),
        },
        GpuSubview::Procs => match field {
            GpuSortField::ProcMem => GpuSortField::ProcPid,
            GpuSortField::ProcPid => GpuSortField::ProcName,
            GpuSortField::ProcName => GpuSortField::ProcDevice,
            GpuSortField::ProcDevice => GpuSortField::ProcMem,
            _ => GpuSortField::default_for(sv),
        },
    }
}

/// Cycle to the next Kube sort field, scoped to the active sub-view.
///
/// An out-of-domain `field` (e.g. holding `PodCpu` while the user just
/// switched to the Nodes view) recovers to the default of the active
/// sub-view rather than panicking — this matches the contract that
/// `switch_kube_subview` resets sort_field, but defensive against future
/// callers that mutate the field directly.
fn next_kube_sort_field(field: KubeSortField, sv: KubeSubview) -> KubeSortField {
    match sv {
        KubeSubview::Pods => match field {
            KubeSortField::PodCpu => KubeSortField::PodMem,
            KubeSortField::PodMem => KubeSortField::PodName,
            KubeSortField::PodName => KubeSortField::PodRestarts,
            KubeSortField::PodRestarts => KubeSortField::PodAge,
            KubeSortField::PodAge => KubeSortField::PodPhase,
            KubeSortField::PodPhase => KubeSortField::PodCpu,
            _ => KubeSortField::default_for(sv),
        },
        KubeSubview::Nodes => match field {
            KubeSortField::NodeCpuPct => KubeSortField::NodeMemPct,
            KubeSortField::NodeMemPct => KubeSortField::NodeName,
            KubeSortField::NodeName => KubeSortField::NodePodCount,
            KubeSortField::NodePodCount => KubeSortField::NodeAge,
            KubeSortField::NodeAge => KubeSortField::NodeCpuPct,
            _ => KubeSortField::default_for(sv),
        },
        KubeSubview::Deployments => match field {
            KubeSortField::DeployName => KubeSortField::DeployReadyRatio,
            KubeSortField::DeployReadyRatio => KubeSortField::DeployNamespace,
            KubeSortField::DeployNamespace => KubeSortField::DeployAge,
            KubeSortField::DeployAge => KubeSortField::DeployName,
            _ => KubeSortField::default_for(sv),
        },
    }
}

/// Cycle to the next container sort field.
fn next_container_sort_field(field: ContainerSortField) -> ContainerSortField {
    match field {
        ContainerSortField::Cpu => ContainerSortField::Mem,
        ContainerSortField::Mem => ContainerSortField::Name,
        ContainerSortField::Name => ContainerSortField::NetRx,
        ContainerSortField::NetRx => ContainerSortField::NetTx,
        ContainerSortField::NetTx => ContainerSortField::Uptime,
        ContainerSortField::Uptime => ContainerSortField::Cpu,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_process(pid: u32, name: &str, cpu: f32, mem: u64) -> ProcessInfo {
        ProcessInfo {
            pid,
            parent_pid: None,
            name: name.to_string(),
            command: format!("/usr/bin/{name}"),
            user: "user".to_string(),
            cpu_percent: cpu,
            memory_bytes: mem,
            memory_percent: 0.0,
            status: "Running".to_string(),
        }
    }

    fn make_snapshot(processes: Vec<ProcessInfo>) -> SystemSnapshot {
        use muxtop_core::network::NetworkSnapshot;
        use muxtop_core::system::{CpuSnapshot, LoadSnapshot, MemorySnapshot};
        SystemSnapshot {
            cpu: CpuSnapshot {
                global_usage: 25.0,
                cores: vec![],
            },
            memory: MemorySnapshot {
                total: 16_000_000_000,
                used: 8_000_000_000,
                available: 8_000_000_000,
                swap_total: 0,
                swap_used: 0,
            },
            load: LoadSnapshot {
                one: 1.0,
                five: 0.8,
                fifteen: 0.5,
                uptime_secs: 3600,
            },
            processes,
            networks: NetworkSnapshot {
                interfaces: vec![],
                total_rx: 0,
                total_tx: 0,
            },
            containers: None,
            kube: None,
            gpu: None,
            timestamp_ms: 0,
        }
    }

    // -- Tab tests (STORY-01) --

    #[test]
    fn test_tab_default_is_general() {
        assert_eq!(Tab::default(), Tab::General);
    }

    /// Derived from `Tab::ALL` rather than hard-coded pairs: adding a tab
    /// used to mean editing half a dozen tests that only cared about the
    /// order being a closed cycle.
    #[test]
    fn test_tab_next_cycles() {
        for (i, tab) in Tab::ALL.iter().enumerate() {
            let expected = Tab::ALL[(i + 1) % Tab::ALL.len()];
            assert_eq!(tab.next(), expected, "next() from {tab}");
        }
        // The cycle is closed: walking ALL.len() steps returns to the start.
        let mut tab = Tab::General;
        for _ in 0..Tab::ALL.len() {
            tab = tab.next();
        }
        assert_eq!(tab, Tab::General);
    }

    #[test]
    fn test_tab_prev_cycles() {
        for (i, tab) in Tab::ALL.iter().enumerate() {
            let expected = Tab::ALL[(i + Tab::ALL.len() - 1) % Tab::ALL.len()];
            assert_eq!(tab.prev(), expected, "prev() from {tab}");
        }
    }

    #[test]
    fn test_tab_label_values() {
        assert_eq!(Tab::General.label(), "General");
        assert_eq!(Tab::Processes.label(), "Processes");
        assert_eq!(Tab::Network.label(), "Network");
        assert_eq!(Tab::Containers.label(), "Containers");
        assert_eq!(Tab::Kube.label(), "Kube");
        assert_eq!(Tab::Gpu.label(), "GPU");
    }

    #[test]
    fn test_tab_display() {
        assert_eq!(format!("{}", Tab::General), "General");
        assert_eq!(format!("{}", Tab::Processes), "Processes");
        assert_eq!(format!("{}", Tab::Network), "Network");
        assert_eq!(format!("{}", Tab::Containers), "Containers");
        assert_eq!(format!("{}", Tab::Kube), "Kube");
        assert_eq!(format!("{}", Tab::Gpu), "GPU");
    }

    #[test]
    fn test_tab_all_contains_all() {
        assert!(Tab::ALL.contains(&Tab::General));
        assert!(Tab::ALL.contains(&Tab::Processes));
        assert!(Tab::ALL.contains(&Tab::Network));
        assert!(Tab::ALL.contains(&Tab::Containers));
        assert!(Tab::ALL.contains(&Tab::Kube));
        assert!(Tab::ALL.contains(&Tab::Gpu));
        assert_eq!(Tab::ALL.len(), 6);
    }

    // -- AppState defaults (STORY-02) --

    #[test]
    fn kube_scope_toggle_without_engine_explains_itself() {
        // Remote mode and `--no-kube` both leave `cluster_engine` as None.
        // The key must report why nothing happened rather than no-op
        // silently — and must not reach `tokio::spawn`, which would panic
        // outside a runtime.
        let mut app = AppState::new();
        app.tab = Tab::Kube;
        assert!(app.cluster_engine.is_none());

        app.handle_key_event(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT));

        let msg = app
            .active_status()
            .map(|t| t.text.clone())
            .expect("A on the Kube tab must set a status message");
        assert!(
            msg.contains("local-only"),
            "message does not explain the limitation: {msg}"
        );
    }

    #[test]
    fn kube_scope_toggle_is_scoped_to_the_kube_tab() {
        // `A` is a bare letter — it must not fire from other tabs, where it
        // is free for future bindings.
        let mut app = AppState::new();
        app.tab = Tab::Processes;
        app.handle_key_event(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT));
        assert!(app.active_status().is_none());
    }

    #[test]
    fn test_app_state_defaults() {
        let app = AppState::new();
        assert_eq!(app.tab, Tab::General);
        assert!(matches!(app.sort_field, SortField::Cpu));
        assert!(matches!(app.sort_order, SortOrder::Desc));
        assert!(app.filter_input.is_empty());
        assert!(!app.filter_active);
        assert!(!app.tree_mode);
        assert_eq!(app.selected, 0);
        assert_eq!(app.scroll_offset, 0);
        assert!(!app.show_palette());
        assert!(app.running());
        assert!(app.last_snapshot.is_none());
    }

    #[test]
    fn test_app_state_running_and_quit() {
        let mut app = AppState::new();
        assert!(app.running());
        app.quit();
        assert!(!app.running());
    }

    #[test]
    fn test_selected_process_none_initially() {
        let app = AppState::new();
        assert!(app.selected_process().is_none());
    }

    #[test]
    fn test_apply_snapshot_populates_visible() {
        let mut app = AppState::new();
        let snap = make_snapshot(vec![
            make_process(1, "firefox", 50.0, 1000),
            make_process(2, "chrome", 30.0, 2000),
        ]);
        app.apply_snapshot(snap);
        assert!(!app.visible_processes.is_empty());
        assert!(app.last_snapshot.is_some());
    }

    #[test]
    fn test_apply_snapshot_sorts_cpu_desc() {
        let mut app = AppState::new();
        app.sort_field = SortField::Cpu;
        app.sort_order = SortOrder::Desc;
        let snap = make_snapshot(vec![
            make_process(1, "low", 10.0, 100),
            make_process(2, "high", 90.0, 200),
            make_process(3, "mid", 50.0, 300),
        ]);
        app.apply_snapshot(snap);
        let cpus: Vec<f32> = app
            .visible_processes
            .iter()
            .map(|p| p.cpu_percent)
            .collect();
        assert_eq!(cpus, vec![90.0, 50.0, 10.0]);
    }

    #[test]
    fn test_apply_snapshot_filters() {
        let mut app = AppState::new();
        app.filter_input = "fire".to_string();
        let snap = make_snapshot(vec![
            make_process(1, "firefox", 50.0, 1000),
            make_process(2, "chrome", 30.0, 2000),
            make_process(3, "firefox-esr", 20.0, 500),
        ]);
        app.apply_snapshot(snap);
        assert_eq!(app.visible_processes.len(), 2);
        assert!(
            app.visible_processes
                .iter()
                .all(|p| p.name.contains("fire"))
        );
    }

    #[test]
    fn test_apply_snapshot_tree_mode() {
        let mut app = AppState::new();
        app.tree_mode = true;
        let snap = make_snapshot(vec![
            make_process(1, "init", 1.0, 100),
            make_process(2, "child", 2.0, 200),
        ]);
        app.apply_snapshot(snap);
        assert!(!app.visible_tree.is_empty());
    }

    #[test]
    fn test_selected_process_after_snapshot() {
        let mut app = AppState::new();
        let snap = make_snapshot(vec![make_process(1, "proc", 10.0, 100)]);
        app.apply_snapshot(snap);
        assert!(app.selected_process().is_some());
    }

    #[test]
    fn test_process_count_flat_vs_tree() {
        let mut app = AppState::new();
        let snap = make_snapshot(vec![
            make_process(1, "a", 10.0, 100),
            make_process(2, "b", 20.0, 200),
        ]);
        app.apply_snapshot(snap);
        let flat_count = app.process_count();
        app.tree_mode = true;
        app.recompute_visible();
        let tree_count = app.process_count();
        // Both should contain the same number of processes
        assert_eq!(flat_count, 2);
        assert_eq!(tree_count, 2);
    }

    #[test]
    fn test_apply_snapshot_clamps_selection() {
        let mut app = AppState::new();
        app.selected = 10; // beyond bounds
        let snap = make_snapshot(vec![
            make_process(1, "a", 10.0, 100),
            make_process(2, "b", 20.0, 200),
        ]);
        app.apply_snapshot(snap);
        assert!(app.selected < app.process_count());
    }

    #[test]
    fn test_app_state_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<AppState>();
    }

    // -- Key handling (STORY-03) --

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn key_mod(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    fn app_with_processes() -> AppState {
        let mut app = AppState::new();
        let snap = make_snapshot(vec![
            make_process(1, "alpha", 90.0, 500),
            make_process(2, "bravo", 50.0, 300),
            make_process(3, "charlie", 30.0, 200),
            make_process(4, "delta", 10.0, 100),
            make_process(5, "echo", 70.0, 400),
        ]);
        app.apply_snapshot(snap);
        app
    }

    #[test]
    fn test_quit_q() {
        let mut app = AppState::new();
        app.handle_key_event(key(KeyCode::Char('q')));
        assert!(!app.running());
    }

    #[test]
    fn test_quit_ctrl_c() {
        let mut app = AppState::new();
        app.handle_key_event(key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(!app.running());
    }

    #[test]
    fn test_navigate_down() {
        let mut app = app_with_processes();
        assert_eq!(app.selected, 0);
        app.handle_key_event(key(KeyCode::Char('j')));
        assert_eq!(app.selected, 1);
        app.handle_key_event(key(KeyCode::Down));
        assert_eq!(app.selected, 2);
    }

    #[test]
    fn test_navigate_up() {
        let mut app = app_with_processes();
        app.selected = 3;
        app.handle_key_event(key(KeyCode::Char('k')));
        assert_eq!(app.selected, 2);
        app.handle_key_event(key(KeyCode::Up));
        assert_eq!(app.selected, 1);
    }

    #[test]
    fn test_navigate_home() {
        let mut app = app_with_processes();
        app.selected = 3;
        app.handle_key_event(key(KeyCode::Char('g')));
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn test_navigate_end() {
        let mut app = app_with_processes();
        app.handle_key_event(key(KeyCode::Char('G')));
        assert_eq!(app.selected, app.process_count() - 1);
    }

    #[test]
    fn test_navigate_clamp_bottom() {
        let mut app = app_with_processes();
        app.selected = app.process_count() - 1;
        app.handle_key_event(key(KeyCode::Down));
        assert_eq!(app.selected, app.process_count() - 1);
    }

    #[test]
    fn test_navigate_clamp_top() {
        let mut app = app_with_processes();
        app.selected = 0;
        app.handle_key_event(key(KeyCode::Up));
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn test_navigate_no_snapshot() {
        let mut app = AppState::new();
        // Must not panic with no processes.
        app.handle_key_event(key(KeyCode::Down));
        app.handle_key_event(key(KeyCode::Up));
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn test_tab_switch() {
        let mut app = AppState::new();
        assert_eq!(app.tab, Tab::General);
        app.handle_key_event(key(KeyCode::Tab));
        assert_eq!(app.tab, Tab::Processes);
        app.handle_key_event(key(KeyCode::Tab));
        assert_eq!(app.tab, Tab::Network);
        app.handle_key_event(key(KeyCode::Tab));
        assert_eq!(app.tab, Tab::Containers);
        app.handle_key_event(key(KeyCode::Tab));
        assert_eq!(app.tab, Tab::Kube);
        app.handle_key_event(key(KeyCode::Tab));
        assert_eq!(app.tab, Tab::Gpu);
        app.handle_key_event(key(KeyCode::Tab));
        assert_eq!(app.tab, Tab::General);
    }

    #[test]
    fn test_backtab_switch() {
        let mut app = AppState::new();
        app.handle_key_event(key(KeyCode::BackTab));
        assert_eq!(app.tab, *Tab::ALL.last().unwrap());
    }

    #[test]
    fn test_tree_toggle() {
        let mut app = app_with_processes();
        app.tab = Tab::Processes;
        assert!(!app.tree_mode);
        app.handle_key_event(key(KeyCode::Char('t')));
        assert!(app.tree_mode);
        app.handle_key_event(key(KeyCode::Char('t')));
        assert!(!app.tree_mode);
    }

    /// `t` used to fire from any tab, silently re-shaping the process table
    /// while the user was looking at something else.
    #[test]
    fn test_tree_toggle_is_scoped_to_the_processes_tab() {
        let mut app = app_with_processes();
        for tab in [Tab::General, Tab::Network, Tab::Containers, Tab::Kube] {
            app.tab = tab;
            app.tree_mode = false;
            app.handle_key_event(key(KeyCode::Char('t')));
            assert!(!app.tree_mode, "`t` must do nothing on {tab:?}");
        }
    }

    #[test]
    fn test_sort_cycle() {
        let mut app = app_with_processes();
        assert!(matches!(app.sort_field, SortField::Cpu));
        app.handle_key_event(key(KeyCode::Char('s')));
        assert!(matches!(app.sort_field, SortField::Mem));
        app.handle_key_event(key(KeyCode::Char('s')));
        assert!(matches!(app.sort_field, SortField::Pid));
    }

    #[test]
    fn test_sort_order_toggle() {
        let mut app = app_with_processes();
        assert!(matches!(app.sort_order, SortOrder::Desc));
        app.handle_key_event(key(KeyCode::Char('S')));
        assert!(matches!(app.sort_order, SortOrder::Asc));
        app.handle_key_event(key(KeyCode::Char('S')));
        assert!(matches!(app.sort_order, SortOrder::Desc));
    }

    #[test]
    fn test_f_keys_follow_the_htop_map() {
        // muxtop advertises htop shortcuts, so F1 must be Help — 0.4 bound it
        // to sort-by-PID, which silently re-sorted the table of a user who
        // pressed it expecting documentation.
        let mut app = app_with_processes();
        app.tab = Tab::Processes;

        app.handle_key_event(key(KeyCode::F(1)));
        assert_eq!(app.overlay, Overlay::Help);
        app.handle_key_event(key(KeyCode::Esc));

        app.handle_key_event(key(KeyCode::F(5)));
        assert!(app.tree_mode, "F5 toggles the tree, as in htop");
        app.handle_key_event(key(KeyCode::F(5)));

        assert!(matches!(app.sort_field, SortField::Cpu));
        app.handle_key_event(key(KeyCode::F(6)));
        assert!(
            matches!(app.sort_field, SortField::Mem),
            "F6 cycles the sort"
        );

        app.handle_key_event(key(KeyCode::F(4)));
        assert!(app.filter_editing(), "F4 opens the filter, as in htop");
    }

    #[test]
    fn test_filter_enter_exit() {
        let mut app = AppState::new();
        assert!(!app.filter_active);
        app.handle_key_event(key(KeyCode::Char('/')));
        assert!(app.filter_active);
        app.handle_key_event(key(KeyCode::Esc));
        assert!(!app.filter_active);
    }

    #[test]
    fn test_filter_typing() {
        let mut app = AppState::new();
        app.handle_key_event(key(KeyCode::Char('/')));
        assert!(app.filter_active);
        app.handle_key_event(key(KeyCode::Char('f')));
        app.handle_key_event(key(KeyCode::Char('o')));
        app.handle_key_event(key(KeyCode::Char('o')));
        assert_eq!(app.filter_input, "foo");
    }

    #[test]
    fn test_filter_backspace() {
        let mut app = AppState::new();
        app.filter_input = "bar".to_string();
        app.filter_active = true;
        app.handle_key_event(key(KeyCode::Backspace));
        assert_eq!(app.filter_input, "ba");
        app.handle_key_event(key(KeyCode::Backspace));
        assert_eq!(app.filter_input, "b");
    }

    #[test]
    fn test_filter_enter_keeps() {
        let mut app = AppState::new();
        app.filter_input = "test".to_string();
        app.filter_active = true;
        app.handle_key_event(key(KeyCode::Enter));
        assert!(!app.filter_active);
        assert_eq!(app.filter_input, "test"); // kept
    }

    // -- Guard fixes: missing test coverage --

    #[test]
    fn test_ctrl_c_quits_in_filter_mode() {
        let mut app = AppState::new();
        app.filter_active = true;
        app.handle_key_event(key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(!app.running());
    }

    // -- Alt+1/Alt+2 tab switching (STORY-08) --

    #[test]
    fn test_tab_alt1_switches_to_general() {
        let mut app = AppState::new();
        app.tab = Tab::Processes;
        app.handle_key_event(key_mod(KeyCode::Char('1'), KeyModifiers::ALT));
        assert_eq!(app.tab, Tab::General);
    }

    #[test]
    fn test_tab_alt2_switches_to_processes() {
        let mut app = AppState::new();
        app.handle_key_event(key_mod(KeyCode::Char('2'), KeyModifiers::ALT));
        assert_eq!(app.tab, Tab::Processes);
    }

    #[test]
    fn test_tab_alt1_idempotent_on_general() {
        let mut app = AppState::new();
        assert_eq!(app.tab, Tab::General);
        app.handle_key_event(key_mod(KeyCode::Char('1'), KeyModifiers::ALT));
        assert_eq!(app.tab, Tab::General);
    }

    #[test]
    fn test_palette_toggle() {
        let mut app = AppState::new();
        assert!(!app.show_palette());
        app.handle_key_event(key_mod(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert!(app.show_palette());
        app.handle_key_event(key_mod(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert!(!app.show_palette());
    }

    // -- Arrow keys (0.5.1: horizontal arrows scroll columns) --

    /// Vertical arrows move the row cursor and horizontal arrows scroll
    /// columns. In 0.4 the horizontal pair changed *screen*, which put "change
    /// row" and "change tab" on the same arrow cluster.
    #[test]
    fn test_horizontal_arrows_do_not_switch_tabs() {
        let mut app = app_with_processes();
        app.tab = Tab::Processes;
        app.handle_key_event(key(KeyCode::Right));
        assert_eq!(app.tab, Tab::Processes);
        assert_eq!(app.col_scroll, 1, "Right scrolls columns");
        app.handle_key_event(key(KeyCode::Left));
        assert_eq!(app.col_scroll, 0);
        assert_eq!(app.tab, Tab::Processes);
    }

    #[test]
    fn test_column_scroll_is_bounded() {
        let mut app = app_with_processes();
        for _ in 0..50 {
            app.handle_key_event(key(KeyCode::Right));
        }
        assert!(
            app.col_scroll <= 8,
            "column scroll ran away: {}",
            app.col_scroll
        );
        for _ in 0..50 {
            app.handle_key_event(key(KeyCode::Left));
        }
        assert_eq!(app.col_scroll, 0);
    }

    #[test]
    fn test_tab_key_cycles_tabs_forwards_and_backwards() {
        let mut app = AppState::new();
        assert_eq!(app.tab, Tab::General);
        app.handle_key_event(key(KeyCode::Tab));
        assert_eq!(app.tab, Tab::Processes);
        app.handle_key_event(key(KeyCode::BackTab));
        assert_eq!(app.tab, Tab::General);
        app.handle_key_event(key(KeyCode::BackTab));
        assert_eq!(
            app.tab,
            *Tab::ALL.last().unwrap(),
            "cycling wraps to the last tab, whatever it is"
        );
    }

    // -- Mouse handling (STORY-04) --

    fn mouse_scroll(kind: MouseEventKind) -> MouseEvent {
        MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// Regression: 0.4 moved `scroll_offset` without the selection, so the
    /// next frame's `viewport_offset` snapped the view straight back and the
    /// wheel appeared to do nothing at all.
    #[test]
    fn test_mouse_scroll_down_moves_the_viewport_and_the_cursor() {
        let mut app = app_with_processes();
        assert_eq!(app.scroll_offset, 0);
        app.handle_mouse_event(mouse_scroll(MouseEventKind::ScrollDown));
        assert!(app.scroll_offset > 0, "wheel did not scroll");
        assert!(
            app.selected >= app.scroll_offset,
            "the cursor must follow the viewport, or the next frame undoes the scroll"
        );
    }

    #[test]
    fn test_mouse_scroll_up() {
        let mut app = app_with_processes();
        app.scroll_offset = 3;
        app.selected = 3;
        app.handle_mouse_event(mouse_scroll(MouseEventKind::ScrollUp));
        assert!(app.scroll_offset < 3);
    }

    /// Regression: the wheel always moved the *process* offset, whatever tab
    /// was on screen.
    #[test]
    fn test_mouse_scroll_follows_the_active_tab() {
        let mut app = app_with_processes();
        app.tab = Tab::Network;
        let before = app.scroll_offset;
        app.handle_mouse_event(mouse_scroll(MouseEventKind::ScrollDown));
        assert_eq!(
            app.scroll_offset, before,
            "scrolling the Network tab must not move the process table"
        );
    }

    #[test]
    fn test_mouse_scroll_clamp_min() {
        let mut app = app_with_processes();
        app.scroll_offset = 0;
        app.handle_mouse_event(mouse_scroll(MouseEventKind::ScrollUp));
        assert_eq!(app.scroll_offset, 0);
    }

    #[test]
    fn test_mouse_scroll_clamp_max() {
        let mut app = app_with_processes();
        app.scroll_offset = app.process_count().saturating_sub(1);
        app.handle_mouse_event(mouse_scroll(MouseEventKind::ScrollDown));
        assert_eq!(app.scroll_offset, app.process_count().saturating_sub(1));
    }

    #[test]
    fn test_mouse_scroll_on_an_empty_list_does_nothing() {
        let mut app = AppState::new();
        app.handle_mouse_event(mouse_scroll(MouseEventKind::ScrollDown));
        assert_eq!(app.scroll_offset, 0);
        assert_eq!(app.selected, 0);
    }

    // -- Command registry tests (Epic 6) --

    #[test]
    fn test_command_registry_count() {
        assert!(
            Command::ALL.len() >= 14,
            "Registry should have at least 14 commands, got {}",
            Command::ALL.len()
        );
    }

    #[test]
    fn test_command_labels_non_empty() {
        for cmd in Command::ALL {
            assert!(!cmd.label().is_empty(), "Command {:?} has empty label", cmd);
        }
    }

    // -- Palette state tests (Epic 6) --

    #[test]
    fn test_palette_state_new() {
        let ps = PaletteState::new();
        assert!(ps.input.is_empty());
        assert_eq!(ps.selected, 0);
        assert_eq!(ps.filtered.len(), argument_free_command_count());
    }

    /// Commands that need an argument (`kill <name>`) are meaningless on their
    /// own, so they stay hidden until their verb has been typed.
    fn argument_free_command_count() -> usize {
        Command::ALL.iter().filter(|c| !c.is_verb_only()).count()
    }

    #[test]
    fn test_palette_refilter_empty_shows_all() {
        let mut ps = PaletteState::new();
        ps.refilter();
        assert_eq!(ps.filtered.len(), argument_free_command_count());
        assert!(
            ps.filtered.iter().all(|(c, _)| !c.is_verb_only()),
            "an argument form with no argument is noise"
        );
    }

    #[test]
    fn test_palette_parses_argument_commands() {
        // The README has advertised `kill firefox` since 0.3 against a command
        // enum that could not carry an argument.
        let mut ps = PaletteState::new();
        ps.input = "kill firefox".to_string();
        ps.refilter();
        assert_eq!(ps.filtered.len(), 1);
        assert_eq!(ps.filtered[0].0, Command::KillNamed);
        assert_eq!(ps.arg.as_deref(), Some("firefox"));
    }

    #[test]
    fn test_palette_argument_commands_cover_the_documented_verbs() {
        for (input, expected) in [
            ("kill firefox", Command::KillNamed),
            ("stop nginx", Command::StopNamed),
            ("restart postgres", Command::RestartNamed),
            ("sort mem", Command::SortBy),
            ("filter ngin", Command::FilterBy),
            ("theme mono", Command::SetTheme),
            ("tab kube", Command::GoToTab),
        ] {
            let mut ps = PaletteState::new();
            ps.input = input.to_string();
            ps.refilter();
            assert_eq!(
                ps.filtered.first().map(|(c, _)| *c),
                Some(expected),
                "`{input}`"
            );
            assert!(ps.arg.is_some(), "`{input}` must carry an argument");
        }
    }

    #[test]
    fn test_palette_ranks_the_active_tab_first() {
        let mut ps = PaletteState::new();
        ps.input = "sort".to_string();
        ps.refilter_ctx(&[], Some(Tab::Network));
        let first = ps.filtered.first().map(|(c, _)| c.label()).unwrap_or("");
        assert!(
            first.contains("network"),
            "a sort query on the Network tab should surface network sorts first, got `{first}`"
        );
    }

    #[test]
    fn test_palette_history_promotes_recent_commands() {
        let mut ps = PaletteState::new();
        ps.remember(Command::SwitchToKube);
        ps.refilter();
        assert_eq!(
            ps.filtered.first().map(|(c, _)| *c),
            Some(Command::SwitchToKube)
        );
    }

    #[test]
    fn test_palette_refilter_fuzzy_match() {
        let mut ps = PaletteState::new();
        ps.input = "sortcpu".to_string();
        ps.refilter();
        assert!(!ps.filtered.is_empty(), "Should match at least one command");
        assert_eq!(
            ps.filtered[0].0,
            Command::SortByCpu,
            "First result should be Sort by CPU"
        );
    }

    /// `sort cpu` is now the *argument* form, not a fuzzy query — which is
    /// what a user typing it actually means.
    #[test]
    fn test_palette_prefers_the_argument_form_over_fuzzy_matching() {
        let mut ps = PaletteState::new();
        ps.input = "sort cpu".to_string();
        ps.refilter();
        assert_eq!(ps.filtered[0].0, Command::SortBy);
        assert_eq!(ps.arg.as_deref(), Some("cpu"));
    }

    #[test]
    fn test_palette_refilter_no_match() {
        let mut ps = PaletteState::new();
        ps.input = "zzzzznonexistent".to_string();
        ps.refilter();
        assert!(ps.filtered.is_empty(), "Should have no matches");
    }

    #[test]
    fn test_palette_refilter_clamps_selection() {
        let mut ps = PaletteState::new();
        ps.selected = 100;
        ps.input = "quit".to_string();
        ps.refilter();
        assert!(ps.selected < ps.filtered.len());
    }

    // -- Palette key handling tests (Epic 6) --

    #[test]
    fn test_palette_opens_with_ctrl_p() {
        let mut app = AppState::new();
        assert!(!app.show_palette());
        app.handle_key_event(key_mod(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert!(app.show_palette());
        assert!(app.palette.input.is_empty());
    }

    #[test]
    fn test_palette_closes_with_esc() {
        let mut app = AppState::new();
        app.overlay = Overlay::Palette;
        app.palette.input = "test".to_string();
        app.handle_key_event(key(KeyCode::Esc));
        assert!(!app.show_palette());
        assert!(app.palette.input.is_empty());
    }

    #[test]
    fn test_palette_closes_with_ctrl_p() {
        let mut app = AppState::new();
        app.overlay = Overlay::Palette;
        app.handle_key_event(key_mod(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert!(!app.show_palette());
    }

    #[test]
    fn test_palette_typing_captures_input() {
        let mut app = AppState::new();
        app.overlay = Overlay::Palette;
        app.handle_key_event(key(KeyCode::Char('s')));
        app.handle_key_event(key(KeyCode::Char('o')));
        app.handle_key_event(key(KeyCode::Char('r')));
        app.handle_key_event(key(KeyCode::Char('t')));
        assert_eq!(app.palette.input, "sort");
    }

    #[test]
    fn test_palette_backspace() {
        let mut app = AppState::new();
        app.overlay = Overlay::Palette;
        app.palette.input = "sor".to_string();
        app.handle_key_event(key(KeyCode::Backspace));
        assert_eq!(app.palette.input, "so");
    }

    #[test]
    fn test_palette_blocks_quit() {
        let mut app = AppState::new();
        app.overlay = Overlay::Palette;
        app.handle_key_event(key(KeyCode::Char('q')));
        assert!(app.running(), "Pressing 'q' in palette should NOT quit");
        assert_eq!(app.palette.input, "q", "Should type 'q' into palette");
    }

    #[test]
    fn test_palette_ctrl_c_quits() {
        let mut app = AppState::new();
        app.overlay = Overlay::Palette;
        app.handle_key_event(key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(!app.running(), "Ctrl+C should quit even with palette open");
    }

    #[test]
    fn test_palette_navigate_down() {
        let mut app = AppState::new();
        app.overlay = Overlay::Palette;
        app.palette.refilter();
        assert_eq!(app.palette.selected, 0);
        app.handle_key_event(key(KeyCode::Down));
        assert_eq!(app.palette.selected, 1);
    }

    #[test]
    fn test_palette_navigate_up() {
        let mut app = AppState::new();
        app.overlay = Overlay::Palette;
        app.palette.refilter();
        app.palette.selected = 3;
        app.handle_key_event(key(KeyCode::Up));
        assert_eq!(app.palette.selected, 2);
    }

    #[test]
    fn test_palette_navigate_clamp_top() {
        let mut app = AppState::new();
        app.overlay = Overlay::Palette;
        app.palette.selected = 0;
        app.handle_key_event(key(KeyCode::Up));
        assert_eq!(app.palette.selected, 0);
    }

    #[test]
    fn test_palette_navigate_clamp_bottom() {
        let mut app = AppState::new();
        app.overlay = Overlay::Palette;
        app.palette.refilter();
        let max = app.palette.filtered.len() - 1;
        app.palette.selected = max;
        app.handle_key_event(key(KeyCode::Down));
        assert_eq!(app.palette.selected, max);
    }

    // -- Command execution tests (Epic 6) --

    #[test]
    fn test_palette_execute_quit() {
        let mut app = AppState::new();
        app.execute_command(Command::Quit);
        assert!(!app.running());
    }

    #[test]
    fn test_palette_execute_toggle_tree() {
        let mut app = app_with_processes();
        assert!(!app.tree_mode);
        app.execute_command(Command::ToggleTreeView);
        assert!(app.tree_mode);
    }

    #[test]
    fn test_palette_execute_sort_cpu() {
        let mut app = app_with_processes();
        app.sort_field = SortField::Pid;
        app.execute_command(Command::SortByCpu);
        assert!(matches!(app.sort_field, SortField::Cpu));
    }

    #[test]
    fn test_palette_execute_sort_mem() {
        let mut app = app_with_processes();
        app.execute_command(Command::SortByMem);
        assert!(matches!(app.sort_field, SortField::Mem));
    }

    #[test]
    fn test_palette_execute_toggle_sort_order() {
        let mut app = app_with_processes();
        assert!(matches!(app.sort_order, SortOrder::Desc));
        app.execute_command(Command::ToggleSortOrder);
        assert!(matches!(app.sort_order, SortOrder::Asc));
    }

    #[test]
    fn test_palette_execute_switch_tab() {
        let mut app = AppState::new();
        app.execute_command(Command::SwitchToProcesses);
        assert_eq!(app.tab, Tab::Processes);
        app.execute_command(Command::SwitchToGeneral);
        assert_eq!(app.tab, Tab::General);
    }

    #[test]
    fn test_palette_execute_open_filter() {
        let mut app = AppState::new();
        app.execute_command(Command::OpenFilter);
        assert!(app.filter_active);
    }

    #[test]
    fn test_palette_enter_executes_and_closes() {
        let mut app = AppState::new();
        app.overlay = Overlay::Palette;
        app.palette.refilter();
        // First command in the list is Quit
        app.palette.selected = 0;
        assert_eq!(app.palette.filtered[0].0, Command::Quit);
        app.handle_key_event(key(KeyCode::Enter));
        assert!(!app.show_palette());
        assert!(!app.running());
    }

    // -- Epic 7: Confirm dialog tests --

    #[test]
    fn test_confirm_dialog_opens_on_f9() {
        let mut app = app_with_processes();
        app.tab = Tab::Processes;
        app.handle_key_event(key(KeyCode::F(9)));
        assert!(app.confirm.is_some());
        if let Some(ConfirmAction::Kill { signal, .. }) = &app.confirm {
            assert_eq!(*signal, Signal::Term);
        } else {
            panic!("Expected Kill confirm action");
        }
    }

    #[test]
    fn test_confirm_dialog_opens_on_f10_sigkill() {
        let mut app = app_with_processes();
        app.tab = Tab::Processes;
        app.handle_key_event(key(KeyCode::F(10)));
        assert!(app.confirm.is_some());
        if let Some(ConfirmAction::Kill { signal, .. }) = &app.confirm {
            assert_eq!(*signal, Signal::Kill);
        } else {
            panic!("Expected Kill confirm with SIGKILL");
        }
    }

    #[test]
    fn test_confirm_cancel_with_n() {
        let mut app = app_with_processes();
        app.tab = Tab::Processes;
        app.handle_key_event(key(KeyCode::F(9)));
        assert!(app.confirm.is_some());
        app.handle_key_event(key(KeyCode::Char('n')));
        assert!(app.confirm.is_none());
    }

    #[test]
    fn test_confirm_cancel_with_esc() {
        let mut app = app_with_processes();
        app.tab = Tab::Processes;
        app.handle_key_event(key(KeyCode::F(9)));
        assert!(app.confirm.is_some());
        app.handle_key_event(key(KeyCode::Esc));
        assert!(app.confirm.is_none());
    }

    #[test]
    fn test_confirm_blocks_other_keys() {
        let mut app = app_with_processes();
        app.tab = Tab::Processes;
        app.handle_key_event(key(KeyCode::F(9)));
        assert!(app.confirm.is_some());
        // Pressing 'q' should NOT quit
        app.handle_key_event(key(KeyCode::Char('q')));
        assert!(app.running());
        assert!(app.confirm.is_some());
    }

    #[test]
    fn test_confirm_ctrl_c_quits() {
        let mut app = app_with_processes();
        app.tab = Tab::Processes;
        app.handle_key_event(key(KeyCode::F(9)));
        assert!(app.confirm.is_some());
        app.handle_key_event(key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(!app.running());
    }

    #[test]
    fn test_kill_no_op_on_general_tab() {
        let mut app = app_with_processes();
        app.tab = Tab::General;
        app.handle_key_event(key(KeyCode::F(9)));
        assert!(app.confirm.is_none());
    }

    #[test]
    fn test_kill_no_op_without_process() {
        let mut app = AppState::new();
        app.tab = Tab::Processes;
        // No snapshot → no selected process
        app.handle_key_event(key(KeyCode::F(9)));
        assert!(app.confirm.is_none());
    }

    #[test]
    fn test_renice_opens_confirm_on_f7() {
        let mut app = app_with_processes();
        app.tab = Tab::Processes;
        app.handle_key_event(key(KeyCode::F(7)));
        assert!(app.confirm.is_some());
        if let Some(ConfirmAction::Renice { delta, .. }) = &app.confirm {
            assert_eq!(*delta, 1);
        } else {
            panic!("Expected Renice confirm action");
        }
    }

    #[test]
    fn test_renice_opens_confirm_on_f8() {
        let mut app = app_with_processes();
        app.tab = Tab::Processes;
        app.handle_key_event(key(KeyCode::F(8)));
        assert!(app.confirm.is_some());
        if let Some(ConfirmAction::Renice { delta, .. }) = &app.confirm {
            assert_eq!(*delta, -1);
        } else {
            panic!("Expected Renice confirm action with delta -1");
        }
    }

    // -- Epic 7: Clear filter & reverse sort --

    #[test]
    fn test_esc_clears_filter() {
        let mut app = app_with_processes();
        app.filter_input = "firefox".to_string();
        app.recompute_visible();
        let before = app.visible_processes.len();
        app.handle_key_event(key(KeyCode::Esc));
        assert!(app.filter_input.is_empty());
        assert!(app.visible_processes.len() >= before);
    }

    #[test]
    fn test_esc_no_op_when_filter_empty() {
        let mut app = app_with_processes();
        assert!(app.filter_input.is_empty());
        let count = app.process_count();
        app.handle_key_event(key(KeyCode::Esc));
        assert_eq!(app.process_count(), count);
    }

    #[test]
    fn test_i_reverses_sort_order() {
        let mut app = app_with_processes();
        assert!(matches!(app.sort_order, SortOrder::Desc));
        app.handle_key_event(key(KeyCode::Char('I')));
        assert!(matches!(app.sort_order, SortOrder::Asc));
    }

    // -- Epic 7: Status message --

    #[test]
    fn test_status_message_set_and_read() {
        let mut app = AppState::new();
        app.notify(Level::Info, "Test message".to_string());
        assert_eq!(
            app.active_status().map(|t| t.text.as_str()),
            Some("Test message")
        );
    }

    /// MED-S5: a process name carrying an OSC sequence must not reach the
    /// footer verbatim — the terminal would interpret it.
    #[test]
    fn test_set_status_scrubs_control_chars() {
        let mut app = AppState::new();
        app.notify(
            Level::Info,
            "Sent SIGTERM to \x1b]0;pwn\x07evil (PID 42)".to_string(),
        );
        let status = app
            .active_status()
            .expect("status must be set")
            .text
            .clone();
        assert!(
            !status.contains('\x1b'),
            "ESC must not survive a notification: {status:?}"
        );
        assert!(
            !status.contains('\x07'),
            "BEL must not survive a notification: {status:?}"
        );
        assert_eq!(status, "Sent SIGTERM to ?]0;pwn?evil (PID 42)");
    }

    /// MED-S5: same guarantee for the confirmation dialog, which builds its
    /// own string outside the table renderers.
    #[test]
    fn test_confirm_prompt_scrubs_control_chars() {
        let actions = [
            ConfirmAction::Kill {
                pid: 42,
                name: "bash\x1b]0;pwn\x07".to_string(),
                signal: Signal::Term,
            },
            ConfirmAction::Renice {
                pid: 42,
                name: "bash\x1b]0;pwn\x07".to_string(),
                delta: 1,
            },
            ConfirmAction::StopContainer {
                id: "abc123".to_string(),
                name: "nginx\x1b[31m".to_string(),
            },
            ConfirmAction::KillContainer {
                id: "abc123".to_string(),
                name: "nginx\x1b[31m".to_string(),
            },
            ConfirmAction::RestartContainer {
                id: "abc123".to_string(),
                name: "nginx\x1b[31m".to_string(),
            },
        ];

        for action in &actions {
            let prompt = action.prompt();
            assert!(
                !prompt.contains('\x1b'),
                "ESC must not survive prompt() for {action:?}: {prompt:?}"
            );
            assert!(
                !prompt.contains('\x07'),
                "BEL must not survive prompt() for {action:?}: {prompt:?}"
            );
        }
    }

    #[test]
    fn test_status_survives_the_next_keystroke() {
        // 0.4 wiped the status line on the very next key press, so an outcome
        // the user had not finished reading vanished the moment they moved the
        // cursor. Messages now expire on their own schedule.
        let mut app = app_with_processes();
        app.notify(Level::Info, "Test message".to_string());
        app.handle_key_event(key(KeyCode::Down));
        assert!(
            app.active_status().is_some(),
            "moving the cursor ate the message"
        );
    }

    #[test]
    fn test_escape_dismisses_messages() {
        let mut app = app_with_processes();
        app.notify(Level::Error, "Kill failed".to_string());
        app.handle_key_event(key(KeyCode::Esc));
        assert!(app.active_status().is_none(), "Esc must dismiss toasts");
        // Dismissed, not lost.
        assert_eq!(app.notifier.history().len(), 1);
    }

    // -- Epic 7: Command registry expanded --

    #[test]
    fn test_command_registry_has_new_commands() {
        assert!(
            Command::ALL.len() >= 19,
            "Registry should have at least 19 commands, got {}",
            Command::ALL.len()
        );
        assert!(Command::ALL.contains(&Command::KillProcess));
        assert!(Command::ALL.contains(&Command::ForceKillProcess));
        assert!(Command::ALL.contains(&Command::NiceDown));
        assert!(Command::ALL.contains(&Command::NiceUp));
        assert!(Command::ALL.contains(&Command::ClearFilter));
    }

    #[test]
    fn test_palette_execute_clear_filter() {
        let mut app = app_with_processes();
        app.filter_input = "test".to_string();
        app.execute_command(Command::ClearFilter);
        assert!(app.filter_input.is_empty());
    }

    // -- Epic 9: ConfirmAction::prompt() tests --

    #[test]
    fn test_confirm_prompt_kill_sigterm() {
        let action = ConfirmAction::Kill {
            pid: 1234,
            name: "firefox".to_string(),
            signal: Signal::Term,
        };
        let prompt = action.prompt();
        assert!(prompt.contains("SIGTERM"), "Should contain SIGTERM");
        assert!(prompt.contains("firefox"), "Should contain process name");
        assert!(prompt.contains("1234"), "Should contain PID");
        assert!(prompt.contains("[y/n]"), "Should contain y/n prompt");
    }

    #[test]
    fn test_confirm_prompt_kill_sigkill() {
        let action = ConfirmAction::Kill {
            pid: 999,
            name: "chrome".to_string(),
            signal: Signal::Kill,
        };
        let prompt = action.prompt();
        assert!(prompt.contains("SIGKILL"), "Should contain SIGKILL");
        assert!(prompt.contains("chrome"), "Should contain process name");
        assert!(prompt.contains("999"), "Should contain PID");
    }

    #[test]
    fn test_confirm_prompt_renice_down() {
        let action = ConfirmAction::Renice {
            pid: 42,
            name: "bash".to_string(),
            delta: 1,
        };
        let prompt = action.prompt();
        assert!(
            prompt.contains("lower priority"),
            "delta>0 should say 'lower priority'"
        );
        assert!(prompt.contains("bash"), "Should contain process name");
        assert!(prompt.contains("42"), "Should contain PID");
    }

    #[test]
    fn test_confirm_prompt_renice_up() {
        let action = ConfirmAction::Renice {
            pid: 77,
            name: "vim".to_string(),
            delta: -1,
        };
        let prompt = action.prompt();
        assert!(
            prompt.contains("higher priority"),
            "delta<0 should say 'higher priority'"
        );
        assert!(prompt.contains("vim"), "Should contain process name");
    }

    // -- Epic 9: next_sort_field() tests --

    #[test]
    fn test_next_sort_field_full_cycle() {
        let mut field = SortField::Cpu;
        field = next_sort_field(field);
        assert!(matches!(field, SortField::Mem));
        field = next_sort_field(field);
        assert!(matches!(field, SortField::Pid));
        field = next_sort_field(field);
        assert!(matches!(field, SortField::Name));
        field = next_sort_field(field);
        assert!(matches!(field, SortField::User));
        field = next_sort_field(field);
        assert!(matches!(field, SortField::Cpu), "Should cycle back to Cpu");
    }

    #[test]
    fn test_next_sort_field_is_deterministic() {
        // Same input always gives same output
        assert!(matches!(next_sort_field(SortField::Cpu), SortField::Mem));
        assert!(matches!(next_sort_field(SortField::Cpu), SortField::Mem));
    }

    // -- Epic 9: with_config tests --

    #[test]
    fn test_with_config_sort_field() {
        let config = crate::CliConfig {
            sort_field: SortField::Name,
            ..Default::default()
        };
        let app = AppState::with_config(config, TermCaps::default());
        assert!(matches!(app.sort_field, SortField::Name));
    }

    #[test]
    fn test_with_config_tree_mode() {
        let config = crate::CliConfig {
            tree_mode: true,
            ..Default::default()
        };
        let app = AppState::with_config(config, TermCaps::default());
        assert!(app.tree_mode);
    }

    #[test]
    fn test_with_config_filter() {
        let config = crate::CliConfig {
            filter: Some("rust".to_string()),
            ..Default::default()
        };
        let app = AppState::with_config(config, TermCaps::default());
        assert_eq!(app.filter_input, "rust");
    }

    #[test]
    fn test_with_config_no_filter() {
        let config = crate::CliConfig::default();
        let app = AppState::with_config(config, TermCaps::default());
        assert!(app.filter_input.is_empty());
    }

    // -- Epic 9: AppState edge cases --

    #[test]
    fn test_apply_empty_snapshot() {
        let mut app = AppState::new();
        let snap = make_snapshot(vec![]);
        app.apply_snapshot(snap);
        assert!(app.visible_processes.is_empty());
        assert_eq!(app.process_count(), 0);
        assert!(app.selected_process().is_none());
    }

    #[test]
    fn test_page_down_navigation() {
        let mut app = app_with_processes();
        app.handle_key_event(key(KeyCode::PageDown));
        // 5 processes, PageDown moves by 20, clamped to 4
        assert_eq!(app.selected, 4);
    }

    #[test]
    fn test_page_up_from_bottom() {
        let mut app = app_with_processes();
        app.selected = 4;
        app.handle_key_event(key(KeyCode::PageUp));
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn test_home_key_navigation() {
        let mut app = app_with_processes();
        app.selected = 3;
        app.handle_key_event(key(KeyCode::Home));
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn test_end_key_navigation() {
        let mut app = app_with_processes();
        app.handle_key_event(key(KeyCode::End));
        assert_eq!(app.selected, app.process_count() - 1);
    }

    // -- Network tab tests (Epic 12) --

    #[test]
    fn test_alt3_switches_to_network() {
        let mut app = AppState::new();
        app.handle_key_event(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::ALT));
        assert_eq!(app.tab, Tab::Network);
    }

    /// Every tab must be reachable by its documented `Alt+N` shortcut — the
    /// Kube tab shipped in v0.4 without one.
    #[test]
    fn test_alt_n_reaches_every_tab() {
        let expected = [
            ('1', Tab::General),
            ('2', Tab::Processes),
            ('3', Tab::Network),
            ('4', Tab::Containers),
            ('5', Tab::Kube),
            ('6', Tab::Gpu),
        ];
        assert_eq!(expected.len(), Tab::ALL.len(), "one Alt+N per tab");

        for (digit, tab) in expected {
            let mut app = AppState::new();
            app.handle_key_event(KeyEvent::new(KeyCode::Char(digit), KeyModifiers::ALT));
            assert_eq!(app.tab, tab, "Alt+{digit} must select {tab:?}");
        }
    }

    /// The palette must expose a switch command for the Kube tab, like every
    /// other tab.
    #[test]
    fn test_palette_switches_to_kube() {
        assert!(Command::ALL.contains(&Command::SwitchToKube));
        assert_eq!(Command::SwitchToKube.shortcut(), "Alt+5");

        let mut app = AppState::new();
        app.execute_command(Command::SwitchToKube);
        assert_eq!(app.tab, Tab::Kube);
    }

    #[test]
    fn test_network_history_populated_on_snapshot() {
        let mut app = AppState::new();
        assert!(app.network_history.is_empty());
        let snap = make_snapshot(vec![]);
        app.apply_snapshot(snap);
        assert_eq!(app.network_history.len(), 1);
    }

    #[test]
    fn test_network_tab_uses_net_selected() {
        let mut app = AppState::new();
        app.tab = Tab::Network;
        // Create snapshot with network interfaces
        let mut snap = make_snapshot(vec![]);
        snap.networks.interfaces = vec![
            muxtop_core::network::NetworkInterfaceSnapshot {
                name: "eth0".to_string(),
                bytes_rx: 1000,
                bytes_tx: 500,
                packets_rx: 10,
                packets_tx: 5,
                errors_rx: 0,
                errors_tx: 0,
                mac_address: "00:00:00:00:00:00".to_string(),
                is_up: true,
            },
            muxtop_core::network::NetworkInterfaceSnapshot {
                name: "eth1".to_string(),
                bytes_rx: 2000,
                bytes_tx: 1000,
                packets_rx: 20,
                packets_tx: 10,
                errors_rx: 0,
                errors_tx: 0,
                mac_address: "00:00:00:00:00:01".to_string(),
                is_up: true,
            },
        ];
        app.apply_snapshot(snap);
        assert_eq!(app.net_selected, 0);
        app.handle_key_event(key(KeyCode::Char('j')));
        assert_eq!(app.net_selected, 1);
        // Process selected should not change
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn test_network_sort_cycling() {
        let mut app = AppState::new();
        app.tab = Tab::Network;
        assert!(matches!(app.net_sort_field, NetworkSortField::RxRate));
        app.handle_key_event(key(KeyCode::Char('s')));
        assert!(matches!(app.net_sort_field, NetworkSortField::TxRate));
        app.handle_key_event(key(KeyCode::Char('s')));
        assert!(matches!(app.net_sort_field, NetworkSortField::Name));
    }

    #[test]
    fn test_network_filter_activation() {
        let mut app = AppState::new();
        app.tab = Tab::Network;
        assert!(!app.net_filter_active);
        app.handle_key_event(key(KeyCode::Char('/')));
        assert!(app.net_filter_active);
        // Should not affect process filter
        assert!(!app.filter_active);
    }

    #[test]
    fn test_network_filter_input() {
        let mut app = AppState::new();
        app.tab = Tab::Network;
        app.handle_key_event(key(KeyCode::Char('/')));
        assert!(app.net_filter_active);
        app.handle_key_event(key(KeyCode::Char('e')));
        app.handle_key_event(key(KeyCode::Char('t')));
        app.handle_key_event(key(KeyCode::Char('h')));
        assert_eq!(app.net_filter_input, "eth");
        app.handle_key_event(key(KeyCode::Esc));
        assert!(!app.net_filter_active);
        assert_eq!(app.net_filter_input, "eth");
    }

    #[test]
    fn test_switch_to_network_command() {
        let mut app = AppState::new();
        assert_eq!(app.tab, Tab::General);
        // Open palette and execute SwitchToNetwork
        app.execute_command(Command::SwitchToNetwork);
        assert_eq!(app.tab, Tab::Network);
    }

    #[test]
    fn test_sort_net_commands() {
        let mut app = AppState::new();
        app.execute_command(Command::SortNetByTx);
        assert!(matches!(app.net_sort_field, NetworkSortField::TxRate));
        app.execute_command(Command::SortNetByName);
        assert!(matches!(app.net_sort_field, NetworkSortField::Name));
        app.execute_command(Command::SortNetByErrors);
        assert!(matches!(app.net_sort_field, NetworkSortField::Errors));
        app.execute_command(Command::SortNetByRx);
        assert!(matches!(app.net_sort_field, NetworkSortField::RxRate));
    }

    #[test]
    fn test_network_esc_clears_filter() {
        let mut app = AppState::new();
        app.tab = Tab::Network;
        app.net_filter_input = "eth".to_string();
        app.handle_key_event(key(KeyCode::Esc));
        assert!(app.net_filter_input.is_empty());
    }

    #[test]
    fn test_net_defaults() {
        let app = AppState::new();
        assert_eq!(app.net_selected, 0);
        assert_eq!(app.net_scroll_offset, 0);
        assert!(matches!(app.net_sort_field, NetworkSortField::RxRate));
        assert!(app.net_filter_input.is_empty());
        assert!(!app.net_filter_active);
    }

    // -- Epic 15: Remote mode tests --

    fn make_remote_app() -> AppState {
        let addr: std::net::SocketAddr = "10.0.0.1:4242".parse().unwrap();
        let config = crate::CliConfig {
            connection_mode: crate::ConnectionMode::Remote {
                hostname: "prod-01".to_string(),
                addr,
            },
            ..Default::default()
        };
        AppState::with_config(config, TermCaps::default())
    }

    #[test]
    fn test_connection_mode_remote() {
        let app = make_remote_app();
        assert!(app.is_remote());
        assert!(matches!(
            app.connection_mode,
            crate::ConnectionMode::Remote { .. }
        ));
    }

    #[test]
    fn test_connection_mode_local_default() {
        let app = AppState::new();
        assert!(!app.is_remote());
        assert!(matches!(app.connection_mode, crate::ConnectionMode::Local));
    }

    #[test]
    fn test_actions_disabled_remote_kill() {
        let mut app = make_remote_app();
        app.tab = Tab::Processes;
        // Simulate F9 (Kill) key
        app.handle_key_event(KeyEvent::new(KeyCode::F(9), KeyModifiers::NONE));
        assert!(
            app.active_status()
                .is_some_and(|s| s.text.contains("disabled in remote")),
            "expected 'disabled in remote' status, got {:?}",
            app.active_status()
        );
    }

    #[test]
    fn test_actions_disabled_remote_force_kill() {
        let mut app = make_remote_app();
        app.tab = Tab::Processes;
        app.handle_key_event(KeyEvent::new(KeyCode::F(10), KeyModifiers::NONE));
        assert!(
            app.active_status()
                .is_some_and(|s| s.text.contains("disabled in remote")),
        );
    }

    #[test]
    fn test_actions_disabled_remote_nice() {
        let mut app = make_remote_app();
        app.tab = Tab::Processes;
        app.handle_key_event(KeyEvent::new(KeyCode::F(7), KeyModifiers::NONE));
        assert!(
            app.active_status()
                .is_some_and(|s| s.text.contains("disabled in remote")),
        );
    }

    #[test]
    fn test_palette_filters_remote() {
        let mut app = make_remote_app();
        app.open_palette();
        let cmds: Vec<Command> = app.palette.filtered.iter().map(|(c, _)| *c).collect();
        assert!(
            !cmds.contains(&Command::KillProcess),
            "KillProcess should be hidden"
        );
        assert!(
            !cmds.contains(&Command::ForceKillProcess),
            "ForceKillProcess should be hidden"
        );
        assert!(
            !cmds.contains(&Command::NiceDown),
            "NiceDown should be hidden"
        );
        assert!(!cmds.contains(&Command::NiceUp), "NiceUp should be hidden");
        // But Quit and other commands should still be present.
        assert!(cmds.contains(&Command::Quit));
    }

    #[test]
    fn test_palette_includes_all_local() {
        let mut app = AppState::new();
        app.open_palette();
        let cmds: Vec<Command> = app.palette.filtered.iter().map(|(c, _)| *c).collect();
        assert!(
            cmds.contains(&Command::KillProcess),
            "KillProcess should be in local palette"
        );
        assert!(
            cmds.contains(&Command::NiceDown),
            "NiceDown should be in local palette"
        );
    }

    // ─── Container actions (E6) ───────────────────────────────────────────

    // -- PERF-03: Kube view cache --

    /// Snapshot carrying `pod_names` as `(namespace, name, cpu_millis)`.
    fn snapshot_with_pods(pods: &[(&str, &str, u32)]) -> muxtop_core::system::SystemSnapshot {
        use muxtop_core::kube::{ClusterKind, KubeSnapshot, PodPhase, PodSnapshot, QosClass};
        let mut snap = make_snapshot_with_container("c-id", "ctr");
        snap.kube = Some(std::sync::Arc::new(KubeSnapshot {
            cluster_kind: ClusterKind::Generic,
            server_version: None,
            current_namespace: "default".into(),
            reachable: true,
            metrics_available: true,
            pods: pods
                .iter()
                .map(|(ns, name, cpu)| PodSnapshot {
                    namespace: (*ns).into(),
                    name: (*name).into(),
                    phase: PodPhase::Running,
                    ready: (1, 1),
                    restarts: 0,
                    age_seconds: 60,
                    node: "node-1".into(),
                    cpu_millis: Some(*cpu),
                    mem_bytes: Some(1024),
                    qos: QosClass::BestEffort,
                })
                .collect(),
            nodes: Vec::new(),
            deployments: Vec::new(),
        }));
        snap
    }

    /// The cache feeds both the table and the scroll bounds, so it has to
    /// track the snapshot exactly — a stale cache shows rows that no longer
    /// exist or hides ones that do.
    #[test]
    fn test_kube_view_cache_follows_the_snapshot() {
        let mut app = AppState::new();
        app.tab = Tab::Kube;
        assert_eq!(app.kube_count(), 0, "no snapshot yet");

        app.apply_snapshot(snapshot_with_pods(&[
            ("default", "nginx-1", 100),
            ("kube-system", "coredns", 50),
        ]));
        assert_eq!(app.kube_count(), 2);

        // A shrinking cluster must shrink the cache, or j/k would scroll past
        // the end of the list.
        app.apply_snapshot(snapshot_with_pods(&[("default", "nginx-1", 100)]));
        assert_eq!(app.kube_count(), 1);
    }

    /// Typing a filter is the hottest path through the cache.
    #[test]
    fn test_kube_filter_refreshes_the_cache() {
        let mut app = AppState::new();
        app.tab = Tab::Kube;
        app.apply_snapshot(snapshot_with_pods(&[
            ("default", "nginx-1", 100),
            ("kube-system", "coredns", 50),
        ]));

        app.set_filter("nginx");
        assert_eq!(app.kube_count(), 1, "filter should narrow the view");

        // PERF-03 swapped to_lowercase()-per-row for contains_ignore_case;
        // matching must stay case-insensitive.
        app.set_filter("NGINX");
        assert_eq!(app.kube_count(), 1, "matching must ignore case");

        app.clear_filter();
        assert_eq!(app.kube_count(), 2, "clearing restores every row");
    }

    /// Switching sub-views repoints the indices at a different list, so a
    /// carried-over cache would index the wrong collection.
    #[test]
    fn test_kube_subview_switch_rebuilds_the_cache() {
        let mut app = AppState::new();
        app.tab = Tab::Kube;
        app.apply_snapshot(snapshot_with_pods(&[("default", "nginx-1", 100)]));
        assert_eq!(app.kube_count(), 1);

        // The fixture has no nodes; the count must follow the new sub-view.
        app.switch_kube_subview(KubeSubview::Nodes);
        assert_eq!(app.kube_count(), 0);

        app.switch_kube_subview(KubeSubview::Pods);
        assert_eq!(app.kube_count(), 1);
    }

    /// Regression: `kube_selected` indexes the *sorted* order, but the old
    /// `selected_kube_name` re-derived an unsorted filtered list and took the
    /// nth element — so with a sort active it named a different object than
    /// the one highlighted on screen.
    #[test]
    fn test_selected_kube_name_matches_the_sorted_row() {
        let mut app = AppState::new();
        app.tab = Tab::Kube;
        // List order is alphabetical; CPU order is the reverse.
        app.apply_snapshot(snapshot_with_pods(&[
            ("default", "alpha", 10),
            ("default", "bravo", 500),
        ]));
        app.kube_sort_field = KubeSortField::PodCpu;
        app.kube_sort_order = SortOrder::Desc;
        app.recompute_kube_view();

        app.kube_selected = 0;
        assert_eq!(
            app.selected_kube_name().as_deref(),
            Some("bravo"),
            "row 0 under CPU-desc is the busiest pod, not the first in list order"
        );
    }

    fn make_snapshot_with_container(id: &str, name: &str) -> muxtop_core::system::SystemSnapshot {
        use muxtop_core::containers::{
            ContainerSnapshot, ContainerState, ContainersSnapshot, EngineKind,
        };
        use muxtop_core::network::NetworkSnapshot;
        use muxtop_core::system::{CpuSnapshot, LoadSnapshot, MemorySnapshot, SystemSnapshot};

        SystemSnapshot {
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
            containers: Some(std::sync::Arc::new(ContainersSnapshot {
                engine: EngineKind::Docker,
                daemon_up: true,
                containers: vec![ContainerSnapshot {
                    id: id.into(),
                    id_full: id.into(),
                    name: name.into(),
                    image: "nginx:1.27".into(),
                    state: ContainerState::Running,
                    status_text: "Up 1 minute".into(),
                    cpu_pct: 1.5,
                    mem_used_bytes: 0,
                    mem_limit_bytes: 0,
                    net_rx_bytes: 0,
                    net_tx_bytes: 0,
                    block_read_bytes: 0,
                    block_write_bytes: 0,
                    started_at_ms: 1_700_000_000_000,
                }],
            })),
            kube: None,
            gpu: None,
            timestamp_ms: 1_700_000_000_000,
        }
    }

    #[test]
    fn test_container_stop_opens_confirm() {
        let mut app = AppState::new();
        app.tab = Tab::Containers;
        app.apply_snapshot(make_snapshot_with_container("abc123def456", "my-nginx"));
        app.handle_key_event(key(KeyCode::F(9)));
        match &app.confirm {
            Some(ConfirmAction::StopContainer { id, name }) => {
                assert_eq!(id, "abc123def456");
                assert_eq!(name, "my-nginx");
            }
            other => panic!("expected StopContainer confirm dialog, got {other:?}"),
        }
    }

    #[test]
    fn test_container_kill_opens_confirm() {
        let mut app = AppState::new();
        app.tab = Tab::Containers;
        app.apply_snapshot(make_snapshot_with_container("abc", "svc"));
        app.handle_key_event(key(KeyCode::F(10)));
        assert!(matches!(
            app.confirm,
            Some(ConfirmAction::KillContainer { .. })
        ));
    }

    #[test]
    fn test_container_restart_opens_confirm() {
        let mut app = AppState::new();
        app.tab = Tab::Containers;
        app.apply_snapshot(make_snapshot_with_container("abc", "svc"));
        app.handle_key_event(key(KeyCode::F(11)));
        assert!(matches!(
            app.confirm,
            Some(ConfirmAction::RestartContainer { .. })
        ));
    }

    #[test]
    fn test_container_actions_disabled_in_remote_mode() {
        let mut app = make_remote_app();
        app.tab = Tab::Containers;
        app.apply_snapshot(make_snapshot_with_container("abc", "svc"));
        app.handle_key_event(key(KeyCode::F(9)));
        assert!(app.confirm.is_none(), "confirm dialog should not open");
        assert!(
            app.active_status()
                .is_some_and(|s| s.text.contains("disabled in remote")),
            "expected remote-mode notice, got {:?}",
            app.active_status()
        );
    }

    #[test]
    fn test_container_action_on_other_tabs_ignored() {
        let mut app = AppState::new();
        app.apply_snapshot(make_snapshot_with_container("abc", "svc"));
        for tab in [Tab::General, Tab::Processes, Tab::Network] {
            app.tab = tab;
            app.confirm = None;
            app.handle_key_event(key(KeyCode::F(11)));
            assert!(
                app.confirm.is_none(),
                "F11 on {tab:?} must not open a container confirm"
            );
        }
    }

    #[test]
    fn test_palette_excludes_container_actions_in_remote() {
        let mut app = make_remote_app();
        app.open_palette();
        let cmds: Vec<Command> = app.palette.filtered.iter().map(|(c, _)| *c).collect();
        assert!(!cmds.contains(&Command::StopContainer));
        assert!(!cmds.contains(&Command::KillContainer));
        assert!(!cmds.contains(&Command::RestartContainer));
    }

    #[test]
    fn test_palette_includes_container_actions_in_local() {
        let mut app = AppState::new();
        app.open_palette();
        let cmds: Vec<Command> = app.palette.filtered.iter().map(|(c, _)| *c).collect();
        assert!(cmds.contains(&Command::StopContainer));
        assert!(cmds.contains(&Command::KillContainer));
        assert!(cmds.contains(&Command::RestartContainer));
    }

    #[tokio::test]
    async fn test_execute_container_without_engine_sets_status() {
        let mut app = AppState::new();
        app.tab = Tab::Containers;
        app.apply_snapshot(make_snapshot_with_container("abc", "svc"));
        app.handle_key_event(key(KeyCode::F(9)));
        app.handle_key_event(key(KeyCode::Char('y')));
        assert!(app.confirm.is_none());
        let status = app.active_status().expect("an outcome must be reported");
        assert!(
            status.text.contains("No container engine"),
            "expected a missing-engine message, got {:?}",
            status.text
        );
        assert_eq!(
            status.level,
            Level::Error,
            "a request that cannot be honoured is an error, not a notice"
        );
    }

    #[test]
    fn test_confirm_action_prompt_mentions_container_name() {
        let prompt = ConfirmAction::StopContainer {
            id: "abcdef123456789".into(),
            name: "nginx".into(),
        }
        .prompt();
        assert!(prompt.contains("Stop"));
        assert!(prompt.contains("nginx"));
        assert!(prompt.contains("abcdef123456"));
    }

    // ─── Slice D perf coverage ────────────────────────────────────────────

    /// PERF-H1 — `Event::Tick` must not invalidate the rendered frame.
    /// `take_needs_redraw` is the single source of truth used by the main
    /// loop to decide whether to call `terminal.draw`. After a fresh
    /// snapshot the flag is set; once the loop drains it, an `Event::Tick`
    /// alone (mirrored here as zero state mutation) keeps the flag clear.
    #[test]
    fn test_tick_does_not_request_redraw() {
        let mut app = AppState::new();
        // First flag is the "paint the initial frame" hint set in `new()`.
        assert!(app.take_needs_redraw());
        // Drain again — no further redraws scheduled.
        assert!(!app.take_needs_redraw());

        // A snapshot SHOULD schedule a redraw (mirrors the main loop's
        // Snapshot arm).
        app.apply_snapshot(make_snapshot(vec![]));
        assert!(app.take_needs_redraw());

        // Without any further events, the equivalent of an `Event::Tick`
        // (no state mutation) leaves the flag clear — the main loop will
        // skip `terminal.draw` for that iteration.
        assert!(!app.take_needs_redraw());
    }

    /// PERF-H1 — async container action completion arms a redraw via
    /// `pump_action_results` so the bottom-bar status surfaces without
    /// waiting for the next key press.
    #[test]
    fn test_pump_action_results_marks_dirty() {
        let mut app = AppState::new();
        let _ = app.take_needs_redraw(); // drain seed flag
        // Inject a synthetic action outcome.
        app.action_tx.send((Level::Info, "kill ok".into())).unwrap();
        app.pump_action_results();
        assert!(app.take_needs_redraw(), "action outcome must arm redraw");
        assert_eq!(
            app.active_status().map(|t| t.text.as_str()),
            Some("kill ok")
        );
    }

    /// PERF-H1 — when the status message expires, the main loop schedules
    /// one final repaint via `status_message_just_expired`.
    #[test]
    fn test_status_message_just_expired_returns_true_and_clears() {
        let mut app = AppState::new();
        // Seed an artificially-old status message.
        app.notify(Level::Info, "old");
        app.notifier.backdate_for_test(Duration::from_secs(60));
        assert!(app.status_message_just_expired());
        assert!(app.active_status().is_none());
        // Subsequent calls return false so the main loop doesn't loop-paint.
        assert!(!app.status_message_just_expired());
    }

    /// PERF-H4 — `apply_snapshot` populates the cached projection and
    /// `selected_container` reads from that cache.
    #[test]
    fn test_apply_snapshot_populates_container_cache() {
        let mut app = AppState::new();
        assert!(app.sorted_filtered_containers().is_empty());
        app.apply_snapshot(make_snapshot_with_container("abc", "svc"));
        let rows = app.sorted_filtered_containers();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "svc");
        // selected_container reads from the cache.
        assert_eq!(app.selected_container().unwrap().name, "svc");
    }

    /// PERF-H4 — switching sort field eagerly refreshes the cache so the
    /// next render sees the new ordering with no extra projection cost.
    #[test]
    fn test_sort_change_refreshes_container_cache() {
        let mut app = AppState::new();
        app.tab = Tab::Containers;
        // Two containers with different CPU values so Cpu desc and Mem desc
        // produce opposite orderings.
        let mut snap = make_snapshot_with_container("a-id", "alpha");
        // Sole owner at this point, so `make_mut` mutates in place.
        if let Some(cs) = snap.containers.as_mut().map(std::sync::Arc::make_mut) {
            cs.containers[0].cpu_pct = 1.0;
            cs.containers[0].mem_used_bytes = 9_000;
            cs.containers
                .push(muxtop_core::containers::ContainerSnapshot {
                    id: "b-id".into(),
                    id_full: "b-id".into(),
                    name: "bravo".into(),
                    image: "img".into(),
                    state: muxtop_core::containers::ContainerState::Running,
                    status_text: "Up".into(),
                    cpu_pct: 99.0,
                    mem_used_bytes: 1_000,
                    mem_limit_bytes: 0,
                    net_rx_bytes: 0,
                    net_tx_bytes: 0,
                    block_read_bytes: 0,
                    block_write_bytes: 0,
                    started_at_ms: 0,
                });
        }
        app.apply_snapshot(snap);
        // Default sort is Cpu desc → bravo (cpu=99) first.
        assert_eq!(app.sorted_filtered_containers()[0].name, "bravo");
        // Cycle 's' once → Mem desc, where alpha (mem=9000) leads.
        app.handle_key_event(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        assert_eq!(app.sorted_filtered_containers()[0].name, "alpha");
    }

    /// PERF-L3 — `quit()` cancels the shared shutdown token so spawned
    /// container action tasks unwind via their `tokio::select!` arm.
    #[test]
    fn test_quit_cancels_shutdown_token() {
        let mut app = AppState::new();
        let token = app.shutdown_token();
        assert!(!token.is_cancelled());
        app.quit();
        assert!(token.is_cancelled());
    }

    /// PERF-M3 — palette filtering reuses its `Matcher`. After the first
    /// non-empty `refilter`, the matcher slot must be `Some(_)`.
    #[test]
    fn test_palette_matcher_is_cached() {
        let mut ps = PaletteState::new();
        assert!(ps.matcher.is_none());
        ps.input = "sort".into();
        ps.refilter();
        assert!(
            ps.matcher.is_some(),
            "matcher should be cached after first use"
        );
        // Subsequent refilters reuse the same instance — assert no panic and
        // results stay consistent.
        ps.refilter();
        assert!(!ps.filtered.is_empty());
    }

    /// PERF-H3 — `recompute_visible_debounced` collapses bursts of
    /// keystrokes into a single recompute. Two back-to-back calls inside
    /// the debounce window only trigger one pipeline run; the second
    /// keystroke's filter change is therefore NOT yet visible until either
    /// the debounce window elapses or the user commits via Enter.
    #[test]
    fn test_filter_debounce_coalesces_bursts() {
        let mut app = AppState::new();
        app.apply_snapshot(make_snapshot(vec![
            make_process(1, "firefox", 10.0, 100),
            make_process(2, "kitty", 20.0, 200),
            make_process(3, "tmux", 5.0, 50),
        ]));
        // Type "f": "firefox" matches.
        app.filter_input.push('f');
        app.recompute_visible_debounced();
        assert_eq!(app.visible_processes.len(), 1);
        assert_eq!(app.visible_processes[0].name, "firefox");
        // Immediately type "k": within the debounce window, the visible
        // list does NOT yet reflect the new "fk" filter (which would match
        // nothing) — debounce held off the work.
        app.filter_input.push('k');
        app.recompute_visible_debounced();
        assert_eq!(
            app.visible_processes.len(),
            1,
            "burst should be coalesced — the in-flight `fk` filter has NOT been applied yet"
        );
        // Synchronous recompute path (e.g. user hits Enter) commits the
        // pending change.
        app.last_filter_change = None;
        app.recompute_visible();
        assert_eq!(
            app.visible_processes.len(),
            0,
            "after debounce flush the `fk` filter matches nothing"
        );
    }

    // -- GPU tab tests (v0.5) --

    /// Build a snapshot carrying a GPU payload, leaving everything else empty.
    fn snapshot_with_gpu(gpu: muxtop_core::gpu::GpusSnapshot) -> SystemSnapshot {
        use muxtop_core::network::NetworkSnapshot;
        use muxtop_core::system::{CpuSnapshot, LoadSnapshot, MemorySnapshot};

        SystemSnapshot {
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
            containers: None,
            kube: None,
            gpu: Some(gpu),
            timestamp_ms: 0,
        }
    }

    fn gpu_snapshot_with(devices: usize, processes: usize) -> muxtop_core::gpu::GpusSnapshot {
        use muxtop_core::gpu::{
            GpuBackend, GpuDeviceSnapshot, GpuProcessKind, GpuProcessSnapshot, GpuVendor,
            GpusSnapshot,
        };

        GpusSnapshot {
            backends: vec![GpuBackend::Nvml],
            available: devices > 0,
            devices: (0..devices)
                .map(|i| GpuDeviceSnapshot {
                    index: i as u32,
                    vendor: GpuVendor::Nvidia,
                    backend: GpuBackend::Nvml,
                    name: format!("GPU {i}"),
                    bus_id: String::new(),
                    driver_version: None,
                    utilization_pct: Some(10.0),
                    mem_utilization_pct: None,
                    mem_used_bytes: Some(1024),
                    mem_total_bytes: Some(4096),
                    temperature_c: Some(50.0),
                    power_watts: None,
                    power_limit_watts: None,
                    graphics_clock_mhz: None,
                    memory_clock_mhz: None,
                    fan_pct: None,
                    encoder_pct: None,
                    decoder_pct: None,
                    supports_process_stats: true,
                })
                .collect(),
            processes: (0..processes)
                .map(|i| GpuProcessSnapshot {
                    pid: 1000 + i as u32,
                    device_index: 0,
                    name: format!("worker{i}"),
                    kind: GpuProcessKind::Compute,
                    mem_bytes: Some(512),
                })
                .collect(),
            detail: String::new(),
        }
    }

    #[test]
    fn test_alt6_switches_to_gpu() {
        let mut app = AppState::new();
        app.handle_key_event(KeyEvent::new(KeyCode::Char('6'), KeyModifiers::ALT));
        assert_eq!(app.tab, Tab::Gpu);
    }

    #[test]
    fn test_gpu_subview_keys_switch_views() {
        let mut app = AppState::new();
        app.tab = Tab::Gpu;
        assert_eq!(app.gpu_subview, GpuSubview::Devices);

        app.handle_key_event(key(KeyCode::Char('P')));
        assert_eq!(app.gpu_subview, GpuSubview::Procs);

        app.handle_key_event(key(KeyCode::Char('D')));
        assert_eq!(app.gpu_subview, GpuSubview::Devices);
    }

    /// `D` and `P` are bound on both the Kube and GPU tabs; the guards must
    /// keep them from crossing over.
    #[test]
    fn test_gpu_subview_keys_do_not_leak_into_kube() {
        let mut app = AppState::new();
        app.tab = Tab::Kube;
        app.handle_key_event(key(KeyCode::Char('P')));
        assert_eq!(app.kube_subview, KubeSubview::Pods);
        assert_eq!(
            app.gpu_subview,
            GpuSubview::Devices,
            "the Kube tab must not move the GPU sub-view"
        );

        let mut app = AppState::new();
        app.tab = Tab::Gpu;
        app.handle_key_event(key(KeyCode::Char('D')));
        assert_eq!(
            app.kube_subview,
            KubeSubview::Pods,
            "the GPU tab must not move the Kube sub-view"
        );
    }

    #[test]
    fn test_gpu_subview_switch_resets_sort_filter_and_selection() {
        let mut app = AppState::new();
        app.tab = Tab::Gpu;
        app.gpu_filter_input = "stale".into();
        app.gpu_selected = 7;
        app.gpu_scroll_offset = 3;
        app.gpu_sort_field = GpuSortField::DeviceTemp;

        app.switch_gpu_subview(GpuSubview::Procs);

        assert!(app.gpu_filter_input.is_empty());
        assert_eq!(app.gpu_selected, 0);
        assert_eq!(app.gpu_scroll_offset, 0);
        assert_eq!(app.gpu_sort_field, GpuSortField::ProcMem);
    }

    #[test]
    fn test_gpu_sort_cycles_within_the_active_subview() {
        // Devices cycle stays in the Device domain and returns to its start.
        let mut field = GpuSortField::default_for(GpuSubview::Devices);
        let start = field;
        let mut seen = vec![field];
        for _ in 0..5 {
            field = next_gpu_sort_field(field, GpuSubview::Devices);
            assert!(
                matches!(
                    field,
                    GpuSortField::DeviceIndex
                        | GpuSortField::DeviceName
                        | GpuSortField::DeviceUtil
                        | GpuSortField::DeviceMem
                        | GpuSortField::DeviceTemp
                        | GpuSortField::DevicePower
                ),
                "cycle escaped the Devices domain: {field:?}"
            );
            seen.push(field);
        }
        assert_eq!(
            next_gpu_sort_field(field, GpuSubview::Devices),
            start,
            "the cycle must close"
        );
        assert_eq!(seen.len(), 6, "every Devices sort field is reachable");
    }

    #[test]
    fn test_gpu_sort_recovers_from_an_out_of_domain_field() {
        // A Devices field while the Procs sub-view is active must fall back
        // to the Procs default instead of getting stuck.
        assert_eq!(
            next_gpu_sort_field(GpuSortField::DeviceTemp, GpuSubview::Procs),
            GpuSortField::ProcMem
        );
        assert_eq!(
            next_gpu_sort_field(GpuSortField::ProcPid, GpuSubview::Devices),
            GpuSortField::DeviceIndex
        );
    }

    #[test]
    fn test_gpu_count_tracks_the_active_subview() {
        let mut app = AppState::new();
        app.tab = Tab::Gpu;
        app.apply_snapshot(snapshot_with_gpu(gpu_snapshot_with(2, 5)));

        assert_eq!(app.gpu_count(), 2, "Devices sub-view counts devices");
        app.switch_gpu_subview(GpuSubview::Procs);
        assert_eq!(app.gpu_count(), 5, "Procs sub-view counts processes");
    }

    #[test]
    fn test_gpu_count_is_zero_without_a_snapshot() {
        let app = AppState::new();
        assert_eq!(app.gpu_count(), 0);
    }

    #[test]
    fn test_gpu_filter_matches_pid_and_name() {
        let mut app = AppState::new();
        app.tab = Tab::Gpu;
        app.apply_snapshot(snapshot_with_gpu(gpu_snapshot_with(1, 3)));
        app.switch_gpu_subview(GpuSubview::Procs);

        app.gpu_filter_input = "worker1".into();
        assert_eq!(app.gpu_count(), 1);

        app.gpu_filter_input = "1002".into();
        assert_eq!(app.gpu_count(), 1, "filtering by PID must work");

        app.gpu_filter_input = "nope".into();
        assert_eq!(app.gpu_count(), 0);
    }

    #[test]
    fn test_gpu_filter_input_mode_captures_keys() {
        let mut app = AppState::new();
        app.tab = Tab::Gpu;
        app.handle_key_event(key(KeyCode::Char('/')));
        assert!(app.gpu_filter_active);

        app.handle_key_event(key(KeyCode::Char('a')));
        app.handle_key_event(key(KeyCode::Char('b')));
        assert_eq!(app.gpu_filter_input, "ab");

        app.handle_key_event(key(KeyCode::Backspace));
        assert_eq!(app.gpu_filter_input, "a");

        app.handle_key_event(key(KeyCode::Enter));
        assert!(!app.gpu_filter_active);
        assert_eq!(
            app.gpu_filter_input, "a",
            "Enter commits, it does not clear"
        );
    }

    #[test]
    fn test_gpu_esc_clears_a_committed_filter() {
        let mut app = AppState::new();
        app.tab = Tab::Gpu;
        app.gpu_filter_input = "abc".into();
        app.gpu_selected = 4;

        app.handle_key_event(key(KeyCode::Esc));
        assert!(app.gpu_filter_input.is_empty());
        assert_eq!(app.gpu_selected, 0);
    }

    #[test]
    fn test_gpu_selection_is_bounded_by_the_row_count() {
        let mut app = AppState::new();
        app.tab = Tab::Gpu;
        app.apply_snapshot(snapshot_with_gpu(gpu_snapshot_with(2, 0)));

        // `G` jumps to the last row; with 2 devices that is index 1.
        app.handle_key_event(key(KeyCode::Char('G')));
        assert_eq!(app.gpu_selected, 1);

        // And moving past the end must not run away.
        for _ in 0..10 {
            app.handle_key_event(key(KeyCode::Char('j')));
        }
        assert!(
            app.gpu_selected < 2,
            "selection escaped the row count: {}",
            app.gpu_selected
        );
    }

    #[test]
    fn test_gpu_palette_command_switches_tab() {
        let mut app = AppState::new();
        app.execute_command(Command::SwitchToGpu);
        assert_eq!(app.tab, Tab::Gpu);
    }

    /// Every tab reachable from the palette — the Kube tab shipped in v0.4
    /// without an entry and the gap went unnoticed for two releases.
    #[test]
    fn test_palette_has_a_switch_command_for_every_tab() {
        for tab in Tab::ALL {
            let expected = format!("Switch to {}", tab.label());
            let found = Command::ALL.iter().any(|c| {
                let label = c.label();
                label == expected
                    // Kube's entry reads "Switch to Kubernetes tab".
                    || (*tab == Tab::Kube && label == "Switch to Kubernetes tab")
                    || label == format!("{expected} tab")
            });
            assert!(found, "no palette command switches to the {tab} tab");
        }
    }
    // @generated by:gauntlet run:muxtop-audit-fixes
    #[test]
    fn audit_filter_escape_flushes_rapid_input_while_paused() {
        let mut app = AppState::new();
        app.apply_snapshot(make_snapshot(vec![make_process(1, "firefox", 1.0, 1)]));
        app.paused = true;
        app.filter_active = true;
        app.handle_key_event(key(KeyCode::Char('f')));
        app.handle_key_event(key(KeyCode::Char('z')));
        app.handle_key_event(key(KeyCode::Esc));
        assert!(app.visible_processes.is_empty());
    }

    #[test]
    fn audit_gpu_sort_command_targets_active_view() {
        let mut app = AppState::new();
        app.tab = Tab::Gpu;
        app.switch_gpu_subview(GpuSubview::Procs);
        app.apply_snapshot(snapshot_with_gpu(gpu_snapshot_with(1, 2)));
        app.execute_invocation(Command::SortBy, Some("pid".into()));
        assert_eq!(app.gpu_sort_field, GpuSortField::ProcPid);
        assert_eq!(app.selected_identifier().as_deref(), Some("1001"));
    }

    #[test]
    fn audit_snapshot_shrink_clamps_every_view() {
        let mut app = AppState::new();
        app.net_selected = 99;
        app.net_scroll_offset = 99;
        app.containers_selected = 99;
        app.containers_scroll_offset = 99;
        app.kube_selected = 99;
        app.kube_scroll_offset = 99;
        app.gpu_selected = 99;
        app.gpu_scroll_offset = 99;
        app.apply_snapshot(make_snapshot(vec![]));
        assert_eq!((app.net_selected, app.net_scroll_offset), (0, 0));
        assert_eq!(
            (app.containers_selected, app.containers_scroll_offset),
            (0, 0)
        );
        assert_eq!((app.kube_selected, app.kube_scroll_offset), (0, 0));
        assert_eq!((app.gpu_selected, app.gpu_scroll_offset), (0, 0));
    }

    #[test]
    fn audit_gpu_projection_sort_reverse_and_pid_filter() {
        let mut app = AppState::new();
        app.tab = Tab::Gpu;
        app.switch_gpu_subview(GpuSubview::Procs);
        let mut gpu = gpu_snapshot_with(1, 2);
        gpu.processes[0].mem_bytes = Some(1);
        gpu.processes[1].mem_bytes = Some(100);
        app.apply_snapshot(snapshot_with_gpu(gpu));
        assert_eq!(app.selected_identifier().as_deref(), Some("1001"));
        app.overlay = Overlay::Inspector;
        let text =
            crate::ui::test_support::all_text(&crate::ui::test_support::render_with(&app, 62, 24));
        assert!(text.contains("worker1"));
        assert!(!text.contains("worker0"));
        app.reverse_sort();
        assert_eq!(app.selected_identifier().as_deref(), Some("1000"));
        app.set_filter("1001");
        assert_eq!(app.gpu_count(), 1);
        assert_eq!(app.selected_identifier().as_deref(), Some("1001"));
        let text =
            crate::ui::test_support::all_text(&crate::ui::test_support::render_with(&app, 62, 24));
        assert!(text.contains("worker1"));
        app.overlay = Overlay::None;
        let text =
            crate::ui::test_support::all_text(&crate::ui::test_support::render_with(&app, 100, 30));
        assert!(text.contains("worker1"));
        assert!(!text.contains("worker0"));
    }
    #[test]
    fn audit_gpu_vendor_filter_reaches_table_inspector_and_copy() {
        use crate::ui::test_support::{all_text, render_with};
        let mut app = AppState::new();
        app.tab = Tab::Gpu;
        app.apply_snapshot(snapshot_with_gpu(gpu_snapshot_with(1, 0)));
        app.set_filter("NVIDIA");
        assert_eq!(app.gpu_count(), 1);
        assert_eq!(app.selected_identifier().as_deref(), Some("GPU 0"));
        assert!(all_text(&render_with(&app, 100, 30)).contains("GPU 0"));
        app.overlay = Overlay::Inspector;
        assert!(all_text(&render_with(&app, 62, 24)).contains("GPU 0"));
    }

    #[test]
    fn audit_kube_inspector_uses_sorted_projection() {
        use crate::ui::test_support::{all_text, render_with};
        let mut app = AppState::new();
        app.tab = Tab::Kube;
        app.apply_snapshot(snapshot_with_pods(&[
            ("default", "alpha", 10),
            ("default", "bravo", 500),
        ]));
        app.overlay = Overlay::Inspector;
        let text = all_text(&render_with(&app, 62, 24));
        assert!(text.contains("bravo"));
        assert!(!text.contains("alpha"));
        app.reverse_sort();
        assert!(all_text(&render_with(&app, 62, 24)).contains("alpha"));
        app.set_filter("bravo");
        assert!(all_text(&render_with(&app, 62, 24)).contains("bravo"));
    }

    #[test]
    fn audit_snapshot_shrink_keeps_remaining_rows_visible_on_inactive_tabs() {
        let mut app = AppState::new();
        let mut snap = snapshot_with_pods(&[("default", "remaining", 1)]);
        snap.networks = crate::ui::test_support::snapshot().networks;
        snap.networks.interfaces.truncate(1);
        snap.gpu = Some(gpu_snapshot_with(1, 0));
        app.selected = 2;
        app.scroll_offset = 2;
        app.net_selected = 2;
        app.net_scroll_offset = 2;
        app.containers_selected = 2;
        app.containers_scroll_offset = 2;
        app.kube_selected = 2;
        app.kube_scroll_offset = 2;
        app.gpu_selected = 2;
        app.gpu_scroll_offset = 2;
        app.apply_snapshot(snap);
        for tab in [Tab::Network, Tab::Containers, Tab::Kube, Tab::Gpu] {
            app.tab = tab;
            assert!(
                app.selected_identifier().is_some(),
                "{tab:?} lost its remaining row"
            );
            let (selected, offset) = app.selected_mut();
            assert_eq!((*selected, *offset), (0, 0));
        }
    }

    #[test]
    fn audit_filter_tick_applies_final_paused_input_after_deadline() {
        let mut app = AppState::new();
        app.apply_snapshot(make_snapshot(vec![make_process(1, "firefox", 1.0, 1)]));
        app.paused = true;
        app.filter_input = "no-match".into();
        app.last_filter_change = Some(Instant::now() - FILTER_DEBOUNCE);
        app.take_needs_redraw();
        app.tick();
        assert!(app.visible_processes.is_empty());
        assert!(app.take_needs_redraw());
    }
    #[test]
    fn audit_filter_deadline_uses_controlled_time() {
        let mut app = AppState::new();
        app.apply_snapshot(make_snapshot(vec![make_process(1, "firefox", 1.0, 1)]));
        app.paused = true;
        app.filter_input = "no-match".into();
        let changed = Instant::now();
        app.last_filter_change = Some(changed);
        app.take_needs_redraw();
        assert!(!crate::handle_idle_tick_at(
            &mut app,
            changed + FILTER_DEBOUNCE - Duration::from_nanos(1),
        ));
        assert_eq!(app.process_count(), 1);
        assert!(crate::handle_idle_tick_at(
            &mut app,
            changed + FILTER_DEBOUNCE
        ));
        assert_eq!(app.process_count(), 0);
        assert!(!crate::handle_idle_tick_at(
            &mut app,
            changed + FILTER_DEBOUNCE
        ));
    }

    #[test]
    fn audit_kube_text_sort_changes_the_visible_selection() {
        let mut app = AppState::new();
        app.tab = Tab::Kube;
        app.apply_snapshot(snapshot_with_pods(&[
            ("default", "zebra", 500),
            ("default", "alpha", 10),
        ]));
        assert_eq!(app.selected_identifier().as_deref(), Some("zebra"));
        app.execute_invocation(Command::SortBy, Some("name".into()));
        assert_eq!(app.selected_identifier().as_deref(), Some("alpha"));
    }

    #[test]
    fn audit_gpu_text_sort_rejects_columns_from_another_subview() {
        let mut app = AppState::new();
        app.tab = Tab::Gpu;
        let original = app.gpu_sort_field;
        app.execute_invocation(Command::SortBy, Some("pid".into()));
        assert_eq!(app.gpu_sort_field, original);
        assert!(matches!(app.sort_field, SortField::Cpu));
        assert_eq!(app.notifier.latest().unwrap().level, Level::Warning);
        assert!(
            app.notifier
                .latest()
                .unwrap()
                .text
                .contains("not a sortable column")
        );
    }
}
