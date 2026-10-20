use crate::data::{Catalog, Row};
use pinyin::ToPinyinMulti;
use std::collections::{HashMap, HashSet};

pub fn ranked_matches(
    rows: &[Row],
    titles: &[usize],
    bodies: &HashSet<String>,
    searching: bool,
) -> Vec<usize> {
    let titles: HashSet<_> = titles.iter().copied().collect();
    let mut matches: Vec<_> = rows
        .iter()
        .enumerate()
        .filter_map(|(i, r)| (titles.contains(&i) || bodies.contains(&r.id)).then_some(i))
        .collect();
    if searching {
        matches.sort_by(|&a, &b| {
            (rows[a].state != "Busy", !titles.contains(&a))
                .cmp(&(rows[b].state != "Busy", !titles.contains(&b)))
                .then_with(|| rows[b].updated_at.total_cmp(&rows[a].updated_at))
                .then(a.cmp(&b))
        });
    }
    matches
}

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

/// Shared name/pinyin matcher for tag names, cached independently of chat search.
#[derive(Default)]
pub struct LabelSearch { entries: HashMap<String, SearchText> }
impl LabelSearch {
    pub fn sync<'a>(&mut self,labels:impl Iterator<Item=(&'a str,&'a str)>){
        let mut ids=HashSet::new();
        for(key,label)in labels{
            ids.insert(key.to_string());
            if self.entries.get(key).is_none_or(|e|e.source!=label){
                self.entries.insert(key.into(),SearchText::new(label,label));
            }
        }
        self.entries.retain(|key,_|ids.contains(key));
    }
    pub fn matches(&self,key:&str,query:&str)->bool{
        let query=query.trim().to_lowercase();
        query.is_empty()||self.entries.get(key).is_some_and(|e|query.split_whitespace().all(|term|e.matches(term)))
    }
}

#[cfg(test)]
mod label_tests {
    use super::*;
    #[test]
    fn tag_names_full_pinyin_initials_and_renames() {
        let mut index=LabelSearch::default();
        index.sync([("a","图像排查"),("b","IP-research"),("c","重庆采集")].into_iter());
        for query in ["排查","paicha","pc","TUXIANG pc"]{assert!(index.matches("a",query),"{query}");}
        assert!(index.matches("b","ip"));assert!(index.matches("c","chongqing"));
        index.sync([("a","图像仿真")].into_iter());
        assert!(!index.matches("a","pc"));assert!(index.matches("a","fangzhen"));assert!(!index.matches("b","ip"));
    }
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
    fn busy_then_title_then_body_and_recency_breaks_ties() {
        let rows: Vec<_> = ["Done", "Busy", "Done", "Busy", "Busy"]
            .iter()
            .enumerate()
            .map(|(i, s)| Row {
                id: i.to_string(),
                state: (*s).into(),
                updated_at: [15., 5., 25., 35., 45.][i],
                ..Row::default()
            })
            .collect();
        let bodies = ["1".to_string(), "2".to_string(), "3".to_string()]
            .into_iter()
            .collect();
        assert_eq!(
            ranked_matches(&rows, &[0, 4], &bodies, true),
            [4, 3, 1, 0, 2]
        );
        assert_eq!(
            ranked_matches(&rows, &[0, 1, 2, 3, 4], &HashSet::new(), false),
            [0, 1, 2, 3, 4]
        );
    }
    #[test]
    fn title_filters_remain_bound_to_ids_after_reorder_and_rename() {
        let paths = crate::Paths::discover().unwrap();
        let catalog = Catalog::load(&paths).unwrap();
        let mut rows = vec![
            Row {
                id: "output".into(),
                label: "输出 JPEG".into(),
                ..Default::default()
            },
            Row {
                id: "camera".into(),
                label: "相机采集".into(),
                ..Default::default()
            },
        ];
        let mut search = ChatSearch::default();
        search.sync(&rows, &catalog);
        assert_eq!(search.filter(&rows, "shuchu", false), [0]);
        rows.reverse();
        search.sync(&rows, &catalog);
        assert_eq!(search.filter(&rows, "shuchu", false), [1]);
        rows[1].label = "重命名的相机".into();
        rows[0].label = "输出新报告".into();
        search.sync(&rows, &catalog);
        assert_eq!(search.filter(&rows, "shuchu", false), [0]);
    }
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
