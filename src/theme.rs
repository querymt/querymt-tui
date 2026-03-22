use ratatui::style::{Color, Modifier, Style};

use crate::themes_gen::{Base16Palette, DARK_THEMES};

/// Runtime theme built from a Base16 palette.
/// All UI styles are derived from 16 colors.
///
/// Base16 slot mapping:
///   00 bg        04 dim fg      08 red       0C cyan
///   01 bg card   05 fg          09 orange    0D blue
///   02 bg hl     06 bright fg   0A yellow    0E magenta
///   03 comments  07 brightest   0B green     0F brown/extra
pub struct Theme;

pub(crate) fn u32_to_color(c: u32) -> Color {
    Color::Rgb((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

/// Global theme instance — uses atomic index into DARK_THEMES.
static THEME_IDX: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

// Per-frame snapshot of the theme index. Avoids repeated atomic loads
// during a single render pass. Updated by `Theme::begin_frame()`.
thread_local! {
    static FRAME_THEME_IDX: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl Theme {
    pub fn init(id: &str) {
        let idx = DARK_THEMES.iter().position(|t| t.id == id).unwrap_or(0);
        THEME_IDX.store(idx, std::sync::atomic::Ordering::Relaxed);
        FRAME_THEME_IDX.set(idx);
    }

    pub fn set_by_index(idx: usize) {
        if idx < DARK_THEMES.len() {
            THEME_IDX.store(idx, std::sync::atomic::Ordering::Relaxed);
        }
    }

    pub fn current_index() -> usize {
        THEME_IDX.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn current_id() -> &'static str {
        DARK_THEMES[Self::current_index()].id
    }

    fn palette() -> &'static Base16Palette {
        &DARK_THEMES[FRAME_THEME_IDX.get()]
    }

    fn c(slot: usize) -> Color {
        u32_to_color(Self::palette().colors[slot])
    }

    // -- raw colors from palette --
    pub fn bg() -> Color {
        Self::c(0x00)
    }
    pub fn bg_dim() -> Color {
        // darken base00 slightly for status bars
        let Color::Rgb(r, g, b) = Self::c(0x00) else {
            return Self::c(0x00);
        };
        Color::Rgb(
            r.saturating_sub(10),
            g.saturating_sub(10),
            b.saturating_sub(10),
        )
    }
    pub fn bg_card() -> Color {
        Self::c(0x01)
    }
    pub fn bg_hl() -> Color {
        Self::c(0x02)
    }
    pub fn fg() -> Color {
        Self::c(0x05)
    }
    pub fn dim() -> Color {
        Self::c(0x03)
    }
    pub fn bright() -> Color {
        Self::c(0x06)
    }
    pub fn accent() -> Color {
        Self::c(0x0D)
    } // blue
    pub fn info() -> Color {
        Self::c(0x0C)
    } // cyan
    pub fn ok() -> Color {
        Self::c(0x0B)
    } // green
    pub fn warn() -> Color {
        Self::c(0x0A)
    } // yellow
    pub fn err() -> Color {
        Self::c(0x08)
    } // red

    // extra accents
    fn orange() -> Color {
        Self::c(0x09)
    }
    fn magenta() -> Color {
        Self::c(0x0E)
    }
    fn extra() -> Color {
        Self::c(0x0F)
    }
    fn brightest() -> Color {
        Self::c(0x07)
    }
    fn dim_fg() -> Color {
        Self::c(0x04)
    }

    // -- composed styles --

    pub fn base() -> Style {
        Style::default().fg(Self::fg()).bg(Self::bg())
    }
    pub fn title() -> Style {
        Style::default()
            .fg(Self::accent())
            .add_modifier(Modifier::BOLD)
    }
    pub fn title_accent() -> Style {
        Style::default()
            .fg(Self::magenta())
            .add_modifier(Modifier::BOLD)
    }
    pub fn status() -> Style {
        Style::default().fg(Self::dim()).bg(Self::bg_dim())
    }
    pub fn status_accent() -> Style {
        Style::default().fg(Self::magenta()).bg(Self::bg_dim())
    }
    pub fn input() -> Style {
        Style::default().fg(Self::fg()).bg(Self::bg_card())
    }
    pub fn input_label() -> Style {
        Style::default()
            .fg(Self::accent())
            .bg(Self::bg_card())
            .add_modifier(Modifier::BOLD)
    }
    pub fn input_thinking() -> Style {
        Style::default().fg(Self::dim()).bg(Self::bg_card())
    }
    pub fn input_undo() -> Style {
        Style::default()
            .fg(Self::orange())
            .bg(Self::bg_card())
            .add_modifier(Modifier::BOLD)
    }
    pub fn input_redo() -> Style {
        Style::default()
            .fg(Self::info())
            .bg(Self::bg_card())
            .add_modifier(Modifier::BOLD)
    }
    pub fn input_cancel_confirm() -> Style {
        Style::default()
            .fg(Self::warn())
            .bg(Self::bg_card())
            .add_modifier(Modifier::BOLD)
    }
    pub fn input_compacting() -> Style {
        Style::default()
            .fg(Self::accent())
            .bg(Self::bg_card())
            .add_modifier(Modifier::BOLD)
    }
    pub fn input_border() -> Style {
        Style::default().fg(Self::accent()).bg(Self::bg_card())
    }
    pub fn input_border_thinking() -> Style {
        Style::default().fg(Self::magenta()).bg(Self::bg_card())
    }
    pub fn input_border_undo() -> Style {
        Style::default().fg(Self::orange()).bg(Self::bg_card())
    }
    pub fn input_border_redo() -> Style {
        Style::default().fg(Self::info()).bg(Self::bg_card())
    }
    pub fn input_border_cancel_confirm() -> Style {
        Style::default().fg(Self::warn()).bg(Self::bg_card())
    }
    pub fn input_border_compacting() -> Style {
        Style::default().fg(Self::accent()).bg(Self::bg_card())
    }

    // -- mode colors --
    pub fn mode_color(mode: &str) -> Color {
        match mode {
            "plan" => Self::orange(),
            "review" => Self::magenta(),
            _ => Self::ok(),
        }
    }
    pub fn mode_border(mode: &str) -> Style {
        Style::default()
            .fg(Self::mode_color(mode))
            .bg(Self::bg_card())
    }
    pub fn mode_badge(mode: &str) -> Style {
        Style::default()
            .fg(Self::mode_color(mode))
            .add_modifier(Modifier::BOLD)
    }

    // -- message cards --

    pub fn user_card() -> Style {
        Style::default().bg(Self::bg_card())
    }
    pub fn user_label() -> Style {
        Style::default()
            .fg(Self::accent())
            .bg(Self::bg_card())
            .add_modifier(Modifier::BOLD)
    }
    pub fn user_text() -> Style {
        Style::default().fg(Self::bright()).bg(Self::bg_card())
    }

    pub fn assistant_card() -> Style {
        Style::default().bg(Self::bg())
    }
    pub fn assistant_label() -> Style {
        Style::default()
            .fg(Self::ok())
            .bg(Self::bg())
            .add_modifier(Modifier::BOLD)
    }
    pub fn assistant_text() -> Style {
        Style::default().fg(Self::fg()).bg(Self::bg())
    }

    pub fn tool_label() -> Style {
        Style::default().fg(Self::dim()).bg(Self::bg())
    }
    pub fn tool_text() -> Style {
        Style::default()
            .fg(Self::dim())
            .bg(Self::bg())
            .add_modifier(Modifier::ITALIC)
    }
    pub fn tool_error() -> Style {
        Style::default().fg(Self::orange()).bg(Self::bg())
    }
    pub fn tool_output() -> Style {
        Style::default().fg(Self::dim())
    }

    pub fn thinking() -> Style {
        Style::default().fg(Self::magenta()).bg(Self::bg())
    }
    pub fn error_text() -> Style {
        Style::default().fg(Self::err()).bg(Self::bg())
    }
    pub fn info_text() -> Style {
        Style::default().fg(Self::info()).bg(Self::bg())
    }

    // -- selection --
    pub fn selected() -> Style {
        Style::default().fg(Self::bright()).bg(Self::bg_hl())
    }
    pub fn list_item() -> Style {
        Style::default().fg(Self::fg()).bg(Self::bg_card())
    }
    pub fn list_dim() -> Style {
        Style::default().fg(Self::dim()).bg(Self::bg_card())
    }
    pub fn session_time() -> Style {
        Style::default().fg(Self::info())
    }

    // -- popup --
    pub fn popup_bg() -> Style {
        Style::default().fg(Self::fg()).bg(Self::bg_dim())
    }
    pub fn popup_title() -> Style {
        Style::default()
            .fg(Self::magenta())
            .bg(Self::bg_dim())
            .add_modifier(Modifier::BOLD)
    }

    // -- markdown --
    pub fn md_bold() -> Style {
        Style::default()
            .fg(Self::bright())
            .add_modifier(Modifier::BOLD)
    }
    pub fn md_italic() -> Style {
        Style::default().add_modifier(Modifier::ITALIC)
    }
    pub fn md_bold_italic() -> Style {
        Style::default()
            .fg(Self::bright())
            .add_modifier(Modifier::BOLD | Modifier::ITALIC)
    }
    pub fn md_code_inline() -> Style {
        Style::default().fg(Self::ok()).bg(Self::bg_hl())
    }
    pub fn md_code_block() -> Style {
        Style::default().fg(Self::fg()).bg(Self::bg_card())
    }
    pub fn md_code_lang() -> Style {
        Style::default().fg(Self::dim()).bg(Self::bg_card())
    }
    pub fn md_code_line_nr() -> Style {
        Style::default().fg(Self::dim()).bg(Self::bg_card())
    }
    pub fn md_heading() -> Style {
        Style::default()
            .fg(Self::accent())
            .add_modifier(Modifier::BOLD)
    }
    pub fn md_link() -> Style {
        Style::default()
            .fg(Self::info())
            .add_modifier(Modifier::UNDERLINED)
    }
    pub fn md_link_title() -> Style {
        Style::default().fg(Self::info())
    }
    pub fn md_list_bullet() -> Style {
        Style::default().fg(Self::magenta())
    }
    pub fn md_blockquote() -> Style {
        Style::default()
            .fg(Self::dim())
            .add_modifier(Modifier::ITALIC)
    }
    pub fn md_hr() -> Style {
        Style::default().fg(Self::dim())
    }
    pub fn md_strikethrough() -> Style {
        Style::default()
            .fg(Self::dim())
            .add_modifier(Modifier::CROSSED_OUT)
    }
    pub fn md_table_border() -> Style {
        Style::default().fg(Self::dim())
    }
    pub fn md_table_header() -> Style {
        Style::default()
            .fg(Self::accent())
            .add_modifier(Modifier::BOLD)
    }

    // -- diff --
    pub fn diff_file() -> Style {
        Style::default().fg(Self::info()).bg(Self::bg_card())
    }
    pub fn diff_removed() -> Style {
        Style::default().fg(Self::err()).bg(Self::bg_card())
    }
    pub fn diff_added() -> Style {
        Style::default().fg(Self::ok()).bg(Self::bg_card())
    }
    pub fn diff_context() -> Style {
        Style::default().fg(Self::dim()).bg(Self::bg_card())
    }
    pub fn diff_removed_hl() -> Style {
        Style::default()
            .fg(Self::bright())
            .bg(Self::err())
            .add_modifier(Modifier::BOLD)
    }
    pub fn diff_added_hl() -> Style {
        Style::default()
            .fg(Self::bright())
            .bg(Self::ok())
            .add_modifier(Modifier::BOLD)
    }

    // -- connection indicator --
    pub fn conn_ok() -> Style {
        Style::default().fg(Self::ok()).bg(Self::bg_dim())
    }
    pub fn conn_pending() -> Style {
        Style::default().fg(Self::warn()).bg(Self::bg_dim())
    }
    pub fn conn_err() -> Style {
        Style::default().fg(Self::err()).bg(Self::bg_dim())
    }

    // -- theme list helpers --
    pub fn available_themes() -> &'static [Base16Palette] {
        DARK_THEMES
    }

    /// Snapshot the global theme index into a thread-local cache.
    /// Call once at frame start to avoid repeated atomic loads.
    pub fn begin_frame() {
        FRAME_THEME_IDX.set(THEME_IDX.load(std::sync::atomic::Ordering::Relaxed));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn begin_frame_snapshots_theme_index() {
        // Set theme to index 2
        Theme::set_by_index(2);
        Theme::begin_frame();

        // palette() should use the snapshotted index
        let p1 = Theme::palette();
        assert_eq!(p1.id, DARK_THEMES[2].id);

        // Change the atomic — palette() should still return the snapshot
        Theme::set_by_index(5);
        let p2 = Theme::palette();
        assert_eq!(
            p2.id, DARK_THEMES[2].id,
            "palette() should read from frame snapshot, not atomic"
        );

        // After begin_frame, it should pick up the new index
        Theme::begin_frame();
        let p3 = Theme::palette();
        assert_eq!(p3.id, DARK_THEMES[5].id);

        // Reset
        Theme::set_by_index(0);
        Theme::begin_frame();
    }

    #[test]
    fn styles_use_frame_snapshot() {
        Theme::set_by_index(3);
        Theme::begin_frame();

        let style_a = Theme::base();

        // Change theme atomically but don't call begin_frame
        Theme::set_by_index(7);

        let style_b = Theme::base();

        // Both should be identical — reading from the same snapshot
        assert_eq!(style_a.fg, style_b.fg);
        assert_eq!(style_a.bg, style_b.bg);

        // Reset
        Theme::set_by_index(0);
        Theme::begin_frame();
    }
}
