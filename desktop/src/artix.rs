use std::env;

#[derive(Clone, Copy)]
enum ArtixGame {
    Aqw,
    EpicDuel,
    DragonFable,
    AdventureQuest,
    MechQuest,
    OverSoul,
}

impl ArtixGame {
    fn from_normalized(value: &str) -> Option<Self> {
        match value {
            "aqw" | "aqworlds" | "adventurequestworlds" | "adventurequestworldsv03" => {
                Some(Self::Aqw)
            }
            "epicduel" | "ed" => Some(Self::EpicDuel),
            "dragonfable" | "df" => Some(Self::DragonFable),
            "adventurequest" | "aq" => Some(Self::AdventureQuest),
            "mechquest" | "mq" => Some(Self::MechQuest),
            "oversoul" | "os" => Some(Self::OverSoul),
            _ => None,
        }
    }

    fn from_title(title: &str) -> Option<Self> {
        let normalized = normalize(title);
        if normalized.contains("adventurequestworlds") {
            Some(Self::Aqw)
        } else if normalized.contains("epicduel") {
            Some(Self::EpicDuel)
        } else if normalized.contains("dragonfable") {
            Some(Self::DragonFable)
        } else if normalized.contains("adventurequest") {
            Some(Self::AdventureQuest)
        } else if normalized.contains("mechquest") {
            Some(Self::MechQuest)
        } else if normalized.contains("oversoul") {
            Some(Self::OverSoul)
        } else {
            None
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Aqw => "Artix Entertainment - AdventureQuest Worlds V2.8",
            Self::EpicDuel => "Artix Entertainment - Epic Duel",
            Self::DragonFable => "Artix Entertainment - Dragon Fable",
            Self::AdventureQuest => "Artix Entertainment - AdventureQuest",
            Self::MechQuest => "Artix Entertainment - MechQuest",
            Self::OverSoul => "Artix Entertainment - OverSoul",
        }
    }
}

fn normalize(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn env_game() -> Option<ArtixGame> {
    env::var("ARTIX_RUFFLE_GAME_ICON")
        .ok()
        .or_else(|| env::var("ARTIX_RUFFLE_GAME").ok())
        .and_then(|value| ArtixGame::from_normalized(&normalize(&value)))
}

fn title_game() -> Option<ArtixGame> {
    env::var("ARTIX_RUFFLE_WINDOW_TITLE")
        .ok()
        .and_then(|title| ArtixGame::from_title(&title))
}

fn exe_game() -> Option<ArtixGame> {
    env::current_exe()
        .ok()
        .and_then(|path| {
            path.file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
        })
        .and_then(|stem| ArtixGame::from_normalized(&normalize(&stem)))
}

fn current_game() -> Option<ArtixGame> {
    env_game()
        .or_else(title_game)
        .or_else(|| {
            if env::var_os("ARTIX_AQW_WINDOW_ICON").is_some() {
                Some(ArtixGame::Aqw)
            } else {
                None
            }
        })
        .or_else(exe_game)
}

pub fn window_title() -> String {
    env::var("ARTIX_RUFFLE_WINDOW_TITLE")
        .ok()
        .or_else(|| current_game().map(|game| game.title().to_string()))
        .unwrap_or_else(|| ArtixGame::Aqw.title().to_string())
}

/// A pre-baked mouse cursor bitmap: `size` x `size` straight (non-premultiplied)
/// RGBA with a top-left origin, and the hotspot in that same pixel space.
pub struct CursorArt {
    pub rgba: &'static [u8],
    pub size: u16,
    pub hotspot_x: u16,
    pub hotspot_y: u16,
}

/// The two shapes the player's `MouseCursor` is drawn with.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CursorKind {
    Arrow,
    Hand,
}

pub struct CursorSet {
    /// Nominal size the set is picked by. The arrow's own bitmap is smaller: fit
    /// to the same height it reads as much bigger than the hand, being a solid
    /// triangle against a narrow gauntlet.
    pub size: u16,
    pub arrow: CursorArt,
    pub hand: CursorArt,
}

