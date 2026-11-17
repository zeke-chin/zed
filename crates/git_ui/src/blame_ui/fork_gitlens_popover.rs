use super::*;
use anyhow::{Result, anyhow};
use gpui::{EntityId, Global};
use settings::{ForkBlamePopoverStyle, RegisterSetting, SettingsContent};
use std::{collections::VecDeque, ops::Range};
use time::UtcOffset;
use ui::{ScrollAxes, Scrollbars, WithScrollbar};
use util::ResultExt as _;

#[derive(Clone, Copy, Debug, PartialEq, RegisterSetting)]
pub(super) struct ForkBlamePopoverSettings {
    pub style: ForkBlamePopoverStyle,
}

impl settings::Settings for ForkBlamePopoverSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        Self {
            style: content
                .fork
                .as_ref()
                .and_then(|fork| fork.blame_popover.as_ref())
                .and_then(|popover| popover.style)
                .unwrap_or_default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DiffKey {
    repository: EntityId,
    sha: Oid,
    filename: String,
    // Merge commits can blame lines against different parents of the same file.
    previous: Option<(Oid, String)>,
}

impl DiffKey {
    fn new(repository: EntityId, blame: &BlameEntry) -> Self {
        Self {
            repository,
            sha: blame.sha,
            filename: blame.filename.clone(),
            previous: blame
                .previous_sha_and_filename()
                .map(|(sha, filename)| (sha, filename.to_owned())),
        }
    }

    fn revisions(&self) -> Vec<String> {
        let mut revisions = vec![format!("{}:{}", self.sha, self.filename)];
        if let Some((sha, filename)) = &self.previous {
            revisions.push(format!("{sha}:{filename}"));
        }
        revisions
    }
}

#[derive(Debug)]
struct DiffHunk {
    new_range: Range<u32>,
    old_lines: Vec<String>,
    new_lines: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LineDiff {
    deleted: Vec<String>,
    added: String,
}

fn build_hunks(old: &str, new: &str) -> Vec<DiffHunk> {
    let old_lines: Vec<_> = old.lines().collect();
    let new_lines: Vec<_> = new.lines().collect();
    language::line_diff(old, new)
        .into_iter()
        .filter_map(|(old_range, new_range)| {
            if new_range.is_empty() {
                return None;
            }
            Some(DiffHunk {
                old_lines: old_lines
                    .get(old_range.start as usize..old_range.end as usize)?
                    .iter()
                    .map(|line| line.replace('\t', "    "))
                    .collect(),
                new_lines: new_lines
                    .get(new_range.start as usize..new_range.end as usize)?
                    .iter()
                    .map(|line| line.replace('\t', "    "))
                    .collect(),
                new_range,
            })
        })
        .collect()
}

fn select_line(hunks: &[DiffHunk], original_line_number: u32) -> Option<LineDiff> {
    let row = original_line_number.checked_sub(1)?;
    let hunk = hunks.iter().find(|hunk| hunk.new_range.contains(&row))?;
    let offset = row.checked_sub(hunk.new_range.start)? as usize;
    let deleted = if hunk.old_lines.len() == hunk.new_lines.len() {
        vec![hunk.old_lines.get(offset)?.clone()]
    } else {
        hunk.old_lines.clone()
    };
    Some(LineDiff {
        deleted,
        added: hunk.new_lines.get(offset)?.clone(),
    })
}

#[derive(Debug)]
enum DiffState {
    Loading,
    Ready(Vec<DiffHunk>),
    Unavailable,
}

struct CachedDiff {
    key: DiffKey,
    generation: u64,
    state: DiffState,
}

#[derive(Default)]
struct DiffCache {
    entries: VecDeque<CachedDiff>,
    generation: u64,
}

impl Global for DiffCache {}

impl DiffCache {
    const CAPACITY: usize = 64;

    fn touch(&mut self, key: &DiffKey) -> Option<&DiffState> {
        let position = self.entries.iter().position(|entry| &entry.key == key)?;
        let entry = self.entries.remove(position)?;
        self.entries.push_back(entry);
        Some(&self.entries.back()?.state)
    }

    fn insert(&mut self, key: DiffKey, state: DiffState) -> u64 {
        if self.entries.len() >= Self::CAPACITY {
            self.entries.pop_front();
        }
        self.generation += 1;
        self.entries.push_back(CachedDiff {
            key,
            generation: self.generation,
            state,
        });
        self.generation
    }

