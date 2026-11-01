//! Works out which game a window belongs to, for sorting clips into folders. Store install paths
//! give clean names without a database. A small table covers games they don't, and anything else
//! counts as a game only if it fills a monitor without a title bar.

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::naming;

/// The folder for anything that isn't a game, the same name NVIDIA uses.
pub const DESKTOP: &str = "Desktop";

/// The window with keyboard focus.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Foreground {
    pub title: String,
    /// Full path of its executable. Empty if Windows wouldn't say.
    pub exe: String,
    /// Covers a whole monitor and has no title bar.
    pub fullscreen: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Seen {
    Game(String),
    /// tape's own window or overlay, which says nothing about what's being played.
    Tape,
    Other,
}

/// Games whose install path gives no name or the wrong one.
const KNOWN: &[(&str, &str)] = &[
    ("blackops3.exe", "Call of Duty Black Ops 3"),
    ("cod.exe", "Call of Duty"),
    ("cs2.exe", "Counter-Strike 2"),
    ("dolphin.exe", "Dolphin Emulator"),
    ("fallguys_client_game.exe", "Fall Guys"),
    ("fortniteclient-win64-shipping.exe", "Fortnite"),
    ("gta5.exe", "Grand Theft Auto V"),
    ("gta5_enhanced.exe", "Grand Theft Auto V Enhanced"),
    ("minecraft.windows.exe", "Minecraft"),
    ("modernwarfare.exe", "Call of Duty Modern Warfare 2019"),
    ("plutonium-bootstrapper-win32.exe", "Plutonium"),
    ("t6mp.exe", "Call of Duty Black Ops II"),
    ("t6sp.exe", "Call of Duty Black Ops II"),
    ("t6zm.exe", "Call of Duty Black Ops II"),
];

/// Folders whose children are one game each, as lowercase path pieces.
const LIBRARIES: &[&[&str]] = &[
    &["epic games"],
    &["ea games"],
    &["gog games"],
    &["gog galaxy", "games"],
    &["origin games"],
    &["riot games"],
    &["rockstar games"],
    &["ubisoft game launcher", "games"],
    &["xboxgames"],
];

/// Things that live in those folders but aren't games.
const NOT_LIBRARY_GAMES: &[&str] = &["launcher", "riot client", "social club", "steam controller configs", "steamworks shared"];

/// Programs that often fill a monitor without being games.
const NOT_FULLSCREEN_GAMES: &[&str] = &[
    "applicationframehost.exe",
    "brave.exe",
    "chrome.exe",
    "discord.exe",
    "explorer.exe",
    "firefox.exe",
    "lockapp.exe",
    "mpc-hc64.exe",
    "mpv.exe",
    "msedge.exe",
    "obs64.exe",
    "opera.exe",
    "potplayer64.exe",
    "powerpnt.exe",
    "searchhost.exe",
    "shellexperiencehost.exe",
    "spotify.exe",
    "startmenuexperiencehost.exe",
    "textinputhost.exe",
    "vivaldi.exe",
    "vlc.exe",
    "wmplayer.exe",
];

pub fn identify(app: &Foreground) -> Seen {
    let parts: Vec<&str> = app.exe.split(['\\', '/']).filter(|p| !p.is_empty()).collect();
    let file = parts.last().map(|p| p.to_lowercase()).unwrap_or_default();
    if file == "tape.exe" || file == "tape" {
        return Seen::Tape;
    }
    if let Some((_, name)) = KNOWN.iter().find(|(exe, _)| *exe == file) {
        return Seen::Game((*name).to_string());
    }
    // Java runs Minecraft, and the title carries a version and the world type.
    if file == "javaw.exe" && app.title.starts_with("Minecraft") {
        return Seen::Game("Minecraft".to_string());
    }
    let named = steam_game(&app.exe, &parts).or_else(|| library_game(&parts)).map(|n| naming::clean(&n)).filter(|n| !n.is_empty());
    if let Some(name) = named {
        return Seen::Game(name);
    }
    if app.fullscreen && !NOT_FULLSCREEN_GAMES.contains(&file.as_str()) {
        let name = from_title(&app.title);
        let name = if name.is_empty() { naming::clean(file.trim_end_matches(".exe")) } else { name };
        if !name.is_empty() {
            return Seen::Game(name);
        }
    }
    Seen::Other
}

/// Follows the foreground window over time. Pressing save while alt-tabbed to Discord, or from
/// tape's own overlay, should still file the clip under the game that's in it.
#[derive(Default)]
pub struct Tracker {
    window: Option<Foreground>,
    seen: Option<Seen>,
    last: Option<(String, Instant)>,
}

