//! FORK: hover card of the GRAPH rows, like VS Code's source control history
//! hover (design doc 9, "GRAPH 悬浮卡").
//!
//! Upstream tooltips always open next to the mouse, so the card is anchored to
//! the hovered row instead and its show/hide delays are driven from here.

use super::*;
use git::repository::CommitShortStat;
use gpui::{ScrollHandle, point};
use markdown::{Markdown, MarkdownElement, MarkdownStyle};
use time::UtcOffset;
use ui::{CopyButton, ScrollAxes, Scrollbars, WithScrollbar};

const SHOW_DELAY: Duration = Duration::from_millis(500);
const SWITCH_DELAY: Duration = Duration::from_millis(100);
const HIDE_DELAY: Duration = Duration::from_millis(300);
const STAT_CACHE_LIMIT: usize = 256;
const CARD_GAP: Pixels = px(4.);
const CARD_MAX_WIDTH: Rems = rems(32.);
const CARD_MAX_HEIGHT_FRACTION: f32 = 0.6;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct HoverTarget {
    pub index: usize,
    pub sha: Oid,
}

enum CommitStat {
    Loading { _task: Task<()> },
    Ready(CommitShortStat),
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CardStat {
    Loading,
    Ready(CommitShortStat),
    Unavailable,
}

#[derive(Default)]
pub(super) struct CommitHoverState {
    /// The row the card is for, or will be for once the show delay passes.
    target: Option<HoverTarget>,
    visible: bool,
    /// The row under the mouse, which can differ from `target` while the card
    /// waits to switch rows.
    hovered_row: Option<HoverTarget>,
    card_hovered: bool,
    /// A text selection is being dragged from the card; the mouse may leave it
    /// meanwhile, so hiding and switching wait for the button to be released.
    card_pressed: bool,
    timer: Option<Task<()>>,
    stats: HashMap<Oid, CommitStat>,
    scroll_handle: ScrollHandle,
    /// Kept across renders so a text selection survives them.
    texts: Option<CardTexts>,
}

/// The card's selectable texts. Markdown elements are what lets text be
/// selected and copied in Zed.
#[derive(Clone)]
pub(super) struct CardTexts {
    sha: Oid,
    author: Entity<Markdown>,
    date: Entity<Markdown>,
    message: Entity<Markdown>,
    stat: Option<StatTexts>,
}

#[derive(Clone)]
struct StatTexts {
    files: Entity<Markdown>,
    insertions: Option<Entity<Markdown>>,
    deletions: Option<Entity<Markdown>>,
}

impl CommitHoverState {
    pub(super) fn visible_target(&self) -> Option<HoverTarget> {
        self.target.filter(|_| self.visible)
    }

    fn hide(&mut self) -> bool {
        let was_visible = self.visible;
        self.target = None;
        self.visible = false;
        self.card_hovered = false;
        self.card_pressed = false;
        self.timer = None;
        self.texts = None;
        was_visible
    }

    pub(super) fn clear(&mut self) {
        self.hide();
        self.hovered_row = None;
        self.stats.clear();
    }

    pub(super) fn clear_stats(&mut self) {
        self.stats.clear();
    }

    fn card_stat(&self, sha: Oid) -> CardStat {
        match self.stats.get(&sha) {
            None | Some(CommitStat::Loading { .. }) => CardStat::Loading,
            Some(CommitStat::Ready(stat)) => CardStat::Ready(*stat),
            Some(CommitStat::Unavailable) => CardStat::Unavailable,
        }
    }
}

impl GitPanel {
    pub(super) fn fork_commit_row_hover_changed(
        &mut self,
        target: HoverTarget,
        hovered: bool,
        cx: &mut Context<Self>,
    ) {
        let hover = &mut self.fork_layout.commit_hover;
        if hovered {
            hover.hovered_row = Some(target);
            if hover.visible {
                if hover.target == Some(target) {
                    hover.timer = None;
                } else if !hover.card_hovered {
                    self.fork_schedule_commit_hover(SWITCH_DELAY, Self::fork_show_hovered_row, cx);
                }
            } else {
                hover.target = Some(target);
                self.fork_schedule_commit_hover(SHOW_DELAY, Self::fork_show_hovered_row, cx);
            }
            // Started before the card shows, so it is usually ready by then.
            self.fork_request_commit_stat(target.sha, cx);
            return;
        }

        if hover.hovered_row == Some(target) {
            hover.hovered_row = None;
        }
        // Moving between rows can report the new row before the old one.
        if hover.hovered_row.is_some() {
            return;
        }
        if !hover.visible {
            hover.target = None;
            hover.timer = None;
        } else if !hover.card_hovered {
            self.fork_schedule_commit_hover(HIDE_DELAY, Self::fork_hide_unless_hovered, cx);
        }
    }

