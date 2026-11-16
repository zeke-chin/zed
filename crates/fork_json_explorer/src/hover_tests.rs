use std::{cell::RefCell, ops::Range, rc::Rc, sync::Arc};

use anyhow::Result;
use collections::{HashMap, HashSet};
use editor::{
    Editor, GotoDefinitionKind, MultiBufferOffset, RenameTarget, SelectionEffects,
    SemanticsProvider, hover_popover::ForkHoverLinkHandler,
};
use fs::FakeFs;
use futures::future::Shared;
use gpui::{
    Action as _, App, AppContext as _, Entity, Focusable as _, Task, TestAppContext,
    UpdateGlobal as _, VisualTestContext,
};
use language::{Buffer, BufferRow, Language, LanguageConfig, LanguageMatcher};
use lsp::LanguageServerId;
use project::{
    DocumentHighlight, HoverBlock, HoverBlockKind, InlayHint, InvalidationStrategy, LocationLink,
    Project, ProjectTransaction,
    lsp_store::{BufferSemanticTokens, CacheInlayHints},
};
use settings::SettingsStore;
use text::BufferId;
use workspace::{MultiWorkspace, Workspace};

use crate::{HOVER_LINK_PREFIX, ParseNestedValue, handle_hover_link};

const SCHEMA_HOVER: &str = "schema description";

fn init_test(cx: &mut TestAppContext) {
    cx.update(|cx| {
        assets::Assets.load_test_fonts(cx);
        let store = SettingsStore::test(cx);
        cx.set_global(store);
        cx.set_global(db::AppDatabase::test_new());
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        release_channel::init(semver::Version::new(0, 0, 0), cx);
        editor::init(cx);
        crate::init(cx);
        cx.set_global(ForkHoverLinkHandler(|link, window, cx| {
            link.strip_prefix(HOVER_LINK_PREFIX)
                .is_some_and(|path| handle_hover_link(path, window, cx))
        }));
        cx.observe_new(|editor: &mut Editor, window, cx| {
            if editor.mode().is_full()
                && let Some(window) = window
            {
                crate::attach_editor(editor, window, cx);
            }
        })
        .detach();
    });
}

fn language(name: &'static str, suffix: &str) -> Arc<Language> {
    Arc::new(Language::new(
        LanguageConfig {
            name: name.into(),
            matcher: Arc::new(LanguageMatcher {
                path_suffixes: vec![suffix.into()],
                ..LanguageMatcher::default()
            }),
            ..LanguageConfig::default()
        },
        None,
    ))
}

struct Fixture {
    workspace: Entity<Workspace>,
    project: Entity<Project>,
    json: Arc<Language>,
}

async fn setup(cx: &mut TestAppContext) -> (Fixture, &mut VisualTestContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    let project = Project::test(fs, [], cx).await;
    let json = language("JSON", "json");
    project.read_with(cx, |project, _| {
        project.languages().add(json.clone());
        project.languages().add(language("Markdown", "md"));
    });
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
    (
        Fixture {
            workspace,
            project,
            json,
        },
        cx,
    )
}

fn open_editor(
    fixture: &Fixture,
    text: &str,
    language: Option<Arc<Language>>,
    cx: &mut VisualTestContext,
) -> (Entity<Buffer>, Entity<Editor>) {
    let buffer = fixture.project.update(cx, |project, cx| {
        project.create_local_buffer(text, language, false, cx)
    });
    let project = fixture.project.clone();
    let editor = fixture.workspace.update_in(cx, |workspace, window, cx| {
        let editor = cx.new(|cx| Editor::for_buffer(buffer.clone(), Some(project), window, cx));
        workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
        editor
    });
    cx.run_until_parked();
    (buffer, editor)
}

fn offset_of(text: &str, needle: &str) -> usize {
    text.find(needle).expect("needle should be in the text") + 1
}

async fn hover_at(
    editor: &Entity<Editor>,
    buffer: &Entity<Buffer>,
    offset: usize,
    cx: &mut VisualTestContext,
) -> Vec<project::Hover> {
    let task = editor.update(cx, |editor, cx| {
        let provider = editor
            .semantics_provider()
            .expect("editor should have a semantics provider");
        let position = buffer.read(cx).anchor_before(offset);
        provider.hover(buffer, position, cx)
    });
    match task {
        Some(task) => task.await.unwrap_or_default(),
        None => Vec::new(),
    }
}

fn hover_texts(hovers: &[project::Hover]) -> Vec<String> {
    hovers
        .iter()
        .flat_map(|hover| hover.contents.iter().map(|block| block.text.clone()))
        .collect()
}

