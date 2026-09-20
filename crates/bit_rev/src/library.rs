use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::resume;

/// A named download bucket with an optional default save path.
///
/// An empty `save_path` means torrents in this category use the session
/// `download_dir`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Category {
    pub name: String,
    pub save_path: Option<PathBuf>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct CategoriesFile {
    #[serde(default)]
    category: Vec<CategoryToml>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CategoryToml {
    name: String,
    #[serde(default)]
    save_path: String,
}

impl Category {
    pub fn new(name: impl Into<String>, save_path: Option<PathBuf>) -> Self {
        let save_path = save_path.filter(|path| !path.as_os_str().is_empty());
        Self {
            name: name.into(),
            save_path,
        }
    }
}

impl From<&Category> for CategoryToml {
    fn from(category: &Category) -> Self {
        Self {
            name: category.name.clone(),
            save_path: category
                .save_path
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default(),
        }
    }
}

impl From<CategoryToml> for Category {
    fn from(row: CategoryToml) -> Self {
        Category::new(row.name, Some(PathBuf::from(row.save_path)))
    }
}

pub fn categories_path(state_dir: &Path) -> PathBuf {
    util::paths::categories_toml(state_dir)
}

pub fn watch_processed_path(state_dir: &Path) -> PathBuf {
    util::paths::watch_processed_dir(state_dir)
}

pub fn load_categories(state_dir: &Path) -> BTreeMap<String, Category> {
    let path = categories_path(state_dir);
    match std::fs::read_to_string(&path) {
        Ok(text) => match toml::from_str::<CategoriesFile>(&text) {
            Ok(file) => file
                .category
                .into_iter()
                .map(|row| {
                    let category = Category::from(row);
                    (category.name.clone(), category)
                })
                .collect(),
            Err(e) => {
                warn!(path = %path.display(), error = %e, "ignoring corrupt categories.toml");
                BTreeMap::new()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
        Err(e) => {
            warn!(path = %path.display(), error = %e, "failed to read categories.toml");
            BTreeMap::new()
        }
    }
}

pub fn save_categories(
    state_dir: &Path,
    categories: &BTreeMap<String, Category>,
) -> Result<(), resume::ResumeError> {
    let file = CategoriesFile {
        category: categories.values().map(CategoryToml::from).collect(),
    };
    let text = toml::to_string_pretty(&file)
        .map_err(|e| resume::ResumeError::Decode(format!("encode categories.toml: {e}")))?;
    resume::write_atomic(&categories_path(state_dir), text.as_bytes())
}

pub fn is_watch_torrent(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("torrent"))
}

pub fn is_watch_magnet_name(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("magnet"))
}

pub fn file_starts_with_magnet(path: &Path) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    bytes.starts_with(b"magnet:")
}

/// First directory under `watch_dir` that contains `file`, if any.
pub fn watch_subdir_category(watch_dir: &Path, file: &Path) -> Option<String> {
    let parent = file.parent()?;
    if parent == watch_dir {
        return None;
    }
    let rel = parent.strip_prefix(watch_dir).ok()?;
    rel.components().next().and_then(|component| {
        let name = component.as_os_str().to_str()?;
        (!name.is_empty()).then(|| name.to_string())
    })
}

pub fn collect_watch_files(watch_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(watch_dir) else {
        return out;
    };
    let mut entries: Vec<_> = entries.flatten().map(|entry| entry.path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            let Ok(inner) = std::fs::read_dir(&path) else {
                continue;
            };
            let mut inner: Vec<_> = inner.flatten().map(|entry| entry.path()).collect();
            inner.sort();
            for child in inner {
                if child.is_file() {
                    out.push(child);
                }
            }
        } else if path.is_file() {
            out.push(path);
        }
    }
    out
}

pub fn unique_processed_path(processed_dir: &Path, src: &Path) -> PathBuf {
    let name = src
        .file_name()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("watched"));
    let mut dest = processed_dir.join(&name);
    if !dest.exists() {
        return dest;
    }
    let stem = src
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("watched");
    let ext = src.extension().and_then(|s| s.to_str()).unwrap_or("");
    for i in 1..u32::MAX {
        dest = if ext.is_empty() {
            processed_dir.join(format!("{stem}-{i}"))
        } else {
            processed_dir.join(format!("{stem}-{i}.{ext}"))
        };
        if !dest.exists() {
            return dest;
        }
    }
    dest
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bitrev-library-{label}-{}-{}",
            std::process::id(),
            resume::now_unix()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn categories_round_trip_toml() {
        let dir = temp_dir("cats");
        let mut map = BTreeMap::new();
        map.insert(
            "tv-sonarr".into(),
            Category::new("tv-sonarr", Some(PathBuf::from("/data/tv"))),
        );
        map.insert("radarr".into(), Category::new("radarr", None));
        save_categories(&dir, &map).unwrap();
        let loaded = load_categories(&dir);
        assert_eq!(loaded, map);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_categories_file_is_empty() {
        let dir = temp_dir("missing");
        assert!(load_categories(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_categories_file_is_empty() {
        let dir = temp_dir("corrupt");
        std::fs::write(categories_path(&dir), "not toml {").unwrap();
        assert!(load_categories(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn watch_subdir_category_uses_first_component() {
        let watch = Path::new("/watch");
        assert_eq!(
            watch_subdir_category(watch, Path::new("/watch/tv/a.torrent")).as_deref(),
            Some("tv")
        );
        assert_eq!(
            watch_subdir_category(watch, Path::new("/watch/a.torrent")),
            None
        );
    }
}