impl Tracker {
    pub fn update(&mut self, window: Option<Foreground>, now: Instant) {
        // Only on a change: identifying a Steam game reads its manifests.
        if window != self.window || self.seen.is_none() {
            self.seen = Some(window.as_ref().map_or(Seen::Other, identify));
            self.window = window;
        }
        match self.seen.as_ref() {
            Some(Seen::Game(name)) => self.last = Some((name.clone(), now)),
            Some(Seen::Tape) => {
                if let Some(last) = &mut self.last {
                    last.1 = now;
                }
            }
            _ => {}
        }
    }

    /// The game in front within the last `age`, or `DESKTOP`.
    pub fn game(&self, age: Duration, now: Instant) -> String {
        match &self.last {
            Some((name, at)) if now.duration_since(*at) <= age => name.clone(),
            _ => DESKTOP.to_string(),
        }
    }
}

/// ...\steamapps\common\<install dir>\...\game.exe. Steam's manifests hold the store name, which
/// can differ a lot from the folder ("Counter-Strike Global Offensive" is now Counter-Strike 2).
fn steam_game(exe: &str, parts: &[&str]) -> Option<String> {
    let at = parts.windows(3).position(|w| w[0].eq_ignore_ascii_case("steamapps") && w[1].eq_ignore_ascii_case("common"))?;
    let dir = *parts.get(at + 2)?;
    // Past the install folder there must be a file, or this is just the folder itself.
    parts.get(at + 3)?;
    if NOT_LIBRARY_GAMES.contains(&dir.to_lowercase().as_str()) {
        return None;
    }
    let steamapps = Path::new(exe).ancestors().find(|p| p.file_name().is_some_and(|n| n.eq_ignore_ascii_case("steamapps")));
    Some(steamapps.and_then(|p| manifest_name(p, dir)).unwrap_or_else(|| dir.to_string()))
}

fn library_game(parts: &[&str]) -> Option<String> {
    for library in LIBRARIES {
        let size = library.len();
        // Library, game folder, then at least one file inside it.
        let found = parts.windows(size + 2).find(|w| w[..size].iter().zip(*library).all(|(a, b)| a.eq_ignore_ascii_case(b)));
        if let Some(window) = found {
            let dir = window[size];
            if !NOT_LIBRARY_GAMES.contains(&dir.to_lowercase().as_str()) {
                return Some(dir.to_string());
            }
        }
    }
    None
}

fn manifest_name(steamapps: &Path, install_dir: &str) -> Option<String> {
    fs::read_dir(steamapps)
        .ok()?
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("appmanifest_"))
        .filter_map(|e| fs::read_to_string(e.path()).ok())
        .find_map(|text| {
            let installed = manifest_value(&text, "installdir")?;
            installed.eq_ignore_ascii_case(install_dir).then(|| manifest_value(&text, "name")).flatten()
        })
}

/// Lines in Steam's files look like `"key"    "value"`.
fn manifest_value(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let mut pieces = line.split('"');
        let (_, found, _, value) = (pieces.next()?, pieces.next()?, pieces.next()?, pieces.next()?);
        found.eq_ignore_ascii_case(key).then(|| value.to_string())
    })
}

/// "Hades II - v1.0 (Vulkan)" to "Hades II". Titles often carry versions and the current map,
/// which would split one game across many folders.
fn from_title(title: &str) -> String {
    let plain: String = title.chars().filter(|c| !"®™©".contains(*c)).collect();
    let first = plain.split(" - ").next().unwrap_or("").split(" | ").next().unwrap_or("");
    let words: Vec<&str> = first.split_whitespace().take_while(|w| !is_version(w)).collect();
    naming::clean(&words.join(" "))
}

