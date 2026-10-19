//! Geheimnisfreier Tab-Navigationsstand; kein Providerzustand und kein Permit.
use std::collections::BTreeSet;

use crate::{window_commands::PaneStartLocation, AppError};

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct WindowTabs {
    pub(crate) left: PaneTabs,
    pub(crate) right: PaneTabs,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PaneTabs {
    pub(crate) active_id: String,
    pub(crate) tabs: Vec<SavedTab>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SavedTab {
    pub(crate) id: String,
    pub(crate) location: PaneStartLocation,
    pub(crate) history: Vec<String>,
    pub(crate) history_index: i64,
    pub(crate) selected_names: Vec<String>,
    pub(crate) cursor_name: Option<String>,
    pub(crate) anchor_name: Option<String>,
    pub(crate) scroll_top: f64,
    pub(crate) sort: TabSort,
    pub(crate) view_options: TabViewOptions,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct TabSort {
    key: String,
    dir: i8,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct TabViewOptions {
    columns: TabColumns,
    folders_first: bool,
    folders_alphabetical: bool,
    bold_folders: bool,
    zebra_rows: bool,
    relative_dates: bool,
    show_hidden: bool,
    density: String,
    text_size: String,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct TabColumns {
    size: bool,
    kind: bool,
    modified: bool,
    created: bool,
    owner: bool,
    group: bool,
    permissions: bool,
}

impl Default for TabColumns {
    fn default() -> Self {
        Self {
            size: true,
            kind: false,
            modified: true,
            created: false,
            owner: false,
            group: false,
            permissions: false,
        }
    }
}

impl Default for TabViewOptions {
    fn default() -> Self {
        Self {
            columns: TabColumns::default(),
            folders_first: true,
            folders_alphabetical: true,
            bold_folders: true,
            zebra_rows: false,
            relative_dates: false,
            show_hidden: false,
            density: "compact".into(),
            text_size: "m".into(),
        }
    }
}

impl WindowTabs {
    pub(crate) fn from_active(panes: &crate::window_commands::WindowStartState) -> Self {
        Self {
            left: PaneTabs::from_location(panes.left.clone()),
            right: PaneTabs::from_location(panes.right.clone()),
        }
    }

    pub(crate) fn validate(
        &self,
        panes: &crate::window_commands::WindowStartState,
    ) -> Result<(), AppError> {
        self.left.validate(&panes.left)?;
        self.right.validate(&panes.right)
    }
}

impl PaneTabs {
    fn from_location(location: PaneStartLocation) -> Self {
        Self {
            active_id: "tab-initial".into(),
            tabs: vec![SavedTab {
                id: "tab-initial".into(),
                history: vec![location.path.clone()],
                location,
                history_index: 0,
                selected_names: vec![],
                cursor_name: None,
                anchor_name: None,
                scroll_top: 0.0,
                sort: TabSort {
                    key: "name".into(),
                    dir: 1,
                },
                view_options: TabViewOptions::default(),
            }],
        }
    }

    fn validate(&self, active: &PaneStartLocation) -> Result<(), AppError> {
        if self.tabs.is_empty() || self.tabs.len() > 32 {
            return Err(invalid("Eine Pane braucht zwischen einem und 32 Tabs"));
        }
        let mut ids = BTreeSet::new();
        for tab in &self.tabs {
            tab.validate()?;
            if !ids.insert(&tab.id) {
                return Err(invalid("Doppelte Tab-ID in einer Pane"));
            }
        }
        let tab = self
            .tabs
            .iter()
            .find(|tab| tab.id == self.active_id)
            .ok_or_else(|| invalid("Aktive Tab fehlt"))?;
        if tab.location.provider != active.provider
            || tab.location.profile_id != active.profile_id
            || tab.location.profile_revision != active.profile_revision
            || tab.location.path != active.path
        {
            return Err(invalid("Aktive Tab und Pane-Endpunkt widersprechen sich"));
        }
        Ok(())
    }
}

impl SavedTab {
    fn validate(&self) -> Result<(), AppError> {
        self.location.validate()?;
        let path = |value: &str| {
            value.len() <= 4096
                && !value.contains('\0')
                && std::path::Path::new(value).is_absolute()
        };
        let name = |value: &str| {
            !value.is_empty()
                && value.len() <= 255
                && !value.contains(['/', '\0'])
                && !matches!(value, "." | "..")
        };
        if self.id.is_empty()
            || self.id.len() > 64
            || !self
                .id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            || !path(&self.location.path)
            || self.history.len() > 128
            || self.history.iter().any(|item| !path(item))
            || (self.history.is_empty() && self.history_index != -1)
            || (!self.history.is_empty()
                && (self.history_index < 0 || self.history_index as usize >= self.history.len()))
            || self.selected_names.len() > 4096
            || self.selected_names.iter().any(|item| !name(item))
            || self.selected_names.iter().collect::<BTreeSet<_>>().len()
                != self.selected_names.len()
            || self.cursor_name.as_deref().is_some_and(|item| !name(item))
            || self.anchor_name.as_deref().is_some_and(|item| !name(item))
            || !self.scroll_top.is_finite()
            || !(0.0..=1_000_000_000.0).contains(&self.scroll_top)
            || !matches!(
                self.sort.key.as_str(),
                "name"
                    | "size"
                    | "kind"
                    | "modified"
                    | "created"
                    | "owner"
                    | "group"
                    | "permissions"
            )
            || !matches!(self.sort.dir, -1 | 1)
            || !matches!(self.view_options.density.as_str(), "compact" | "normal")
            || !matches!(self.view_options.text_size.as_str(), "s" | "m" | "l")
        {
            return Err(invalid("Ungültiger oder zu großer Tab-Navigationsstand"));
        }
        Ok(())
    }
}

fn invalid(message: &str) -> AppError {
    AppError::new("invalid_input", message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn panes() -> crate::window_commands::WindowStartState {
        serde_json::from_value(serde_json::json!({
            "left": {"provider":"local", "path":"/quelle"},
            "right": {"provider":"ftps", "profileId":"fixture", "profileRevision":7, "path":"/ziel"}
        }))
        .unwrap()
    }

    #[test]
    fn verschachtelte_frontend_felder_und_inaktive_endpunkte_bleiben_erhalten() {
        let panes = panes();
        let mut json = serde_json::to_value(WindowTabs::from_active(&panes)).unwrap();
        let mut tab = json["right"]["tabs"][0].clone();
        tab["id"] = "tab-zweiter".into();
        tab["location"]["path"] = "/anderer".into();
        tab["history"] = serde_json::json!(["/", "/vorher", "/anderer"]);
        tab["historyIndex"] = 2.into();
        tab["selectedNames"] = serde_json::json!(["ä.txt", "zwei.txt"]);
        tab["cursorName"] = "zwei.txt".into();
        tab["anchorName"] = "ä.txt".into();
        tab["scrollTop"] = 93.5.into();
        tab["sort"] = serde_json::json!({"key":"modified", "dir":-1});
        tab["viewOptions"]["columns"]["owner"] = true.into();
        tab["viewOptions"]["foldersFirst"] = false.into();
        tab["viewOptions"]["showHidden"] = true.into();
        json["right"]["tabs"]
            .as_array_mut()
            .unwrap()
            .push(tab.clone());
        let state: WindowTabs = serde_json::from_value(json).unwrap();
        state.validate(&panes).unwrap();
        let encoded = serde_json::to_value(&state).unwrap();
        assert_eq!(encoded["right"]["tabs"][1], tab);
        assert_eq!(state.right.tabs[1].location.profile_revision, Some(7));
        assert_eq!(state.left.tabs.len(), 1);
    }

    #[test]
    fn ungueltige_tabbindung_und_unbegrenzte_navigation_werden_abgelehnt() {
        let panes = panes();
        let initial = WindowTabs::from_active(&panes);
        let mut mutations: Vec<WindowTabs> = Vec::new();
        let mut value = initial.clone();
        value.left.tabs.push(value.left.tabs[0].clone());
        mutations.push(value);
        let mut value = initial.clone();
        value.left.active_id = "fehlt".into();
        mutations.push(value);
        let mut value = initial.clone();
        value.right.tabs[0].location.profile_revision = Some(8);
        mutations.push(value);
        let mut value = initial.clone();
        value.left.tabs[0].history_index = 2;
        mutations.push(value);
        let mut value = initial.clone();
        value.left.tabs[0].history.push("relativ".into());
        mutations.push(value);
        let mut value = initial.clone();
        value.left.tabs[0].selected_names = vec!["../fremd".into()];
        mutations.push(value);
        let mut value = initial.clone();
        value.left.tabs[0].selected_names = vec!["doppelt".into(), "doppelt".into()];
        mutations.push(value);
        let mut value = initial.clone();
        value.left.tabs[0].scroll_top = f64::INFINITY;
        mutations.push(value);
        let mut value = initial.clone();
        value.left.tabs[0].view_options.density = "beliebige-css-klasse".into();
        mutations.push(value);
        for state in mutations {
            assert_eq!(state.validate(&panes).unwrap_err().code, "invalid_input");
        }
        initial.validate(&panes).unwrap();
    }

    #[test]
    fn unbekannte_felder_auch_in_location_sort_und_darstellung_sind_lesefehler() {
        let initial = serde_json::to_value(WindowTabs::from_active(&panes())).unwrap();
        for pointer in [
            "/left",
            "/left/tabs/0",
            "/left/tabs/0/location",
            "/left/tabs/0/sort",
            "/left/tabs/0/viewOptions",
            "/left/tabs/0/viewOptions/columns",
        ] {
            let mut value = initial.clone();
            value
                .pointer_mut(pointer)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .insert("password".into(), "synthetischer-testwert".into());
            assert!(
                serde_json::from_value::<WindowTabs>(value).is_err(),
                "{pointer}"
            );
        }
    }
}