fn links(markdown: &str) -> Vec<String> {
    markdown
        .match_indices("](zed-fork://")
        .filter_map(|(index, _)| {
            let start = index + 2;
            let end = start + markdown[start..].find(')')?;
            Some(markdown[start..end].to_string())
        })
        .collect()
}

fn click(link: &str, cx: &mut VisualTestContext) {
    let link = link.to_string();
    cx.update(|window, cx| {
        editor::hover_popover::open_markdown_url(None, link.into(), window, cx);
    });
    cx.run_until_parked();
}

fn dispatch_parse(cx: &mut VisualTestContext) {
    cx.update(|window, cx| window.dispatch_action(ParseNestedValue.boxed_clone(), cx));
    cx.run_until_parked();
}

fn place_cursor(editor: &Entity<Editor>, offset: usize, cx: &mut VisualTestContext) {
    editor.update_in(cx, |editor, window, cx| {
        editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
            selections.select_ranges([MultiBufferOffset(offset)..MultiBufferOffset(offset)])
        });
    });
}

fn set_settings(json: &str, cx: &mut VisualTestContext) {
    cx.update(|_, cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store
                .set_user_settings(json, cx)
                .result()
                .expect("settings should parse");
        });
    });
    cx.run_until_parked();
}

fn pane_count(fixture: &Fixture, cx: &mut VisualTestContext) -> usize {
    fixture
        .workspace
        .read_with(cx, |workspace, _| workspace.panes().len())
}

struct OpenedDocument {
    title: String,
    text: String,
    language: String,
    tabs_in_pane: usize,
}

fn active_document_in_pane(
    fixture: &Fixture,
    pane_index: usize,
    cx: &mut VisualTestContext,
) -> (Entity<Editor>, OpenedDocument) {
    cx.update(|_, cx| {
        let workspace = fixture.workspace.read(cx);
        let pane = workspace.panes()[pane_index].read(cx);
        let editor = pane
            .active_item()
            .and_then(|item| item.downcast::<Editor>())
            .expect("pane should show an editor");
        let multi_buffer = editor.read(cx).buffer().read(cx);
        let buffer = multi_buffer
            .as_singleton()
            .expect("parsed document should be a singleton");
        let document = OpenedDocument {
            title: multi_buffer.title(cx).to_string(),
            text: buffer.read(cx).text(),
            language: buffer
                .read(cx)
                .language()
                .map(|language| language.name().to_string())
                .unwrap_or_default(),
            tabs_in_pane: pane.items_len(),
        };
        (editor, document)
    })
}