    pub(super) fn fork_commit_card_hover_changed(&mut self, hovered: bool, cx: &mut Context<Self>) {
        let hover = &mut self.fork_layout.commit_hover;
        if !hover.visible {
            return;
        }
        hover.card_hovered = hovered;
        if hovered {
            hover.timer = None;
            return;
        }
        self.fork_commit_card_left(cx);
    }

    pub(super) fn fork_commit_card_pressed(&mut self, pressed: bool, cx: &mut Context<Self>) {
        let hover = &mut self.fork_layout.commit_hover;
        if !hover.visible || hover.card_pressed == pressed {
            return;
        }
        hover.card_pressed = pressed;
        if pressed {
            hover.timer = None;
        } else if !hover.card_hovered {
            self.fork_commit_card_left(cx);
        }
    }

    fn fork_commit_card_left(&mut self, cx: &mut Context<Self>) {
        let hover = &self.fork_layout.commit_hover;
        match hover.hovered_row {
            None => self.fork_schedule_commit_hover(HIDE_DELAY, Self::fork_hide_unless_hovered, cx),
            Some(row) if hover.target != Some(row) => {
                self.fork_schedule_commit_hover(SWITCH_DELAY, Self::fork_show_hovered_row, cx)
            }
            Some(_) => {}
        }
    }

    pub(super) fn fork_hide_commit_hover(&mut self, cx: &mut Context<Self>) {
        if self.fork_layout.commit_hover.hide() {
            cx.notify();
        }
    }

    /// Called before rendering: a reload can drop or move the hovered commit.
    pub(super) fn fork_validate_commit_hover(&mut self) {
        let Some(target) = self.fork_layout.commit_hover.target else {
            return;
        };
        if !self.fork_commit_is_listed(target) || self.context_menu.is_some() {
            self.fork_layout.commit_hover.hide();
        }
    }

    fn fork_commit_is_listed(&self, target: HoverTarget) -> bool {
        matches!(
            &self.commit_history,
            CommitHistory::Loaded(entries)
                if entries.get(target.index).is_some_and(|entry| entry.sha == target.sha)
        )
    }