    fn finish(&mut self, key: &DiffKey, generation: u64, state: DiffState) -> bool {
        let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| &entry.key == key && entry.generation == generation)
        else {
            return false;
        };
        entry.state = state;
        true
    }
}

async fn load_hunks(texts: gpui::Task<Result<Vec<Option<String>>>>) -> Result<Vec<DiffHunk>> {
    let mut texts = texts.await?.into_iter();
    let new = texts
        .next()
        .flatten()
        .ok_or_else(|| anyhow!("Blame hover revision is missing"))?;
    let old = texts.next().flatten().unwrap_or_default();
    Ok(build_hunks(&old, &new))
}

fn request_diff(
    blame: &BlameEntry,
    repository: &Entity<Repository>,
    cx: &mut App,
) -> Option<LineDiff> {
    if blame.sha.is_zero() {
        return None;
    }
    if !cx.has_global::<DiffCache>() {
        cx.set_global(DiffCache::default());
    }
    let key = DiffKey::new(repository.entity_id(), blame);
    if let Some(state) = cx.global_mut::<DiffCache>().touch(&key) {
        return match state {
            DiffState::Ready(hunks) => select_line(hunks, blame.original_line_number),
            DiffState::Loading | DiffState::Unavailable => None,
        };
    }
    if !repository
        .read(cx)
        .git_store()
        .is_some_and(|store| store.read(cx).is_local())
    {
        cx.global_mut::<DiffCache>()
            .insert(key, DiffState::Unavailable);
        return None;
    }
    let generation = cx
        .global_mut::<DiffCache>()
        .insert(key.clone(), DiffState::Loading);
    let texts = repository
        .read(cx)
        .fork_load_revision_texts(key.revisions(), cx);
    let task = cx.background_spawn(load_hunks(texts));
    cx.spawn(async move |cx| {
        let state = match task.await.log_err() {
            Some(hunks) => DiffState::Ready(hunks),
            None => DiffState::Unavailable,
        };
        cx.update(|cx| {
            if cx.global_mut::<DiffCache>().finish(&key, generation, state) {
                cx.refresh_windows();
            }
        });
    })
    .detach();
    None
}

fn ordinal_suffix(day: u8) -> &'static str {
    match day % 100 {
        11..=13 => "th",
        _ => match day % 10 {
            1 => "st",
            2 => "nd",
            3 => "rd",
            _ => "th",
        },
    }
}

fn absolute_timestamp(timestamp: OffsetDateTime, offset: UtcOffset) -> String {
    let timestamp = timestamp.to_offset(offset);
    let hour = timestamp.hour();
    format!(
        "{} {}{}, {} {}:{:02} {}",
        timestamp.month(),
        timestamp.day(),
        ordinal_suffix(timestamp.day()),
        timestamp.year(),
        if hour % 12 == 0 { 12 } else { hour % 12 },
        timestamp.minute(),
        if hour < 12 { "AM" } else { "PM" },
    )
}

fn commit_button(
    id: &'static str,
    sha: Oid,
    filename: Option<String>,
    repository: Entity<Repository>,
    workspace: WeakEntity<Workspace>,
) -> Button {
    Button::new(id, sha.display_short())
        .color(Color::Accent)
        .start_icon(
            Icon::new(IconName::GitCommit)
                .size(IconSize::Small)
                .color(Color::Accent),
        )
        .on_click(move |_, window, cx| {
            CommitView::open(
                sha.to_string(),
                repository.downgrade(),
                workspace.clone(),
                CommitViewOptions {
                    reveal_file: filename
                        .as_ref()
                        .and_then(|filename| RepoPath::new(filename).ok()),
                    ..Default::default()
                },
                window,
                cx,
            );
            cx.stop_propagation();
        })
}