#[gpui::test]
async fn test_hover_appends_preview_after_original_hover(cx: &mut TestAppContext) {
    let (fixture, cx) = setup(cx).await;
    let text = r#"{"data": {"json_str": "{\"a\": 1}", "count": 3}}"#;
    let (buffer, editor) = open_editor(&fixture, text, Some(fixture.json.clone()), cx);
    let calls = install_fake_provider(&editor, cx);

    let hovers = hover_at(&editor, &buffer, offset_of(text, "\"{"), cx).await;
    let texts = hover_texts(&hovers);
    assert_eq!(texts.len(), 2, "{texts:?}");
    assert_eq!(texts[0], SCHEMA_HOVER);
    assert!(texts[1].starts_with("**Parsed JSON** `data.json_str`"));
    let preview_links = links(&texts[1]);
    assert_eq!(preview_links.len(), 2);
    assert_eq!(preview_links[0], preview_links[1]);
    assert!(preview_links[0].starts_with("zed-fork://json-explorer/open?token="));
    let hover_range = hovers[1]
        .range
        .clone()
        .expect("preview should have a range");
    buffer.read_with(cx, |buffer, _| {
        let snapshot = buffer.snapshot();
        let start = text::ToOffset::to_offset(&hover_range.start, &snapshot);
        let end = text::ToOffset::to_offset(&hover_range.end, &snapshot);
        assert_eq!(&text[start..end], r#""{\"a\": 1}""#);
    });

    let number_hovers = hover_at(&editor, &buffer, offset_of(text, "3"), cx).await;
    assert_eq!(hover_texts(&number_hovers), vec![SCHEMA_HOVER.to_string()]);
    assert_eq!(
        calls
            .borrow()
            .iter()
            .filter(|call| **call == "hover")
            .count(),
        2
    );
}

#[gpui::test]
async fn test_hover_passes_through_outside_json_and_when_disabled(cx: &mut TestAppContext) {
    let (fixture, cx) = setup(cx).await;
    let text = r#"{"key": "value"}"#;
    let (plain_buffer, plain_editor) = open_editor(&fixture, text, None, cx);
    install_fake_provider(&plain_editor, cx);
    let hovers = hover_at(&plain_editor, &plain_buffer, offset_of(text, "\"value"), cx).await;
    assert_eq!(hover_texts(&hovers), vec![SCHEMA_HOVER.to_string()]);

    let (json_buffer, json_editor) = open_editor(&fixture, text, Some(fixture.json.clone()), cx);
    install_fake_provider(&json_editor, cx);
    set_settings(
        r#"{ "fork": { "json_explorer": { "enabled": false } } }"#,
        cx,
    );
    let hovers = hover_at(&json_editor, &json_buffer, offset_of(text, "\"value"), cx).await;
    assert_eq!(hover_texts(&hovers), vec![SCHEMA_HOVER.to_string()]);

    set_settings("{}", cx);
    let hovers = hover_at(&json_editor, &json_buffer, offset_of(text, "\"value"), cx).await;
    let texts = hover_texts(&hovers);
    assert_eq!(texts.len(), 2);
    assert!(texts[1].starts_with("**String value** `key`"));
}

#[gpui::test]
async fn test_decorator_forwards_every_other_method(cx: &mut TestAppContext) {
    let (fixture, cx) = setup(cx).await;
    let (buffer, editor) = open_editor(&fixture, "{}", Some(fixture.json.clone()), cx);
    let calls = install_fake_provider(&editor, cx);

    editor.update(cx, |editor, cx| {
        let provider = editor
            .semantics_provider()
            .expect("editor should have a semantics provider");
        let position = buffer.read(cx).anchor_before(0);
        let range = position..position;
        assert!(provider.debugger_hover(&buffer, position, cx).is_none());
        assert!(
            provider
                .inline_values(buffer.clone(), range.clone(), cx)
                .is_none()
        );
        assert_eq!(
            provider.applicable_inlay_chunks(&buffer, std::slice::from_ref(&range), cx),
            vec![7..9]
        );
        provider.invalidate_inlay_hints(&HashSet::default(), cx);
        assert!(
            provider
                .inlay_hints(
                    InvalidationStrategy::BufferEdited,
                    buffer.clone(),
                    vec![range],
                    None,
                    cx
                )
                .is_none()
        );
        assert!(provider.semantic_tokens(buffer.clone(), cx).is_none());
        assert!(provider.supports_inlay_hints(&buffer, cx));
        assert!(provider.supports_semantic_tokens(&buffer, cx));
        assert!(
            provider
                .document_highlights(&buffer, position, cx)
                .is_some()
        );
        assert!(
            provider
                .definitions(&buffer, position, GotoDefinitionKind::Symbol, cx)
                .is_none()
        );
        provider.range_for_rename(&buffer, position, cx).detach();
        assert!(
            provider
                .perform_rename(&buffer, position, "renamed".to_string(), None, cx)
                .is_none()
        );
    });

    assert_eq!(
        *calls.borrow(),
        vec![
            "debugger_hover",
            "inline_values",
            "applicable_inlay_chunks",
            "invalidate_inlay_hints",
            "inlay_hints",
            "semantic_tokens",
            "supports_inlay_hints",
            "supports_semantic_tokens",
            "document_highlights",
            "definitions",
            "range_for_rename",
            "perform_rename",
        ]
    );
}

#[gpui::test]
async fn test_parse_nested_value_opens_documents_and_nests(cx: &mut TestAppContext) {
    let (fixture, cx) = setup(cx).await;
    let text = r#"{"data": {"json_str": "{\"inner\": \"{\\\"deep\\\": 1}\"}", "note": "plain"}}"#;
    let (_, source_editor) = open_editor(&fixture, text, Some(fixture.json.clone()), cx);

    place_cursor(&source_editor, offset_of(text, "\"{"), cx);
    dispatch_parse(cx);
    assert_eq!(pane_count(&fixture, cx), 2);
    let (parsed_editor, parsed) = active_document_in_pane(&fixture, 1, cx);
    assert_eq!(parsed.title, "Parsed-data.json_str.json");
    assert_eq!(parsed.text, "{\n\t\"inner\": \"{\\\"deep\\\": 1}\"\n}");
    assert_eq!(parsed.language, "JSON");

    place_cursor(&parsed_editor, offset_of(&parsed.text, "\"{"), cx);
    dispatch_parse(cx);
    assert_eq!(pane_count(&fixture, cx), 3);
    let (_, deep) = active_document_in_pane(&fixture, 2, cx);
    assert_eq!(deep.title, "Parsed-inner.json");
    assert_eq!(deep.text, "{\n\t\"deep\": 1\n}");
    assert_eq!(deep.language, "JSON");

    source_editor.update_in(cx, |editor, window, cx| {
        window.focus(&editor.focus_handle(cx), cx);
    });
    place_cursor(&source_editor, offset_of(text, "\"plain"), cx);
    dispatch_parse(cx);
    assert_eq!(pane_count(&fixture, cx), 3);
    let (_, plain) = active_document_in_pane(&fixture, 1, cx);
    assert_eq!(plain.title, "Value-data.note.txt");
    assert_eq!(plain.text, "plain");
    assert_eq!(plain.language, "Plain Text");
    assert_eq!(plain.tabs_in_pane, 2);
}

#[gpui::test]
async fn test_parse_nested_value_ignores_non_string_positions(cx: &mut TestAppContext) {
    let (fixture, cx) = setup(cx).await;
    let text = r#"{"count": 3}"#;
    let (_, editor) = open_editor(&fixture, text, Some(fixture.json.clone()), cx);
    place_cursor(&editor, offset_of(text, "3") - 1, cx);
    dispatch_parse(cx);
    assert_eq!(pane_count(&fixture, cx), 1);
}

#[gpui::test]
async fn test_hover_links_open_the_value_like_the_action(cx: &mut TestAppContext) {
    let (fixture, cx) = setup(cx).await;
    let text = r#"{"list": ["{'ok': True}"]}"#;
    let (buffer, editor) = open_editor(&fixture, text, Some(fixture.json.clone()), cx);

    let hovers = hover_at(&editor, &buffer, offset_of(text, "\"{"), cx).await;
    let texts = hover_texts(&hovers);
    let preview = texts.last().expect("preview should be appended");
    assert!(preview.starts_with("**Parsed Python dict** `list[0]`"));
    let preview_links = links(preview);
    assert_eq!(preview_links.len(), 2);

    click(&preview_links[0], cx);
    assert_eq!(pane_count(&fixture, cx), 2);
    let (_, opened) = active_document_in_pane(&fixture, 1, cx);
    assert_eq!(opened.title, "Parsed-py-list_0_.json");
    assert_eq!(opened.text, "{\n\t\"ok\": true\n}");
    assert_eq!(opened.language, "JSON");

    editor.update_in(cx, |editor, window, cx| {
        window.focus(&editor.focus_handle(cx), cx);
    });
    click(&preview_links[1], cx);
    assert_eq!(pane_count(&fixture, cx), 2);
    let (_, opened_again) = active_document_in_pane(&fixture, 1, cx);
    assert_eq!(opened_again.text, opened.text);
    assert_eq!(opened_again.tabs_in_pane, 2);
}

#[gpui::test]
async fn test_hover_link_does_not_open_stale_content(cx: &mut TestAppContext) {
    let (fixture, cx) = setup(cx).await;
    let text = r#"{"a": "{\"b\": 1}"}"#;
    let (buffer, editor) = open_editor(&fixture, text, Some(fixture.json.clone()), cx);

    let hovers = hover_at(&editor, &buffer, offset_of(text, "\"{"), cx).await;
    let stale_link = links(
        hover_texts(&hovers)
            .last()
            .expect("preview should be appended"),
    )
    .into_iter()
    .next()
    .expect("preview should have a link");

    buffer.update(cx, |buffer, cx| {
        buffer.edit([(text.len() - 1..text.len() - 1, ", \"c\": 2")], None, cx);
    });
    click(&stale_link, cx);
    assert_eq!(pane_count(&fixture, cx), 1);

    let edited_text = buffer.read_with(cx, |buffer, _| buffer.text());
    let hovers = hover_at(&editor, &buffer, offset_of(&edited_text, "\"{"), cx).await;
    let fresh_link = links(
        hover_texts(&hovers)
            .last()
            .expect("preview should be appended"),
    )
    .into_iter()
    .next()
    .expect("preview should have a link");
    assert_ne!(fresh_link, stale_link);
    click(&fresh_link, cx);
    assert_eq!(pane_count(&fixture, cx), 2);
    let (_, opened) = active_document_in_pane(&fixture, 1, cx);
    assert_eq!(opened.text, "{\n\t\"b\": 1\n}");
}

#[gpui::test]
async fn test_unknown_fork_links_fall_through(cx: &mut TestAppContext) {
    let (_, cx) = setup(cx).await;
    cx.update(|window, cx| {
        assert!(!handle_hover_link("unknown", window, cx));
        assert!(!handle_hover_link("open?token=not-a-number", window, cx));
        assert!(handle_hover_link("open?token=999999", window, cx));
    });
}

/// Replaces the editor's provider with a fake that answers every hover with
/// `SCHEMA_HOVER`, wrapped in the fork's decorator the same way as `attach_editor`.
fn install_fake_provider(
    editor: &Entity<Editor>,
    cx: &mut VisualTestContext,
) -> Rc<RefCell<Vec<&'static str>>> {
    let calls = Rc::new(RefCell::new(Vec::new()));
    let provider = FakeProvider {
        calls: calls.clone(),
    };
    editor.update(cx, |editor, cx| {
        editor.set_semantics_provider(Some(Rc::new(provider)));
        crate::hover::attach(editor, cx);
    });
    calls
}

