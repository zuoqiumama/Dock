//! The app-drawer's custom categories — user-made rows ("游戏", "工作", …) that group
//! desktop programs. Persisted to `%APPDATA%\FeatherDock\drawer.toml` in the same tiny
//! `[[category]]` table style as the items config. Membership is stored by each
//! program's stable `key` (see `desktop_scan`), so an assignment survives rescans,
//! re-installs, and reboots.

use std::fs;
use std::io;
use std::path::PathBuf;

pub struct Category {
    pub name: String,
    pub items: Vec<String>, // entry keys, in the user's drag order
}

#[derive(Default)]
pub struct Categories {
    pub categories: Vec<Category>,
    pub hidden: Vec<String>, // entry keys the user removed from the drawer
}

impl Categories {
    /// Index of the category that currently owns `key`, if any. (Query helper used by
    /// tests and available to callers; the drawer drives membership via `move_item`.)
    #[allow(dead_code)]
    pub fn category_of(&self, key: &str) -> Option<usize> {
        self.categories
            .iter()
            .position(|c| c.items.iter().any(|k| k == key))
    }

    /// Append a new category, returning its index.
    pub fn add(&mut self, name: &str) -> usize {
        self.categories.push(Category {
            name: name.to_string(),
            items: Vec::new(),
        });
        self.categories.len() - 1
    }

    /// Rename a category (no-op if the index is out of range).
    pub fn rename(&mut self, index: usize, name: &str) {
        if let Some(cat) = self.categories.get_mut(index) {
            cat.name = name.to_string();
        }
    }

    /// Delete a category; its members simply become uncategorized again.
    pub fn remove(&mut self, index: usize) {
        if index < self.categories.len() {
            self.categories.remove(index);
        }
    }

    /// Move `key` to `target` (None = uncategorized) at `index` within that category.
    /// First detaches the key from wherever it currently lives, so it never appears
    /// in two rows. `index` is clamped to the destination's length.
    pub fn move_item(&mut self, key: &str, target: Option<usize>, index: usize) {
        for cat in &mut self.categories {
            cat.items.retain(|k| k != key);
        }
        self.hidden.retain(|k| k != key);
        if let Some(t) = target {
            if let Some(cat) = self.categories.get_mut(t) {
                let at = index.min(cat.items.len());
                cat.items.insert(at, key.to_string());
            }
        }
    }

    /// Hide a drawer item without deleting the underlying shortcut/application.
    pub fn hide_item(&mut self, key: &str) {
        for cat in &mut self.categories {
            cat.items.retain(|k| k != key);
        }
        if !self.hidden.iter().any(|k| k == key) {
            self.hidden.push(key.to_string());
        }
    }

    pub fn is_hidden(&self, key: &str) -> bool {
        self.hidden.iter().any(|k| k == key)
    }

    pub fn restore_hidden(&mut self) {
        self.hidden.clear();
    }
}

fn config_path() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("FeatherDock")
        .join("drawer.toml")
}

const HEADER: &str = "\
# FeatherDock app-drawer customisation.
# `hidden` lines hide programs from the drawer without deleting the shortcut/app.
# One [[category]] per row; `item` lines list member programs by stable key.
";

/// Load the categories, or an empty set if the file is missing / unreadable.
pub fn load() -> Categories {
    let Ok(text) = fs::read_to_string(config_path()) else {
        return Categories::default();
    };
    parse(&text)
}

/// Persist the categories (creating the folder/file as needed).
pub fn save(categories: &Categories) -> io::Result<()> {
    let path = config_path();
    let mut body = String::from(HEADER);
    for key in &categories.hidden {
        body.push_str(&format!("hidden = \"{}\"\n", quote(key)));
    }
    for cat in &categories.categories {
        body.push_str("\n[[category]]\n");
        body.push_str(&format!("name = \"{}\"\n", quote(&cat.name)));
        for key in &cat.items {
            body.push_str(&format!("item = \"{}\"\n", quote(key)));
        }
    }
    crate::atomic::write(&path, body.as_bytes())
}