pub(super) fn render(
    blame: BlameEntry,
    scroll_handle: ScrollHandle,
    details: Option<ParsedCommitMessage>,
    markdown: Entity<Markdown>,
    repository: Entity<Repository>,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let diff = request_diff(&blame, &repository, cx);
    let sha = blame.sha.to_string().into();
    let author_email = blame.author_mail.as_ref().map(|email| {
        SharedString::from(
            email
                .trim_start_matches('<')
                .trim_end_matches('>')
                .to_owned(),
        )
    });
    let avatar = CommitAvatar::new(
        &sha,
        author_email,
        details.as_ref().and_then(|details| details.remote.as_ref()),
    )
    .render(window, cx);
    let relative = blame_entry_timestamp(&blame, settings::GitDateStyle::Relative, cx);
    let absolute = blame
        .author_offset_date_time()
        .map(|timestamp| {
            absolute_timestamp(
                timestamp,
                UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC),
            )
        })
        .unwrap_or_else(|_| "Error parsing date".to_owned());
    let mut markdown_style = hover_markdown_style(window, cx);
    let link_color = cx.theme().colors().text_accent;
    markdown_style.link.refine(&TextStyleRefinement {
        color: Some(link_color),
        underline: Some(UnderlineStyle {
            color: Some(link_color.opacity(0.4)),
            thickness: px(1.),
            ..Default::default()
        }),
        ..Default::default()
    });
    let message = if details.is_some() {
        MarkdownElement::new(markdown, markdown_style).into_any()
    } else {
        "<no commit message>".into_any()
    };
    let boundary_notice = blame
        .boundary
        .then(|| shallow_boundary_notice(repository.clone(), workspace.clone(), window, cx))
        .flatten();
    let max_height =
        window.line_height() * 12 + ThemeSettings::get_global(cx).ui_font_size(cx) / 0.4;
    let filename = blame.filename.clone();
    let changes = h_flex()
        .gap_1()
        .flex_wrap()
        .child(Label::new("Changes").color(Color::Muted))
        .when_some(
            blame.previous_sha_and_filename(),
            |this, (parent_sha, _)| {
                this.child(commit_button(
                    "blame-parent-sha",
                    parent_sha,
                    None,
                    repository.clone(),
                    workspace.clone(),
                ))
                .child("↔")
            },
        )
        .child(commit_button(
            "blame-changes-sha",
            blame.sha,
            Some(filename.clone()),
            repository.clone(),
            workspace.clone(),
        ))
        .child(
            IconButton::new("blame-open-changes", IconName::FileDiff)
                .tooltip(ui::Tooltip::text("Open Changes"))
                .on_click({
                    let repository = repository.clone();
                    let workspace = workspace.clone();
                    let filename = filename.clone();
                    move |_, window, cx| {
                        CommitView::open(
                            sha.to_string(),
                            repository.downgrade(),
                            workspace.clone(),
                            CommitViewOptions {
                                reveal_file: RepoPath::new(&filename).ok(),
                                ..Default::default()
                            },
                            window,
                            cx,
                        );
                        cx.stop_propagation();
                    }
                }),
        );
    let content =
        v_flex()
            .gap_2()
            .max_w(rems(40.))
            .child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .child(avatar)
                    .child(
                        Label::new(
                            blame
                                .author
                                .clone()
                                .unwrap_or_else(|| "<no name>".to_owned()),
                        )
                        .color(Color::Accent),
                    )
                    .child(
                        Icon::new(IconName::Clock)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(Label::new(relative).color(Color::Muted))
                    .child(
                        Label::new(format!("({absolute})"))
                            .color(Color::Muted)
                            .italic(),
                    ),
            )
            .children(boundary_notice)
            .child(message)
            .when(!blame.sha.is_zero(), |this| {
                this.child(
                    h_flex()
                        .gap_1()
                        .child(commit_button(
                            "blame-commit-sha",
                            blame.sha,
                            Some(filename),
                            repository,
                            workspace,
                        ))
                        .child(
                            CopyButton::new("copy-blame-sha", blame.sha.to_string())
                                .tooltip_label("Copy SHA"),
                        ),
                )
                .when_some(diff, |this, diff| {
                    let deleted_color = cx.theme().colors().version_control_deleted;
                    let added_color = cx.theme().colors().version_control_added;
                    this.child(
                        v_flex()
                            .font_buffer(cx)
                            .text_size(ThemeSettings::get_global(cx).buffer_font_size(cx))
                            .children(diff.deleted.into_iter().map(|line| {
                                div().text_color(deleted_color).child(format!("- {line}"))
                            }))
                            .child(
                                div()
                                    .text_color(added_color)
                                    .child(format!("+ {}", diff.added)),
                            ),
                    )
                })
                .child(changes)
            });
    let content = div()
        .id("gitlens-blame-content")
        .max_w(rems(40.))
        .max_h(max_height)
        .overflow_y_scroll()
        .track_scroll(&scroll_handle)
        .child(content)
        .custom_scrollbars(
            Scrollbars::always_visible(ScrollAxes::Vertical).tracked_scroll_handle(&scroll_handle),
            window,
            cx,
        );
    tooltip_container(cx, |this, _| {
        this.occlude()
            .on_mouse_move(|_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_scroll(|_, _, cx| cx.stop_propagation())
            .child(content)
    })
    .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{EmptyView, TestAppContext};
    use settings::SettingsStore;
    use time::macros::{datetime, offset};

    struct PopoverHost {
        blame: BlameEntry,
        markdown: Entity<Markdown>,
        source: SharedString,
        repository: Entity<Repository>,
        workspace: WeakEntity<Workspace>,
    }

    impl Render for PopoverHost {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            GitBlameRenderer
                .render_blame_entry_popover(
                    self.blame.clone(),
                    ScrollHandle::default(),
                    Some(ParsedCommitMessage {
                        message: self.source.clone(),
                        ..Default::default()
                    }),
                    vec![],
                    self.markdown.clone(),
                    self.repository.clone(),
                    self.workspace.clone(),
                    window,
                    cx,
                )
                .expect("blame renderer produces a popover")
        }
    }

    #[test]
    fn ordinal_dates() {
        for (day, expected) in [
            (1, "st"),
            (2, "nd"),
            (3, "rd"),
            (4, "th"),
            (11, "th"),
            (12, "th"),
            (13, "th"),
            (21, "st"),
            (22, "nd"),
            (23, "rd"),
            (31, "st"),
        ] {
            assert_eq!(ordinal_suffix(day), expected);
        }
        assert_eq!(
            absolute_timestamp(datetime!(2026-10-21 03:11 UTC), offset!(+8)),
            "October 21st, 2026 11:11 AM"
        );
        assert_eq!(
            absolute_timestamp(datetime!(2026-10-21 16:00 UTC), offset!(+8)),
            "October 22nd, 2026 12:00 AM"
        );
        assert_eq!(
            absolute_timestamp(datetime!(2026-10-21 04:05 UTC), offset!(+8)),
            "October 21st, 2026 12:05 PM"
        );
    }

    #[test]
    fn selects_hovered_line() {
        let hunks = build_hunks(
            "before\nold one\nold two\nafter\n",
            "before\nnew one\nnew two\nafter\n",
        );
        assert_eq!(
            select_line(&hunks, 3),
            Some(LineDiff {
                deleted: vec!["old two".into()],
                added: "new two".into()
            })
        );
        for row in [0, 1, 4, 5, u32::MAX] {
            assert_eq!(select_line(&hunks, row), None);
        }
        assert_eq!(
            select_line(&build_hunks("old one\nold two\n", "new\n"), 1),
            Some(LineDiff {
                deleted: vec!["old one".into(), "old two".into()],
                added: "new".into(),
            })
        );
        assert_eq!(
            select_line(&build_hunks("same\n", "same\n\tnew\n"), 2),
            Some(LineDiff {
                deleted: vec![],
                added: "    new".into(),
            })
        );
        assert_eq!(
            select_line(&build_hunks("", "one\ntwo\n"), 2),
            Some(LineDiff {
                deleted: vec![],
                added: "two".into()
            })
        );
        assert_eq!(select_line(&build_hunks("deleted\n", ""), 1), None);
        assert_eq!(select_line(&build_hunks("same\n", "same\n"), 1), None);
        assert_eq!(
            select_line(&build_hunks("old\r\n", "new\r\n"), 1),
            Some(LineDiff {
                deleted: vec!["old".into()],
                added: "new".into()
            })
        );
        assert_eq!(
            select_line(&build_hunks("old", "new"), 1),
            Some(LineDiff {
                deleted: vec!["old".into()],
                added: "new".into()
            })
        );
    }

    #[gpui::test]
    fn cache_evicts_and_ignores_stale_tasks(cx: &mut App) {
        let repository = cx.new(|_| EmptyView).entity_id();
        let key = DiffKey {
            repository,
            sha: Oid::from_bytes(&[0; 20]).expect("valid zero SHA"),
            filename: "file".into(),
            previous: None,
        };
        assert_ne!(
            key,
            DiffKey {
                previous: Some((key.sha, "old-file".into())),
                ..key.clone()
            }
        );
        let mut cache = DiffCache::default();
        let first_generation = cache.insert(key.clone(), DiffState::Loading);
        for index in 0..DiffCache::CAPACITY {
            cache.insert(
                DiffKey {
                    filename: format!("file-{index}"),
                    ..key.clone()
                },
                DiffState::Unavailable,
            );
        }
        assert!(cache.touch(&key).is_none());
        let generation = cache.insert(key.clone(), DiffState::Loading);
        assert!(!cache.finish(&key, first_generation, DiffState::Unavailable));
        assert!(cache.finish(&key, generation, DiffState::Ready(vec![])));
        for index in 0..DiffCache::CAPACITY {
            assert!(matches!(cache.touch(&key), Some(DiffState::Ready(_))));
            cache.insert(
                DiffKey {
                    filename: format!("another-{index}"),
                    ..key.clone()
                },
                DiffState::Unavailable,
            );
        }
        assert_eq!(cache.entries.len(), DiffCache::CAPACITY);
        assert!(cache.touch(&key).is_some());
        let parent = DiffKey {
            previous: Some((
                Oid::from_bytes(&[0; 20]).expect("valid zero SHA"),
                "old-name".into(),
            )),
            ..key.clone()
        };
        assert_ne!(parent, key);
        assert!(cache.touch(&parent).is_none());
    }

    #[gpui::test]
    fn setting_defaults_and_updates(cx: &mut App) {
        let store = SettingsStore::test(cx);
        cx.set_global(store);
        assert_eq!(
            ForkBlamePopoverSettings::get_global(cx).style,
            ForkBlamePopoverStyle::Zed
        );
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |settings| {
                settings
                    .fork
                    .get_or_insert_default()
                    .blame_popover
                    .get_or_insert_default()
                    .style = Some(ForkBlamePopoverStyle::Gitlens);
            });
        });
        assert_eq!(
            ForkBlamePopoverSettings::get_global(cx).style,
            ForkBlamePopoverStyle::Gitlens
        );
    }

    #[gpui::test]
    async fn loads_renamed_file_and_caches_diff(cx: &mut TestAppContext) {
        use fs::Fs as _;
        use git::repository::{AskPassDelegate, CommitOptions};
        use project::{FakeFs, Project};
        use serde_json::json;
        use std::path::Path;
        use util::path;

        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            crate::init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({".git": {}, "new.txt": "new one\nnew two\n"}),
        )
        .await;
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let parent = "1123456789abcdef0123456789abcdef01234567";
        fs.set_head_for_repo(
            Path::new(path!("/project/.git")),
            &[("old.txt", "old one\nold two\n".into())],
            parent,
        );
        fs.set_index_for_repo(
            Path::new(path!("/project/.git")),
            &[("new.txt", "new one\nnew two\n".into())],
        );
        let backend = fs
            .open_repo(Path::new(path!("/project/.git")), None)
            .expect("fixture repository opens");
        let askpass = AskPassDelegate::new(&mut cx.to_async(), |_, _, _| {
            panic!("fake commit must not request credentials")
        });
        backend
            .commit(
                "fixture rename".into(),
                None,
                CommitOptions::default(),
                askpass,
                Arc::default(),
            )
            .await
            .expect("fixture commit succeeds");
        fs.set_head_for_repo(
            Path::new(path!("/project/.git")),
            &[("new.txt", "new one\nnew two\n".into())],
            sha,
        );
        let project = Project::test(fs.clone(), [Path::new(path!("/project"))], cx).await;
        cx.run_until_parked();
        let repository = project.read_with(cx, |project, cx| {
            project.active_repository(cx).expect("fixture repository")
        });
        let blame = BlameEntry {
            sha: sha.parse().expect("valid fixture SHA"),
            range: 0..2,
            original_line_number: 2,
            previous: Some(format!("{parent} old.txt")),
            filename: "new.txt".into(),
            author: None,
            author_mail: None,
            author_time: None,
            author_tz: None,
            committer_name: None,
            committer_email: None,
            committer_time: None,
            committer_tz: None,
            summary: None,
            boundary: false,
        };
        cx.update(|cx| {
            let uncommitted = BlameEntry {
                sha: Oid::from_bytes(&[0; 20]).expect("valid zero SHA"),
                ..blame.clone()
            };
            assert!(request_diff(&uncommitted, &repository, cx).is_none());
            assert!(!cx.has_global::<DiffCache>());
        });
        {
            use gpui::{AvailableSpace, canvas};
            use std::{cell::Cell, rc::Rc};
            let (workspace, visual_cx) =
                cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
            let source: SharedString = "long_commit_message_without_spaces".repeat(100).into();
            let markdown = visual_cx.new(|cx| Markdown::new(source.clone(), None, None, cx));
            visual_cx.run_until_parked();
            let host = visual_cx.new(|_| PopoverHost {
                blame: blame.clone(),
                markdown,
                source,
                repository: repository.clone(),
                workspace: workspace.downgrade(),
            });
            let bounds = Rc::new(Cell::new(None));
            for style in [ForkBlamePopoverStyle::Zed, ForkBlamePopoverStyle::Gitlens] {
                visual_cx.update(|_, cx| {
                    cx.update_global::<SettingsStore, _>(|store, cx| {
                        store.update_user_settings(cx, |settings| {
                            settings
                                .fork
                                .get_or_insert_default()
                                .blame_popover
                                .get_or_insert_default()
                                .style = Some(style);
                        });
                    });
                });
                visual_cx.draw(
                    gpui::Point::default(),
                    AvailableSpace::min_size(),
                    |_, _| {
                        div().relative().child(host.clone()).child(
                            canvas(
                                {
                                    let bounds = bounds.clone();
                                    move |element_bounds, _, _| bounds.set(Some(element_bounds))
                                },
                                |_, _, _, _| {},
                            )
                            .absolute()
                            .size_full(),
                        )
                    },
                );
                visual_cx.update(|window, cx| {
                    if style == ForkBlamePopoverStyle::Zed {
                        assert!(!cx.has_global::<DiffCache>());
                    }
                    let bounds = bounds.get().expect("popover was laid out");
                    assert!(bounds.size.width > px(0.));
                    if style == ForkBlamePopoverStyle::Gitlens {
                        assert!(
                            bounds.size.width <= rems(42.).to_pixels(window.rem_size()),
                            "{bounds:?}"
                        );
                        let max_height = window.line_height() * 12
                            + ThemeSettings::get_global(cx).ui_font_size(cx) / 0.4;
                        assert!(
                            bounds.size.height
                                <= max_height + rems(2.).to_pixels(window.rem_size()),
                            "{bounds:?}"
                        );
                    }
                });
            }
        }
        cx.run_until_parked();
        cx.update(|cx| {
            assert_eq!(
                request_diff(&blame, &repository, cx),
                Some(LineDiff {
                    deleted: vec!["old two".into()],
                    added: "new two".into()
                })
            );
        });
        let missing_parent = BlameEntry {
            previous: Some(format!(
                "2123456789abcdef0123456789abcdef01234567 missing.txt"
            )),
            ..blame.clone()
        };
        cx.update(|cx| assert!(request_diff(&missing_parent, &repository, cx).is_none()));
        cx.run_until_parked();
        cx.update(|cx| {
            assert_eq!(
                request_diff(&missing_parent, &repository, cx),
                Some(LineDiff {
                    deleted: vec![],
                    added: "new two".into()
                })
            )
        });
        for (filename, bytes) in [
            ("binary.txt", vec![0; 128]),
            ("large.txt", vec![b'x'; 2 * 1024 * 1024 + 1]),
        ] {
            fs.with_git_state(Path::new(path!("/project/.git")), false, |state| {
                state
                    .head_contents
                    .insert(RepoPath::new(filename).expect("fixture path"), bytes);
            })
            .expect("fixture repository exists");
            let unavailable = BlameEntry {
                filename: filename.into(),
                ..blame.clone()
            };
            cx.update(|cx| assert!(request_diff(&unavailable, &repository, cx).is_none()));
            cx.run_until_parked();
            cx.update(|cx| {
                assert!(matches!(
                    cx.global_mut::<DiffCache>()
                        .touch(&DiffKey::new(repository.entity_id(), &unavailable)),
                    Some(DiffState::Unavailable)
                ))
            });
        }
        let missing_current = BlameEntry {
            filename: "missing.txt".into(),
            ..blame.clone()
        };
        cx.update(|cx| assert!(request_diff(&missing_current, &repository, cx).is_none()));
        cx.run_until_parked();
        cx.update(|cx| {
            assert!(matches!(
                cx.global_mut::<DiffCache>()
                    .touch(&DiffKey::new(repository.entity_id(), &missing_current)),
                Some(DiffState::Unavailable)
            ))
        });
    }
}
