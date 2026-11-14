//! Unicode and Chinese search, shared by the palette and drawer.
use pinyin::ToPinyin;

struct SearchName {
    direct: String,
    full: String,
    initials: String,
}

impl SearchName {
    fn new(name: &str) -> Self {
        let mut full = String::new();
        let mut initials = String::new();
        for ch in name.chars() {
            if let Some(py) = ch.to_pinyin() {
                full.push_str(py.plain());
                if let Some(first) = py.plain().chars().next() {
                    initials.push(first);
                }
            } else {
                full.extend(ch.to_lowercase());
                initials.extend(ch.to_lowercase());
            }
        }
        Self {
            direct: name.to_lowercase(),
            full,
            initials,
        }
    }

    fn matches(&self, term: &str) -> bool {
        self.direct.contains(term) || self.full.contains(term) || self.initials.contains(term)
    }

    fn score(&self, query: &str) -> i32 {
        if self.direct == query {
            -1
        } else if self.direct.starts_with(query) {
            0
        } else if self
            .direct
            .split(|ch: char| ch.is_whitespace() || ch == '-' || ch == '_')
            .any(|word| word.starts_with(query))
        {
            1
        } else if self.direct.contains(query) {
            2
        } else if self.full == query || self.initials == query {
            0
        } else if self.full.starts_with(query) || self.initials.starts_with(query) {
            1
        } else if self.full.contains(query) || self.initials.contains(query) {
            2
        } else {
            3
        }
    }
}

pub fn matches_names<'a>(
    names: impl IntoIterator<Item = &'a str>,
    detail: &str,
    query: &str,
) -> bool {
    let names: Vec<SearchName> = names.into_iter().map(SearchName::new).collect();
    let detail = detail.to_lowercase();
    query.split_whitespace().all(|term| {
        let term = term.to_lowercase();
        detail.contains(&term) || names.iter().any(|name| name.matches(&term))
    })
}

pub fn score_names<'a>(names: impl IntoIterator<Item = &'a str>, query: &str) -> i32 {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return 0;
    }
    names
        .into_iter()
        .map(SearchName::new)
        .map(|name| name.score(&query))
        .min()
        .unwrap_or(3)
}

pub fn matches(label: &str, detail: &str, query: &str) -> bool {
    matches_names(std::iter::once(label), detail, query)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chinese_full_pinyin_initials_and_mixed_names() {
        assert!(matches("微信", "", "weixin"));
        assert!(matches("微信", "", "wx"));
        assert!(matches(
            "Visual Studio Code",
            r"C:\Tools\Code.exe",
            "tools code"
        ));
        assert!(matches("控制面板", "", "kzmb"));
        assert!(!matches("微信", "", "wangyi"));
    }

    #[test]
    fn aliases_share_direct_and_pinyin_scoring_but_detail_is_direct_only() {
        assert!(matches_names(["Launcher", "微信"], "", "weixin"));
        assert!(matches_names(["Launcher", "微信"], "", "wx"));
        assert!(matches_names(["Launcher", "微信"], "", "微信"));
        assert!(score_names(["Launcher", "Code"], "code") < score_names(["Code Notes"], "code"));
        assert!(matches("微信", "工具", "工具"));
        assert!(!matches("Launcher", "微信", "wx"));
    }
}
