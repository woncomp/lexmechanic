use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Tab {
    pub key: String,
    pub title: String,
    pub permanent: bool,
}

#[derive(Default)]
pub struct Tabs {
    pub items: Vec<Tab>,
    pub active: Option<String>,
    closed: Vec<Tab>,
    history: Vec<String>,
    cycle_order: Option<Vec<String>>,
    cycle_index: usize,
}

impl Tabs {
    pub fn replace_key(&mut self, previous: &str, current: &str) {
        let permanent = self
            .items
            .iter()
            .any(|tab| (tab.key == previous || tab.key == current) && tab.permanent);
        if let Some(tab) = self.items.iter_mut().find(|tab| tab.key == previous) {
            tab.key = current.to_owned();
        }
        let mut seen = std::collections::HashSet::new();
        self.items.retain(|tab| seen.insert(tab.key.clone()));
        if let Some(tab) = self.items.iter_mut().find(|tab| tab.key == current) {
            tab.permanent |= permanent;
        }
        if self.active.as_deref() == Some(previous) {
            self.active = Some(current.to_owned());
        }
        for key in &mut self.history {
            if key == previous {
                *key = current.to_owned();
            }
        }
        let mut seen = std::collections::HashSet::new();
        self.history.reverse();
        self.history.retain(|key| seen.insert(key.clone()));
        self.history.reverse();
        for tab in &mut self.closed {
            if tab.key == previous {
                tab.key = current.to_owned();
            }
        }
        self.cycle_order = None;
    }

    pub fn move_before(&mut self, key: &str, target: &str) {
        if key == target {
            return;
        }
        if let Some(index) = self.items.iter().position(|tab| tab.key == key) {
            let tab = self.items.remove(index);
            let target = self
                .items
                .iter()
                .position(|tab| tab.key == target)
                .unwrap_or(self.items.len());
            self.items.insert(target, tab);
        }
    }

    pub fn open(&mut self, key: String, title: String, permanent: bool) {
        if let Some(tab) = self.items.iter_mut().find(|tab| tab.key == key) {
            tab.permanent |= permanent;
        } else {
            let tab = Tab {
                key: key.clone(),
                title,
                permanent,
            };
            if !permanent {
                if let Some(index) = self.items.iter().position(|tab| !tab.permanent) {
                    let replaced = self.items.remove(index);
                    self.history.retain(|entry| entry != &replaced.key);
                    self.items.insert(index, tab);
                } else {
                    self.items.push(tab);
                }
            } else {
                self.items.push(tab);
            }
        }
        self.activate(&key);
    }

    pub fn activate(&mut self, key: &str) {
        self.cycle_order = None;
        if self.items.iter().any(|tab| tab.key == key) {
            self.active = Some(key.into());
            self.history.retain(|entry| entry != key);
            self.history.push(key.into());
        }
    }

    pub fn keep(&mut self, key: &str) {
        if let Some(tab) = self.items.iter_mut().find(|tab| tab.key == key) {
            tab.permanent = true;
        }
    }

    pub fn close(&mut self, key: &str) {
        self.finish_cycle();
        if let Some(index) = self.items.iter().position(|tab| tab.key == key) {
            self.closed.push(self.items.remove(index));
            if self.closed.len() > 20 {
                self.closed.remove(0);
            }
            self.history.retain(|entry| entry != key);
            if self.active.as_deref() == Some(key) {
                self.active = self
                    .history
                    .iter()
                    .rev()
                    .find(|key| self.items.iter().any(|tab| &tab.key == *key))
                    .cloned()
                    .or_else(|| self.items.last().map(|tab| tab.key.clone()));
            }
        }
    }

    pub fn reopen(&mut self) -> Option<String> {
        let tab = self.closed.pop()?;
        let key = tab.key.clone();
        self.open(tab.key, tab.title, true);
        Some(key)
    }

    pub fn cycle(&mut self, backwards: bool) {
        if self.items.is_empty() {
            return;
        }
        if self.cycle_order.is_none() {
            let order: Vec<_> = self
                .history
                .iter()
                .rev()
                .filter(|key| self.items.iter().any(|tab| &tab.key == *key))
                .cloned()
                .collect();
            if order.is_empty() {
                return;
            }
            self.cycle_index = order
                .iter()
                .position(|key| Some(key) == self.active.as_ref())
                .unwrap_or_default();
            self.cycle_order = Some(order);
        }
        if let Some(order) = &self.cycle_order {
            self.cycle_index = if backwards {
                (self.cycle_index + order.len() - 1) % order.len()
            } else {
                (self.cycle_index + 1) % order.len()
            };
            self.active = Some(order[self.cycle_index].clone());
        }
    }

    pub fn finish_cycle(&mut self) -> bool {
        if self.cycle_order.take().is_none() {
            return false;
        }
        if let Some(key) = &self.active {
            self.history.retain(|entry| entry != key);
            self.history.push(key.clone());
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_replaces_only_preview_and_existing_permanent_never_downgrades() {
        let mut tabs = Tabs::default();
        tabs.open("a".into(), "A".into(), false);
        tabs.open("a".into(), "A".into(), true);
        for key in ["b", "c", "d"] {
            tabs.open(key.into(), key.into(), false);
        }
        assert_eq!(tabs.items.len(), 2);
        tabs.open("a".into(), "A".into(), false);
        assert!(tabs.items[0].permanent);
        tabs.close("a");
        assert_eq!(tabs.active.as_deref(), Some("d"));
        assert_eq!(tabs.reopen().as_deref(), Some("a"));
        assert_eq!(tabs.items.len(), 2);
    }

    #[test]
    fn tab_cycle_freezes_mru_order_until_control_is_released() {
        let mut tabs = Tabs::default();
        for key in ["a", "b", "c"] {
            tabs.open(key.into(), key.into(), true);
        }
        tabs.cycle(false);
        assert_eq!(tabs.active.as_deref(), Some("b"));
        tabs.cycle(false);
        assert_eq!(tabs.active.as_deref(), Some("a"));
        tabs.finish_cycle();
        tabs.cycle(false);
        assert_eq!(tabs.active.as_deref(), Some("c"));
    }

    #[test]
    fn provider_reclassification_preserves_kept_tabs_and_history() {
        let mut tabs = Tabs::default();
        tabs.open("old-cli".into(), "old title".into(), true);
        tabs.open("other".into(), "other".into(), true);
        tabs.activate("old-cli");
        tabs.replace_key("old-cli", "app");
        assert_eq!(tabs.active.as_deref(), Some("app"));
        assert!(
            tabs.items
                .iter()
                .any(|tab| tab.key == "app" && tab.permanent)
        );
        tabs.cycle(false);
        assert_eq!(tabs.active.as_deref(), Some("other"));
    }
}
