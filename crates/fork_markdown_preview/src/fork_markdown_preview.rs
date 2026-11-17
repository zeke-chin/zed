//! Vditor (`.vditor-reset`) typography for the markdown preview.
//!
//! cweijan/vscode-office renders markdown with [Vditor][vditor], a browser
//! WYSIWYG editor whose look comes from two stylesheets: `_markdown.scss` for
//! the structure and `content-theme/{light,dark}.css` for the colors. This
//! crate translates both into [`MarkdownStyle`], so the preview can look like
//! that editor without a browser or a webview.
//!
//! Enable it with `fork.markdown_preview.style = "vditor"`. The default
//! `"zed"` keeps the upstream preview untouched.
//!
//! What a browser gets for free cannot be reproduced by styles alone: markdown-it
//! parses more syntax than pulldown-cmark, and inline code padding has no room
//! in a text layout (see `paint_code_chips`). Everything reachable through
//! `MarkdownStyle` is translated here.
//!
//! [vditor]: https://github.com/Vanessa219/vditor

use gpui::{AbsoluteLength, App, Hsla, Pixels, Rems, Window, point, px, rems};
use markdown::{
    CodeBlockLayoutStyle, HeadingLayoutStyle, InlineCodeLayoutStyle, ListBullet, ListLayoutStyle,
    MarkdownFont, MarkdownStyle, TableLayoutStyle,
};
use settings::{ForkMarkdownPreviewStyle, RegisterSetting, Settings, SettingsContent};
use theme::{ActiveTheme, Appearance, ThemeColors};

/// `body { font-size: 16px; line-height: 1.5 }`
const BASE_FONT_SIZE: Rems = rems(1.);
const BASE_LINE_HEIGHT: f32 = 1.5;

/// `.vditor-reset p { margin-bottom: 16px }`
const PARAGRAPH_SPACING: Rems = rems(1.);
/// `.vditor-reset ul, .vditor-reset ol { margin-bottom: 16px }`
const LIST_SPACING: Rems = rems(1.);
/// `.vditor-reset li + li { margin-top: 0.25em }`
const LIST_ITEM_SPACING: Rems = rems(0.25);
/// `.vditor-reset ul { padding-left: 2em }`
const LIST_INDENT: Rems = rems(2.);

/// `.vditor-reset h1..h6 { margin-top: 24px; margin-bottom: 16px }`
const HEADING_MARGIN_TOP: Rems = rems(1.5);
const HEADING_MARGIN_BOTTOM: Rems = rems(1.);
/// `.vditor-reset h1, .vditor-reset h2 { padding-bottom: 0.3em }`
const HEADING_BORDER_PADDING: Rems = rems(0.3);/// `h1..h6 { font-size: 1.75 / 1.55 / 1.38 / 1.25 / 1.13 / 1em }`
const HEADING_FONT_SIZES: [Rems; 6] = [
    rems(1.75),
    rems(1.55),
    rems(1.38),
    rems(1.25),
    rems(1.13),
    rems(1.),
];
const HEADING_FONT_WEIGHT: f32 = 600.;
const HEADING_LINE_HEIGHT: f32 = 1.25;

/// `.vditor-reset table td, .vditor-reset table th { padding: 6px 13px }`
const TABLE_CELL_PADDING: (Pixels, Pixels) = (px(13.), px(6.));
/// The table frame is drawn by cells; Vditor has no outer border or rounding.
const TABLE_CELL_BORDER_WIDTH: Pixels = px(1.);

/// `pre > code { padding: 0.5em; border-radius: 5px }`
const CODE_BLOCK_PADDING: Pixels = px(8.);
const CODE_BLOCK_CORNER_RADIUS: Pixels = px(5.);
const CODE_BLOCK_MARGIN_TOP: Rems = rems(1.);
const CODE_BLOCK_MARGIN_BOTTOM: Rems = rems(1.);
/// `code { font-size: 85% }`
const CODE_FONT_SIZE: Rems = rems(0.85);

/// `code { border-radius: 3px }`
const INLINE_CODE_CORNER_RADIUS: Pixels = px(3.);

/// `blockquote { border-left: 0.25em solid ... }`
const BLOCK_QUOTE_BORDER: Pixels = px(4.);

/// `.vditor-reset hr { height: 2px }` -- drawn as a 2px bottom border.
const RULE_HEIGHT: Pixels = px(2.);