    fn fork_schedule_commit_hover(
        &mut self,
        delay: Duration,
        action: fn(&mut Self, &mut Context<Self>),
        cx: &mut Context<Self>,
    ) {
        self.fork_layout.commit_hover.timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            this.update(cx, |this, cx| action(this, cx)).ok();
        }));
    }

    fn fork_show_hovered_row(&mut self, cx: &mut Context<Self>) {
        let hover = &self.fork_layout.commit_hover;
        let Some(row) = hover.hovered_row else {
            return;
        };
        if hover.card_hovered
            || hover.card_pressed
            || hover.visible_target() == Some(row)
            || self.context_menu.is_some()
            || !self.fork_commit_is_listed(row)
        {
            return;
        }
        let hover = &mut self.fork_layout.commit_hover;
        hover.target = Some(row);
        hover.visible = true;
        hover.scroll_handle = ScrollHandle::new();
        self.fork_request_commit_stat(row.sha, cx);
        cx.notify();
    }

    fn fork_hide_unless_hovered(&mut self, cx: &mut Context<Self>) {
        let hover = &self.fork_layout.commit_hover;
        if hover.card_hovered || hover.card_pressed || hover.hovered_row.is_some() {
            return;
        }
        self.fork_hide_commit_hover(cx);
    }

    /// Needs the commit's parent, so it does nothing until the commit data is
    /// loaded; showing the card asks again.
    fn fork_request_commit_stat(&mut self, sha: Oid, cx: &mut Context<Self>) {
        if self.fork_layout.commit_hover.stats.contains_key(&sha) {
            return;
        }
        let Some(repository) = self.active_repository.clone() else {
            return;
        };
        let stat = if self.project.read(cx).is_local() {
            let parent = repository.update(cx, |repository, cx| {
                match repository.fetch_commit_data(sha, false, cx) {
                    CommitDataState::Loaded(data) => Some(data.parents.first().copied()),
                    CommitDataState::Loading(_) => None,
                }
            });
            let Some(parent) = parent else {
                return;
            };
            let task = repository.read(cx).fork_commit_short_stat(sha, parent, cx);
            CommitStat::Loading {
                _task: cx.spawn(async move |this, cx| {
                    let stat = match task.await.log_err() {
                        Some(stat) => CommitStat::Ready(stat),
                        None => CommitStat::Unavailable,
                    };
                    this.update(cx, |this, cx| {
                        let stats = &mut this.fork_layout.commit_hover.stats;
                        if let Some(entry) = stats.get_mut(&sha)
                            && matches!(entry, CommitStat::Loading { .. })
                        {
                            *entry = stat;
                            cx.notify();
                        }
                    })
                    .ok();
                }),
            }
        } else {
            CommitStat::Unavailable
        };
        let stats = &mut self.fork_layout.commit_hover.stats;
        if stats.len() >= STAT_CACHE_LIMIT {
            stats.clear();
        }
        stats.insert(sha, stat);
    }

    /// Called before rendering: builds the card's texts once per shown commit,
    /// and the statistics line once they arrive.
    pub(super) fn fork_sync_commit_hover_texts(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.fork_layout.commit_hover.visible_target() else {
            self.fork_layout.commit_hover.texts = None;
            return;
        };
        let Some(repository) = self.active_repository.clone() else {
            return;
        };
        let Some(data) = repository.update(cx, |repository, cx| {
            match repository.fetch_commit_data(target.sha, false, cx) {
                CommitDataState::Loaded(data) => Some(data.clone()),
                CommitDataState::Loading(_) => None,
            }
        }) else {
            return;
        };
        let stat = self.fork_layout.commit_hover.card_stat(target.sha);
        let texts = &mut self.fork_layout.commit_hover.texts;
        if texts.as_ref().is_none_or(|texts| texts.sha != target.sha) {
            *texts = Some(CardTexts {
                sha: target.sha,
                author: plain_text(data.author_name.clone(), cx),
                date: plain_text(commit_date(&data, cx), cx),
                message: plain_text(message_paragraphs(&data.message).join("\n\n"), cx),
                stat: None,
            });
        }
        if let Some(texts) = texts
            && texts.stat.is_none()
            && let CardStat::Ready(stat) = stat
        {
            let parts = stat_parts(stat);
            texts.stat = Some(StatTexts {
                files: plain_text(parts.files, cx),
                insertions: parts.insertions.map(|text| plain_text(text, cx)),
                deletions: parts.deletions.map(|text| plain_text(text, cx)),
            });
        }
    }

    /// The card for the row it is shown for, ready to be placed in that row.
    pub(super) fn fork_commit_hover_card(
        &self,
        refs: Vec<SharedString>,
        badges: CardBadges,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<(usize, CommitHoverCard)> {
        let target = self.fork_layout.commit_hover.visible_target()?;
        let texts = self
            .fork_layout
            .commit_hover
            .texts
            .clone()
            .filter(|texts| texts.sha == target.sha)?;
        let repository = self.active_repository.clone()?;
        let data = repository.update(cx, |repository, cx| {
            match repository.fetch_commit_data(target.sha, false, cx) {
                CommitDataState::Loaded(data) => Some(data.clone()),
                CommitDataState::Loading(_) => None,
            }
        })?;
        let remote = repository.read(cx).default_remote_url().and_then(|url| {
            let registry = git::GitHostingProviderRegistry::default_global(cx);
            let (provider, parsed) = git::parse_git_remote_url(registry, &url)?;
            Some(git::GitRemote {
                host: provider,
                owner: parsed.owner.into(),
                repo: parsed.repo.into(),
            })
        });
        Some((
            target.index,
            CommitHoverCard {
                data,
                texts,
                stat: self.fork_layout.commit_hover.card_stat(target.sha),
                refs,
                badges,
                remote,
                dock: self.position(window, cx),
                scroll_handle: self.fork_layout.commit_hover.scroll_handle.clone(),
                git_panel: cx.weak_entity(),
                repository: repository.downgrade(),
                workspace: self.workspace.clone(),
            },
        ))
    }
}

/// How the ref badges of the hovered row are drawn, shared with the row.
#[derive(Clone)]
pub(super) struct CardBadges {
    pub head_branch: Option<SharedString>,
    pub remote_names: Vec<SharedString>,
    pub filter: ForkGitPanelLayoutSettings,
    pub color: Hsla,
    /// Width of the row's graph column, so a card below the panel lines up
    /// with the commit subjects.
    pub graph_width: Pixels,
}