fn quote(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        let mut out = String::new();
        let mut chars = s[1..s.len() - 1].chars();
        while let Some(ch) = chars.next() {
            if ch == '\\' {
                match chars.next() {
                    Some('\\') => out.push('\\'),
                    Some('"') => out.push('"'),
                    Some(other) => out.push(other),
                    None => out.push('\\'),
                }
            } else {
                out.push(ch);
            }
        }
        out
    } else {
        s.to_string()
    }
}

pub fn parse(text: &str) -> Categories {
    let mut categories: Vec<Category> = Vec::new();
    let mut hidden: Vec<String> = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line == "[[category]]" {
            categories.push(Category {
                name: String::new(),
                items: Vec::new(),
            });
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "hidden" => {
                let v = unquote(value);
                if !v.is_empty() && !hidden.iter().any(|k| k == &v) {
                    hidden.push(v);
                }
            }
            "name" => {
                if let Some(cat) = categories.last_mut() {
                    cat.name = unquote(value);
                }
            }
            "item" => {
                if let Some(cat) = categories.last_mut() {
                    let v = unquote(value);
                    if !v.is_empty() {
                        cat.items.push(v);
                    }
                }
            }
            _ => {}
        }
    }
    Categories { categories, hidden }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_names_and_members_with_backslashes() {
        let mut cats = Categories::default();
        let g = cats.add("游戏");
        cats.move_item(r"c:\games\lol.lnk", Some(g), 0);
        cats.move_item("::{645ff040-5081-101b-9f08-00aa002f954e}", Some(g), 1);
        let w = cats.add("工作");
        cats.move_item(r"c:\tools\code.exe", Some(w), 0);

        let text = {
            // serialize via the same body builder save() uses
            let mut body = String::new();
            for cat in &cats.categories {
                body.push_str("[[category]]\n");
                body.push_str(&format!("name = \"{}\"\n", quote(&cat.name)));
                for key in &cat.items {
                    body.push_str(&format!("item = \"{}\"\n", quote(key)));
                }
            }
            body
        };

        let back = parse(&text);
        assert_eq!(back.categories.len(), 2);
        assert_eq!(back.categories[0].name, "游戏");
        assert_eq!(back.categories[0].items[0], r"c:\games\lol.lnk");
        assert_eq!(
            back.categories[0].items[1],
            "::{645ff040-5081-101b-9f08-00aa002f954e}"
        );
        assert_eq!(back.categories[1].name, "工作");
    }

    #[test]
    fn move_item_is_exclusive_and_clamps_index() {
        let mut cats = Categories::default();
        let a = cats.add("A");
        let b = cats.add("B");
        cats.move_item("x", Some(a), 99); // clamps to 0
        assert_eq!(cats.categories[a].items, vec!["x".to_string()]);
        cats.move_item("x", Some(b), 0); // detaches from A first
        assert!(cats.categories[a].items.is_empty());
        assert_eq!(cats.categories[b].items, vec!["x".to_string()]);
        assert_eq!(cats.category_of("x"), Some(b));
        cats.move_item("x", None, 0); // back to uncategorized
        assert_eq!(cats.category_of("x"), None);
    }

    #[test]
    fn hidden_items_are_detached_and_can_be_restored() {
        let mut cats = Categories::default();
        let games = cats.add("Games");
        cats.move_item("steam", Some(games), 0);

        cats.hide_item("steam");

        assert!(cats.is_hidden("steam"));
        assert_eq!(cats.category_of("steam"), None);
        assert!(cats.categories[games].items.is_empty());

        cats.restore_hidden();

        assert!(!cats.is_hidden("steam"));
    }

    #[test]
    fn parses_hidden_items_outside_category_tables() {
        let cats = parse(
            r#"
hidden = "virt:control-panel"

[[category]]
name = "Work"
item = "code"
"#,
        );

        assert!(cats.is_hidden("virt:control-panel"));
        assert_eq!(cats.categories.len(), 1);
        assert_eq!(cats.categories[0].items, vec!["code".to_string()]);
    }
}