#[derive(Clone, Copy, Debug, Default, PartialEq, RegisterSetting)]
pub struct ForkMarkdownPreviewSettings {
    pub style: ForkMarkdownPreviewStyle,
}

impl Settings for ForkMarkdownPreviewSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        Self {
            style: content
                .fork
                .as_ref()
                .and_then(|fork| fork.markdown_preview.as_ref())
                .and_then(|markdown_preview| markdown_preview.style)
                .unwrap_or_default(),
        }
    }
}

impl ForkMarkdownPreviewSettings {
    /// Whether the preview should render with Vditor's typography.
    pub fn uses_vditor_style(&self) -> bool {
        self.style == ForkMarkdownPreviewStyle::Vditor
    }
}

/// `.vditor-reset --anchor` uses `em`, which tracks the base font size; the
/// preview's rem size is that same base, so rems convert directly.
const BASE_REM_SIZE: Pixels = px(16.);

fn rems_to_definite(rems: Rems) -> gpui::DefiniteLength {
    gpui::DefiniteLength::Absolute(AbsoluteLength::Pixels(rems.to_pixels(BASE_REM_SIZE)))
}

/// Rewrites `style` in place to Vditor's typography. `dark` selects the dark
/// content theme's colors.
pub fn apply_vditor_style(style: &mut MarkdownStyle, colors: &ThemeColors, dark: bool) {
    let palette = Palette::new(dark, colors);

    apply_base(style, &palette, colors);
    apply_headings(style, &palette);
    apply_lists(style);
    apply_tables(style, &palette);
    apply_code(style, &palette, colors);
    apply_block_quotes(style, &palette);
    apply_rule(style, &palette);
}

/// Colors of the Vditor content theme. Values are taken from
/// `src/css/content-theme/{light,dark}.css`; the entries that would repeat the
/// user's own theme colors reuse them instead of hardcoding a light value.
struct Palette {
    text: Hsla,
    heading_border: Hsla,
    table_border: Hsla,
    table_header_background: Hsla,
    table_body_background: Hsla,
    table_stripe_background: Hsla,
    inline_code_background: Hsla,
    block_quote_border: Hsla,
    block_quote_text: Hsla,
    rule: Hsla,
    link: Hsla,
}

impl Palette {
    fn new(dark: bool, colors: &ThemeColors) -> Self {
        if dark {
            Self {
                // `.vditor-reset { color: #d1d5da }`
                text: colors.text,
                // `h1, h2 { border-bottom: 1px solid #d1d5da }`
                heading_border: colors.border,
                // `td, th { border: 1px solid #dfe2e5 }`
                table_border: colors.border,
                // `table tr { background-color: #2f363d }`
                table_header_background: colors.panel_background,
                // `table tbody tr:nth-child(2n) { background-color: #24292e }`
                table_body_background: colors.editor_background,
                table_stripe_background: colors.panel_background,
                // `code { background-color: rgba(66, 133, 244, .36) }` is too
                // vivid for a text chip, so the preview keeps its own shading.
                inline_code_background: colors
                    .editor_foreground
                    .opacity(0.12),
                // `--blockquote-border-color: rgba(230, 230, 230, 0.1)`
                block_quote_border: colors.border,
                block_quote_text: colors.text_muted,
                // `hr { background-color: #d1d5da }`
                rule: colors.border,
                // `.vditor-reset a { color: #4285f4 }`
                link: colors.text_accent,
            }
        } else {
            Self {
                // `.vditor-reset { color: #24292e }`
                text: colors.text,
                // `h1, h2 { border-bottom: 1px solid #eaecef }`
                heading_border: colors.border,
                // `td, th { border: 1px solid #dfe2e5 }`
                table_border: colors.border,
                // `table tr { background-color: #fafbfc }`
                table_header_background: colors.panel_background,
                // `table tbody tr:nth-child(2n) { background-color: #fff }`
                table_body_background: colors.editor_background,
                table_stripe_background: colors.panel_background,
                // `code { background-color: rgba(27, 31, 35, .05) }`
                inline_code_background: colors.editor_foreground.opacity(0.05),
                // `blockquote { border-left: 0.25em solid #eaecef }`
                block_quote_border: colors.border,
                block_quote_text: colors.text_muted,
                // `hr { background-color: #eaecef }`
                rule: colors.border,
                // Vditor only tints links in its dark theme; the light theme
                // leaves them at the inherited color. The preview keeps its
                // accent so links stay recognizable.
                link: colors.text_accent,
            }
        }
    }
}