#[derive(Clone, IntoElement)]
pub(super) struct CommitHoverCard {
    data: Arc<CommitData>,
    texts: CardTexts,
    stat: CardStat,
    refs: Vec<SharedString>,
    badges: CardBadges,
    remote: Option<git::GitRemote>,
    dock: DockPosition,
    scroll_handle: ScrollHandle,
    git_panel: WeakEntity<GitPanel>,
    repository: WeakEntity<Repository>,
    workspace: WeakEntity<Workspace>,
}

impl CommitHoverCard {
    /// A zero-sized point at the edge of the row the card hangs from. The card
    /// is deferred so the panel and list don't clip it.
    pub(super) fn into_anchor(self) -> AnyElement {
        let dock = self.dock;
        let graph_width = self.badges.graph_width;
        let (anchor, offset) = match dock {
            DockPosition::Left => (Anchor::TopLeft, point(CARD_GAP, px(0.))),
            DockPosition::Right => (Anchor::TopRight, point(-CARD_GAP, px(0.))),
            DockPosition::Bottom => (Anchor::BottomLeft, point(px(0.), -CARD_GAP)),
        };
        div()
            .absolute()
            .size_0()
            .map(|this| match dock {
                DockPosition::Left => this.top_0().left_full(),
                DockPosition::Right => this.top_0().left_0(),
                DockPosition::Bottom => this.top_0().left(graph_width),
            })
            .child(
                deferred(
                    anchored()
                        .anchor(anchor)
                        .offset(offset)
                        .snap_to_window_with_margin(px(8.))
                        .child(self),
                )
                .with_priority(1),
            )
            .into_any_element()
    }
}

impl RenderOnce for CommitHoverCard {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let data = &self.data;
        let sha: SharedString = data.sha.to_string().into();
        let short_sha: SharedString = data.sha.display_short().into();
        let author_email = (!data.author_email.is_empty()).then(|| data.author_email.clone());
        let avatar = CommitAvatar::new(&sha, author_email, self.remote.as_ref())
            .size(px(16.))
            .render(window, cx);
        let texts = &self.texts;
        let text = |markdown: &Entity<Markdown>, color: Hsla| {
            MarkdownElement::new(markdown.clone(), plain_text_style(color, window, cx))
        };

        let divider = || div().mx_2().h_px().bg(colors.border_variant);
        let section = || v_flex().px_2().py_1p5();

        let header = h_flex()
            .gap_1()
            .flex_wrap()
            .child(avatar)
            .child(
                h_flex()
                    .child(text(&texts.author, colors.text_accent))
                    .child(Label::new(",")),
            )
            .child(
                Icon::new(IconName::Clock)
                    .size(IconSize::Small)
                    .color(Color::Muted),
            )
            .child(text(&texts.date, colors.text_muted));

        let message = text(&texts.message, colors.text);

        let stat = match self.stat {
            CardStat::Loading => Some(
                Label::new("Loading…")
                    .color(Color::Muted)
                    .into_any_element(),
            ),
            CardStat::Ready(_) => texts.stat.as_ref().map(|stat| {
                h_flex()
                    .flex_wrap()
                    .child(text(&stat.files, colors.text))
                    .when_some(stat.insertions.as_ref(), |this, insertions| {
                        this.child(text(insertions, colors.version_control_added))
                    })
                    .when_some(stat.deletions.as_ref(), |this, deletions| {
                        this.child(text(deletions, colors.version_control_deleted))
                    })
                    .into_any_element()
            }),
            CardStat::Unavailable => None,
        };

        let badges = render_ref_badges(
            &self.refs,
            self.badges.head_branch.as_ref(),
            &self.badges.remote_names,
            self.badges.filter,
            false,
            self.badges.color,
            usize::MAX,
            cx,
        );
        let has_badges =
            has_visible_refs(&self.refs, &self.badges.remote_names, self.badges.filter);

        let footer = h_flex()
            .px_1()
            .py_0p5()
            .gap_0p5()
            .bg(colors.element_background)
            .child(
                Button::new("fork-commit-hover-sha", short_sha)
                    .color(Color::Accent)
                    .start_icon(
                        Icon::new(IconName::GitCommit)
                            .size(IconSize::Small)
                            .color(Color::Accent),
                    )
                    .on_click({
                        let git_panel = self.git_panel.clone();
                        let repository = self.repository.clone();
                        let workspace = self.workspace.clone();
                        let sha = sha.clone();
                        move |_, window, cx| {
                            git_panel
                                .update(cx, |panel, cx| panel.fork_hide_commit_hover(cx))
                                .ok();
                            CommitView::open(
                                sha.to_string(),
                                repository.clone(),
                                workspace.clone(),
                                Default::default(),
                                window,
                                cx,
                            );
                            cx.stop_propagation();
                        }
                    }),
            )
            .child(
                CopyButton::new("fork-commit-hover-copy-sha", sha.clone())
                    .tooltip_label("Copy Commit SHA"),
            );

