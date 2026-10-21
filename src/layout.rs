//! The dockable panes and the built-in layouts.
//!
//! Panes are tabs in an `egui_dock` tree that the user can drag, split,
//! resize, float and close; closed panes come back from the View menu.
//! Each pane appears at most once.

use egui_dock::{DockState, NodeIndex};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Pane {
    /// Mode buttons, filter and the process table.
    Processes,
    Treemap,
    /// Large system CPU/RAM/disk graphs and per-core load.
    Performance,
    /// The selected app's history and processes.
    Details,
    /// Totals and the top apps, for the compact layout.
    Mini,
}

impl Pane {
    pub const ALL: [Pane; 5] = [Pane::Processes, Pane::Treemap, Pane::Performance, Pane::Details, Pane::Mini];

    pub fn title(self) -> &'static str {
        match self {
            Self::Processes => "Processes",
            Self::Treemap => "Treemap",
            Self::Performance => "Performance",
            Self::Details => "App details",
            Self::Mini => "Top apps",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Preset {
    Default,
    Visual,
    Detailed,
    /// Small always-on-top window with just the totals and top apps.
    Compact,
}

impl Preset {
    pub const ALL: [Preset; 4] = [Preset::Default, Preset::Visual, Preset::Detailed, Preset::Compact];

    pub fn label(self) -> &'static str {
        match self {
            Self::Default => "Default",
            Self::Visual => "Visual",
            Self::Detailed => "Detailed",
            Self::Compact => "Compact",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Default => "Treemap above the table, details on the right",
            Self::Visual => "A big treemap with a small table",
            Self::Detailed => "A big table; the treemap is a tab beside it",
            Self::Compact => "A small always-on-top window with totals and the top 5 apps",
        }
    }

    pub fn dock(self) -> DockState<Pane> {
        // `split_*` fractions are the share the existing node keeps.
        let side = vec![Pane::Details, Pane::Performance];
        match self {
            Self::Default => {
                let mut dock = DockState::new(vec![Pane::Treemap]);
                let tree = dock.main_surface_mut();
                let [left, _] = tree.split_right(NodeIndex::root(), 0.7, side);
                tree.split_below(left, 0.4, vec![Pane::Processes]);
                dock
            }
            Self::Visual => {
                let mut dock = DockState::new(vec![Pane::Treemap]);
                let tree = dock.main_surface_mut();
                let [left, _] = tree.split_right(NodeIndex::root(), 0.75, side);
                tree.split_below(left, 0.72, vec![Pane::Processes]);
                dock
            }
            Self::Detailed => {
                let mut dock = DockState::new(vec![Pane::Processes, Pane::Treemap]);
                dock.main_surface_mut().split_right(NodeIndex::root(), 0.72, side);
                dock
            }
            Self::Compact => DockState::new(vec![Pane::Mini]),
        }
    }
}

/// Makes `pane` the visible tab of its leaf, if it is open.
pub fn focus(dock: &mut DockState<Pane>, pane: Pane) {
    if let Some(at) = dock.find_tab(&pane) {
        dock.set_active_tab(at);
    }
}

pub fn close(dock: &mut DockState<Pane>, pane: Pane) {
    if let Some(at) = dock.find_tab(&pane) {
        dock.remove_tab(at);
    }
}

/// Opens `pane` (or brings it forward) roughly where the default layout
/// has it: details on the right, the treemap on top, others as a tab.
pub fn open(dock: &mut DockState<Pane>, pane: Pane) {
    if dock.find_tab(&pane).is_some() {
        focus(dock, pane);
        return;
    }
    let tree = dock.main_surface_mut();
    if tree.is_empty() {
        tree.push_to_first_leaf(pane);
        return;
    }
    match pane {
        Pane::Details => {
            tree.split_right(NodeIndex::root(), 0.72, vec![pane]);
        }
        Pane::Treemap => {
            tree.split_above(NodeIndex::root(), 0.6, vec![pane]);
        }
        _ => tree.push_to_focused_leaf(pane),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_hold_each_pane_once() {
        for preset in Preset::ALL {
            let dock = preset.dock();
            let tabs: Vec<Pane> = dock.iter_all_tabs().map(|(_, &t)| t).collect();
            for pane in Pane::ALL {
                assert!(tabs.iter().filter(|&&t| t == pane).count() <= 1, "{preset:?}");
            }
        }
    }

    #[test]
    fn closed_panes_reopen() {
        let mut dock = Preset::Default.dock();
        for pane in Pane::ALL {
            close(&mut dock, pane);
        }
        assert!(dock.main_surface().is_empty());
        for pane in Pane::ALL {
            open(&mut dock, pane);
            open(&mut dock, pane);
        }
        assert_eq!(dock.iter_all_tabs().count(), Pane::ALL.len());
    }
}