fn apply_base(style: &mut MarkdownStyle, palette: &Palette, colors: &ThemeColors) {
    style.base_text_style.font_size = BASE_FONT_SIZE.into();
    style.base_text_style.color = palette.text;
    style.base_text_style.line_height = rems(BASE_LINE_HEIGHT).into();
    style.container_style.text.font_size = Some(BASE_FONT_SIZE.into());

    style.paragraph_spacing = PARAGRAPH_SPACING.to_pixels(px(16.));
    style.paragraph_line_height = rems(BASE_LINE_HEIGHT).into();
    style.selection_background_color = colors.element_selection_background;

    style.link = gpui::TextStyleRefinement {
        color: Some(palette.link),
        background_color: None,
        underline: None,
        ..Default::default()
    };

    style.inline_code_layout = InlineCodeLayoutStyle {
        corner_radius: Some(INLINE_CODE_CORNER_RADIUS),
    };
}

fn apply_headings(style: &mut MarkdownStyle, palette: &Palette) {
    style.heading.text.color = Some(palette.text);
    style.heading_level_styles = Some(markdown::HeadingLevelStyles {
        h1: Some(heading_text_style(HEADING_FONT_SIZES[0])),
        h2: Some(heading_text_style(HEADING_FONT_SIZES[1])),
        h3: Some(heading_text_style(HEADING_FONT_SIZES[2])),
        h4: Some(heading_text_style(HEADING_FONT_SIZES[3])),
        h5: Some(heading_text_style(HEADING_FONT_SIZES[4])),
        h6: Some(heading_text_style(HEADING_FONT_SIZES[5])),
    });
    // The border color is applied by `apply_heading_style`; the heading's own
    // margin comes from `heading_layout` so both Vditor values apply to every
    // level, including H1.
    style.heading.margin = Default::default();
    style.heading_border_color = Some(palette.heading_border);
    style.heading_layout = HeadingLayoutStyle {
        margin_top: Some(rems_to_definite(HEADING_MARGIN_TOP)),
        margin_bottom: Some(rems_to_definite(HEADING_MARGIN_BOTTOM)),
        border_padding: Some(rems_to_definite(HEADING_BORDER_PADDING)),
        border_levels: Some(vec![
            pulldown_cmark::HeadingLevel::H1,
            pulldown_cmark::HeadingLevel::H2,
        ]),
    };
}

fn heading_text_style(font_size: Rems) -> gpui::TextStyleRefinement {
    gpui::TextStyleRefinement {
        font_size: Some(font_size.into()),
        font_weight: Some(gpui::FontWeight(HEADING_FONT_WEIGHT)),
        line_height: Some(rems(HEADING_LINE_HEIGHT).into()),
        ..Default::default()
    }
}

fn apply_lists(style: &mut MarkdownStyle) {
    style.list_spacing = LIST_SPACING.to_pixels(px(16.));
    style.list_layout = ListLayoutStyle {
        indent: Some(rems_to_definite(LIST_INDENT)),
        bullets: Some(vec![
            ListBullet::Disc,
            ListBullet::Circle,
            ListBullet::Square,
        ]),
        // `.vditor-reset li p` keeps paragraphs in list items, so the item
        // itself adds no gap beyond the list's own margin.
        bullet_gap: px(4.),
        item_spacing: LIST_ITEM_SPACING.to_pixels(px(16.)),
    };
}

fn apply_tables(style: &mut MarkdownStyle, palette: &Palette) {
    style.table_cell_padding = point(TABLE_CELL_PADDING.0, TABLE_CELL_PADDING.1);
    style.table_layout = TableLayoutStyle {
        // Vditor's table is `border-collapse: collapse` with no outer frame;
        // the cells draw every line.
        outer_border_visible: false,
        outer_border_width: None,
        outer_border_color: None,
        outer_corner_radius: None,
        cell_border_width: Some(TABLE_CELL_BORDER_WIDTH),
        cell_border_color: Some(palette.table_border),
        header_background: Some(palette.table_header_background),
        body_stripe_background: Some(palette.table_stripe_background),
        body_background: Some(palette.table_body_background),
    };
}