struct FakeProvider {
    calls: Rc<RefCell<Vec<&'static str>>>,
}

impl FakeProvider {
    fn record(&self, call: &'static str) {
        self.calls.borrow_mut().push(call);
    }
}

impl SemanticsProvider for FakeProvider {
    fn hover(
        &self,
        _buffer: &Entity<Buffer>,
        _position: text::Anchor,
        _cx: &mut App,
    ) -> Option<Task<Option<Vec<project::Hover>>>> {
        self.record("hover");
        Some(Task::ready(Some(vec![project::Hover {
            contents: vec![HoverBlock {
                text: SCHEMA_HOVER.to_string(),
                kind: HoverBlockKind::Markdown,
            }],
            range: None,
            language: None,
        }])))
    }

    fn debugger_hover(
        &self,
        _buffer: &Entity<Buffer>,
        _position: text::Anchor,
        _cx: &mut App,
    ) -> Option<Task<Option<project::DebuggerHover>>> {
        self.record("debugger_hover");
        None
    }

    fn inline_values(
        &self,
        _buffer_handle: Entity<Buffer>,
        _range: Range<text::Anchor>,
        _cx: &mut App,
    ) -> Option<Task<Result<Vec<InlayHint>>>> {
        self.record("inline_values");
        None
    }

    fn applicable_inlay_chunks(
        &self,
        _buffer: &Entity<Buffer>,
        _ranges: &[Range<text::Anchor>],
        _cx: &mut App,
    ) -> Vec<Range<BufferRow>> {
        self.record("applicable_inlay_chunks");
        vec![7..9]
    }