        let max_height = window.viewport_size().height * CARD_MAX_HEIGHT_FRACTION;
        let body = div()
            .id("fork-commit-hover-body")
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.scroll_handle)
            .child(
                v_flex()
                    .child(section().gap_2().child(header).child(message))
                    .when_some(stat, |this, stat| {
                        this.child(divider()).child(section().child(stat))
                    })
                    .when(has_badges, |this| {
                        this.child(divider()).child(section().child(badges))
                    }),
            )
            .custom_scrollbars(
                Scrollbars::always_visible(ScrollAxes::Vertical)
                    .tracked_scroll_handle(&self.scroll_handle),
                window,
                cx,
            );

        let git_panel = self.git_panel;
        v_flex()
            .id("fork-commit-hover-card")
            .debug_selector(|| "fork-commit-hover-card".into())
            .occlude()
            .elevation_2(cx)
            .overflow_hidden()
            .font(ThemeSettings::get_global(cx).ui_font.clone())
            .text_ui(cx)
            .max_w(CARD_MAX_WIDTH)
            .max_h(max_height)
            .on_hover({
                let git_panel = git_panel.clone();
                move |hovered, _, cx| {
                    git_panel
                        .update(cx, |panel, cx| {
                            panel.fork_commit_card_hover_changed(*hovered, cx)
                        })
                        .ok();
                }
            })
            .on_mouse_down(MouseButton::Left, {
                let git_panel = git_panel.clone();
                move |_, _, cx| {
                    git_panel
                        .update(cx, |panel, cx| panel.fork_commit_card_pressed(true, cx))
                        .ok();
                    cx.stop_propagation();
                }
            })
            .on_mouse_up(MouseButton::Left, {
                let git_panel = git_panel.clone();
                move |_, _, cx| {
                    git_panel
                        .update(cx, |panel, cx| panel.fork_commit_card_pressed(false, cx))
                        .ok();
                }
            })
            .on_mouse_up_out(MouseButton::Left, {
                move |_, _, cx| {
                    git_panel
                        .update(cx, |panel, cx| panel.fork_commit_card_pressed(false, cx))
                        .ok();
                }
            })
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .on_scroll(|_, _, cx| cx.stop_propagation())
            .child(body)
            .child(div().h_px().bg(colors.border_variant))
            .child(footer)
    }
}

fn plain_text(text: impl Into<SharedString>, cx: &mut Context<GitPanel>) -> Entity<Markdown> {
    let text = text.into();
    cx.new(|cx| Markdown::new_text(text, cx))
}

fn plain_text_style(color: Hsla, window: &Window, cx: &App) -> MarkdownStyle {
    let settings = ThemeSettings::get_global(cx);
    let mut base_text_style = window.text_style();
    base_text_style.color = color;
    base_text_style.font_family = settings.ui_font.family.clone();
    base_text_style.font_features = settings.ui_font.features.clone();
    base_text_style.font_fallbacks = settings.ui_font.fallbacks.clone();
    base_text_style.font_weight = settings.ui_font.weight;
    base_text_style.font_size = settings.ui_font_size(cx).into();
    MarkdownStyle {
        base_text_style,
        selection_background_color: cx.theme().colors().element_selection_background,
        ..Default::default()
    }
}

/// `2 hours ago (November 18, 2026 at 11:27 AM)`, author time in the local
/// time zone.
fn commit_date(data: &CommitData, cx: &App) -> String {
    let Some(timestamp) = OffsetDateTime::from_unix_timestamp(data.commit_timestamp).ok() else {
        return String::new();
    };
    let relative =
        crate::git_timestamp::format_git_date(timestamp, settings::GitDateStyle::Relative, cx);
    let absolute = absolute_timestamp(
        timestamp,
        UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC),
    );
    format!("{relative} ({absolute})")
}