/// Baked from the source artwork by the cursor generator script, which also
/// prints these sizes and hotspots (the arrow's tip and the gauntlet's
/// fingertip). The OS draws cursors at their bitmap size, so the sizes here are
/// the whole scale range on offer rather than a DPI ladder.
const CURSOR_SETS: &[CursorSet] = &[
    CursorSet {
        size: 32,
        arrow: CursorArt {
            rgba: include_bytes!("../assets/aqw-cursor-arrow-32.rgba"),
            size: 26,
            hotspot_x: 3,
            hotspot_y: 0,
        },
        hand: CursorArt {
            rgba: include_bytes!("../assets/aqw-cursor-hand-32.rgba"),
            size: 32,
            hotspot_x: 13,
            hotspot_y: 0,
        },
    },
    CursorSet {
        size: 40,
        arrow: CursorArt {
            rgba: include_bytes!("../assets/aqw-cursor-arrow-40.rgba"),
            size: 32,
            hotspot_x: 3,
            hotspot_y: 0,
        },
        hand: CursorArt {
            rgba: include_bytes!("../assets/aqw-cursor-hand-40.rgba"),
            size: 40,
            hotspot_x: 16,
            hotspot_y: 0,
        },
    },
    CursorSet {
        size: 48,
        arrow: CursorArt {
            rgba: include_bytes!("../assets/aqw-cursor-arrow-48.rgba"),
            size: 38,
            hotspot_x: 5,
            hotspot_y: 0,
        },
        hand: CursorArt {
            rgba: include_bytes!("../assets/aqw-cursor-hand-48.rgba"),
            size: 48,
            hotspot_x: 20,
            hotspot_y: 0,
        },
    },
    CursorSet {
        size: 64,
        arrow: CursorArt {
            rgba: include_bytes!("../assets/aqw-cursor-arrow-64.rgba"),
            size: 51,
            hotspot_x: 7,
            hotspot_y: 0,
        },
        hand: CursorArt {
            rgba: include_bytes!("../assets/aqw-cursor-hand-64.rgba"),
            size: 64,
            hotspot_x: 27,
            hotspot_y: 0,
        },
    },
];

const DEFAULT_CURSOR_SIZE: u16 = 48;

/// The cursor artwork for the running game, or `None` to leave the system
/// cursors alone. `RUFFLE_AQW_NO_CUSTOM_CURSOR` (or a size of `0`) turns it off
/// without a rebuild; `RUFFLE_AQW_CURSOR_SIZE` picks the closest baked size.
pub fn custom_cursors() -> Option<&'static CursorSet> {
    // Unknown means the test build, which stands in for AQW like the title does.
    if !matches!(current_game().unwrap_or(ArtixGame::Aqw), ArtixGame::Aqw) {
        return None;
    }
    if ruffle_render::backend::aqw_env_flag("RUFFLE_AQW_NO_CUSTOM_CURSOR", false) {
        return None;
    }
    let wanted = env::var("RUFFLE_AQW_CURSOR_SIZE")
        .ok()
        .and_then(|value| value.trim().parse::<u16>().ok())
        .unwrap_or(DEFAULT_CURSOR_SIZE);
    if wanted == 0 {
        return None;
    }
    CURSOR_SETS
        .iter()
        .min_by_key(|set| set.size.abs_diff(wanted))
}

pub fn window_icon_rgba() -> &'static [u8] {
    match current_game() {
        Some(ArtixGame::Aqw) => include_bytes!("../assets/aqw-window-32.rgba").as_slice(),
        Some(ArtixGame::EpicDuel) => include_bytes!("../assets/epicduel-window-32.rgba").as_slice(),
        Some(ArtixGame::DragonFable) => {
            include_bytes!("../assets/dragonfable-window-32.rgba").as_slice()
        }
        Some(ArtixGame::AdventureQuest) => {
            include_bytes!("../assets/adventurequest-window-32.rgba").as_slice()
        }
        Some(ArtixGame::MechQuest) => {
            include_bytes!("../assets/mechquest-window-32.rgba").as_slice()
        }
        Some(ArtixGame::OverSoul) => include_bytes!("../assets/oversoul-window-32.rgba").as_slice(),
        None => include_bytes!("../assets/favicon-32.rgba").as_slice(),
    }
}
