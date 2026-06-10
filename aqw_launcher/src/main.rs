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
    use std::fs;
    use std::mem::{size_of, zeroed};
    use std::os::windows::process::CommandExt;
    use std::process::Command;
    use std::ptr::{null, null_mut};
    use std::sync::Arc;

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

    const RUFFLE_EXE: &[u8] = include_bytes!("../../target/release/ruffle_desktop.exe");
    const HERO_IMAGE: &[u8] = include_bytes!("../assets/launcher_entry.png");
    const DRAGON_FABLE_BANNER: &[u8] = include_bytes!("../assets/dragon_fable_banner.png");
    const AQW_BADGE: &[u8] = include_bytes!("../assets/aqw_badge.png");
    const ARTIX_WORDMARK: &[u8] = include_bytes!("../assets/artix_entertainment.png");
    const DRAGON_FABLE_LOGO: &[u8] = include_bytes!("../assets/dragon_fable.png");
    const EPIC_DUEL_LOGO: &[u8] = include_bytes!("../assets/epic_duel.png");
    const ADVENTURE_QUEST_LOGO: &[u8] = include_bytes!("../assets/adventure_quest.png");
    const MECH_QUEST_LOGO: &[u8] = include_bytes!("../assets/mech_quest.png");
    const DRAGON_IMAGE: &[u8] = include_bytes!("../assets/dragon_window.png");

    const CREATE_NO_WINDOW: u32 = 0x08000000;
    const APPLICATION_ICON_ID: usize = 1;
    const MIN_WINDOW_WIDTH: i32 = 960;
    const MIN_WINDOW_HEIGHT: i32 = 620;
    const WM_MOUSELEAVE: u32 = 0x02A3;
    const AQW_SWF_URL: &str = "https://game.aq.com/game/gamefiles/Loader3.swf";
    const AQW_BASE_URL: &str = "https://game.aq.com/game/gamefiles/";
    const AQW_WINDOW_TITLE: &str = "Artix Entertainment - AdventureQuest Worlds V0.1";
    const AQW_DESIGN_NOTES_URL: &str = "https://www.aq.com/gamedesignnotes/";
    const DRAGON_FABLE_SWF_URL: &str = "https://play.dragonfable.com/game/DFLoader.swf";
    const DRAGON_FABLE_BASE_URL: &str = "https://play.dragonfable.com/game/";
    const DRAGON_FABLE_WINDOW_TITLE: &str = "Artix Entertainment -Dragon Fable";

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Screen {
        Home,
        Games,
        Help,
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum ElementId {
        NavHome,
        NavGames,
        NavHelp,
        PlayHome,
        PlayDragonFable,
        OpenDesignNotes,
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
        wordmark: Arc<Bitmap>,
        dragon_fable: Arc<Bitmap>,
        epic_duel: Arc<Bitmap>,
        adventure_quest: Arc<Bitmap>,
        mech_quest: Arc<Bitmap>,
        dragon: Arc<Bitmap>,
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
                status: "Pronto para jogar.".to_string(),
                hero: Arc::new(Bitmap::from_png(HERO_IMAGE)),
                dragon_fable_banner: Arc::new(Bitmap::from_png(DRAGON_FABLE_BANNER)),
                badge: Arc::new(Bitmap::from_png(AQW_BADGE)),
                wordmark: Arc::new(Bitmap::from_png(ARTIX_WORDMARK)),
                dragon_fable: Arc::new(Bitmap::from_png(DRAGON_FABLE_LOGO)),
                epic_duel: Arc::new(Bitmap::from_png(EPIC_DUEL_LOGO)),
                adventure_quest: Arc::new(Bitmap::from_png(ADVENTURE_QUEST_LOGO)),
                mech_quest: Arc::new(Bitmap::from_png(MECH_QUEST_LOGO)),
                dragon: Arc::new(Bitmap::from_png(DRAGON_IMAGE)),
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
                x: nav_width,
                y: 0,
                w: width - nav_width,
                h: 68,
            };
            let content_x = nav_width + 12;
            let content_w = width - nav_width - 24;
            let hero_h = (height * 35 / 100).clamp(220, 360);
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
            let play_home = RectI {
                x: hero_card.x + 22,
                y: hero_card.bottom() - 74,
                w: 190,
                h: 56,
            };
            let play_aqw_games = RectI {
                x: play_home.x,
                y: play_home.y,
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
            let nav_games = RectI {
                x: 0,
                y: nav_home.bottom(),
                w: nav_width,
                h: 48,
            };
            let nav_help = RectI {
                x: 0,
                y: nav_games.bottom(),
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
                    id: ElementId::NavHome,
                    rect: layout.nav_home,
                },
                HitBox {
                    id: ElementId::NavGames,
                    rect: layout.nav_games,
                },
                HitBox {
                    id: ElementId::NavHelp,
                    rect: layout.nav_help,
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
                Screen::Help => {}
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
                Screen::Home | Screen::Help => (Arc::clone(&self.hero), 0.42),
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
                    x: layout.nav_width,
                    y: layout.top_bar.bottom() - 1,
                    w: self.width - layout.nav_width,
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
                    y: 2,
                    w: layout.nav_width - 20,
                    h: 64,
                },
                255,
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
            self.draw_nav_button(
                layout.nav_help,
                ElementId::NavHelp,
                self.screen == Screen::Help,
            );
            let badge = Arc::clone(&self.badge);
            let dragon_fable = Arc::clone(&self.dragon_fable);
            let adventure_quest = Arc::clone(&self.adventure_quest);
            let epic_duel = Arc::clone(&self.epic_duel);
            let mech_quest = Arc::clone(&self.mech_quest);
            self.draw_sidebar_icon(layout.nav_home, &badge, 255);
            self.draw_sidebar_icon(layout.nav_games, &dragon_fable, 255);
            self.draw_sidebar_icon(layout.nav_help, &adventure_quest, 230);
            self.draw_sidebar_static_item(
                RectI {
                    x: 0,
                    y: layout.nav_help.bottom(),
                    w: layout.nav_width,
                    h: 48,
                },
                &epic_duel,
            );
            self.draw_sidebar_static_item(
                RectI {
                    x: 0,
                    y: layout.nav_help.bottom() + 48,
                    w: layout.nav_width,
                    h: 48,
                },
                &adventure_quest,
            );
            self.draw_sidebar_static_item(
                RectI {
                    x: 0,
                    y: layout.nav_help.bottom() + 96,
                    w: layout.nav_width,
                    h: 48,
                },
                &mech_quest,
            );
            self.draw_status_strip(layout.right_panel);

            match self.screen {
                Screen::Home => self.draw_home(layout),
                Screen::Games => self.draw_games(layout),
                Screen::Help => self.draw_help(layout),
            }
        }

        fn draw_home(&mut self, layout: Layout) {
            let badge = Arc::clone(&self.badge);
            self.draw_image_contain(&badge, layout.game_icon, 255);
            self.draw_button(
                layout.play_home,
                ElementId::PlayHome,
                true,
                Color::rgba(128, 18, 14, 240),
                Color::rgba(172, 35, 22, 250),
            );
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

        fn draw_help(&mut self, layout: Layout) {
            self.draw_panel(
                layout.game_card,
                Color::rgba(13, 15, 22, 226),
                Color::rgba(55, 58, 72, 210),
            );
            let guide_1 = RectI {
                x: layout.game_card.x + 34,
                y: layout.game_card.y + 44,
                w: layout.game_card.w - 68,
                h: 48,
            };
            let guide_2 = RectI {
                y: guide_1.y + 64,
                ..guide_1
            };
            let guide_3 = RectI {
                y: guide_2.y + 64,
                ..guide_1
            };
            self.draw_panel(
                guide_1,
                Color::rgba(18, 31, 48, 210),
                Color::rgba(84, 130, 216, 120),
            );
            self.draw_panel(
                guide_2,
                Color::rgba(18, 31, 48, 210),
                Color::rgba(84, 130, 216, 120),
            );
            self.draw_panel(
                guide_3,
                Color::rgba(18, 31, 48, 210),
                Color::rgba(84, 130, 216, 120),
            );
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

        fn hit_test(&self, x: i32, y: i32) -> Option<ElementId> {
            self.hits
                .iter()
                .find(|hit| hit.rect.contains(x, y))
                .map(|hit| hit.id)
        }

        fn set_screen(&mut self, screen: Screen) {
            self.screen = screen;
            self.focused = match screen {
                Screen::Home => ElementId::PlayHome,
                Screen::Games => ElementId::PlayDragonFable,
                Screen::Help => ElementId::NavHome,
            };
        }

        fn focus_order(&self) -> &'static [ElementId] {
            match self.screen {
                Screen::Home => &[
                    ElementId::PlayHome,
                    ElementId::OpenDesignNotes,
                    ElementId::NavHome,
                    ElementId::NavGames,
                    ElementId::NavHelp,
                ],
                Screen::Games => &[
                    ElementId::PlayDragonFable,
                    ElementId::FutureEpicDuel,
                    ElementId::FutureAdventureQuest,
                    ElementId::FutureMechQuest,
                    ElementId::NavHome,
                    ElementId::NavGames,
                    ElementId::NavHelp,
                ],
                Screen::Help => &[ElementId::NavHome, ElementId::NavGames, ElementId::NavHelp],
            }
        }

        fn focus_next(&mut self) {
            let order = self.focus_order();
            let current = order.iter().position(|id| *id == self.focused).unwrap_or(0);
            self.focused = order[(current + 1) % order.len()];
        }

        fn activate(&mut self, id: ElementId, hwnd: HWND) {
            match id {
                ElementId::NavHome => self.set_screen(Screen::Home),
                ElementId::NavGames => self.set_screen(Screen::Games),
                ElementId::NavHelp => self.set_screen(Screen::Help),
                ElementId::PlayHome => match launch_aqw() {
                    Ok(_) => {
                        self.status = "AdventureQuest Worlds iniciado pelo Ruffle.".to_string();
                    }
                    Err(error) => {
                        self.status = "Falha ao iniciar o jogo.".to_string();
                        show_error(hwnd, &format!("Nao foi possivel iniciar o AQW.\n\n{error}"));
                    }
                },
                ElementId::PlayDragonFable => match launch_dragon_fable() {
                    Ok(_) => {
                        self.status = "DragonFable iniciado pelo Ruffle.".to_string();
                    }
                    Err(error) => {
                        self.status = "Falha ao iniciar DragonFable.".to_string();
                        show_error(
                            hwnd,
                            &format!("Nao foi possivel iniciar DragonFable.\n\n{error}"),
                        );
                    }
                },
                ElementId::OpenDesignNotes => match open_design_notes(hwnd) {
                    Ok(_) => {
                        self.status = "Design Notes aberto no navegador.".to_string();
                    }
                    Err(error) => {
                        self.status = "Falha ao abrir Design Notes.".to_string();
                        show_error(
                            hwnd,
                            &format!("Nao foi possivel abrir as Design Notes.\n\n{error}"),
                        );
                    }
                },
                ElementId::FutureEpicDuel => {
                    self.status =
                        "EpicDuel reservado para futura funcao de abertura/instalacao.".to_string();
                }
                ElementId::FutureAdventureQuest => {
                    self.status =
                        "AdventureQuest reservado para futura funcao de abertura/instalacao."
                            .to_string();
                }
                ElementId::FutureMechQuest => {
                    self.status = "MechQuest reservado para futura funcao de abertura/instalacao."
                        .to_string();
                }
            }
        }

        fn draw_texts(&self, hdc: windows_sys::Win32::Graphics::Gdi::HDC) {
            unsafe {
                SetBkMode(hdc, TRANSPARENT as i32);
            }

            let layout = self.layout(self.width, self.height);
            draw_text(
                hdc,
                "GAMES",
                RectI {
                    x: layout.nav_width + 20,
                    y: 21,
                    w: 78,
                    h: 24,
                },
                13,
                FW_BOLD as i32,
                Color::rgb(255, 255, 255),
                DT_LEFT | DT_TOP | DT_SINGLELINE,
            );
            draw_text(
                hdc,
                "NEWS",
                RectI {
                    x: layout.nav_width + 92,
                    y: 21,
                    w: 68,
                    h: 24,
                },
                13,
                FW_BOLD as i32,
                Color::rgb(210, 214, 224),
                DT_LEFT | DT_TOP | DT_SINGLELINE,
            );
            draw_text(
                hdc,
                "VIDEOS",
                RectI {
                    x: layout.nav_width + 154,
                    y: 21,
                    w: 82,
                    h: 24,
                },
                13,
                FW_BOLD as i32,
                Color::rgb(210, 214, 224),
                DT_LEFT | DT_TOP | DT_SINGLELINE,
            );
            draw_text(
                hdc,
                "STREAM",
                RectI {
                    x: layout.nav_width + 236,
                    y: 21,
                    w: 90,
                    h: 24,
                },
                13,
                FW_BOLD as i32,
                Color::rgb(210, 214, 224),
                DT_LEFT | DT_TOP | DT_SINGLELINE,
            );
            self.draw_nav_text(hdc, layout.nav_home, "AQWorlds");
            self.draw_nav_text(hdc, layout.nav_games, "DragonFable");
            self.draw_nav_text(hdc, layout.nav_help, "Support");
            self.draw_static_sidebar_text(hdc, layout.nav_help.bottom(), "EpicDuel");
            self.draw_static_sidebar_text(hdc, layout.nav_help.bottom() + 48, "AdventureQuest");
            self.draw_static_sidebar_text(hdc, layout.nav_help.bottom() + 96, "MechQuest");

            self.draw_status_text(hdc, layout.right_panel);

            match self.screen {
                Screen::Home => self.draw_home_text(hdc, layout),
                Screen::Games => self.draw_games_text(hdc, layout),
                Screen::Help => self.draw_help_text(hdc, layout),
            }
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
                "AdventureQuest Worlds",
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
                "MMORPG em Flash rodando pelo Ruffle embutido.",
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
                layout.play_home,
                26,
                FW_BOLD as i32,
                Color::rgb(255, 247, 218),
                DT_CENTER | DT_VCENTER | DT_SINGLELINE,
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
                "Aventura classica da Artix pronta para jogar.",
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
                "Jogar agora",
                true,
            );
            self.draw_game_slot_text(hdc, layout.epic_duel_card, "EpicDuel", "Em breve", false);
            self.draw_game_slot_text(
                hdc,
                layout.adventure_quest_card,
                "AdventureQuest",
                "Em breve",
                false,
            );
            self.draw_game_slot_text(hdc, layout.mech_quest_card, "MechQuest", "Em breve", false);
        }

        fn draw_help_text(&self, hdc: windows_sys::Win32::Graphics::Gdi::HDC, layout: Layout) {
            draw_text(
                hdc,
                "Como navegar",
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
                "Use mouse, Tab e Enter para navegar pelo launcher.",
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
            let guide = [
                (
                    "1",
                    "Inicio mostra o botao rapido para abrir o AdventureQuest Worlds.",
                ),
                (
                    "2",
                    "Jogos exibe AQW, DragonFable e espacos para outros titulos da Artix.",
                ),
                (
                    "3",
                    "Tab navega pelos cards; Enter ou Espaco ativa o item selecionado.",
                ),
            ];
            for (index, (number, text)) in guide.iter().enumerate() {
                let y = layout.game_card.y + 48 + index as i32 * 64;
                draw_text(
                    hdc,
                    number,
                    RectI {
                        x: layout.game_card.x + 48,
                        y,
                        w: 36,
                        h: 44,
                    },
                    24,
                    FW_BOLD as i32,
                    Color::rgb(255, 216, 106),
                    DT_CENTER | DT_VCENTER | DT_SINGLELINE,
                );
                draw_text(
                    hdc,
                    text,
                    RectI {
                        x: layout.game_card.x + 98,
                        y: y + 4,
                        w: layout.game_card.w - 132,
                        h: 42,
                    },
                    16,
                    FW_NORMAL as i32,
                    Color::rgb(224, 234, 247),
                    DT_LEFT | DT_VCENTER | DT_WORDBREAK,
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
                "ABRIR PAGINA",
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
                    "Novidades oficiais, eventos semanais e recompensas do AQW.",
                ),
                (
                    "Updates, Events & Releases",
                    "Esta area substitui os cards de outros jogos no AQWorlds.",
                ),
                (
                    "Official AQW News Page",
                    "Abra o conteudo ao vivo diretamente no site oficial.",
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
        unsafe {
            let operation = wide("open");
            let url = wide(AQW_DESIGN_NOTES_URL);
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
        let mut temp_path = std::env::temp_dir();
        temp_path.push("aqw_ruffle");
        fs::create_dir_all(&temp_path).map_err(|error| error.to_string())?;

        let ruffle_path = temp_path.join("AQW-Ruffle.exe");
        let should_write = if ruffle_path.exists() {
            fs::metadata(&ruffle_path)
                .map(|metadata| metadata.len() != RUFFLE_EXE.len() as u64)
                .unwrap_or(true)
        } else {
            true
        };

        if should_write {
            fs::write(&ruffle_path, RUFFLE_EXE).map_err(|error| error.to_string())?;
        }

        let mut command = Command::new(&ruffle_path);
        command
            .env("ARTIX_RUFFLE_WINDOW_TITLE", window_title)
            .env("RUST_LOG", "warn")
            .arg(swf_url)
            .arg("--spoof-url")
            .arg(swf_url)
            .arg("--base")
            .arg(base_url)
            .args([
                "--quality",
                "high",
                "--power",
                "high",
                "--frame-rate",
                "24",
                "--scale",
                "exact-fit",
                "--force-scale",
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

        command.spawn().map_err(|error| error.to_string())?;

        Ok(ruffle_path.display().to_string())
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
                show_error(null_mut(), "Nao foi possivel criar a janela do launcher.");
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