fn has_visible_refs(
    refs: &[SharedString],
    remote_names: &[SharedString],
    filter: ForkGitPanelLayoutSettings,
) -> bool {
    refs.iter().any(|decoration| {
        if decoration.as_ref() == "HEAD" {
            false
        } else if decoration.starts_with("tag: ") {
            filter.show_tags
        } else if remote_names.iter().any(|remote| {
            decoration
                .strip_prefix(remote.as_ref())
                .is_some_and(|rest| rest.starts_with('/'))
        }) {
            filter.show_remote_branches
        } else {
            true
        }
    })
}

/// Paragraphs of the message as written: blank lines separate them and the
/// line breaks inside one are kept.
fn message_paragraphs(message: &str) -> Vec<String> {
    let mut paragraphs = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    for line in message.trim().lines() {
        let line = line.trim_end();
        if line.is_empty() {
            if !current.is_empty() {
                paragraphs.push(current.join("\n"));
                current.clear();
            }
        } else {
            current.push(line);
        }
    }
    if !current.is_empty() {
        paragraphs.push(current.join("\n"));
    }
    paragraphs
}

#[derive(Debug, PartialEq, Eq)]
struct StatParts {
    files: String,
    insertions: Option<String>,
    deletions: Option<String>,
}

/// VS Code's wording: singular for one, and a zero count drops its part.
fn stat_parts(stat: CommitShortStat) -> StatParts {
    let plural = |count: usize, singular: &str, plural: &str| {
        if count == 1 {
            singular.to_string()
        } else {
            plural.to_string()
        }
    };
    StatParts {
        files: format!(
            "{} {} changed",
            stat.files,
            plural(stat.files, "file", "files")
        ),
        insertions: (stat.insertions > 0).then(|| {
            format!(
                ", {} {}(+)",
                stat.insertions,
                plural(stat.insertions, "insertion", "insertions")
            )
        }),
        deletions: (stat.deletions > 0).then(|| {
            format!(
                ", {} {}(-)",
                stat.deletions,
                plural(stat.deletions, "deletion", "deletions")
            )
        }),
    }
}

/// VS Code's English `toLocaleString` with a long month:
/// `November 18, 2026 at 11:27 AM`.
fn absolute_timestamp(timestamp: OffsetDateTime, offset: UtcOffset) -> String {
    let timestamp = timestamp.to_offset(offset);
    let hour = timestamp.hour();
    format!(
        "{} {}, {} at {}:{:02} {}",
        timestamp.month(),
        timestamp.day(),
        timestamp.year(),
        if hour.is_multiple_of(12) {
            12
        } else {
            hour % 12
        },
        timestamp.minute(),
        if hour < 12 { "AM" } else { "PM" },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::{datetime, offset};

    #[test]
    fn stat_wording() {
        let stat = |files, insertions, deletions| {
            stat_parts(CommitShortStat {
                files,
                insertions,
                deletions,
            })
        };
        assert_eq!(
            stat(6, 52, 46),
            StatParts {
                files: "6 files changed".into(),
                insertions: Some(", 52 insertions(+)".into()),
                deletions: Some(", 46 deletions(-)".into()),
            }
        );
        assert_eq!(
            stat(1, 1, 0),
            StatParts {
                files: "1 file changed".into(),
                insertions: Some(", 1 insertion(+)".into()),
                deletions: None,
            }
        );
        assert_eq!(
            stat(2, 0, 1),
            StatParts {
                files: "2 files changed".into(),
                insertions: None,
                deletions: Some(", 1 deletion(-)".into()),
            }
        );
        assert_eq!(
            stat(0, 0, 0),
            StatParts {
                files: "0 files changed".into(),
                insertions: None,
                deletions: None,
            }
        );
    }

    #[test]
    fn absolute_dates() {
        assert_eq!(
            absolute_timestamp(datetime!(2026-11-18 03:27 UTC), offset!(+8)),
            "November 18, 2026 at 11:27 AM"
        );
        assert_eq!(
            absolute_timestamp(datetime!(2026-11-18 04:05 UTC), offset!(+8)),
            "November 18, 2026 at 12:05 PM"
        );
        assert_eq!(
            absolute_timestamp(datetime!(2026-11-18 16:00 UTC), offset!(+8)),
            "November 19, 2026 at 12:00 AM"
        );
        assert_eq!(
            absolute_timestamp(datetime!(2026-01-02 13:09 UTC), offset!(+0)),
            "January 2, 2026 at 1:09 PM"
        );
    }

    #[test]
    fn paragraphs_keep_line_breaks() {
        assert_eq!(
            message_paragraphs("fix: subject\n\nbody line one\nline two\n\n\n- item\n"),
            vec!["fix: subject", "body line one\nline two", "- item"]
        );
    }
}
