use crate::data::{Catalog, Row};
use pinyin::ToPinyinMulti;
use std::collections::HashMap;

#[derive(Default)]
pub struct ChatSearch {
    entries: HashMap<String, SearchText>,
}
impl ChatSearch {
    // Build pronunciations once per title, not per keystroke or status refresh.
    pub fn sync(&mut self, rows: &[Row], catalog: &Catalog) -> bool {
        let mut changed = self.entries.len() != rows.len();
        for row in rows {
            if self
                .entries
                .get(&row.id)
                .is_none_or(|e| e.source != row.label)
            {
                self.entries.insert(
                    row.id.clone(),
                    SearchText::new(&row.label, catalog.strip(&row.label)),
                );
                changed = true;
            }
        }
        if self.entries.len() != rows.len() {
            let ids: std::collections::HashSet<_> = rows.iter().map(|r| r.id.as_str()).collect();
            self.entries.retain(|id, _| ids.contains(id.as_str()));
        }
        changed
    }
    pub fn filter(&self, rows: &[Row], query: &str, exact: bool) -> Vec<usize> {
        let query = query.trim().to_lowercase();
        rows.iter()
            .enumerate()
            .filter_map(|(i, row)| {
                let matched = query.is_empty()
                    || self.entries.get(&row.id).is_some_and(|e| {
                        if exact {
                            e.title == query || row.id.to_lowercase() == query
                        } else {
                            query.split_whitespace().all(|term| e.matches(term))
                        }
                    });
                matched.then_some(i)
            })
            .collect()
    }
}

struct SearchText {
    source: String,
    title: String,
    compact: String,
    syllables: Vec<Vec<String>>,
    initials: Vec<Vec<String>>,
}
fn compact(text: &str) -> String {
    text.chars().filter(|c| c.is_alphanumeric()).collect()
}
impl SearchText {
    fn new(source: &str, title: &str) -> Self {
        let title = title.trim().to_lowercase();
        let compact = compact(&title);
        let mut syllables = Vec::new();
        let mut initials = Vec::new();
        for c in compact.chars() {
            let mut sounds: Vec<String> = c
                .to_pinyin_multi()
                .map(|p| p.into_iter().map(|v| v.plain().to_string()).collect())
                .unwrap_or_else(|| vec![c.to_string()]);
            sounds.sort();
            sounds.dedup();
            let mut first: Vec<String> = sounds
                .iter()
                .map(|s| s.chars().next().unwrap().to_string())
                .collect();
            first.sort();
            first.dedup();
            syllables.push(sounds);
            initials.push(first);
        }
        Self {
            source: source.into(),
            title,
            compact,
            syllables,
            initials,
        }
    }
    fn matches(&self, query: &str) -> bool {
        if self.title.contains(query) {
            return true;
        }
        let query = compact(query);
        !query.is_empty()
            && (self.compact.contains(&query)
                || phonetic_match(&self.syllables, &query)
                || phonetic_match(&self.initials, &query))
    }
}

// A small dynamic program handles every pronunciation without expanding an
// exponential number of strings for titles containing several polyphonic words.
fn phonetic_match(pieces: &[Vec<String>], query: &str) -> bool {
    let mut positions = vec![false; query.len() + 1];
    for alternatives in pieces {
        positions[0] = true;
        let mut next = vec![false; positions.len()];
        for (at, reachable) in positions.iter().enumerate() {
            if !reachable {
                continue;
            }
            let remaining = &query[at..];
            for sound in alternatives {
                if sound.starts_with(remaining) {
                    return true;
                }
                if remaining.starts_with(sound) {
                    next[at + sound.len()] = true;
                }
            }
        }
        if next[query.len()] {
            return true;
        }
        positions = next;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn chinese_pinyin_initials_mixed_text_and_polyphones() {
        let title = SearchText::new("📷 GEV 重庆相机采集", "GEV 重庆相机采集");
        for query in [
            "重庆",
            "相机",
            "chongqing",
            "zhongqing",
            "cq",
            "xjcj",
            "gevchongqing",
            "gev重庆",
            "xiangjicai",
            "xiangji",
        ] {
            assert!(title.matches(query), "Query should match: {query}");
        }
        for query in ["usb", "xiangjix", "chongcai"] {
            assert!(!title.matches(query), "Unexpected match: {query}");
        }
    }
    #[test]
    fn exact_match_is_literal_and_filter_keeps_recent_order() {
        let paths = crate::Paths::discover().unwrap();
        let catalog = Catalog::load(&paths).unwrap();
        let rows = vec![
            Row {
                id: "recent".into(),
                label: "📷 GEV 相机".into(),
                ..Row::default()
            },
            Row {
                id: "older".into(),
                label: "🧪 GEV 相机测试".into(),
                ..Row::default()
            },
        ];
        let mut search = ChatSearch::default();
        assert!(search.sync(&rows, &catalog));
        assert!(!search.sync(&rows, &catalog));
        assert_eq!(search.filter(&rows, "GEV xj", false), [0, 1]);
        assert_eq!(search.filter(&rows, "GEV 相机", true), [0]);
        assert!(search.filter(&rows, "xiangji", true).is_empty());
        assert_eq!(search.filter(&rows, "相机测试", false), [1]);
    }
}
