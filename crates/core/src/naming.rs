use std::path::{Path, PathBuf};

use chrono::{DateTime, Local};

/// Videos\tape in the user's profile.
pub fn default_folder() -> PathBuf {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    home.map(PathBuf::from).unwrap_or_default().join("Videos").join("tape")
}

/// "<window title> 2026-10-31 21-14-03.mp4". The title makes a folder of clips browsable by game.
pub fn file_name(title: Option<&str>, when: DateTime<Local>) -> String {
    let stamp = when.format("%Y-%m-%d %H-%M-%S");
    match title.map(clean).filter(|t| !t.is_empty()) {
        Some(t) => format!("{t} {stamp}.mp4"),
        None => format!("{stamp}.mp4"),
    }
}

fn clean(title: &str) -> String {
    let safe: String = title
        .chars()
        .filter(|c| !c.is_control() && !r#"<>:"/\|?*"#.contains(*c))
        .take(60)
        .collect();
    safe.trim().trim_end_matches('.').trim().to_string()
}

/// `path`, or the same name with " (2)" and up if it exists.
pub fn unique(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    (2..)
        .map(|n| path.with_file_name(format!("{stem} ({n}).mp4")))
        .find(|p| !p.exists())
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn when() -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 10, 31, 21, 14, 3).unwrap()
    }

    #[test]
    fn includes_a_cleaned_title() {
        assert_eq!(file_name(Some("Valheim: \"Ashlands\"?"), when()), "Valheim Ashlands 2026-10-31 21-14-03.mp4");
    }

    #[test]
    fn no_title_or_an_unusable_one_leaves_the_date() {
        assert_eq!(file_name(None, when()), "2026-10-31 21-14-03.mp4");
        assert_eq!(file_name(Some(" ?? ... "), when()), "2026-10-31 21-14-03.mp4");
    }

    #[test]
    fn caps_long_titles() {
        let long = "x".repeat(300);
        assert_eq!(file_name(Some(&long), when()).len(), 60 + 24);
    }
}