    fn invalidate_inlay_hints(&self, _for_buffers: &HashSet<BufferId>, _cx: &mut App) {
        self.record("invalidate_inlay_hints");
    }

    fn inlay_hints(
        &self,
        _invalidate: InvalidationStrategy,
        _buffer: Entity<Buffer>,
        _ranges: Vec<Range<text::Anchor>>,
        _known_chunks: Option<(clock::Global, HashSet<Range<BufferRow>>)>,
        _cx: &mut App,
    ) -> Option<HashMap<Range<BufferRow>, Task<Result<CacheInlayHints>>>> {
        self.record("inlay_hints");
        None
    }

    fn semantic_tokens(
        &self,
        _buffer: Entity<Buffer>,
        _cx: &mut App,
    ) -> Option<Shared<Task<std::result::Result<BufferSemanticTokens, Arc<anyhow::Error>>>>> {
        self.record("semantic_tokens");
        None
    }

    fn supports_inlay_hints(&self, _buffer: &Entity<Buffer>, _cx: &mut App) -> bool {
        self.record("supports_inlay_hints");
        true
    }

    fn supports_semantic_tokens(&self, _buffer: &Entity<Buffer>, _cx: &mut App) -> bool {
        self.record("supports_semantic_tokens");
        true
    }

    fn document_highlights(
        &self,
        _buffer: &Entity<Buffer>,
        _position: text::Anchor,
        _cx: &mut App,
    ) -> Option<Task<Result<Vec<DocumentHighlight>>>> {
        self.record("document_highlights");
        Some(Task::ready(Ok(Vec::new())))
    }

    fn definitions(
        &self,
        _buffer: &Entity<Buffer>,
        _position: text::Anchor,
        _kind: GotoDefinitionKind,
        _cx: &mut App,
    ) -> Option<Task<Result<Option<Vec<LocationLink>>>>> {
        self.record("definitions");
        None
    }

    fn range_for_rename(
        &self,
        _buffer: &Entity<Buffer>,
        _position: text::Anchor,
        _cx: &mut App,
    ) -> Task<Result<Option<RenameTarget>>> {
        self.record("range_for_rename");
        Task::ready(Ok(None))
    }

    fn perform_rename(
        &self,
        _buffer: &Entity<Buffer>,
        _position: text::Anchor,
        _new_name: String,
        _language_server_id: Option<LanguageServerId>,
        _cx: &mut App,
    ) -> Option<Task<Result<ProjectTransaction>>> {
        self.record("perform_rename");
        None
    }
}
