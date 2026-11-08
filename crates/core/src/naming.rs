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
    clean_to(title, 60)
}

/// `clean`, keeping up to `max` characters.
fn clean_to(title: &str, max: usize) -> String {
    let safe: String = title.chars().filter(|c| !c.is_control() && !r#"<>:"/\|?*"#.contains(*c)).take(max).collect();
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

/// A clip's name without its ending, and the ending: ".DVR.mp4" for instant replay clips (in
/// whatever case the file has it), ".mp4" otherwise. The ending is what says which kind it is, so
/// renaming and trimming keep it.
pub fn split_name(name: &str) -> (&str, &str) {
    let lower = name.to_ascii_lowercase();
    let cut = if lower.ends_with(".dvr.mp4") {
        name.len() - 8
    } else if lower.ends_with(".mp4") {
        name.len() - 4
    } else {
        name.len()
    };
    name.split_at(cut)
}

/// Where a trimmed copy of `path` goes: beside it, "<name> trim.DVR.mp4", then "trim 2" and up.
pub fn trimmed_path(path: &Path) -> PathBuf {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let (stem, ending) = split_name(&name);
    (1..)
        .map(|n| path.with_file_name(if n == 1 { format!("{stem} trim{ending}") } else { format!("{stem} trim {n}{ending}") }))
        .find(|p| !p.exists())
        .unwrap()
}

/// `path` renamed to `title`, keeping its folder and ending. None if nothing usable is left of
/// the title.
pub fn renamed_path(path: &Path, title: &str) -> Option<PathBuf> {
    let name = path.file_name()?.to_string_lossy().into_owned();
    let (_, ending) = split_name(&name);
    let title = clean_to(title, 120);
    // "x.DVR" typed as the title would otherwise turn a recording into a clip.
    let (title, _) = split_name(&title);
    let title = title.trim_end_matches(['.', ' ']);
    let lower = title.to_ascii_lowercase();
    let title = if lower.ends_with(".dvr") { title[..title.len() - 4].trim_end() } else { title };
    if title.is_empty() || RESERVED.contains(&title.to_uppercase().as_str()) {
        return None;
    }
    Some(path.with_file_name(format!("{title}{ending}")))
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
    fn endings_say_what_kind_a_file_is() {
        assert_eq!(split_name("Fortnite 2026.10.08 - 22.14.44.26.DVR.mp4"), ("Fortnite 2026.10.08 - 22.14.44.26", ".DVR.mp4"));
        assert_eq!(split_name("a.dvr.MP4"), ("a", ".dvr.MP4"));
        assert_eq!(split_name("a.mp4"), ("a", ".mp4"));
    }

    #[test]
    fn trimmed_copies_sit_beside_the_original() {
        let dir = std::env::temp_dir().join(format!("tape-trim-name-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("CS2 2026.11.01 - 20.00.00.00.DVR.mp4");
        assert_eq!(trimmed_path(&clip), dir.join("CS2 2026.11.01 - 20.00.00.00 trim.DVR.mp4"));
        std::fs::write(dir.join("CS2 2026.11.01 - 20.00.00.00 trim.DVR.mp4"), b"x").unwrap();
        assert_eq!(trimmed_path(&clip), dir.join("CS2 2026.11.01 - 20.00.00.00 trim 2.DVR.mp4"));
        assert_eq!(trimmed_path(&dir.join("r.mp4")), dir.join("r trim.mp4"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn renaming_keeps_the_folder_and_kind() {
        let clip = Path::new("/v/Tape/CS2/CS2 2026.11.01 - 20.00.00.00.DVR.mp4");
        assert_eq!(renamed_path(clip, "ace on mirage").unwrap(), Path::new("/v/Tape/CS2/ace on mirage.DVR.mp4"));
        assert_eq!(renamed_path(clip, " ace: on/mirage? ").unwrap(), Path::new("/v/Tape/CS2/ace onmirage.DVR.mp4"));
        assert_eq!(renamed_path(clip, "ace.DVR.mp4").unwrap(), Path::new("/v/Tape/CS2/ace.DVR.mp4"));
        let recording = Path::new("/v/Tape/CS2/long game.mp4");
        assert_eq!(renamed_path(recording, "x.DVR").unwrap(), Path::new("/v/Tape/CS2/x.mp4"));
        assert_eq!(renamed_path(recording, "x.mp4").unwrap(), Path::new("/v/Tape/CS2/x.mp4"));
        assert!(renamed_path(clip, " ?? ").is_none());
        assert!(renamed_path(clip, "con").is_none());
    }

    #[test]
    fn cleaning_makes_names_windows_accepts() {
        assert_eq!(clean("Valheim: \"Ashlands\"?"), "Valheim Ashlands");
        assert_eq!(clean(" ?? ... "), "");
        assert_eq!(clean("nul"), "");
        assert_eq!(clean(&"x".repeat(300)).len(), 60);
    }
}