fn apply_code(style: &mut MarkdownStyle, palette: &Palette, colors: &ThemeColors) {
    let code_text = style.code_block.text.clone();

    style.code_block.padding = gpui::EdgesRefinement {
        top: Some(gpui::DefiniteLength::Absolute(gpui::AbsoluteLength::Pixels(
            CODE_BLOCK_PADDING,
        ))),
        left: Some(gpui::DefiniteLength::Absolute(gpui::AbsoluteLength::Pixels(
            CODE_BLOCK_PADDING,
        ))),
        right: Some(gpui::DefiniteLength::Absolute(gpui::AbsoluteLength::Pixels(
            CODE_BLOCK_PADDING,
        ))),
        bottom: Some(gpui::DefiniteLength::Absolute(gpui::AbsoluteLength::Pixels(
            CODE_BLOCK_PADDING,
        ))),
    };
    style.code_block.margin = gpui::EdgesRefinement {
        top: Some(gpui::Length::Definite(CODE_BLOCK_MARGIN_TOP.into())),
        left: Some(gpui::Length::Definite(px(0.).into())),
        right: Some(gpui::Length::Definite(px(0.).into())),
        bottom: Some(gpui::Length::Definite(CODE_BLOCK_MARGIN_BOTTOM.into())),
    };
    style.code_block.border_widths = Default::default();
    style.code_block.background = Some(colors.editor_background.into());
    let radius = AbsoluteLength::Pixels(CODE_BLOCK_CORNER_RADIUS);
    style.code_block.corner_radii.top_left = Some(radius);
    style.code_block.corner_radii.top_right = Some(radius);
    style.code_block.corner_radii.bottom_left = Some(radius);
    style.code_block.corner_radii.bottom_right = Some(radius);
    style.code_block.text = code_text;

    style.inline_code = gpui::TextStyleRefinement {
        font_size: Some(CODE_FONT_SIZE.into()),
        background_color: Some(palette.inline_code_background),
        ..style.inline_code.clone()
    };
    style.code_block_layout = CodeBlockLayoutStyle {
        corner_radius: Some(CODE_BLOCK_CORNER_RADIUS),
    };
}

fn apply_block_quotes(style: &mut MarkdownStyle, palette: &Palette) {
    style.block_quote_border_color = palette.block_quote_border;
    style.block_quote = gpui::TextStyleRefinement {
        color: Some(palette.block_quote_text),
        ..Default::default()
    };
    style.block_quote_layout = markdown::BlockQuoteLayoutStyle {
        border_width: Some(BLOCK_QUOTE_BORDER),
    };
}

fn apply_rule(style: &mut MarkdownStyle, palette: &Palette) {
    style.rule_color = palette.rule;
    style.rule_height = Some(RULE_HEIGHT);
}

/// Convenience wrapper used by the preview: builds the upstream preview style
/// and, when enabled, converts it to Vditor's typography.
pub fn preview_markdown_style(
    preview_theme: Option<&std::sync::Arc<theme::Theme>>,
    window: &Window,
    cx: &App,
) -> MarkdownStyle {
    let mut style = match preview_theme {
        Some(theme) => MarkdownStyle::themed_with_overrides(
            MarkdownFont::Preview,
            theme.colors(),
            theme.syntax(),
            window,
            cx,
        ),
        None => MarkdownStyle::themed(MarkdownFont::Preview, window, cx),
    };

    if ForkMarkdownPreviewSettings::get_global(cx).uses_vditor_style() {
        let theme = preview_theme.cloned().unwrap_or_else(|| cx.theme().clone());
        apply_vditor_style(&mut style, theme.colors(), theme.appearance() == Appearance::Dark);
    }

    style
}