fn is_version(word: &str) -> bool {
    let digits = word.strip_prefix(['v', 'V']).unwrap_or(word);
    digits.contains('.') && digits.starts_with(|c: char| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn seen(exe: &str, title: &str, fullscreen: bool) -> Seen {
        identify(&Foreground { title: title.into(), exe: exe.into(), fullscreen })
    }

    fn game(name: &str) -> Seen {
        Seen::Game(name.into())
    }

    #[test]
    fn known_executables_win_over_their_titles() {
        let fortnite = r"C:\Program Files\Epic Games\Fortnite\FortniteGame\Binaries\Win64\FortniteClient-Win64-Shipping.exe";
        assert_eq!(seen(fortnite, "Fortnite  ", false), game("Fortnite"));
        assert_eq!(seen(r"D:\anywhere\t6mp.exe", "Call of Duty®: Black Ops II - Multiplayer", false), game("Call of Duty Black Ops II"));
    }

    #[test]
    fn java_is_only_a_game_when_it_says_minecraft() {
        let java = r"C:\Program Files\Java\bin\javaw.exe";
        assert_eq!(seen(java, "Minecraft* 1.21.1 - Multiplayer (3rd-party Server)", false), game("Minecraft"));
        assert_eq!(seen(java, "Some IDE", false), Seen::Other);
    }

    #[test]
    fn store_folders_name_a_game_even_in_a_window() {
        let lethal = r"E:\SteamLibrary\steamapps\common\Lethal Company\Lethal Company.exe";
        assert_eq!(seen(lethal, "Lethal Company", false), game("Lethal Company"));
        assert_eq!(seen(r"C:\XboxGames\Sea of Thieves\Content\game.exe", "", false), game("Sea of Thieves"));
        assert_eq!(seen(r"C:\Riot Games\VALORANT\live\VALORANT.exe", "", false), game("VALORANT"));
        assert_eq!(seen(r"D:\Ubisoft\Ubisoft Game Launcher\games\Rainbow Six Siege\R6.exe", "", false), game("Rainbow Six Siege"));
    }

    #[test]
    fn launchers_in_store_folders_are_not_games() {
        let epic = r"C:\Program Files (x86)\Epic Games\Launcher\Portal\Binaries\Win64\EpicGamesLauncher.exe";
        assert_eq!(seen(epic, "Epic Games Launcher", false), Seen::Other);
        assert_eq!(seen(r"C:\Riot Games\Riot Client\RiotClientServices.exe", "Riot Client", false), Seen::Other);
    }

    #[test]
    fn steam_manifests_give_the_store_name() {
        let root = std::env::temp_dir().join(format!("tape-steam-{}", std::process::id()));
        let apps = root.join("steamapps");
        let dir = apps.join("common").join("KTANE");
        fs::create_dir_all(&dir).unwrap();
        fs::write(apps.join("appmanifest_341800.acf"), "\"AppState\"\n{\n\t\"appid\"\t\t\"341800\"\n\t\"name\"\t\t\"Keep Talking and Nobody Explodes\"\n\t\"installdir\"\t\t\"KTANE\"\n}\n").unwrap();
        fs::write(apps.join("appmanifest_1.acf"), "\t\"name\"\t\t\"Wrong Game\"\n\t\"installdir\"\t\t\"Other\"\n").unwrap();
        let exe = dir.join("ktane.exe").display().to_string();
        assert_eq!(seen(&exe, "ktane", false), game("Keep Talking and Nobody Explodes"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn fullscreen_windows_count_as_games_unless_they_obviously_arent() {
        assert_eq!(seen(r"C:\Indie\game.exe", "Hades II - v1.0 (Vulkan)", true), game("Hades II"));
        assert_eq!(seen(r"C:\Indie\game.exe", "Cool Game™ v2.13.4 beta", true), game("Cool Game"));
        assert_eq!(seen(r"C:\Indie\mygame.exe", "", true), game("mygame"));
        assert_eq!(seen("", "Protected Game", true), game("Protected Game"));
        assert_eq!(seen(r"C:\Program Files\Google\Chrome\chrome.exe", "YouTube", true), Seen::Other);
        assert_eq!(seen(r"C:\Windows\explorer.exe", "", true), Seen::Other);
    }

    #[test]
    fn ordinary_windows_are_not_games() {
        assert_eq!(seen(r"C:\Indie\game.exe", "A window", false), Seen::Other);
        assert_eq!(seen("", "", false), Seen::Other);
    }

    fn window(exe: &str) -> Option<Foreground> {
        Some(Foreground { title: String::new(), exe: exe.into(), fullscreen: false })
    }

    #[test]
    fn a_game_stays_current_for_a_while_after_alt_tabbing() {
        let start = Instant::now();
        let later = |s| start + Duration::from_secs(s);
        let mut tracker = Tracker::default();
        assert_eq!(tracker.game(Duration::from_secs(60), start), DESKTOP);
        tracker.update(window(r"C:\x\cs2.exe"), start);
        tracker.update(window(r"C:\x\notepad.exe"), later(10));
        assert_eq!(tracker.game(Duration::from_secs(60), later(10)), "Counter-Strike 2");
        assert_eq!(tracker.game(Duration::from_secs(5), later(10)), DESKTOP);
    }

    #[test]
    fn time_in_the_overlay_counts_as_time_in_the_game() {
        let start = Instant::now();
        let later = |s| start + Duration::from_secs(s);
        let mut tracker = Tracker::default();
        tracker.update(window(r"C:\x\cs2.exe"), start);
        tracker.update(window(r"C:\x\tape.exe"), later(100));
        assert_eq!(tracker.game(Duration::from_secs(5), later(101)), "Counter-Strike 2");
    }

    #[test]
    fn tape_itself_is_set_apart() {
        assert_eq!(seen(r"C:\Program Files\tape\tape.exe", "tape", true), Seen::Tape);
    }
}
