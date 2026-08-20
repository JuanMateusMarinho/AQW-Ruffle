#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("AELauncher is only available on Windows.");
}

#[cfg(target_os = "windows")]
fn main() {
    windows_launcher::run();
}

#[cfg(target_os = "windows")]
mod windows_launcher {
    use std::ffi::c_void;
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::mem::{size_of, zeroed};
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::ptr::{null, null_mut};
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    use image::ImageFormat;
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows_sys::Win32::Graphics::Gdi::{
        BeginPaint, CreateFontW, DeleteObject, DrawTextW, EndPaint, GetDeviceCaps, InvalidateRect,
        SelectObject, SetBkMode, SetTextColor, StretchDIBits, UpdateWindow, BITMAPINFO,
        BITMAPINFOHEADER, BI_RGB, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET,
        DEFAULT_PITCH, DIB_RGB_COLORS, DT_CENTER, DT_LEFT, DT_SINGLELINE, DT_TOP, DT_VCENTER,
        DT_WORDBREAK, FW_BOLD, FW_NORMAL, LOGPIXELSY, OUT_DEFAULT_PRECIS, PAINTSTRUCT, RGBQUAD,
        SRCCOPY, TRANSPARENT,
    };
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        SetFocus, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_ESCAPE, VK_RETURN, VK_SPACE,
        VK_TAB,
    };
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect,
        GetMessageW, GetWindowLongPtrW, LoadCursorW, LoadIconW, MessageBoxW, PostQuitMessage,
        RegisterClassW, SendMessageW, SetWindowLongPtrW, ShowWindow, TranslateMessage,
        CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, GWLP_USERDATA, ICON_BIG, ICON_SMALL,
        IDC_ARROW, MB_ICONERROR, MB_OK, MINMAXINFO, MSG, SW_SHOW, SW_SHOWNORMAL, WM_DESTROY,
        WM_GETMINMAXINFO, WM_KEYDOWN, WM_LBUTTONDOWN, WM_MOUSEMOVE, WM_NCCREATE, WM_NCDESTROY,
        WM_PAINT, WM_SETICON, WM_SIZE, WNDCLASSW, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
    };

    const RUFFLE_EXE: &[u8] = include_bytes!("../../release/AQW.exe");
    const HERO_IMAGE: &[u8] = include_bytes!("../assets/launcher_entry.png");
    const DRAGON_FABLE_BANNER: &[u8] = include_bytes!("../assets/dragon_fable_banner.png");
    const AQW_BADGE: &[u8] = include_bytes!("../assets/aqw_badge.png");
    const AQW_PLAY_BUTTON: &[u8] = include_bytes!("../assets/aqw_play_button.png");
    const ARTIX_WORDMARK: &[u8] = include_bytes!("../assets/artix_entertainment.png");
    const DRAGON_FABLE_LOGO: &[u8] = include_bytes!("../assets/dragon_fable.png");
    const EPIC_DUEL_LOGO: &[u8] = include_bytes!("../assets/epic_duel.png");
    const ADVENTURE_QUEST_LOGO: &[u8] = include_bytes!("../assets/adventure_quest.png");
    const MECH_QUEST_LOGO: &[u8] = include_bytes!("../assets/mech_quest.png");
    const DRAGON_IMAGE: &[u8] = include_bytes!("../assets/dragon_window.png");
    const YOUTUBE_LOGO: &[u8] = include_bytes!("../assets/youtube.png");
    const TWITCH_LOGO: &[u8] = include_bytes!("../assets/twitch.png");

    const CREATE_NO_WINDOW: u32 = 0x08000000;
    const APPLICATION_ICON_ID: usize = 1;
    const MIN_WINDOW_WIDTH: i32 = 960;
    const MIN_WINDOW_HEIGHT: i32 = 620;
    const WM_MOUSELEAVE: u32 = 0x02A3;
    const AQW_SWF_URL: &str = "https://game.aq.com/game/gamefiles/Loader3.swf";
    const AQW_BASE_URL: &str = "https://game.aq.com/game/gamefiles/";
    const AQW_WINDOW_TITLE: &str = "Artix Entertainment - AdventureQuest Worlds V3.0";
    const AQW_DESIGN_NOTES_URL: &str = "https://www.aq.com/gamedesignnotes/";
    const AQW_YOUTUBE_RECENT_URL: &str =
        "https://www.youtube.com/channel/UC0vYUqgESNR3sqEPiJ4SpeA/recent";
    const AQW_YOUTUBE_LIVE_URL: &str =
        "https://www.youtube.com/channel/UC0vYUqgESNR3sqEPiJ4SpeA/live";
    const AQW_YOUTUBE_FEED_URL: &str =
        "https://www.youtube.com/feeds/videos.xml?channel_id=UC0vYUqgESNR3sqEPiJ4SpeA";
    const AQW_TWITCH_DIRECTORY_URL: &str =
        "https://www.twitch.tv/directory/category/adventurequest-worlds";
    const FLASH_GRAPHICS_BACKEND: &str = "vulkan";
    const FLASH_QUALITY: &str = "low";
    const FLASH_FRAME_RATE: &str = "24";
    const DRAGON_FABLE_SWF_URL: &str = "https://play.dragonfable.com/game/DFLoader.swf";
    const DRAGON_FABLE_BASE_URL: &str = "https://play.dragonfable.com/game/";
    const DRAGON_FABLE_WINDOW_TITLE: &str = "Artix Entertainment -Dragon Fable";

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Screen {
        Home,
        Games,
        News,
        Videos,
        Live,
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum ElementId {
        NavHome,
        NavGames,
        TopGames,
        TopNews,
        TopVideos,
        TopLive,
        PlayHome,
        PlayDragonFable,
        OpenDesignNotes,
        OpenMedia(usize),
        FutureEpicDuel,
        FutureAdventureQuest,
        FutureMechQuest,
    }

    #[derive(Clone, Copy)]
    struct RectI {
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    }

    impl RectI {
        fn right(self) -> i32 {
            self.x + self.w
        }

        fn bottom(self) -> i32 {
            self.y + self.h
        }

        fn inset(self, amount: i32) -> Self {
            Self {
                x: self.x + amount,
                y: self.y + amount,
                w: self.w - amount * 2,
                h: self.h - amount * 2,
            }
        }

        fn contains(self, x: i32, y: i32) -> bool {
            x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
        }
    }

    #[derive(Clone, Copy)]
    struct HitBox {
        id: ElementId,
        rect: RectI,
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum MediaSource {
        YouTube,
        Twitch,
    }

    struct MediaEntry {
        source: MediaSource,
        title: String,
        channel: String,
        url: String,
        thumbnail_url: Option<String>,
    }

    #[derive(Clone)]
    struct MediaItem {
        source: MediaSource,
        title: String,
        channel: String,
        url: String,
        thumbnail: Option<Arc<Bitmap>>,
    }

    struct Bitmap {
        width: i32,
        height: i32,
        rgba: Vec<u8>,
    }

    impl Bitmap {
        fn from_png(bytes: &[u8]) -> Self {
            let image = image::load_from_memory_with_format(bytes, ImageFormat::Png)
                .expect("failed to load launcher image")
                .to_rgba8();
            let (width, height) = image.dimensions();
            Self {
                width: width as i32,
                height: height as i32,
                rgba: image.into_raw(),
            }
        }

        fn from_memory(bytes: &[u8]) -> Option<Self> {
            let image = image::load_from_memory(bytes).ok()?.to_rgba8();
            let (width, height) = image.dimensions();
            Some(Self {
                width: width as i32,
                height: height as i32,
                rgba: image.into_raw(),
            })
        }
    }

    #[derive(Clone, Copy)]
    struct Layout {
        nav_width: i32,
        top_bar: RectI,
        hero_card: RectI,
        game_card: RectI,
        right_panel: RectI,
        play_home: RectI,
        play_aqw_games: RectI,
        design_notes_button: RectI,
        nav_home: RectI,
        nav_games: RectI,
        nav_help: RectI,
        top_games: RectI,
        top_news: RectI,
        top_videos: RectI,
        top_live: RectI,
        dragon: RectI,
        game_icon: RectI,
        dragon_fable_card: RectI,
        epic_duel_card: RectI,
        adventure_quest_card: RectI,
        mech_quest_card: RectI,
    }

    struct AppState {
        screen: Screen,
        hovered: Option<ElementId>,
        focused: ElementId,
        status: String,
        hero: Arc<Bitmap>,
        dragon_fable_banner: Arc<Bitmap>,
        badge: Arc<Bitmap>,
        aqw_play_button: Arc<Bitmap>,
        wordmark: Arc<Bitmap>,
        dragon_fable: Arc<Bitmap>,
        epic_duel: Arc<Bitmap>,
        adventure_quest: Arc<Bitmap>,
        mech_quest: Arc<Bitmap>,
        dragon: Arc<Bitmap>,
        youtube: Arc<Bitmap>,
        twitch: Arc<Bitmap>,
        youtube_videos: Vec<MediaItem>,
        live_media: Vec<MediaItem>,
        youtube_videos_loaded: bool,
        live_media_loaded: bool,
        buffer: Vec<u8>,
        width: i32,
        height: i32,
        hits: Vec<HitBox>,
        tracking_mouse: bool,
    }

    impl AppState {
        fn new() -> Self {
            Self {
                screen: Screen::Home,
                hovered: None,
                focused: ElementId::PlayHome,
                status: "Ready to play.".to_string(),
                hero: Arc::new(Bitmap::from_png(HERO_IMAGE)),
                dragon_fable_banner: Arc::new(Bitmap::from_png(DRAGON_FABLE_BANNER)),
                badge: Arc::new(Bitmap::from_png(AQW_BADGE)),
                aqw_play_button: Arc::new(Bitmap::from_png(AQW_PLAY_BUTTON)),
                wordmark: Arc::new(Bitmap::from_png(ARTIX_WORDMARK)),
                dragon_fable: Arc::new(Bitmap::from_png(DRAGON_FABLE_LOGO)),
                epic_duel: Arc::new(Bitmap::from_png(EPIC_DUEL_LOGO)),
                adventure_quest: Arc::new(Bitmap::from_png(ADVENTURE_QUEST_LOGO)),
                mech_quest: Arc::new(Bitmap::from_png(MECH_QUEST_LOGO)),
                dragon: Arc::new(Bitmap::from_png(DRAGON_IMAGE)),
                youtube: Arc::new(Bitmap::from_png(YOUTUBE_LOGO)),
                twitch: Arc::new(Bitmap::from_png(TWITCH_LOGO)),
                youtube_videos: Vec::new(),
                live_media: Vec::new(),
                youtube_videos_loaded: false,
                live_media_loaded: false,
                buffer: Vec::new(),
                width: 0,
                height: 0,
                hits: Vec::new(),
                tracking_mouse: false,
            }
        }

        fn layout(&self, width: i32, height: i32) -> Layout {
            let nav_width = if width < 1040 { 184 } else { 196 };
            let top_bar = RectI {
                x: 0,
                y: 0,
                w: width,
                h: 68,
            };
            let top_nav_y = 22;
            let top_nav_h = 36;
            let top_games = RectI {
                x: nav_width + 20,
                y: top_nav_y,
                w: 74,
                h: top_nav_h,
            };
            let top_news = RectI {
                x: top_games.right() + 4,
                y: top_nav_y,
                w: 70,
                h: top_nav_h,
            };
            let top_videos = RectI {
                x: top_news.right() + 4,
                y: top_nav_y,
                w: 86,
                h: top_nav_h,
            };
            let top_live = RectI {
                x: top_videos.right() + 4,
                y: top_nav_y,
                w: 62,
                h: top_nav_h,
            };
            let content_x = nav_width + 12;
            let content_w = width - nav_width - 24;
            let hero_h = (height * 45 / 100).clamp(320, 420);
            let right_panel = RectI {
                x: content_x,
                y: top_bar.bottom() + hero_h + 8,
                w: content_w,
                h: 34,
            };
            let hero_card = RectI {
                x: nav_width,
                y: top_bar.bottom(),
                w: width - nav_width,
                h: hero_h,
            };
            let game_card = RectI {
                x: content_x,
                y: right_panel.bottom() + 10,
                w: content_w,
                h: height - right_panel.bottom() - 24,
            };
            let play_home_w = if hero_card.w < 900 { 230 } else { 260 };
            let play_home_h = play_home_w * 190 / 320;
            let play_home = RectI {
                x: hero_card.x + 34,
                y: hero_card.bottom() - play_home_h - 18,
                w: play_home_w,
                h: play_home_h,
            };
            let play_aqw_games = RectI {
                x: hero_card.x + 22,
                y: hero_card.bottom() - 74,
                w: 190,
                h: 56,
            };
            let design_notes_button = RectI {
                x: game_card.right() - 184,
                y: game_card.y + 8,
                w: 176,
                h: 34,
            };
            let nav_home = RectI {
                x: 0,
                y: top_bar.bottom(),
                w: nav_width,
                h: 48,
            };
            let nav_help = RectI {
                x: 0,
                y: nav_home.bottom(),
                w: nav_width,
                h: 48,
            };
            let nav_games = RectI {
                x: 0,
                y: nav_help.bottom(),
                w: nav_width,
                h: 48,
            };
            let dragon = RectI {
                x: 16,
                y: height - 176,
                w: nav_width - 32,
                h: 98,
            };
            let game_icon = RectI {
                x: hero_card.x + 24,
                y: hero_card.y + 52,
                w: 112,
                h: 112,
            };
            let slot_gap = 14;
            let columns = if content_w > 1180 { 4 } else { 2 };
            let rows = if columns == 4 { 1 } else { 2 };
            let slot_x = game_card.x;
            let slot_top = game_card.y + 48;
            let slot_w = ((game_card.w - slot_gap * (columns - 1)) / columns).max(180);
            let available_h = (game_card.bottom() - slot_top - slot_gap * (rows - 1)).max(90);
            let slot_h = if rows == 1 {
                (available_h / rows).clamp(170, 240)
            } else {
                (available_h / rows).clamp(90, 138)
            };
            let dragon_fable_card = RectI {
                x: slot_x,
                y: slot_top,
                w: slot_w,
                h: slot_h,
            };
            let epic_duel_card = RectI {
                x: slot_x + slot_w + slot_gap,
                y: slot_top,
                w: slot_w,
                h: slot_h,
            };
            let adventure_quest_card = RectI {
                x: slot_x + (slot_w + slot_gap) * if columns == 4 { 2 } else { 0 },
                y: slot_top + if columns == 4 { 0 } else { slot_h + slot_gap },
                w: slot_w,
                h: slot_h,
            };
            let mech_quest_card = RectI {
                x: slot_x + (slot_w + slot_gap) * if columns == 4 { 3 } else { 1 },
                y: slot_top + if columns == 4 { 0 } else { slot_h + slot_gap },
                w: slot_w,
                h: slot_h,
            };

            Layout {
                nav_width,
                top_bar,
                hero_card,
                game_card,
                right_panel,
                play_home,
                play_aqw_games,
                design_notes_button,
                nav_home,
                nav_games,
                nav_help,
                top_games,
                top_news,
                top_videos,
                top_live,
                dragon,
                game_icon,
                dragon_fable_card,
                epic_duel_card,
                adventure_quest_card,
                mech_quest_card,
            }
        }

        fn update_hits(&mut self, width: i32, height: i32) {
            let layout = self.layout(width, height);
            let mut hits = vec![
                HitBox {
                    id: ElementId::TopGames,
                    rect: layout.top_games,
                },
                HitBox {
                    id: ElementId::TopNews,
                    rect: layout.top_news,
                },
                HitBox {
                    id: ElementId::TopVideos,
                    rect: layout.top_videos,
                },
                HitBox {
                    id: ElementId::TopLive,
                    rect: layout.top_live,
                },
                HitBox {
                    id: ElementId::NavHome,
                    rect: layout.nav_home,
                },
                HitBox {
                    id: ElementId::NavGames,
                    rect: layout.nav_games,
                },
            ];

            match self.screen {
                Screen::Home => {
                    hits.push(HitBox {
                        id: ElementId::PlayHome,
                        rect: layout.play_home,
                    });
                    hits.push(HitBox {
                        id: ElementId::OpenDesignNotes,
                        rect: layout.design_notes_button,
                    });
                }
                Screen::Games => {
                    hits.push(HitBox {
                        id: ElementId::PlayDragonFable,
                        rect: layout.play_aqw_games,
                    });
                    hits.push(HitBox {
                        id: ElementId::PlayDragonFable,
                        rect: layout.dragon_fable_card,
                    });
                    hits.push(HitBox {
                        id: ElementId::FutureEpicDuel,
                        rect: layout.epic_duel_card,
                    });
                    hits.push(HitBox {
                        id: ElementId::FutureAdventureQuest,
                        rect: layout.adventure_quest_card,
                    });
                    hits.push(HitBox {
                        id: ElementId::FutureMechQuest,
                        rect: layout.mech_quest_card,
                    });
                }
                Screen::News => {
                    hits.push(HitBox {
                        id: ElementId::OpenDesignNotes,
                        rect: layout.design_notes_button,
                    });
                }
                Screen::Videos => {
                    for (index, rect) in self
                        .media_card_rects(layout, self.media_items_for_screen().len().max(1))
                        .into_iter()
                        .enumerate()
                    {
                        hits.push(HitBox {
                            id: ElementId::OpenMedia(index),
                            rect,
                        });
                    }
                }
                Screen::Live => {
                    for (index, rect) in self
                        .media_card_rects(layout, self.media_items_for_screen().len().max(1))
                        .into_iter()
                        .enumerate()
                    {
                        hits.push(HitBox {
                            id: ElementId::OpenMedia(index),
                            rect,
                        });
                    }
                }
            }

            self.hits = hits;
        }

        fn render(&mut self, width: i32, height: i32) {
            self.width = width.max(1);
            self.height = height.max(1);
            self.buffer
                .resize((self.width * self.height * 4) as usize, 0);

            let layout = self.layout(self.width, self.height);
            self.update_hits(self.width, self.height);

            self.fill(Color::rgb(14, 15, 23));
            let (hero, hero_focus_y) = match self.screen {
                Screen::Games => (Arc::clone(&self.dragon_fable_banner), 0.16),
                Screen::Home | Screen::News | Screen::Videos | Screen::Live => {
                    (Arc::clone(&self.hero), 0.42)
                }
            };
            self.draw_image_cover_focus(&hero, layout.hero_card, 255, hero_focus_y);
            self.fill_rect_alpha(
                RectI {
                    x: layout.hero_card.x,
                    y: layout.hero_card.y,
                    w: layout.hero_card.w,
                    h: layout.hero_card.h,
                },
                Color::rgba(42, 4, 50, 96),
            );
            self.vertical_gradient(
                layout.hero_card,
                Color::rgba(0, 0, 0, 36),
                Color::rgba(0, 0, 0, 214),
            );
            self.fill_rect_alpha(
                RectI {
                    x: 0,
                    y: 0,
                    w: layout.nav_width,
                    h: self.height,
                },
                Color::rgba(3, 4, 8, 244),
            );
            self.fill_rect_alpha(layout.top_bar, Color::rgba(6, 7, 11, 238));
            self.fill_rect_alpha(
                RectI {
                    x: 0,
                    y: layout.top_bar.bottom() - 1,
                    w: self.width,
                    h: 1,
                },
                Color::rgba(219, 120, 16, 210),
            );
            self.fill_rect_alpha(
                RectI {
                    x: layout.nav_width,
                    y: layout.hero_card.bottom() - 1,
                    w: self.width - layout.nav_width,
                    h: 1,
                },
                Color::rgba(51, 55, 73, 210),
            );
            let dragon = Arc::clone(&self.dragon);
            self.draw_image_contain(&dragon, layout.dragon, 70);
            let wordmark = Arc::clone(&self.wordmark);
            self.draw_image_contain(
                &wordmark,
                RectI {
                    x: 10,
                    y: 6,
                    w: layout.nav_width - 20,
                    h: 58,
                },
                255,
            );

            self.draw_top_nav_button(
                layout.top_games,
                ElementId::TopGames,
                matches!(self.screen, Screen::Home | Screen::Games),
            );
            self.draw_top_nav_button(
                layout.top_news,
                ElementId::TopNews,
                self.screen == Screen::News,
            );
            self.draw_top_nav_button(
                layout.top_videos,
                ElementId::TopVideos,
                self.screen == Screen::Videos,
            );
            self.draw_top_nav_button(
                layout.top_live,
                ElementId::TopLive,
                self.screen == Screen::Live,
            );

            self.draw_nav_button(
                layout.nav_home,
                ElementId::NavHome,
                self.screen == Screen::Home,
            );
            self.draw_nav_button(
                layout.nav_games,
                ElementId::NavGames,
                self.screen == Screen::Games,
            );
            let badge = Arc::clone(&self.badge);
            let dragon_fable = Arc::clone(&self.dragon_fable);
            let adventure_quest = Arc::clone(&self.adventure_quest);
            let epic_duel = Arc::clone(&self.epic_duel);
            let mech_quest = Arc::clone(&self.mech_quest);
            self.draw_sidebar_icon(layout.nav_home, &badge, 255);
            self.draw_sidebar_icon(layout.nav_games, &dragon_fable, 255);
            self.draw_sidebar_static_item(
                RectI {
                    x: 0,
                    y: layout.nav_help.y,
                    w: layout.nav_width,
                    h: 48,
                },
                &epic_duel,
            );
            self.draw_sidebar_static_item(
                RectI {
                    x: 0,
                    y: layout.nav_games.bottom(),
                    w: layout.nav_width,
                    h: 48,
                },
                &adventure_quest,
            );
            self.draw_sidebar_static_item(
                RectI {
                    x: 0,
                    y: layout.nav_games.bottom() + 48,
                    w: layout.nav_width,
                    h: 48,
                },
                &mech_quest,
            );
            self.draw_status_strip(layout.right_panel);

            match self.screen {
                Screen::Home => self.draw_home(layout),
                Screen::Games => self.draw_games(layout),
                Screen::News => self.draw_news(layout),
                Screen::Videos => self.draw_videos(layout),
                Screen::Live => self.draw_live(layout),
            }
        }

        fn draw_home(&mut self, layout: Layout) {
            let play_button = Arc::clone(&self.aqw_play_button);
            self.draw_aqw_play_button(layout.play_home, &play_button);
            self.draw_design_notes_page(layout);
        }

        fn draw_games(&mut self, layout: Layout) {
            let dragon_fable = Arc::clone(&self.dragon_fable);
            self.draw_image_contain(&dragon_fable, layout.game_icon, 255);
            self.draw_button(
                layout.play_aqw_games,
                ElementId::PlayDragonFable,
                true,
                Color::rgba(128, 18, 14, 240),
                Color::rgba(172, 35, 22, 250),
            );
            let epic_duel = Arc::clone(&self.epic_duel);
            let adventure_quest = Arc::clone(&self.adventure_quest);
            let mech_quest = Arc::clone(&self.mech_quest);
            self.draw_game_slot(
                layout.dragon_fable_card,
                ElementId::PlayDragonFable,
                &dragon_fable,
                true,
            );
            self.draw_game_slot(
                layout.epic_duel_card,
                ElementId::FutureEpicDuel,
                &epic_duel,
                false,
            );
            self.draw_game_slot(
                layout.adventure_quest_card,
                ElementId::FutureAdventureQuest,
                &adventure_quest,
                false,
            );
            self.draw_game_slot(
                layout.mech_quest_card,
                ElementId::FutureMechQuest,
                &mech_quest,
                false,
            );
        }

        fn draw_news(&mut self, layout: Layout) {
            self.draw_design_notes_page(layout);
        }

        fn draw_videos(&mut self, layout: Layout) {
            self.draw_media_panel(layout);
        }

        fn draw_live(&mut self, layout: Layout) {
            self.draw_media_panel(layout);
        }

        fn draw_media_panel(&mut self, layout: Layout) {
            self.draw_panel(
                layout.game_card,
                Color::rgba(13, 15, 22, 226),
                Color::rgba(55, 58, 72, 210),
            );
            let items = self.media_items_for_screen().to_vec();
            let count = items.len().max(1);
            for (index, rect) in self.media_card_rects(layout, count).into_iter().enumerate() {
                let item = items.get(index);
                self.draw_media_card(rect, ElementId::OpenMedia(index), item);
            }
        }

        fn draw_design_notes_page(&mut self, layout: Layout) {
            self.draw_panel(
                layout.game_card,
                Color::rgba(12, 13, 18, 238),
                Color::rgba(52, 56, 70, 220),
            );
            self.fill_rect_alpha(
                RectI {
                    x: layout.game_card.x + 1,
                    y: layout.game_card.y + 1,
                    w: layout.game_card.w - 2,
                    h: 44,
                },
                Color::rgba(7, 8, 12, 245),
            );
            self.fill_rect_alpha(
                RectI {
                    x: layout.game_card.x + 1,
                    y: layout.game_card.y + 45,
                    w: layout.game_card.w - 2,
                    h: 1,
                },
                Color::rgba(225, 134, 22, 190),
            );
            self.draw_button(
                layout.design_notes_button,
                ElementId::OpenDesignNotes,
                false,
                Color::rgba(34, 44, 63, 228),
                Color::rgba(54, 70, 98, 244),
            );
            let content = layout.game_card.inset(18);
            let article_top = content.y + 74;
            let article_gap = 14;
            let article_h = ((layout.game_card.bottom() - article_top - article_gap * 2 - 18) / 3)
                .clamp(52, 86);
            for index in 0..3 {
                let y = article_top + index * (article_h + article_gap);
                self.draw_panel(
                    RectI {
                        x: content.x,
                        y,
                        w: content.w,
                        h: article_h,
                    },
                    Color::rgba(20, 22, 29, 232),
                    Color::rgba(74, 82, 104, 170),
                );
            }
        }

        fn draw_status_strip(&mut self, rect: RectI) {
            self.fill_rect_alpha(rect, Color::rgba(9, 10, 15, 224));
            self.fill_rect_alpha(
                RectI {
                    x: rect.x,
                    y: rect.y,
                    w: rect.w,
                    h: 1,
                },
                Color::rgba(225, 134, 22, 180),
            );
        }

        fn draw_panel(&mut self, rect: RectI, fill: Color, border: Color) {
            self.fill_round_rect(rect, 6, border);
            self.fill_round_rect(rect.inset(1), 5, fill);
            self.fill_rect_alpha(
                RectI {
                    x: rect.x + 2,
                    y: rect.y + 2,
                    w: rect.w - 4,
                    h: 1,
                },
                Color::rgba(255, 255, 255, 48),
            );
        }

        fn draw_nav_button(&mut self, rect: RectI, id: ElementId, selected: bool) {
            let hovered = self.hovered == Some(id);
            let focused = self.focused == id;
            let fill = if selected {
                Color::rgba(28, 10, 36, 224)
            } else if hovered || focused {
                Color::rgba(20, 18, 30, 228)
            } else {
                Color::rgba(3, 4, 8, 0)
            };
            self.fill_rect_alpha(rect, fill);
            self.fill_rect_alpha(
                RectI {
                    x: rect.x,
                    y: rect.bottom() - 1,
                    w: rect.w,
                    h: 1,
                },
                Color::rgba(255, 255, 255, 18),
            );
            if selected {
                self.fill_rect_alpha(
                    RectI {
                        x: rect.x,
                        y: rect.y,
                        w: 4,
                        h: rect.h,
                    },
                    Color::rgba(231, 146, 18, 255),
                );
            }
            if focused {
                self.fill_rect_alpha(
                    RectI {
                        x: rect.x + 4,
                        y: rect.y,
                        w: rect.w - 4,
                        h: 1,
                    },
                    Color::rgba(255, 225, 118, 210),
                );
            }
        }

        fn draw_top_nav_button(&mut self, rect: RectI, id: ElementId, selected: bool) {
            let active = selected || self.hovered == Some(id) || self.focused == id;
            if active {
                self.fill_round_rect(
                    RectI {
                        x: rect.x,
                        y: rect.y + 4,
                        w: rect.w,
                        h: rect.h - 8,
                    },
                    5,
                    if selected {
                        Color::rgba(42, 19, 15, 210)
                    } else {
                        Color::rgba(24, 23, 30, 190)
                    },
                );
            }

            if selected {
                self.fill_rect_alpha(
                    RectI {
                        x: rect.x + 10,
                        y: rect.bottom() - 7,
                        w: rect.w - 20,
                        h: 3,
                    },
                    Color::rgba(255, 178, 20, 240),
                );
            }

            if self.focused == id {
                self.fill_rect_alpha(
                    RectI {
                        x: rect.x + 8,
                        y: rect.y + 5,
                        w: rect.w - 16,
                        h: 1,
                    },
                    Color::rgba(255, 230, 132, 180),
                );
            }
        }

        fn draw_sidebar_icon(&mut self, rect: RectI, image: &Bitmap, opacity: u8) {
            self.draw_image_contain(
                image,
                RectI {
                    x: rect.x + 16,
                    y: rect.y + 9,
                    w: 30,
                    h: 30,
                },
                opacity,
            );
        }

        fn draw_sidebar_static_item(&mut self, rect: RectI, image: &Bitmap) {
            self.fill_rect_alpha(rect, Color::rgba(3, 4, 8, 0));
            self.fill_rect_alpha(
                RectI {
                    x: rect.x,
                    y: rect.bottom() - 1,
                    w: rect.w,
                    h: 1,
                },
                Color::rgba(255, 255, 255, 14),
            );
            self.draw_sidebar_icon(rect, image, 185);
        }

        fn draw_button(
            &mut self,
            rect: RectI,
            id: ElementId,
            primary: bool,
            fill: Color,
            hover_fill: Color,
        ) {
            let active = self.hovered == Some(id) || self.focused == id;
            let base = if active { hover_fill } else { fill };
            let border = if self.focused == id {
                Color::rgba(255, 244, 168, 255)
            } else if primary {
                Color::rgba(255, 230, 136, 230)
            } else {
                Color::rgba(142, 168, 210, 190)
            };
            self.fill_round_rect(rect, 5, border);
            self.fill_round_rect(rect.inset(2), 4, base);
            self.fill_rect_alpha(
                RectI {
                    x: rect.x + 10,
                    y: rect.y + 7,
                    w: rect.w - 20,
                    h: 1,
                },
                Color::rgba(255, 255, 255, 90),
            );
        }

        fn draw_aqw_play_button(&mut self, rect: RectI, image: &Bitmap) {
            let hovered = self.hovered == Some(ElementId::PlayHome);
            let button_rect = if hovered {
                RectI {
                    x: rect.x,
                    y: rect.y - 2,
                    w: rect.w,
                    h: rect.h,
                }
            } else {
                rect
            };

            if hovered {
                for (pad, alpha) in [(22, 18), (14, 28), (7, 38)] {
                    self.fill_round_rect(
                        RectI {
                            x: button_rect.x - pad,
                            y: button_rect.y - pad / 2,
                            w: button_rect.w + pad * 2,
                            h: button_rect.h + pad,
                        },
                        16,
                        Color::rgba(255, 190, 40, alpha),
                    );
                }
            }

            let source = RectI {
                x: 0,
                y: 0,
                w: image.width / 2,
                h: image.height,
            };
            self.draw_image_region_contain(
                image,
                source,
                button_rect,
                if hovered { 255 } else { 242 },
            );
        }

        fn draw_game_slot(&mut self, rect: RectI, id: ElementId, image: &Bitmap, playable: bool) {
            let active = self.hovered == Some(id) || self.focused == id;
            let border = if playable {
                if active {
                    Color::rgba(255, 232, 128, 250)
                } else {
                    Color::rgba(219, 156, 38, 200)
                }
            } else if active {
                Color::rgba(126, 162, 216, 230)
            } else {
                Color::rgba(84, 102, 132, 150)
            };
            let fill = if playable {
                Color::rgba(29, 24, 14, 222)
            } else {
                Color::rgba(18, 19, 24, 232)
            };
            self.draw_panel(rect, fill, border);
            if rect.h > 130 {
                self.fill_rect_alpha(
                    RectI {
                        x: rect.x + 1,
                        y: rect.y + 1,
                        w: rect.w - 2,
                        h: 96,
                    },
                    Color::rgba(0, 0, 0, 110),
                );
                self.draw_image_contain(
                    image,
                    RectI {
                        x: rect.x + 14,
                        y: rect.y + 12,
                        w: rect.w - 28,
                        h: 82,
                    },
                    if playable { 255 } else { 210 },
                );
            } else {
                self.draw_image_contain(
                    image,
                    RectI {
                        x: rect.x + 14,
                        y: rect.y + 10,
                        w: 84,
                        h: rect.h - 20,
                    },
                    if playable { 255 } else { 205 },
                );
            }
        }

        fn media_items_for_screen(&self) -> &[MediaItem] {
            match self.screen {
                Screen::Videos => &self.youtube_videos,
                Screen::Live => &self.live_media,
                _ => &[],
            }
        }

        fn media_card_rects(&self, layout: Layout, count: usize) -> Vec<RectI> {
            let gap = 12;
            let top = layout.game_card.y + 54;
            let columns = if layout.game_card.w > 1180 {
                4
            } else if layout.game_card.w > 820 {
                3
            } else {
                2
            };
            let card_w = ((layout.game_card.w - gap * (columns - 1)) / columns).max(220);
            let card_h = (card_w * 9 / 16 + 92).clamp(198, 280);
            let max_rows = ((layout.game_card.bottom() - top + gap) / (card_h + gap)).max(1);
            let max_cards = (columns * max_rows) as usize;
            let total = count.min(max_cards).max(1);
            let mut rects = Vec::with_capacity(total);
            for index in 0..total {
                let column = index as i32 % columns;
                let row = index as i32 / columns;
                rects.push(RectI {
                    x: layout.game_card.x + column * (card_w + gap),
                    y: top + row * (card_h + gap),
                    w: card_w,
                    h: card_h,
                });
            }
            rects
        }

        fn draw_media_card(&mut self, rect: RectI, id: ElementId, item: Option<&MediaItem>) {
            let active = self.hovered == Some(id) || self.focused == id;
            let source = item.map(|item| item.source).unwrap_or(match self.screen {
                Screen::Live => MediaSource::YouTube,
                _ => MediaSource::YouTube,
            });
            let border = if active {
                Color::rgba(255, 238, 160, 255)
            } else {
                match source {
                    MediaSource::YouTube => Color::rgba(229, 45, 39, 210),
                    MediaSource::Twitch => Color::rgba(145, 71, 255, 210),
                }
            };
            self.draw_panel(rect, Color::rgba(17, 18, 24, 238), border);

            let thumb = RectI {
                x: rect.x + 8,
                y: rect.y + 8,
                w: rect.w - 16,
                h: ((rect.w - 16) * 9 / 16).min(rect.h - 96),
            };
            self.fill_rect_alpha(thumb, Color::rgba(0, 0, 0, 170));

            if let Some(item) = item {
                if let Some(thumbnail) = &item.thumbnail {
                    self.draw_image_cover_focus(thumbnail, thumb, 255, 0.5);
                } else {
                    let logo = match item.source {
                        MediaSource::YouTube => Arc::clone(&self.youtube),
                        MediaSource::Twitch => Arc::clone(&self.twitch),
                    };
                    self.draw_image_contain(&logo, thumb.inset(18), 235);
                }
            } else {
                let logo = Arc::clone(&self.youtube);
                self.draw_image_contain(&logo, thumb.inset(18), 210);
            }

            self.fill_rect_alpha(
                RectI {
                    x: thumb.x,
                    y: thumb.bottom() - 24,
                    w: thumb.w,
                    h: 24,
                },
                Color::rgba(0, 0, 0, 150),
            );
        }

        fn hit_test(&self, x: i32, y: i32) -> Option<ElementId> {
            self.hits
                .iter()
                .find(|hit| hit.rect.contains(x, y))
                .map(|hit| hit.id)
        }

        fn set_screen(&mut self, screen: Screen) {
            self.screen = screen;
            self.refresh_media_for_screen(screen);
            self.focused = match screen {
                Screen::Home => ElementId::PlayHome,
                Screen::Games => ElementId::PlayDragonFable,
                Screen::News => ElementId::OpenDesignNotes,
                Screen::Videos => ElementId::OpenMedia(0),
                Screen::Live => ElementId::OpenMedia(0),
            };
        }

        fn focus_order(&self) -> &'static [ElementId] {
            match self.screen {
                Screen::Home => &[
                    ElementId::PlayHome,
                    ElementId::TopGames,
                    ElementId::TopNews,
                    ElementId::TopVideos,
                    ElementId::TopLive,
                    ElementId::OpenDesignNotes,
                    ElementId::NavHome,
                    ElementId::NavGames,
                ],
                Screen::Games => &[
                    ElementId::PlayDragonFable,
                    ElementId::FutureEpicDuel,
                    ElementId::FutureAdventureQuest,
                    ElementId::FutureMechQuest,
                    ElementId::TopGames,
                    ElementId::TopNews,
                    ElementId::TopVideos,
                    ElementId::TopLive,
                    ElementId::NavHome,
                    ElementId::NavGames,
                ],
                Screen::News => &[
                    ElementId::OpenDesignNotes,
                    ElementId::TopGames,
                    ElementId::TopNews,
                    ElementId::TopVideos,
                    ElementId::TopLive,
                    ElementId::NavHome,
                    ElementId::NavGames,
                ],
                Screen::Videos => &[
                    ElementId::OpenMedia(0),
                    ElementId::OpenMedia(1),
                    ElementId::OpenMedia(2),
                    ElementId::OpenMedia(3),
                    ElementId::OpenMedia(4),
                    ElementId::OpenMedia(5),
                    ElementId::TopGames,
                    ElementId::TopNews,
                    ElementId::TopVideos,
                    ElementId::TopLive,
                    ElementId::NavHome,
                    ElementId::NavGames,
                ],
                Screen::Live => &[
                    ElementId::OpenMedia(0),
                    ElementId::OpenMedia(1),
                    ElementId::OpenMedia(2),
                    ElementId::OpenMedia(3),
                    ElementId::OpenMedia(4),
                    ElementId::OpenMedia(5),
                    ElementId::TopGames,
                    ElementId::TopNews,
                    ElementId::TopVideos,
                    ElementId::TopLive,
                    ElementId::NavHome,
                    ElementId::NavGames,
                ],
            }
        }

        fn focus_next(&mut self) {
            let order = self.focus_order();
            let current = order.iter().position(|id| *id == self.focused).unwrap_or(0);
            self.focused = order[(current + 1) % order.len()];
        }

        fn refresh_media_for_screen(&mut self, screen: Screen) {
            match screen {
                Screen::Videos if !self.youtube_videos_loaded => {
                    self.status = "Loading YouTube videos...".to_string();
                    let entries = fetch_youtube_recent_entries(8).unwrap_or_else(|_| {
                        vec![MediaEntry {
                            source: MediaSource::YouTube,
                            title: "Latest AdventureQuest Worlds videos".to_string(),
                            channel: "YouTube".to_string(),
                            url: AQW_YOUTUBE_RECENT_URL.to_string(),
                            thumbnail_url: None,
                        }]
                    });
                    self.youtube_videos = build_media_items(entries);
                    self.youtube_videos_loaded = true;
                    self.status = "YouTube videos loaded.".to_string();
                }
                Screen::Live if !self.live_media_loaded => {
                    self.status = "Loading live channels...".to_string();
                    let mut entries = fetch_youtube_live_entries().unwrap_or_default();
                    entries.extend(fetch_twitch_live_entries(5).unwrap_or_default());
                    if entries.is_empty() {
                        entries = vec![
                            MediaEntry {
                                source: MediaSource::YouTube,
                                title: "YouTube live channel".to_string(),
                                channel: "AdventureQuest Worlds".to_string(),
                                url: AQW_YOUTUBE_LIVE_URL.to_string(),
                                thumbnail_url: None,
                            },
                            MediaEntry {
                                source: MediaSource::Twitch,
                                title: "AdventureQuest Worlds streams".to_string(),
                                channel: "Twitch directory".to_string(),
                                url: AQW_TWITCH_DIRECTORY_URL.to_string(),
                                thumbnail_url: None,
                            },
                        ];
                    }
                    self.live_media = build_media_items(entries);
                    self.live_media_loaded = true;
                    self.status = "Live channels loaded.".to_string();
                }
                _ => {}
            }
        }

        fn media_url(&self, index: usize) -> Option<&str> {
            self.media_items_for_screen()
                .get(index)
                .map(|item| item.url.as_str())
        }

        fn activate(&mut self, id: ElementId, hwnd: HWND) {
            match id {
                ElementId::NavHome => self.set_screen(Screen::Home),
                ElementId::NavGames => self.set_screen(Screen::Games),
                ElementId::TopGames => self.set_screen(Screen::Home),
                ElementId::TopNews => self.set_screen(Screen::News),
                ElementId::TopVideos => self.set_screen(Screen::Videos),
                ElementId::TopLive => self.set_screen(Screen::Live),
                ElementId::PlayHome => match launch_aqw() {
                    Ok(_) => {
                        self.status = "AdventureQuest Worlds started through Ruffle.".to_string();
                    }
                    Err(error) => {
                        self.status = "Failed to start the game.".to_string();
                        show_error(hwnd, &format!("Could not start AQW.\n\n{error}"));
                    }
                },
                ElementId::PlayDragonFable => match launch_dragon_fable() {
                    Ok(_) => {
                        self.status = "DragonFable started through Ruffle.".to_string();
                    }
                    Err(error) => {
                        self.status = "Failed to start DragonFable.".to_string();
                        show_error(hwnd, &format!("Could not start DragonFable.\n\n{error}"));
                    }
                },
                ElementId::OpenDesignNotes => match open_design_notes(hwnd) {
                    Ok(_) => {
                        self.status = "Design Notes opened in your browser.".to_string();
                    }
                    Err(error) => {
                        self.status = "Failed to open Design Notes.".to_string();
                        show_error(hwnd, &format!("Could not open Design Notes.\n\n{error}"));
                    }
                },
                ElementId::OpenMedia(index) => {
                    if let Some(url) = self.media_url(index).map(|url| url.to_string()) {
                        match open_url(hwnd, &url) {
                            Ok(_) => {
                                self.status = "Media opened in your browser.".to_string();
                            }
                            Err(error) => {
                                self.status = "Failed to open media.".to_string();
                                show_error(hwnd, &format!("Could not open media.\n\n{error}"));
                            }
                        }
                    }
                }
                ElementId::FutureEpicDuel => {
                    self.status =
                        "EpicDuel support is reserved for a future launcher update.".to_string();
                }
                ElementId::FutureAdventureQuest => {
                    self.status =
                        "AdventureQuest support is reserved for a future launcher update."
                            .to_string();
                }
                ElementId::FutureMechQuest => {
                    self.status =
                        "MechQuest support is reserved for a future launcher update.".to_string();
                }
            }
        }

        fn draw_texts(&self, hdc: windows_sys::Win32::Graphics::Gdi::HDC) {
            unsafe {
                SetBkMode(hdc, TRANSPARENT as i32);
            }

            let layout = self.layout(self.width, self.height);
            self.draw_top_nav_text(
                hdc,
                layout.top_games,
                ElementId::TopGames,
                "GAMES",
                matches!(self.screen, Screen::Home | Screen::Games),
            );
            self.draw_top_nav_text(
                hdc,
                layout.top_news,
                ElementId::TopNews,
                "NEWS",
                self.screen == Screen::News,
            );
            self.draw_top_nav_text(
                hdc,
                layout.top_videos,
                ElementId::TopVideos,
                "VIDEOS",
                self.screen == Screen::Videos,
            );
            self.draw_top_nav_text(
                hdc,
                layout.top_live,
                ElementId::TopLive,
                "LIVE",
                self.screen == Screen::Live,
            );
            self.draw_nav_text(hdc, layout.nav_home, "AQWorlds");
            self.draw_nav_text(hdc, layout.nav_games, "DragonFable");
            self.draw_static_sidebar_text(hdc, layout.nav_help.y, "EpicDuel");
            self.draw_static_sidebar_text(hdc, layout.nav_games.bottom(), "AdventureQuest");
            self.draw_static_sidebar_text(hdc, layout.nav_games.bottom() + 48, "MechQuest");

            self.draw_status_text(hdc, layout.right_panel);

            match self.screen {
                Screen::Home => self.draw_home_text(hdc, layout),
                Screen::Games => self.draw_games_text(hdc, layout),
                Screen::News => self.draw_news_text(hdc, layout),
                Screen::Videos => self.draw_videos_text(hdc, layout),
                Screen::Live => self.draw_live_text(hdc, layout),
            }
        }

        fn draw_top_nav_text(
            &self,
            hdc: windows_sys::Win32::Graphics::Gdi::HDC,
            rect: RectI,
            id: ElementId,
            text: &str,
            selected: bool,
        ) {
            let active = selected || self.hovered == Some(id) || self.focused == id;
            draw_text(
                hdc,
                text,
                RectI {
                    x: rect.x,
                    y: rect.y + 1,
                    w: rect.w,
                    h: rect.h - 5,
                },
                13,
                FW_BOLD as i32,
                if active {
                    Color::rgb(255, 247, 218)
                } else {
                    Color::rgb(216, 222, 234)
                },
                DT_CENTER | DT_VCENTER | DT_SINGLELINE,
            );
        }

        fn draw_nav_text(
            &self,
            hdc: windows_sys::Win32::Graphics::Gdi::HDC,
            rect: RectI,
            text: &str,
        ) {
            draw_text(
                hdc,
                text,
                RectI {
                    x: rect.x + 56,
                    y: rect.y,
                    w: rect.w - 62,
                    h: rect.h,
                },
                12,
                FW_BOLD as i32,
                Color::rgb(235, 240, 248),
                DT_LEFT | DT_VCENTER | DT_SINGLELINE,
            );
        }

        fn draw_static_sidebar_text(
            &self,
            hdc: windows_sys::Win32::Graphics::Gdi::HDC,
            y: i32,
            text: &str,
        ) {
            let layout = self.layout(self.width, self.height);
            draw_text(
                hdc,
                text,
                RectI {
                    x: 56,
                    y,
                    w: layout.nav_width - 62,
                    h: 48,
                },
                12,
                FW_BOLD as i32,
                Color::rgb(214, 219, 230),
                DT_LEFT | DT_VCENTER | DT_SINGLELINE,
            );
        }

        fn draw_home_text(&self, hdc: windows_sys::Win32::Graphics::Gdi::HDC, layout: Layout) {
            draw_text(
                hdc,
                "Flash MMORPG running through the bundled Ruffle build.",
                RectI {
                    x: layout.play_home.right() + 20,
                    y: layout.play_home.y + layout.play_home.h / 2 - 12,
                    w: layout.hero_card.right() - layout.play_home.right() - 44,
                    h: 28,
                },
                15,
                FW_NORMAL as i32,
                Color::rgb(216, 228, 245),
                DT_LEFT | DT_VCENTER | DT_SINGLELINE,
            );
            self.draw_design_notes_text(hdc, layout);
        }

        fn draw_games_text(&self, hdc: windows_sys::Win32::Graphics::Gdi::HDC, layout: Layout) {
            draw_text(
                hdc,
                "DragonFable",
                RectI {
                    x: layout.hero_card.x + 154,
                    y: layout.hero_card.y + 66,
                    w: layout.hero_card.w - 210,
                    h: 62,
                },
                34,
                FW_BOLD as i32,
                Color::rgb(255, 238, 160),
                DT_LEFT | DT_TOP | DT_SINGLELINE,
            );
            draw_text(
                hdc,
                "Classic Artix adventure ready to play.",
                RectI {
                    x: layout.hero_card.x + 156,
                    y: layout.hero_card.y + 118,
                    w: layout.hero_card.w - 230,
                    h: 42,
                },
                16,
                FW_NORMAL as i32,
                Color::rgb(216, 228, 245),
                DT_LEFT | DT_TOP | DT_SINGLELINE,
            );
            draw_text(
                hdc,
                "Play",
                layout.play_aqw_games,
                26,
                FW_BOLD as i32,
                Color::rgb(255, 247, 218),
                DT_CENTER | DT_VCENTER | DT_SINGLELINE,
            );
            self.draw_feed_header_text(hdc, layout);
            self.draw_game_slot_text(
                hdc,
                layout.dragon_fable_card,
                "DragonFable",
                "Play now",
                true,
            );
            self.draw_game_slot_text(hdc, layout.epic_duel_card, "EpicDuel", "Coming soon", false);
            self.draw_game_slot_text(
                hdc,
                layout.adventure_quest_card,
                "AdventureQuest",
                "Coming soon",
                false,
            );
            self.draw_game_slot_text(
                hdc,
                layout.mech_quest_card,
                "MechQuest",
                "Coming soon",
                false,
            );
        }

        fn draw_news_text(&self, hdc: windows_sys::Win32::Graphics::Gdi::HDC, layout: Layout) {
            draw_text(
                hdc,
                "AQW News",
                RectI {
                    x: layout.hero_card.x + 154,
                    y: layout.hero_card.y + 66,
                    w: layout.hero_card.w - 210,
                    h: 62,
                },
                34,
                FW_BOLD as i32,
                Color::rgb(255, 238, 160),
                DT_LEFT | DT_TOP | DT_SINGLELINE,
            );
            draw_text(
                hdc,
                "Official AdventureQuest Worlds updates and Design Notes.",
                RectI {
                    x: layout.hero_card.x + 156,
                    y: layout.hero_card.y + 118,
                    w: layout.hero_card.w - 230,
                    h: 42,
                },
                16,
                FW_NORMAL as i32,
                Color::rgb(216, 228, 245),
                DT_LEFT | DT_TOP | DT_SINGLELINE,
            );
            self.draw_design_notes_text(hdc, layout);
        }

        fn draw_videos_text(&self, hdc: windows_sys::Win32::Graphics::Gdi::HDC, layout: Layout) {
            draw_text(
                hdc,
                "AQW Videos",
                RectI {
                    x: layout.hero_card.x + 154,
                    y: layout.hero_card.y + 66,
                    w: layout.hero_card.w - 210,
                    h: 62,
                },
                34,
                FW_BOLD as i32,
                Color::rgb(255, 238, 160),
                DT_LEFT | DT_TOP | DT_SINGLELINE,
            );
            draw_text(
                hdc,
                "Watch the latest official AdventureQuest Worlds videos on YouTube.",
                RectI {
                    x: layout.hero_card.x + 156,
                    y: layout.hero_card.y + 118,
                    w: layout.hero_card.w - 230,
                    h: 42,
                },
                16,
                FW_NORMAL as i32,
                Color::rgb(216, 228, 245),
                DT_LEFT | DT_TOP | DT_SINGLELINE,
            );
            draw_text(
                hdc,
                "LATEST VIDEOS",
                RectI {
                    x: layout.game_card.x + 18,
                    y: layout.game_card.y + 16,
                    w: layout.game_card.w - 36,
                    h: 28,
                },
                15,
                FW_BOLD as i32,
                Color::rgb(235, 242, 255),
                DT_LEFT | DT_TOP | DT_SINGLELINE,
            );
            self.draw_media_card_texts(hdc, layout);
        }

        fn draw_live_text(&self, hdc: windows_sys::Win32::Graphics::Gdi::HDC, layout: Layout) {
            draw_text(
                hdc,
                "AQW Live",
                RectI {
                    x: layout.hero_card.x + 154,
                    y: layout.hero_card.y + 66,
                    w: layout.hero_card.w - 210,
                    h: 62,
                },
                34,
                FW_BOLD as i32,
                Color::rgb(255, 238, 160),
                DT_LEFT | DT_TOP | DT_SINGLELINE,
            );
            draw_text(
                hdc,
                "Browse creators currently streaming AdventureQuest Worlds on Twitch.",
                RectI {
                    x: layout.hero_card.x + 156,
                    y: layout.hero_card.y + 118,
                    w: layout.hero_card.w - 230,
                    h: 42,
                },
                16,
                FW_NORMAL as i32,
                Color::rgb(216, 228, 245),
                DT_LEFT | DT_TOP | DT_SINGLELINE,
            );
            draw_text(
                hdc,
                "LIVE STREAMS",
                RectI {
                    x: layout.game_card.x + 18,
                    y: layout.game_card.y + 16,
                    w: layout.game_card.w - 36,
                    h: 28,
                },
                15,
                FW_BOLD as i32,
                Color::rgb(235, 242, 255),
                DT_LEFT | DT_TOP | DT_SINGLELINE,
            );
            self.draw_media_card_texts(hdc, layout);
        }

        fn draw_media_card_texts(
            &self,
            hdc: windows_sys::Win32::Graphics::Gdi::HDC,
            layout: Layout,
        ) {
            let items = self.media_items_for_screen();
            let cards = self.media_card_rects(layout, items.len().max(1));
            for (index, rect) in cards.into_iter().enumerate() {
                let item = items.get(index);
                let thumb_h = ((rect.w - 16) * 9 / 16).min(rect.h - 96);
                let label = match item.map(|item| item.source).unwrap_or(MediaSource::YouTube) {
                    MediaSource::YouTube => "YOUTUBE",
                    MediaSource::Twitch => "TWITCH",
                };
                draw_text(
                    hdc,
                    label,
                    RectI {
                        x: rect.x + 18,
                        y: rect.y + 8 + thumb_h - 21,
                        w: rect.w - 36,
                        h: 18,
                    },
                    10,
                    FW_BOLD as i32,
                    Color::rgb(255, 238, 174),
                    DT_LEFT | DT_VCENTER | DT_SINGLELINE,
                );

                let (title, channel) = if let Some(item) = item {
                    (item.title.as_str(), item.channel.as_str())
                } else if self.screen == Screen::Live {
                    ("Loading live channels...", "YouTube / Twitch")
                } else {
                    ("Loading latest videos...", "YouTube")
                };

                draw_text(
                    hdc,
                    title,
                    RectI {
                        x: rect.x + 14,
                        y: rect.y + thumb_h + 18,
                        w: rect.w - 28,
                        h: 44,
                    },
                    15,
                    FW_BOLD as i32,
                    Color::rgb(255, 238, 174),
                    DT_LEFT | DT_TOP | DT_WORDBREAK,
                );
                draw_text(
                    hdc,
                    channel,
                    RectI {
                        x: rect.x + 14,
                        y: rect.bottom() - 30,
                        w: rect.w - 28,
                        h: 20,
                    },
                    12,
                    FW_NORMAL as i32,
                    Color::rgb(198, 210, 230),
                    DT_LEFT | DT_VCENTER | DT_SINGLELINE,
                );
            }
        }

        fn draw_game_slot_text(
            &self,
            hdc: windows_sys::Win32::Graphics::Gdi::HDC,
            rect: RectI,
            title: &str,
            action: &str,
            playable: bool,
        ) {
            let title_color = if playable {
                Color::rgb(255, 238, 174)
            } else {
                Color::rgb(218, 228, 244)
            };
            if rect.h > 130 {
                let text_y = rect.y + 108;
                draw_text(
                    hdc,
                    title,
                    RectI {
                        x: rect.x + 16,
                        y: text_y,
                        w: rect.w - 32,
                        h: 30,
                    },
                    18,
                    FW_BOLD as i32,
                    title_color,
                    DT_LEFT | DT_TOP | DT_SINGLELINE,
                );
                draw_text(
                    hdc,
                    action,
                    RectI {
                        x: rect.x + 16,
                        y: text_y + 32,
                        w: rect.w - 32,
                        h: 44,
                    },
                    14,
                    FW_NORMAL as i32,
                    if playable {
                        Color::rgb(255, 185, 28)
                    } else {
                        Color::rgb(172, 182, 204)
                    },
                    DT_LEFT | DT_TOP | DT_WORDBREAK,
                );
                return;
            }
            draw_text(
                hdc,
                title,
                RectI {
                    x: rect.x + 112,
                    y: rect.y + 18,
                    w: rect.w - 126,
                    h: 22,
                },
                14,
                FW_BOLD as i32,
                title_color,
                DT_LEFT | DT_TOP | DT_SINGLELINE,
            );
            draw_text(
                hdc,
                action,
                RectI {
                    x: rect.x + 112,
                    y: rect.y + 43,
                    w: rect.w - 126,
                    h: 20,
                },
                13,
                FW_NORMAL as i32,
                if playable {
                    Color::rgb(255, 216, 106)
                } else {
                    Color::rgb(150, 169, 198)
                },
                DT_LEFT | DT_TOP | DT_SINGLELINE,
            );
        }

        fn draw_feed_header_text(
            &self,
            hdc: windows_sys::Win32::Graphics::Gdi::HDC,
            layout: Layout,
        ) {
            draw_text(
                hdc,
                "MMORPG - UPDATED WEEKLY!",
                RectI {
                    x: layout.game_card.x,
                    y: layout.game_card.y + 8,
                    w: 246,
                    h: 28,
                },
                13,
                FW_BOLD as i32,
                Color::rgb(235, 242, 255),
                DT_LEFT | DT_TOP | DT_SINGLELINE,
            );
            draw_text(
                hdc,
                "News",
                RectI {
                    x: layout.game_card.x + 260,
                    y: layout.game_card.y + 8,
                    w: 58,
                    h: 28,
                },
                13,
                FW_BOLD as i32,
                Color::rgb(255, 178, 20),
                DT_LEFT | DT_TOP | DT_SINGLELINE,
            );
            draw_text(
                hdc,
                "Artix Support",
                RectI {
                    x: layout.game_card.x + 326,
                    y: layout.game_card.y + 8,
                    w: 120,
                    h: 28,
                },
                13,
                FW_BOLD as i32,
                Color::rgb(255, 178, 20),
                DT_LEFT | DT_TOP | DT_SINGLELINE,
            );
            draw_text(
                hdc,
                "Manage Account",
                RectI {
                    x: layout.game_card.x + 456,
                    y: layout.game_card.y + 8,
                    w: 150,
                    h: 28,
                },
                13,
                FW_BOLD as i32,
                Color::rgb(255, 178, 20),
                DT_LEFT | DT_TOP | DT_SINGLELINE,
            );
        }

        fn draw_design_notes_text(
            &self,
            hdc: windows_sys::Win32::Graphics::Gdi::HDC,
            layout: Layout,
        ) {
            draw_text(
                hdc,
                "AdventureQuest Worlds Design Notes",
                RectI {
                    x: layout.game_card.x + 18,
                    y: layout.game_card.y + 10,
                    w: layout.design_notes_button.x - layout.game_card.x - 32,
                    h: 28,
                },
                15,
                FW_BOLD as i32,
                Color::rgb(235, 242, 255),
                DT_LEFT | DT_TOP | DT_SINGLELINE,
            );
            draw_text(
                hdc,
                "OPEN PAGE",
                layout.design_notes_button,
                12,
                FW_BOLD as i32,
                Color::rgb(238, 244, 255),
                DT_CENTER | DT_VCENTER | DT_SINGLELINE,
            );
            draw_text(
                hdc,
                AQW_DESIGN_NOTES_URL,
                RectI {
                    x: layout.game_card.x + 20,
                    y: layout.game_card.y + 54,
                    w: layout.game_card.w - 40,
                    h: 24,
                },
                13,
                FW_NORMAL as i32,
                Color::rgb(255, 178, 20),
                DT_LEFT | DT_TOP | DT_SINGLELINE,
            );

            let content = layout.game_card.inset(18);
            let article_top = content.y + 74;
            let article_gap = 14;
            let article_h = ((layout.game_card.bottom() - article_top - article_gap * 2 - 18) / 3)
                .clamp(52, 86);
            let items = [
                (
                    "Latest Game Design Notes",
                    "Official AQW updates, weekly events, and reward notes.",
                ),
                (
                    "Updates, Events & Releases",
                    "This area keeps AQWorlds news separate from the game cards.",
                ),
                (
                    "Official AQW News Page",
                    "Open the official news page directly in your browser.",
                ),
            ];
            for (index, (title, body)) in items.iter().enumerate() {
                let y = article_top + index as i32 * (article_h + article_gap);
                draw_text(
                    hdc,
                    title,
                    RectI {
                        x: content.x + 18,
                        y: y + 10,
                        w: content.w - 36,
                        h: 24,
                    },
                    16,
                    FW_BOLD as i32,
                    Color::rgb(255, 238, 174),
                    DT_LEFT | DT_TOP | DT_SINGLELINE,
                );
                draw_text(
                    hdc,
                    body,
                    RectI {
                        x: content.x + 18,
                        y: y + 36,
                        w: content.w - 36,
                        h: 22,
                    },
                    13,
                    FW_NORMAL as i32,
                    Color::rgb(218, 228, 244),
                    DT_LEFT | DT_TOP | DT_SINGLELINE,
                );
            }
        }

        fn draw_status_text(&self, hdc: windows_sys::Win32::Graphics::Gdi::HDC, panel: RectI) {
            draw_text(
                hdc,
                "Status:",
                RectI {
                    x: panel.x + 12,
                    y: panel.y + 7,
                    w: 66,
                    h: 22,
                },
                12,
                FW_BOLD as i32,
                Color::rgb(255, 232, 148),
                DT_LEFT | DT_VCENTER | DT_SINGLELINE,
            );
            draw_text(
                hdc,
                &self.status,
                RectI {
                    x: panel.x + 74,
                    y: panel.y + 7,
                    w: panel.w - 86,
                    h: 22,
                },
                12,
                FW_NORMAL as i32,
                Color::rgb(219, 230, 245),
                DT_LEFT | DT_VCENTER | DT_SINGLELINE,
            );
        }

        fn fill(&mut self, color: Color) {
            for pixel in self.buffer.chunks_exact_mut(4) {
                pixel[0] = color.b;
                pixel[1] = color.g;
                pixel[2] = color.r;
                pixel[3] = 255;
            }
        }

        fn fill_rect_alpha(&mut self, rect: RectI, color: Color) {
            let x0 = rect.x.max(0);
            let y0 = rect.y.max(0);
            let x1 = rect.right().min(self.width);
            let y1 = rect.bottom().min(self.height);
            for y in y0..y1 {
                for x in x0..x1 {
                    self.blend_pixel(x, y, color);
                }
            }
        }

        fn vertical_gradient(&mut self, rect: RectI, top: Color, bottom: Color) {
            let x0 = rect.x.max(0);
            let y0 = rect.y.max(0);
            let x1 = rect.right().min(self.width);
            let y1 = rect.bottom().min(self.height);
            let height = (y1 - y0).max(1);
            for y in y0..y1 {
                let t = (y - y0) as f32 / height as f32;
                let color = Color::rgba(
                    lerp(top.r, bottom.r, t),
                    lerp(top.g, bottom.g, t),
                    lerp(top.b, bottom.b, t),
                    lerp(top.a, bottom.a, t),
                );
                for x in x0..x1 {
                    self.blend_pixel(x, y, color);
                }
            }
        }

        fn fill_round_rect(&mut self, rect: RectI, radius: i32, color: Color) {
            let x0 = rect.x.max(0);
            let y0 = rect.y.max(0);
            let x1 = rect.right().min(self.width);
            let y1 = rect.bottom().min(self.height);
            let radius = radius.max(1);
            let radius_sq = radius * radius;
            for y in y0..y1 {
                for x in x0..x1 {
                    let dx = if x < rect.x + radius {
                        rect.x + radius - x
                    } else if x >= rect.right() - radius {
                        x - (rect.right() - radius - 1)
                    } else {
                        0
                    };
                    let dy = if y < rect.y + radius {
                        rect.y + radius - y
                    } else if y >= rect.bottom() - radius {
                        y - (rect.bottom() - radius - 1)
                    } else {
                        0
                    };
                    if dx == 0 || dy == 0 || dx * dx + dy * dy <= radius_sq {
                        self.blend_pixel(x, y, color);
                    }
                }
            }
        }

        fn draw_image_cover_focus(
            &mut self,
            image: &Bitmap,
            rect: RectI,
            opacity: u8,
            focus_y: f32,
        ) {
            let scale = (rect.w as f32 / image.width as f32)
                .max(rect.h as f32 / image.height as f32)
                .max(0.01);
            let src_w = rect.w as f32 / scale;
            let src_h = rect.h as f32 / scale;
            let src_x = (image.width as f32 - src_w) * 0.5;
            let src_y = ((image.height as f32 - src_h) * focus_y).clamp(0.0, image.height as f32);
            self.draw_image_scaled(image, rect, src_x, src_y, scale, opacity);
        }

        fn draw_image_contain(&mut self, image: &Bitmap, rect: RectI, opacity: u8) {
            let scale = (rect.w as f32 / image.width as f32)
                .min(rect.h as f32 / image.height as f32)
                .max(0.01);
            let draw_w = (image.width as f32 * scale) as i32;
            let draw_h = (image.height as f32 * scale) as i32;
            let dst = RectI {
                x: rect.x + (rect.w - draw_w) / 2,
                y: rect.y + (rect.h - draw_h) / 2,
                w: draw_w,
                h: draw_h,
            };
            self.draw_image_scaled(image, dst, 0.0, 0.0, scale, opacity);
        }

        fn draw_image_region_contain(
            &mut self,
            image: &Bitmap,
            source: RectI,
            rect: RectI,
            opacity: u8,
        ) {
            if source.w <= 0 || source.h <= 0 {
                return;
            }

            let scale = (rect.w as f32 / source.w as f32)
                .min(rect.h as f32 / source.h as f32)
                .max(0.01);
            let draw_w = (source.w as f32 * scale) as i32;
            let draw_h = (source.h as f32 * scale) as i32;
            let dst = RectI {
                x: rect.x + (rect.w - draw_w) / 2,
                y: rect.y + (rect.h - draw_h) / 2,
                w: draw_w,
                h: draw_h,
            };
            self.draw_image_scaled(image, dst, source.x as f32, source.y as f32, scale, opacity);
        }

        fn draw_image_scaled(
            &mut self,
            image: &Bitmap,
            rect: RectI,
            src_x: f32,
            src_y: f32,
            scale: f32,
            opacity: u8,
        ) {
            let x0 = rect.x.max(0);
            let y0 = rect.y.max(0);
            let x1 = rect.right().min(self.width);
            let y1 = rect.bottom().min(self.height);

            for y in y0..y1 {
                let sample_y = ((y - rect.y) as f32 / scale + src_y)
                    .clamp(0.0, (image.height - 1) as f32) as i32;
                for x in x0..x1 {
                    let sample_x = ((x - rect.x) as f32 / scale + src_x)
                        .clamp(0.0, (image.width - 1) as f32)
                        as i32;
                    let index = ((sample_y * image.width + sample_x) * 4) as usize;
                    let alpha = (image.rgba[index + 3] as u16 * opacity as u16 / 255) as u8;
                    self.blend_pixel(
                        x,
                        y,
                        Color::rgba(
                            image.rgba[index],
                            image.rgba[index + 1],
                            image.rgba[index + 2],
                            alpha,
                        ),
                    );
                }
            }
        }

        fn blend_pixel(&mut self, x: i32, y: i32, color: Color) {
            if x < 0 || y < 0 || x >= self.width || y >= self.height || color.a == 0 {
                return;
            }
            let index = ((y * self.width + x) * 4) as usize;
            if color.a == 255 {
                self.buffer[index] = color.b;
                self.buffer[index + 1] = color.g;
                self.buffer[index + 2] = color.r;
                self.buffer[index + 3] = 255;
                return;
            }
            let alpha = color.a as u16;
            let inverse = 255 - alpha;
            self.buffer[index] =
                ((color.b as u16 * alpha + self.buffer[index] as u16 * inverse) / 255) as u8;
            self.buffer[index + 1] =
                ((color.g as u16 * alpha + self.buffer[index + 1] as u16 * inverse) / 255) as u8;
            self.buffer[index + 2] =
                ((color.r as u16 * alpha + self.buffer[index + 2] as u16 * inverse) / 255) as u8;
            self.buffer[index + 3] = 255;
        }
    }

    #[derive(Clone, Copy)]
    struct Color {
        r: u8,
        g: u8,
        b: u8,
        a: u8,
    }

    impl Color {
        fn rgb(r: u8, g: u8, b: u8) -> Self {
            Self { r, g, b, a: 255 }
        }

        fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
            Self { r, g, b, a }
        }

        fn color_ref(self) -> u32 {
            self.r as u32 | ((self.g as u32) << 8) | ((self.b as u32) << 16)
        }
    }

    fn lerp(a: u8, b: u8, t: f32) -> u8 {
        (a as f32 + (b as f32 - a as f32) * t).round() as u8
    }

    fn launch_aqw() -> Result<String, String> {
        launch_flash_game(AQW_SWF_URL, AQW_BASE_URL, AQW_WINDOW_TITLE)
    }

    fn launch_dragon_fable() -> Result<String, String> {
        launch_flash_game(
            DRAGON_FABLE_SWF_URL,
            DRAGON_FABLE_BASE_URL,
            DRAGON_FABLE_WINDOW_TITLE,
        )
    }

    fn open_design_notes(hwnd: HWND) -> Result<(), String> {
        open_url(hwnd, AQW_DESIGN_NOTES_URL)
    }

    fn build_media_items(entries: Vec<MediaEntry>) -> Vec<MediaItem> {
        entries
            .into_iter()
            .map(|entry| {
                let thumbnail = entry
                    .thumbnail_url
                    .as_deref()
                    .and_then(|url| fetch_bitmap(url).ok())
                    .map(Arc::new);
                MediaItem {
                    source: entry.source,
                    title: entry.title,
                    channel: entry.channel,
                    url: entry.url,
                    thumbnail,
                }
            })
            .collect()
    }

    fn fetch_youtube_recent_entries(limit: usize) -> Result<Vec<MediaEntry>, String> {
        let xml = fetch_text(AQW_YOUTUBE_FEED_URL)?;
        let mut entries = Vec::new();
        for chunk in xml.split("<entry>").skip(1).take(limit) {
            let video_id = xml_tag(chunk, "yt:videoId").unwrap_or_default();
            let title = xml_tag(chunk, "media:title")
                .or_else(|| xml_tag(chunk, "title"))
                .unwrap_or_else(|| "AdventureQuest Worlds video".to_string());
            let channel =
                xml_tag(chunk, "name").unwrap_or_else(|| "AdventureQuest Worlds".to_string());
            let thumbnail_url = tag_attr(chunk, "media:thumbnail", "url").or_else(|| {
                if video_id.is_empty() {
                    None
                } else {
                    Some(format!("https://i.ytimg.com/vi/{video_id}/hqdefault.jpg"))
                }
            });
            let url = if video_id.is_empty() {
                tag_attr(chunk, "link", "href")
                    .unwrap_or_else(|| AQW_YOUTUBE_RECENT_URL.to_string())
            } else {
                format!("https://www.youtube.com/watch?v={video_id}")
            };
            entries.push(MediaEntry {
                source: MediaSource::YouTube,
                title,
                channel,
                url,
                thumbnail_url,
            });
        }
        if entries.is_empty() {
            Err("YouTube feed did not return videos.".to_string())
        } else {
            Ok(entries)
        }
    }

    fn fetch_youtube_live_entries() -> Result<Vec<MediaEntry>, String> {
        let html = fetch_text(AQW_YOUTUBE_LIVE_URL)?;
        let title = meta_content(&html, "og:title")
            .or_else(|| json_string_field(&html, "title"))
            .unwrap_or_else(|| "YouTube live channel".to_string());
        let thumbnail_url =
            meta_content(&html, "og:image").or_else(|| json_string_field(&html, "thumbnailUrl"));
        let url = meta_content(&html, "og:url").unwrap_or_else(|| AQW_YOUTUBE_LIVE_URL.to_string());
        let channel = json_string_field(&html, "ownerChannelName")
            .or_else(|| json_string_field(&html, "author"))
            .unwrap_or_else(|| "AdventureQuest Worlds".to_string());

        Ok(vec![MediaEntry {
            source: MediaSource::YouTube,
            title,
            channel,
            url,
            thumbnail_url,
        }])
    }

    fn fetch_twitch_live_entries(limit: usize) -> Result<Vec<MediaEntry>, String> {
        let html = fetch_text(AQW_TWITCH_DIRECTORY_URL)?;
        let mut entries = Vec::new();
        let mut rest = html.as_str();
        while entries.len() < limit {
            let Some(position) = rest
                .find("previewImageURL")
                .or_else(|| rest.find("thumbnailURL"))
            else {
                break;
            };
            let chunk_start = position.saturating_sub(2200);
            let chunk_end = (position + 2600).min(rest.len());
            let chunk = &rest[chunk_start..chunk_end];
            let title = json_string_field(chunk, "title")
                .or_else(|| json_string_field(chunk, "streamTitle"))
                .unwrap_or_else(|| "AdventureQuest Worlds live stream".to_string());
            let channel = json_string_field(chunk, "displayName")
                .or_else(|| json_string_field(chunk, "login"))
                .unwrap_or_else(|| "Twitch channel".to_string());
            let thumbnail_url = json_string_field(chunk, "previewImageURL")
                .or_else(|| json_string_field(chunk, "thumbnailURL"))
                .map(|url| url.replace("{width}", "640").replace("{height}", "360"));
            let login = json_string_field(chunk, "login").unwrap_or_else(|| channel.clone());
            let url = format!("https://www.twitch.tv/{login}");
            if !entries.iter().any(|entry: &MediaEntry| entry.url == url) {
                entries.push(MediaEntry {
                    source: MediaSource::Twitch,
                    title,
                    channel,
                    url,
                    thumbnail_url,
                });
            }
            rest = &rest[position + 1..];
        }
        if entries.is_empty() {
            Err("Twitch page did not expose stream cards.".to_string())
        } else {
            Ok(entries)
        }
    }

    fn fetch_bitmap(url: &str) -> Result<Bitmap, String> {
        let bytes = fetch_bytes(url)?;
        Bitmap::from_memory(&bytes).ok_or_else(|| "Could not decode thumbnail.".to_string())
    }

    fn fetch_text(url: &str) -> Result<String, String> {
        let script = format!(
            "$ProgressPreference='SilentlyContinue'; [Net.ServicePointManager]::SecurityProtocol=[Net.SecurityProtocolType]::Tls12; (Invoke-WebRequest -UseBasicParsing -TimeoutSec 10 -Uri {} -Headers @{{'User-Agent'='Mozilla/5.0 (Windows NT 10.0; Win64; x64) AELauncher'}}).Content",
            ps_quote(url)
        );
        let output = Command::new("powershell")
            .creation_flags(CREATE_NO_WINDOW)
            .args([
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                &script,
            ])
            .output()
            .map_err(|error| error.to_string())?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
        }
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    fn fetch_bytes(url: &str) -> Result<Vec<u8>, String> {
        let temp_dir = std::env::temp_dir().join("AELauncher-media");
        fs::create_dir_all(&temp_dir).map_err(|error| error.to_string())?;
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_millis();
        let path = temp_dir.join(format!("thumb-{stamp}.bin"));
        let script = format!(
            "$ProgressPreference='SilentlyContinue'; [Net.ServicePointManager]::SecurityProtocol=[Net.SecurityProtocolType]::Tls12; Invoke-WebRequest -UseBasicParsing -TimeoutSec 10 -Uri {} -OutFile {} -Headers @{{'User-Agent'='Mozilla/5.0 (Windows NT 10.0; Win64; x64) AELauncher'}}",
            ps_quote(url),
            ps_quote(&path.to_string_lossy())
        );
        let output = Command::new("powershell")
            .creation_flags(CREATE_NO_WINDOW)
            .args([
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                &script,
            ])
            .output()
            .map_err(|error| error.to_string())?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
        }
        let bytes = fs::read(&path).map_err(|error| error.to_string())?;
        let _ = fs::remove_file(path);
        Ok(bytes)
    }

    fn xml_tag(content: &str, tag: &str) -> Option<String> {
        extract_between(content, &format!("<{tag}>"), &format!("</{tag}>"))
            .map(|value| html_decode(value.trim()))
    }

    fn tag_attr(content: &str, tag: &str, attr: &str) -> Option<String> {
        let tag_position = content.find(tag)?;
        let tag_end = content[tag_position..].find('>')? + tag_position;
        attr_value(&content[tag_position..tag_end], attr).map(|value| html_decode(&value))
    }

    fn meta_content(content: &str, property: &str) -> Option<String> {
        let marker = format!("property=\"{property}\"");
        let position = content.find(&marker)?;
        let start = position.saturating_sub(120);
        let end = (position + 420).min(content.len());
        attr_value(&content[start..end], "content").map(|value| html_decode(&value))
    }

    fn attr_value(content: &str, attr: &str) -> Option<String> {
        let marker = format!("{attr}=\"");
        let start = content.find(&marker)? + marker.len();
        let end = content[start..].find('"')? + start;
        Some(content[start..end].to_string())
    }

    fn extract_between<'a>(content: &'a str, start: &str, end: &str) -> Option<&'a str> {
        let start_position = content.find(start)? + start.len();
        let end_position = content[start_position..].find(end)? + start_position;
        Some(&content[start_position..end_position])
    }

    fn json_string_field(content: &str, field: &str) -> Option<String> {
        let marker = format!("\"{field}\":\"");
        let start = content.find(&marker)? + marker.len();
        let mut value = String::new();
        let mut escaped = false;
        for character in content[start..].chars() {
            if escaped {
                value.push(match character {
                    'n' => '\n',
                    't' => '\t',
                    '"' => '"',
                    '\\' => '\\',
                    '/' => '/',
                    other => other,
                });
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                break;
            } else {
                value.push(character);
            }
        }
        if value.is_empty() {
            None
        } else {
            Some(html_decode(&value))
        }
    }

    fn html_decode(value: &str) -> String {
        value
            .replace("&amp;", "&")
            .replace("&quot;", "\"")
            .replace("&#39;", "'")
            .replace("&apos;", "'")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
    }

    fn ps_quote(value: &str) -> String {
        format!("'{}'", value.replace('\'', "''"))
    }

    fn open_url(hwnd: HWND, url: &str) -> Result<(), String> {
        unsafe {
            let operation = wide("open");
            let url = wide(url);
            let result = ShellExecuteW(
                hwnd,
                operation.as_ptr(),
                url.as_ptr(),
                null(),
                null(),
                SW_SHOWNORMAL,
            ) as isize;

            if result <= 32 {
                Err(format!("ShellExecuteW retornou codigo {result}."))
            } else {
                Ok(())
            }
        }
    }

    fn launch_flash_game(
        swf_url: &str,
        base_url: &str,
        window_title: &str,
    ) -> Result<String, String> {
        let diagnostics = diagnostics_enabled();
        let debug_window_title;
        let effective_window_title = if diagnostics {
            debug_window_title = format!("{window_title} [DEBUG]");
            debug_window_title.as_str()
        } else {
            window_title
        };
        let mut temp_path = std::env::temp_dir();
        temp_path.push("aqw_ruffle");
        fs::create_dir_all(&temp_path).map_err(|error| error.to_string())?;

        let ruffle_path = temp_path.join(if diagnostics {
            "AQW-Ruffle-debug.exe"
        } else {
            "AQW-Ruffle.exe"
        });
        let should_write = fs::read(&ruffle_path)
            .map(|existing| existing.as_slice() != RUFFLE_EXE)
            .unwrap_or(true);

        if should_write {
            fs::write(&ruffle_path, RUFFLE_EXE).map_err(|error| error.to_string())?;
        }

        let mut command = Command::new(&ruffle_path);
        command
            .env("ARTIX_RUFFLE_WINDOW_TITLE", effective_window_title)
            .env(
                "RUST_LOG",
                if diagnostics {
                    "warn,ruffle=info,aqw_diag=info"
                } else {
                    "warn"
                },
            )
            .env("RUFFLE_AQW_SUPERSAMPLE", "1.25")
            .arg(swf_url)
            .arg("--spoof-url")
            .arg(swf_url)
            .arg("--base")
            .arg(base_url)
            .args([
                "--graphics",
                FLASH_GRAPHICS_BACKEND,
                "--quality",
                FLASH_QUALITY,
                "--power",
                "high",
                "--frame-rate",
                FLASH_FRAME_RATE,
                "--scale",
                "show-all",
                "--letterbox",
                "on",
                "--upgrade-to-https",
                "--player-version",
                "32",
                "-m",
                "60",
                "--no-gui",
                "--tcp-connections",
                "allow",
            ])
            .creation_flags(CREATE_NO_WINDOW);

        if diagnostics {
            command.env("RUFFLE_AQW_DIAGNOSTICS", "1");
            command.env("RUST_BACKTRACE", "1");
            command.env("RUFFLE_AQW_CACHE_BUDGET_MB", "1024");

            let log_path = temp_path.join("ruffle-debug.log");
            let mut log_file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
                .map_err(|error| error.to_string())?;

            writeln!(
                log_file,
                "\n=== Launching {effective_window_title} with diagnostics: {} ===",
                chrono_like_timestamp()
            )
            .map_err(|error| error.to_string())?;

            let stderr_file = log_file.try_clone().map_err(|error| error.to_string())?;
            command.stdout(Stdio::from(log_file));
            command.stderr(Stdio::from(stderr_file));
        }

        command.spawn().map_err(|error| error.to_string())?;

        Ok(ruffle_path.display().to_string())
    }

    fn diagnostics_enabled() -> bool {
        std::env::current_exe()
            .ok()
            .and_then(|path| path.file_stem().map(|stem| stem.to_owned()))
            .map(|stem| {
                stem.to_string_lossy()
                    .to_ascii_lowercase()
                    .contains("debug")
            })
            .unwrap_or(false)
    }

    fn chrono_like_timestamp() -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        format!("unix_ms={}", now.as_millis())
    }

    pub fn run() {
        unsafe {
            let hinstance = GetModuleHandleW(null());
            let class_name = wide("AQWLauncherWindow");
            let window_title = wide("Artix Games Launcher");
            let app_icon = LoadIconW(hinstance, APPLICATION_ICON_ID as *const u16);
            let wnd_class = WNDCLASSW {
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(window_proc),
                hInstance: hinstance,
                hIcon: app_icon,
                lpszClassName: class_name.as_ptr(),
                hCursor: LoadCursorW(null_mut(), IDC_ARROW),
                ..zeroed()
            };

            RegisterClassW(&wnd_class);

            let mut app = Box::new(AppState::new());
            let hwnd = CreateWindowExW(
                0,
                class_name.as_ptr(),
                window_title.as_ptr(),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                1180,
                720,
                null_mut(),
                null_mut(),
                hinstance,
                app.as_mut() as *mut AppState as *const c_void,
            );

            if hwnd.is_null() {
                show_error(null_mut(), "Could not create the launcher window.");
                return;
            }

            if !app_icon.is_null() {
                SendMessageW(hwnd, WM_SETICON, ICON_BIG as usize, app_icon as isize);
                SendMessageW(hwnd, WM_SETICON, ICON_SMALL as usize, app_icon as isize);
            }

            let _ = Box::into_raw(app);
            ShowWindow(hwnd, SW_SHOW);
            UpdateWindow(hwnd);

            let mut message: MSG = zeroed();
            while GetMessageW(&mut message, null_mut(), 0, 0) > 0 {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match message {
            WM_NCCREATE => {
                let create = lparam as *const CREATESTRUCTW;
                let app = (*create).lpCreateParams as *mut AppState;
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, app as isize);
                1
            }
            WM_GETMINMAXINFO => {
                let info = lparam as *mut MINMAXINFO;
                (*info).ptMinTrackSize.x = MIN_WINDOW_WIDTH;
                (*info).ptMinTrackSize.y = MIN_WINDOW_HEIGHT;
                0
            }
            WM_PAINT => {
                paint(hwnd);
                0
            }
            WM_SIZE => {
                if let Some(app) = app_mut(hwnd) {
                    let (width, height) = client_size(hwnd);
                    app.update_hits(width, height);
                }
                InvalidateRect(hwnd, null(), 0);
                0
            }
            WM_MOUSEMOVE => {
                let x = loword_signed(lparam);
                let y = hiword_signed(lparam);
                if let Some(app) = app_mut(hwnd) {
                    let (width, height) = client_size(hwnd);
                    app.update_hits(width, height);
                    let hovered = app.hit_test(x, y);
                    if app.hovered != hovered {
                        app.hovered = hovered;
                        InvalidateRect(hwnd, null(), 0);
                    }
                    if !app.tracking_mouse {
                        let mut tracker = TRACKMOUSEEVENT {
                            cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                            dwFlags: TME_LEAVE,
                            hwndTrack: hwnd,
                            dwHoverTime: 0,
                        };
                        TrackMouseEvent(&mut tracker);
                        app.tracking_mouse = true;
                    }
                }
                0
            }
            WM_MOUSELEAVE => {
                if let Some(app) = app_mut(hwnd) {
                    app.hovered = None;
                    app.tracking_mouse = false;
                    InvalidateRect(hwnd, null(), 0);
                }
                0
            }
            WM_LBUTTONDOWN => {
                SetFocus(hwnd);
                let x = loword_signed(lparam);
                let y = hiword_signed(lparam);
                if let Some(app) = app_mut(hwnd) {
                    let (width, height) = client_size(hwnd);
                    app.update_hits(width, height);
                    if let Some(id) = app.hit_test(x, y) {
                        app.focused = id;
                        app.activate(id, hwnd);
                        InvalidateRect(hwnd, null(), 0);
                    }
                }
                0
            }
            WM_KEYDOWN => {
                if let Some(app) = app_mut(hwnd) {
                    match wparam as u16 {
                        VK_TAB => app.focus_next(),
                        VK_RETURN | VK_SPACE => app.activate(app.focused, hwnd),
                        VK_ESCAPE => {
                            DestroyWindow(hwnd);
                        }
                        _ => {}
                    }
                    InvalidateRect(hwnd, null(), 0);
                }
                0
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                0
            }
            WM_NCDESTROY => {
                let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut AppState;
                if !ptr.is_null() {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    drop(Box::from_raw(ptr));
                }
                DefWindowProcW(hwnd, message, wparam, lparam)
            }
            _ => DefWindowProcW(hwnd, message, wparam, lparam),
        }
    }

    unsafe fn paint(hwnd: HWND) {
        let mut paint: PAINTSTRUCT = zeroed();
        let hdc = BeginPaint(hwnd, &mut paint);
        let (width, height) = client_size(hwnd);

        if let Some(app) = app_mut(hwnd) {
            app.render(width, height);
            let bitmap_info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    biHeight: -height,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB,
                    biSizeImage: 0,
                    biXPelsPerMeter: 0,
                    biYPelsPerMeter: 0,
                    biClrUsed: 0,
                    biClrImportant: 0,
                },
                bmiColors: [RGBQUAD {
                    rgbBlue: 0,
                    rgbGreen: 0,
                    rgbRed: 0,
                    rgbReserved: 0,
                }],
            };

            StretchDIBits(
                hdc,
                0,
                0,
                width,
                height,
                0,
                0,
                width,
                height,
                app.buffer.as_ptr() as *const c_void,
                &bitmap_info,
                DIB_RGB_COLORS,
                SRCCOPY,
            );
            app.draw_texts(hdc);
        }

        EndPaint(hwnd, &paint);
    }

    unsafe fn app_mut(hwnd: HWND) -> Option<&'static mut AppState> {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut AppState;
        ptr.as_mut()
    }

    unsafe fn client_size(hwnd: HWND) -> (i32, i32) {
        let mut rect: RECT = zeroed();
        GetClientRect(hwnd, &mut rect);
        (
            (rect.right - rect.left).max(1),
            (rect.bottom - rect.top).max(1),
        )
    }

    fn draw_text(
        hdc: windows_sys::Win32::Graphics::Gdi::HDC,
        text: &str,
        rect: RectI,
        size: i32,
        weight: i32,
        color: Color,
        flags: u32,
    ) {
        unsafe {
            let font_name = wide("Segoe UI");
            let dpi = GetDeviceCaps(hdc, LOGPIXELSY as i32).max(96);
            let font_height = -((size * dpi) / 72);
            let font = CreateFontW(
                font_height,
                0,
                0,
                0,
                weight,
                0,
                0,
                0,
                DEFAULT_CHARSET as u32,
                OUT_DEFAULT_PRECIS as u32,
                CLIP_DEFAULT_PRECIS as u32,
                CLEARTYPE_QUALITY as u32,
                DEFAULT_PITCH as u32,
                font_name.as_ptr(),
            );
            let previous = SelectObject(hdc, font);
            SetTextColor(hdc, color.color_ref());
            SetBkMode(hdc, TRANSPARENT as i32);
            let wide_text = wide(text);
            let mut win_rect = RECT {
                left: rect.x,
                top: rect.y,
                right: rect.right(),
                bottom: rect.bottom(),
            };
            DrawTextW(
                hdc,
                wide_text.as_ptr(),
                (wide_text.len() - 1) as i32,
                &mut win_rect,
                flags,
            );
            SelectObject(hdc, previous);
            DeleteObject(font);
        }
    }

    fn show_error(hwnd: HWND, message: &str) {
        unsafe {
            let title = wide("AQW Launcher");
            let message = wide(message);
            MessageBoxW(hwnd, message.as_ptr(), title.as_ptr(), MB_OK | MB_ICONERROR);
        }
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(Some(0)).collect()
    }

    fn loword_signed(value: LPARAM) -> i32 {
        (value as u32 & 0xffff) as i16 as i32
    }

    fn hiword_signed(value: LPARAM) -> i32 {
        ((value as u32 >> 16) & 0xffff) as i16 as i32
    }
}