pub fn init(_cx: &mut App) {}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, UpdateGlobal};

    fn test_colors(cx: &mut TestAppContext) -> ThemeColors {
        cx.update(|cx| {
            if !cx.has_global::<settings::SettingsStore>() {
                settings::init(cx);
            }
            if !cx.has_global::<theme::GlobalTheme>() {
                theme_settings::init(theme::LoadThemes::JustBase, cx);
            }
            cx.theme().colors().clone()
        })
    }

    fn vditor_style(cx: &mut TestAppContext) -> MarkdownStyle {
        let colors = test_colors(cx);
        let mut style = MarkdownStyle::default();
        apply_vditor_style(&mut style, &colors, true);
        style
    }

    #[gpui::test]
    fn vditor_style_sets_every_heading_size(cx: &mut TestAppContext) {
        let style = vditor_style(cx);
        let levels = style
            .heading_level_styles
            .as_ref()
            .expect("headings should be styled");
        let sizes = [
            levels.h1.as_ref(),
            levels.h2.as_ref(),
            levels.h3.as_ref(),
            levels.h4.as_ref(),
            levels.h5.as_ref(),
            levels.h6.as_ref(),
        ];
        for (index, level) in sizes.into_iter().enumerate() {
            let level = level.expect("every heading level should be styled");
            assert_eq!(level.font_size, Some(HEADING_FONT_SIZES[index].into()));
            assert_eq!(
                level.font_weight,
                Some(gpui::FontWeight(HEADING_FONT_WEIGHT))
            );
        }
    }

    #[gpui::test]
    fn vditor_style_draws_heading_rule_on_first_two_levels_only(cx: &mut TestAppContext) {
        let style = vditor_style(cx);
        assert!(
            style
                .heading_layout
                .draws_border(pulldown_cmark::HeadingLevel::H1)
        );
        assert!(
            style
                .heading_layout
                .draws_border(pulldown_cmark::HeadingLevel::H2)
        );
        assert!(
            !style
                .heading_layout
                .draws_border(pulldown_cmark::HeadingLevel::H3)
        );
        assert!(style.heading_border_color.is_some());
    }

    #[gpui::test]
    fn vditor_style_cycles_list_bullets_by_depth(cx: &mut TestAppContext) {
        let style = vditor_style(cx);
        let bullets = style
            .list_layout
            .bullets
            .as_ref()
            .expect("bullets should be configured");
        assert_eq!(
            bullets,
            &vec![ListBullet::Disc, ListBullet::Circle, ListBullet::Square]
        );
        assert_eq!(style.list_layout.indent, Some(rems_to_definite(LIST_INDENT)));
    }

    #[gpui::test]
    fn vditor_style_drops_the_table_outer_frame(cx: &mut TestAppContext) {
        let style = vditor_style(cx);
        assert!(!style.table_layout.outer_border_visible);
        assert!(style.table_layout.cell_border_color.is_some());
        assert!(style.table_layout.header_background.is_some());
        assert!(style.table_layout.body_stripe_background.is_some());
        assert_eq!(
            style.table_cell_padding,
            point(TABLE_CELL_PADDING.0, TABLE_CELL_PADDING.1)
        );
    }

    #[gpui::test]
    fn vditor_style_sizes_code_and_rules(cx: &mut TestAppContext) {
        let style = vditor_style(cx);
        assert_eq!(
            style.code_block_layout.corner_radius,
            Some(CODE_BLOCK_CORNER_RADIUS)
        );
        assert_eq!(
            style.inline_code_layout.corner_radius,
            Some(INLINE_CODE_CORNER_RADIUS)
        );
        assert_eq!(
            style.block_quote_layout.border_width,
            Some(BLOCK_QUOTE_BORDER)
        );
        assert_eq!(style.rule_height, Some(RULE_HEIGHT));
    }

    #[gpui::test]
    fn setting_switches_the_preview_between_typographies(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
            settings::SettingsStore::update_global(cx, |store, cx| {
                store
                    .set_user_settings(r#"{"fork":{"markdown_preview":{"style":"vditor"}}}"#, cx)
                    .result()
                    .expect("settings should parse");
            });
        });

        let settings = cx.read(|cx| *ForkMarkdownPreviewSettings::get_global(cx));
        assert!(settings.uses_vditor_style());

        cx.update(|cx| {
            settings::SettingsStore::update_global(cx, |store, cx| {
                store
                    .set_user_settings(r#"{"fork":{"markdown_preview":{"style":"zed"}}}"#, cx)
                    .result()
                    .expect("settings should parse");
            });
        });

        let settings = cx.read(|cx| *ForkMarkdownPreviewSettings::get_global(cx));
        assert!(!settings.uses_vditor_style());
    }

    #[gpui::test]
    fn upstream_style_keeps_its_defaults(cx: &mut TestAppContext) {
        let colors = test_colors(cx);
        // `apply_vditor_style` must be the only source of these values: the
        // upstream preview leaves them unset.
        let upstream = MarkdownStyle::default();
        assert!(upstream.heading_border_color.is_none());
        assert!(upstream.list_layout.bullets.is_none());
        assert!(upstream.table_layout.cell_border_color.is_none());
        assert!(upstream.rule_height.is_none());

        let mut switched = MarkdownStyle::default();
        apply_vditor_style(&mut switched, &colors, false);
        assert!(switched.heading_border_color.is_some());
        assert!(switched.list_layout.bullets.is_some());
    }
}
