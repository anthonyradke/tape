use std::path::{Path, PathBuf};

use chrono::{DateTime, Local, Timelike};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipKind {
    Replay,
    Recording,
}

/// Names Windows won't accept for a file or folder, whatever the extension.
const RESERVED: &[&str] = &["CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "LPT1", "LPT2", "LPT3"];

/// Videos\Tape in the user's profile.
pub fn default_folder() -> PathBuf {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    home.map(PathBuf::from).unwrap_or_default().join("Videos").join("Tape")
}

/// Earlier versions used Videos\tape. Windows would treat that as the same folder and keep the old
/// spelling, so it's renamed once.
pub fn rename_old_default_folder() {
    let folder = default_folder();
    let (Some(parent), Some(wanted)) = (folder.parent(), folder.file_name()) else { return };
    let Ok(entries) = std::fs::read_dir(parent) else { return };
    let old = entries.flatten().map(|e| e.file_name()).find(|n| n != wanted && n.eq_ignore_ascii_case(wanted));
    if let Some(old) = old {
        let _ = std::fs::rename(parent.join(old), &folder);
    }
}

/// The scheme NVIDIA uses, so tools and habits built around it keep working:
/// "Fortnite 2026.10.08 - 22.14.44.26.DVR.mp4", without ".DVR" for a recording.
pub fn file_name(game: &str, kind: ClipKind, when: DateTime<Local>) -> String {
    let stamp = when.format("%Y.%m.%d - %H.%M.%S");
    let hundredths = when.nanosecond() / 10_000_000 % 100;
    let kind = if kind == ClipKind::Replay { ".DVR" } else { "" };
    format!("{game} {stamp}.{hundredths:02}{kind}.mp4")
}

/// <root>\<game>\<file>, with a number added if that somehow exists already.
pub fn output_path(root: &Path, game: &str, kind: ClipKind, when: DateTime<Local>) -> PathBuf {
    unique(&root.join(game).join(file_name(game, kind, when)))
}

/// Safe as both a folder name and the start of a file name. Empty if nothing usable is left.
pub fn clean(title: &str) -> String {
    let safe: String = title.chars().filter(|c| !c.is_control() && !r#"<>:"/\|?*"#.contains(*c)).take(60).collect();
    let safe = safe.trim().trim_end_matches('.').trim();
    if RESERVED.contains(&safe.to_uppercase().as_str()) {
        return String::new();
    }
    safe.to_string()
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
        Local.with_ymd_and_hms(2026, 10, 8, 22, 14, 44).unwrap() + chrono::Duration::milliseconds(260)
    }

    #[test]
    fn replay_clips_match_nvidias_names() {
        assert_eq!(file_name("Fortnite", ClipKind::Replay, when()), "Fortnite 2026.10.08 - 22.14.44.26.DVR.mp4");
    }

    #[test]
    fn recordings_leave_out_dvr() {
        assert_eq!(file_name("Fortnite", ClipKind::Recording, when()), "Fortnite 2026.10.08 - 22.14.44.26.mp4");
    }

    #[test]
    fn clips_go_in_a_folder_for_the_game() {
        let path = output_path(Path::new("/nonexistent/Tape"), "Lethal Company", ClipKind::Replay, when());
        assert_eq!(path, Path::new("/nonexistent/Tape/Lethal Company/Lethal Company 2026.10.08 - 22.14.44.26.DVR.mp4"));
    }

    #[test]
    fn a_name_already_taken_gets_a_number() {
        let dir = std::env::temp_dir().join(format!("tape-naming-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let taken = dir.join("a.DVR.mp4");
        std::fs::write(&taken, b"x").unwrap();
        assert_eq!(unique(&taken), dir.join("a.DVR (2).mp4"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cleaning_makes_names_windows_accepts() {
        assert_eq!(clean("Valheim: \"Ashlands\"?"), "Valheim Ashlands");
        assert_eq!(clean(" ?? ... "), "");
        assert_eq!(clean("nul"), "");
        assert_eq!(clean(&"x".repeat(300)).len(), 60);
    }
}
