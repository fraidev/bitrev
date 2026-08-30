use rand::seq::SliceRandom;

use crate::file::TorrentFile;

/// Transport selected from a tracker URL scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackerScheme {
    Http,
    Udp,
}

/// BEP-0012 tier list. URLs inside each tier are shuffled once at load.
/// Successful URLs are moved to the front of their tier and tried first
/// on the next announce cycle. Tiers themselves stay in file order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackerTiers {
    tiers: Vec<Vec<String>>,
}

impl TrackerTiers {
    pub fn from_torrent_file(file: &TorrentFile) -> Self {
        Self::from_resolved(file.announce_tiers(), true)
    }

    pub fn from_metainfo(announce: Option<&str>, announce_list: Option<&[Vec<String>]>) -> Self {
        Self::from_resolved(resolve_tiers(announce, announce_list), true)
    }

    pub fn from_tiers_unshuffled(tiers: Vec<Vec<String>>) -> Self {
        Self::from_resolved(tiers, false)
    }

    fn from_resolved(tiers: Vec<Vec<String>>, shuffle: bool) -> Self {
        let mut tiers = normalize_tiers(tiers);
        if shuffle {
            shuffle_tiers(&mut tiers);
        }
        Self { tiers }
    }

    pub fn is_empty(&self) -> bool {
        self.tiers.iter().all(|tier| tier.is_empty())
    }

    pub fn tiers(&self) -> &[Vec<String>] {
        &self.tiers
    }

    /// URLs in announce-try order: tiers first to last, then left to right.
    pub fn urls(&self) -> impl Iterator<Item = &str> {
        self.tiers.iter().flatten().map(String::as_str)
    }

    /// Move `url` to the front of the tier that contains it.
    pub fn promote(&mut self, url: &str) {
        for tier in &mut self.tiers {
            if let Some(idx) = tier.iter().position(|candidate| candidate == url) {
                if idx > 0 {
                    let chosen = tier.remove(idx);
                    tier.insert(0, chosen);
                }
                return;
            }
        }
    }
}

pub fn tracker_scheme(url: &str) -> Option<TrackerScheme> {
    let scheme = url.split("://").next().unwrap_or(url);
    match scheme.to_ascii_lowercase().as_str() {
        "http" | "https" => Some(TrackerScheme::Http),
        "udp" => Some(TrackerScheme::Udp),
        _ => None,
    }
}

fn resolve_tiers(
    announce: Option<&str>,
    announce_list: Option<&[Vec<String>]>,
) -> Vec<Vec<String>> {
    let from_list = announce_list
        .map(|list| normalize_tiers(list.to_vec()))
        .unwrap_or_default();
    if !from_list.is_empty() {
        return from_list;
    }
    match announce {
        Some(url) if !url.is_empty() => vec![vec![url.to_string()]],
        _ => Vec::new(),
    }
}

fn shuffle_tiers(tiers: &mut [Vec<String>]) {
    let mut rng = rand::thread_rng();
    for tier in tiers {
        tier.shuffle(&mut rng);
    }
}

fn normalize_tiers(tiers: Vec<Vec<String>>) -> Vec<Vec<String>> {
    tiers
        .into_iter()
        .map(|tier| {
            tier.into_iter()
                .filter(|url| !url.is_empty())
                .collect::<Vec<_>>()
        })
        .filter(|tier| !tier.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unshuffled(tiers: &[&[&str]]) -> TrackerTiers {
        TrackerTiers::from_tiers_unshuffled(
            tiers
                .iter()
                .map(|tier| tier.iter().map(|url| (*url).to_string()).collect())
                .collect(),
        )
    }

    #[test]
    fn from_metainfo_synthesizes_single_tier_when_list_absent() {
        let tiers = TrackerTiers::from_metainfo(Some("http://tracker.example/announce"), None);
        assert_eq!(
            tiers.tiers(),
            &[vec!["http://tracker.example/announce".to_string()]]
        );
    }

    #[test]
    fn from_metainfo_prefers_announce_list() {
        let list = vec![
            vec!["http://a.example/announce".to_string()],
            vec!["http://b.example/announce".to_string()],
        ];
        let tiers = TrackerTiers::from_metainfo(
            Some("http://legacy.example/announce"),
            Some(list.as_slice()),
        );
        assert_eq!(tiers.tiers(), list.as_slice());
        assert!(!tiers
            .urls()
            .any(|url| url == "http://legacy.example/announce"));
    }

    #[test]
    fn promote_moves_url_to_front_of_its_tier() {
        let mut tiers = unshuffled(&[&["a", "b", "c"], &["d"]]);
        tiers.promote("c");
        assert_eq!(
            tiers.tiers(),
            &[
                vec!["c".to_string(), "a".to_string(), "b".to_string()],
                vec!["d".to_string()]
            ]
        );
        assert_eq!(tiers.urls().collect::<Vec<_>>(), vec!["c", "a", "b", "d"]);
    }

    #[test]
    fn promote_unknown_url_is_a_no_op() {
        let mut tiers = unshuffled(&[&["a", "b"]]);
        tiers.promote("missing");
        assert_eq!(tiers.tiers(), &[vec!["a".to_string(), "b".to_string()]]);
    }

    #[test]
    fn walk_order_is_tiers_then_urls() {
        let tiers = unshuffled(&[&["t0"], &["t1a", "t1b"], &["t2"]]);
        assert_eq!(
            tiers.urls().collect::<Vec<_>>(),
            vec!["t0", "t1a", "t1b", "t2"]
        );
    }

    #[test]
    fn from_metainfo_shuffles_urls_within_each_tier() {
        let original: Vec<String> = (0..8)
            .map(|i| format!("http://t{i}.example/announce"))
            .collect();
        let list = vec![
            original.clone(),
            vec!["http://only.example/announce".into()],
        ];
        let tiers = TrackerTiers::from_metainfo(None, Some(list.as_slice()));

        let mut first = tiers.tiers()[0].clone();
        first.sort();
        let mut expected = original;
        expected.sort();
        assert_eq!(first, expected);
        assert_eq!(
            tiers.tiers()[1],
            vec!["http://only.example/announce".to_string()]
        );
    }

    #[test]
    fn tracker_scheme_classifies_known_and_skips_unknown() {
        assert_eq!(
            tracker_scheme("http://tracker.example/announce"),
            Some(TrackerScheme::Http)
        );
        assert_eq!(
            tracker_scheme("HTTPS://tracker.example/announce"),
            Some(TrackerScheme::Http)
        );
        assert_eq!(
            tracker_scheme("udp://tracker.example:80/announce"),
            Some(TrackerScheme::Udp)
        );
        assert_eq!(tracker_scheme("wss://tracker.example/announce"), None);
        assert_eq!(tracker_scheme("not-a-url"), None);
    }

    #[test]
    fn from_tiers_unshuffled_drops_empty_entries() {
        let tiers = TrackerTiers::from_tiers_unshuffled(vec![
            vec![String::new()],
            vec!["http://ok.example/announce".into(), String::new()],
            vec![],
        ]);
        assert_eq!(
            tiers.tiers(),
            &[vec!["http://ok.example/announce".to_string()]]
        );
    }
}
