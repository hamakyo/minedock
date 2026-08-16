use std::{ops::Range, path::PathBuf};

use gpui::{
    App, Application, Bounds, Context, Element, ElementId, ElementInputHandler, Entity,
    EntityInputHandler, FocusHandle, Focusable, GlobalElementId, KeyBinding, LayoutId, MouseButton,
    PaintQuad, Pixels, Point, ShapedLine, SharedString, Style, TextRun, UTF16Selection, Window,
    WindowBounds, WindowOptions, actions, div, fill, prelude::*, px, relative, rgb, rgba, size,
};
use minedock_core::{
    CreateWorldRequest, JsonWorldRepository, LifecycleSupervisor, TemplateCatalog, TemplateId,
    World, WorldLibrary, WorldStatus, normalize_world_name,
};

#[allow(dead_code)]
mod http_transport;
mod java_adapter;
mod lifecycle_adapter;
mod native_process;
mod native_safety;

actions!(
    text_input,
    [
        Backspace,
        Delete,
        Left,
        Right,
        SelectLeft,
        SelectRight,
        SelectAll,
        Home,
        End,
        Paste,
        Cut,
        Copy,
        CreateWorldAction,
        CancelWizardAction,
    ]
);

/// Resolve app data in the presentation layer. Core only receives a supplied
/// root and never knows about Windows environment variables.
pub fn resolve_app_data_root() -> anyhow::Result<PathBuf> {
    if let Some(value) = std::env::var_os("MINEDOCK_DATA_DIR") {
        let value = PathBuf::from(value);
        if !value.as_os_str().is_empty() {
            return Ok(value);
        }
    }
    let local_app_data = std::env::var_os("LOCALAPPDATA").ok_or_else(|| {
        anyhow::anyhow!("LOCALAPPDATA is not set; set MINEDOCK_DATA_DIR to choose a data directory")
    })?;
    Ok(PathBuf::from(local_app_data).join("MineDock"))
}

#[derive(Debug)]
struct TextInput {
    focus_handle: FocusHandle,
    content: SharedString,
    placeholder: SharedString,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    last_layout: Option<ShapedLine>,
    last_bounds: Option<Bounds<Pixels>>,
    is_selecting: bool,
}

impl TextInput {
    fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            content: "".into(),
            placeholder: "e.g. Sunday Survival".into(),
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
            is_selecting: false,
        }
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;
        for character in self.content.chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += character.len_utf8();
            utf16_offset += character.len_utf16();
        }
        utf16_offset
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        utf16_range_to_byte_range(&self.content, range.clone())
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content[..offset]
            .char_indices()
            .last()
            .map(|(index, _)| index)
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content[offset..]
            .chars()
            .next()
            .map(|character| offset + character.len_utf8())
            .unwrap_or(self.content.len())
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        self.selection_reversed = false;
        cx.notify();
    }

    fn replace_text(&mut self, range: Range<usize>, text: &str, cx: &mut Context<Self>) {
        self.content =
            (self.content[..range.start].to_owned() + text + &self.content[range.end..]).into();
        let end = range.start + text.len();
        self.selected_range = end..end;
        self.marked_range = None;
        cx.notify();
    }

    fn backspace(&mut self, _: &Backspace, _: &mut Window, cx: &mut Context<Self>) {
        let range = if self.selected_range.is_empty() {
            self.previous_boundary(self.cursor_offset())..self.cursor_offset()
        } else {
            self.selected_range.clone()
        };
        self.replace_text(range, "", cx);
    }

    fn delete(&mut self, _: &Delete, _: &mut Window, cx: &mut Context<Self>) {
        let range = if self.selected_range.is_empty() {
            self.cursor_offset()..self.next_boundary(self.cursor_offset())
        } else {
            self.selected_range.clone()
        };
        self.replace_text(range, "", cx);
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(
            if self.selected_range.is_empty() {
                self.previous_boundary(self.cursor_offset())
            } else {
                self.selected_range.start
            },
            cx,
        );
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(
            if self.selected_range.is_empty() {
                self.next_boundary(self.cursor_offset())
            } else {
                self.selected_range.end
            },
            cx,
        );
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        let target = if self.selected_range.is_empty() {
            self.previous_boundary(self.cursor_offset())
        } else if self.selection_reversed {
            self.previous_boundary(self.selected_range.start)
        } else {
            self.previous_boundary(self.selected_range.end)
        };
        self.select_to(target, cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        let target = if self.selected_range.is_empty() {
            self.next_boundary(self.cursor_offset())
        } else if self.selection_reversed {
            self.next_boundary(self.selected_range.start)
        } else {
            self.next_boundary(self.selected_range.end)
        };
        self.select_to(target, cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.selected_range = 0..self.content.len();
        self.selection_reversed = false;
        cx.notify();
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.content.len(), cx);
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.replace_text_in_range(None, &text.replace('\n', " "), window, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
            self.replace_text_in_range(None, "", window, cx);
        }
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        let (Some(bounds), Some(line)) = (&self.last_bounds, &self.last_layout) else {
            return 0;
        };
        if position.y < bounds.top() {
            return 0;
        }
        if position.y > bounds.bottom() {
            return self.content.len();
        }
        line.closest_index_for_x(position.x - bounds.left())
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.selection_reversed {
            self.selected_range.start = offset;
        } else {
            self.selected_range.end = offset;
        }
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        cx.notify();
    }

    fn on_mouse_down(
        &mut self,
        event: &gpui::MouseDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.modifiers.shift {
            self.select_to(self.index_for_mouse_position(event.position), cx);
        } else {
            self.move_to(self.index_for_mouse_position(event.position), cx);
        }
        self.is_selecting = true;
    }

    fn on_mouse_up(&mut self, _: &gpui::MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(
        &mut self,
        event: &gpui::MouseMoveEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_selecting {
            self.select_to(self.index_for_mouse_position(event.position), cx);
        }
    }
}

/// Convert a UTF-16 range from the platform text system to a valid UTF-8 byte
/// range. Windows IME ranges may be stale or may point inside a surrogate pair;
/// clamp starts down and ends up to character boundaries so slicing is safe.
fn utf16_range_to_byte_range(text: &str, range: Range<usize>) -> Range<usize> {
    let start = utf16_offset_to_byte_index(text, range.start, false);
    let end = utf16_offset_to_byte_index(text, range.end, true);
    if start <= end { start..end } else { end..start }
}

fn utf16_offset_to_byte_index(text: &str, offset: usize, round_up: bool) -> usize {
    let mut utf16_offset = 0;
    for (byte_index, character) in text.char_indices() {
        if offset == utf16_offset {
            return byte_index;
        }
        let next_utf16_offset = utf16_offset + character.len_utf16();
        if offset < next_utf16_offset {
            return if round_up {
                byte_index + character.len_utf8()
            } else {
                byte_index
            };
        }
        utf16_offset = next_utf16_offset;
    }
    text.len()
}

impl EntityInputHandler for TextInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|value| self.range_from_utf16(value))
            .or_else(|| self.marked_range.clone())
            .unwrap_or_else(|| self.selected_range.clone());
        self.replace_text(range, new_text, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|value| self.range_from_utf16(value))
            .or_else(|| self.marked_range.clone())
            .unwrap_or_else(|| self.selected_range.clone());
        let start = range.start;
        self.content =
            (self.content[..range.start].to_owned() + new_text + &self.content[range.end..]).into();
        self.marked_range = (!new_text.is_empty()).then(|| start..start + new_text.len());
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|value| utf16_range_to_byte_range(new_text, value.clone()))
            .map(|value| value.start + start..value.end + start)
            .unwrap_or_else(|| start + new_text.len()..start + new_text.len());
        self.selection_reversed = false;
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let line = self.last_layout.as_ref()?;
        let range = self.range_from_utf16(&range_utf16);
        Some(Bounds::from_corners(
            gpui::point(bounds.left() + line.x_for_index(range.start), bounds.top()),
            gpui::point(bounds.left() + line.x_for_index(range.end), bounds.bottom()),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        let bounds = self.last_bounds.as_ref()?;
        let line = self.last_layout.as_ref()?;
        Some(self.offset_to_utf16(line.closest_index_for_x(point.x - bounds.left())))
    }
}

struct TextElement {
    input: Entity<TextInput>,
}

struct TextElementState {
    line: Option<ShapedLine>,
    cursor: Option<PaintQuad>,
    selection: Option<PaintQuad>,
}

impl IntoElement for TextElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = TextElementState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = window.line_height().into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let input = self.input.read(cx);
        let content = input.content.clone();
        let style = window.text_style();
        let display_text = if content.is_empty() {
            input.placeholder.clone()
        } else {
            content
        };
        let text_color = if input.content.is_empty() {
            gpui::hsla(0., 0., 0.4, 1.)
        } else {
            style.color
        };
        let run = TextRun {
            len: display_text.len(),
            font: style.font(),
            color: text_color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line = window
            .text_system()
            .shape_line(display_text, font_size, &[run], None);
        let cursor_pos = line.x_for_index(input.cursor_offset());
        let (selection, cursor) = if input.selected_range.is_empty() {
            (
                None,
                Some(fill(
                    Bounds::new(
                        gpui::point(bounds.left() + cursor_pos, bounds.top()),
                        size(px(2.), bounds.bottom() - bounds.top()),
                    ),
                    gpui::blue(),
                )),
            )
        } else {
            (
                Some(fill(
                    Bounds::from_corners(
                        gpui::point(
                            bounds.left() + line.x_for_index(input.selected_range.start),
                            bounds.top(),
                        ),
                        gpui::point(
                            bounds.left() + line.x_for_index(input.selected_range.end),
                            bounds.bottom(),
                        ),
                    ),
                    rgba(0x3311ff30),
                )),
                None,
            )
        };
        TextElementState {
            line: Some(line),
            cursor,
            selection,
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        if let Some(selection) = prepaint.selection.take() {
            window.paint_quad(selection);
        }
        if let Some(line) = prepaint.line.take() {
            let _ = line.paint(bounds.origin, window.line_height(), window, cx);
            self.input.update(cx, |input, _| {
                input.last_layout = Some(line);
                input.last_bounds = Some(bounds);
            });
        }
        if focus_handle.is_focused(window) {
            if let Some(cursor) = prepaint.cursor.take() {
                window.paint_quad(cursor);
            }
        }
    }
}

impl Render for TextInput {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .key_context("TextInput")
            .track_focus(&self.focus_handle(cx))
            .cursor(gpui::CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::cut))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .line_height(px(24.))
            .text_size(px(16.))
            .child(
                div()
                    .h(px(32.))
                    .w_full()
                    .p(px(4.))
                    .child(TextElement { input: cx.entity() }),
            )
    }
}

impl Focusable for TextInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

struct MineDockView {
    library: Option<WorldLibrary<JsonWorldRepository>>,
    _app_data_lease: Option<lifecycle_adapter::AppDataLease>,
    _lifecycle: Option<
        LifecycleSupervisor<
            lifecycle_adapter::JsonLifecyclePersistence,
            native_process::NativeProcessFactory,
            lifecycle_adapter::AppDataLeaseProvider,
        >,
    >,
    catalog: Option<TemplateCatalog>,
    startup_error: Option<String>,
    name_input: Entity<TextInput>,
    wizard_open: bool,
    selected_template: Option<TemplateId>,
    wizard_error: Option<String>,
    java_status: String,
}

impl MineDockView {
    fn new(name_input: Entity<TextInput>, cx: &mut Context<Self>) -> Self {
        cx.observe(&name_input, |_, _, cx| cx.notify()).detach();
        let catalog = match TemplateCatalog::built_in() {
            Ok(catalog) => Some(catalog),
            Err(error) => {
                return Self {
                    library: None,
                    _app_data_lease: None,
                    _lifecycle: None,
                    catalog: None,
                    startup_error: Some(format!("Built-in templates could not load: {error}")),
                    name_input,
                    wizard_open: false,
                    selected_template: None,
                    wizard_error: None,
                    java_status: "Java readiness unavailable".into(),
                };
            }
        };
        let mut startup_error = None;
        let mut app_data_lease = None;
        let mut lifecycle = None;
        let library = match resolve_app_data_root() {
            Ok(root) => {
                let repository = JsonWorldRepository::new(root);
                match lifecycle_adapter::AppDataLease::acquire(repository.root()) {
                    Ok(lease) => {
                        app_data_lease = Some(lease);
                        if let Err(error) =
                            lifecycle_adapter::JsonLifecyclePersistence::recover_startup(
                                &repository,
                            )
                        {
                            startup_error =
                                Some(format!("MineDock startup recovery failed: {error}"));
                        }
                    }
                    Err(error) => {
                        startup_error = Some(format!(
                            "MineDock is already in use or could not acquire its lease: {error}"
                        ));
                    }
                }
                let mut library = WorldLibrary::new(repository);
                lifecycle = Some(LifecycleSupervisor::new(
                    lifecycle_adapter::JsonLifecyclePersistence::new(library.repository().clone()),
                    native_process::NativeProcessFactory,
                    app_data_lease
                        .as_ref()
                        .map_or_else(lifecycle_adapter::AppDataLeaseProvider::default, |lease| {
                            lifecycle_adapter::AppDataLeaseProvider::with_lease(lease.clone())
                        }),
                ));
                if startup_error.is_none() {
                    if let Err(error) = library.load() {
                        startup_error = Some(format!("MineDock metadata could not load: {error}"));
                    }
                }
                Some(library)
            }
            Err(error) => {
                startup_error = Some(error.to_string());
                None
            }
        };
        let selected_template = catalog
            .as_ref()
            .and_then(|catalog| catalog.templates().first())
            .map(|template| template.id.clone());
        // Current-release has not been resolved in the non-mutating library
        // view. Probe for an installed Java only; Start remains disabled until
        // a provider resolves the authoritative Java requirement.
        let java_receiver = java_adapter::discover_any_java_off_event_loop();
        cx.spawn(async move |this, cx| {
            let readiness = cx
                .background_executor()
                .spawn(async move { java_receiver.recv().ok() })
                .await;
            if let Some(readiness) = readiness {
                let _ = this.update(cx, |view, cx| {
                    view.java_status = readiness.runtime.as_ref().map_or_else(
                        || readiness.status_text(),
                        |runtime| {
                            format!(
                                "Java detected (major {}; release check pending)",
                                runtime.version.major
                            )
                        },
                    );
                    cx.notify();
                });
            }
        })
        .detach();
        Self {
            library,
            _app_data_lease: app_data_lease,
            _lifecycle: lifecycle,
            catalog,
            startup_error,
            name_input,
            wizard_open: false,
            selected_template,
            wizard_error: None,
            java_status: "Checking Java readiness…".into(),
        }
    }

    fn open_wizard(&mut self, _: &gpui::ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.can_mutate() {
            self.wizard_open = true;
            self.wizard_error = None;
            window.focus(&self.name_input.focus_handle(cx));
            cx.notify();
        }
    }

    fn close_wizard(&mut self, cx: &mut Context<Self>) {
        self.wizard_open = false;
        self.wizard_error = None;
        cx.notify();
    }

    fn cancel_wizard(&mut self, _: &gpui::ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.close_wizard(cx);
    }

    fn cancel_wizard_action(
        &mut self,
        _: &CancelWizardAction,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_wizard(cx);
    }

    fn select_template(
        &mut self,
        template_id: TemplateId,
        _: &gpui::ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected_template = Some(template_id);
        self.wizard_error = None;
        cx.notify();
    }

    fn create_world(&mut self, _: &gpui::ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.create_world_from_input(cx);
    }

    fn create_world_action(
        &mut self,
        _: &CreateWorldAction,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.create_world_from_input(cx);
    }

    fn create_world_from_input(&mut self, cx: &mut Context<Self>) {
        if !self.can_create(cx) {
            return;
        }
        let Some(template_id) = self.selected_template.clone() else {
            self.wizard_error = Some("Select a template before creating a world.".into());
            cx.notify();
            return;
        };
        let Some(catalog) = self.catalog.as_ref() else {
            self.wizard_error = Some("Built-in templates are unavailable.".into());
            cx.notify();
            return;
        };
        let name = self.name_input.read(cx).content.to_string();
        let request = match CreateWorldRequest::new(name, template_id) {
            Ok(request) => request,
            Err(error) => {
                self.wizard_error = Some(error.to_string());
                cx.notify();
                return;
            }
        };
        let result = self
            .library
            .as_mut()
            .expect("can_mutate guarantees a library")
            .create_from_template(&request, catalog);
        match result {
            Ok(_) => {
                self.wizard_open = false;
                self.wizard_error = None;
            }
            Err(error) => {
                // Keep the draft in the input entity so a transient write
                // failure does not discard the user's work.
                self.wizard_error = Some(error.to_string());
            }
        }
        cx.notify();
    }

    fn draft_name_is_valid(&self, cx: &App) -> bool {
        normalize_world_name(&self.name_input.read(cx).content).is_ok()
    }

    fn can_create(&self, cx: &App) -> bool {
        if !self.can_mutate() || !self.draft_name_is_valid(cx) {
            return false;
        }
        let Some(template_id) = self.selected_template.as_ref() else {
            return false;
        };
        self.catalog
            .as_ref()
            .is_some_and(|catalog| catalog.get(template_id).is_some())
    }

    fn can_mutate(&self) -> bool {
        self.startup_error.is_none() && self.library.is_some() && self.catalog.is_some()
    }

    fn render_header(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .justify_between()
            .items_center()
            .child(
                div()
                    .text_size(px(26.))
                    .font_weight(gpui::FontWeight::BOLD)
                    .child("MineDock"),
            )
            .child(
                div()
                    .text_color(rgb(0x9CA3AF))
                    .child(format!("Local library  ·  {}", self.java_status)),
            )
            .child(
                div()
                    .id("new-world")
                    .px(px(14.))
                    .py(px(8.))
                    .rounded(px(8.))
                    .bg(if self.can_mutate() {
                        rgb(0x34415C)
                    } else {
                        rgb(0x252932)
                    })
                    .text_color(if self.can_mutate() {
                        rgb(0xE7EEFF)
                    } else {
                        rgb(0x737985)
                    })
                    .child("+ New World")
                    .when(self.can_mutate(), |element| {
                        element.focusable().on_click(cx.listener(Self::open_wizard))
                    }),
            )
    }

    fn render_library(&mut self) -> impl IntoElement {
        let mut cards = div().flex().flex_col().gap(px(12.));
        if let Some(library) = &self.library {
            for world in library.list() {
                cards = cards.child(world_card(world));
            }
        }
        if self
            .library
            .as_ref()
            .is_some_and(|library| library.list().is_empty())
        {
            cards = cards.child(
                div()
                    .p(px(24.))
                    .rounded(px(12.))
                    .bg(rgb(0x1A1E25))
                    .text_color(rgb(0x9CA3AF))
                    .child("No worlds yet. Create one from a built-in template to get started."),
            );
        }
        cards
    }

    fn render_wizard(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut templates = div().flex().gap(px(8.));
        if let Some(catalog) = &self.catalog {
            for template in catalog.iter() {
                let id = template.id.clone();
                let selected = self.selected_template.as_ref() == Some(&id);
                templates = templates.child(
                    div()
                        .id(SharedString::from(format!("template-{id}")))
                        .focusable()
                        .px(px(12.))
                        .py(px(8.))
                        .rounded(px(8.))
                        .bg(if selected {
                            rgb(0x3D5A88)
                        } else {
                            rgb(0x292E38)
                        })
                        .text_color(rgb(0xF2F4F8))
                        .child(template.name.clone())
                        .on_click(cx.listener(move |this, event, window, cx| {
                            this.select_template(id.clone(), event, window, cx)
                        })),
                );
            }
        }
        let summary = self
            .selected_template
            .as_ref()
            .and_then(|id| self.catalog.as_ref()?.get(id))
            .map(|template| {
                format!(
                    "Safe defaults: online-mode=true  ·  whitelist={}  ·  {} players  ·  {}",
                    template.server.whitelist, template.server.max_players, template.name
                )
            })
            .unwrap_or_else(|| "Select a template to see its safe defaults.".into());
        let create_enabled = self.can_create(cx);
        div()
            .absolute()
            .inset_0()
            .bg(rgba(0xCC0D1016))
            .key_context("MineDockWizard")
            .on_action(cx.listener(Self::cancel_wizard_action))
            .on_action(cx.listener(Self::create_world_action))
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .w(px(560.))
                    .p(px(24.))
                    .rounded(px(14.))
                    .bg(rgb(0x202631))
                    .flex()
                    .flex_col()
                    .gap(px(14.))
                    .child(div().text_size(px(22.)).child("Create a World"))
                    .child(label("Name"))
                    .child(
                        div()
                            .w_full()
                            .rounded(px(7.))
                            .bg(rgb(0x11151D))
                            .border_1()
                            .border_color(rgb(0x424B5B))
                            .child(self.name_input.clone()),
                    )
                    .child(label("Template"))
                    .child(templates)
                    .child(label("Minecraft"))
                    .child(
                        div()
                            .text_color(rgb(0xB9C0CC))
                            .child("Vanilla · current release"),
                    )
                    .child(div().text_color(rgb(0x91A0B7)).child(summary))
                    .when_some(self.wizard_error.clone(), |element, error| {
                        element.child(div().text_color(rgb(0xFF9B9B)).child(error))
                    })
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap(px(10.))
                            .child(button(
                                "Cancel",
                                rgb(0x303744),
                                true,
                                cx.listener(Self::cancel_wizard),
                            ))
                            .child(button(
                                "Create",
                                rgb(0x3D5A88),
                                create_enabled,
                                cx.listener(Self::create_world),
                            )),
                    ),
            )
    }
}

impl Render for MineDockView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut content = div()
            .size_full()
            .bg(rgb(0x111318))
            .text_color(rgb(0xF2F4F8))
            .p(px(28.))
            .flex()
            .flex_col()
            .gap(px(20.))
            .child(self.render_header(cx))
            .child(div().text_size(px(20.)).child("Worlds"))
            .child(self.render_library());
        if let Some(error) = &self.startup_error {
            content = content.child(
                div()
                    .p(px(12.))
                    .rounded(px(8.))
                    .bg(rgb(0x42262B))
                    .text_color(rgb(0xFFB4B4))
                    .child(format!(
                        "Startup error — {error}. Mutating actions are disabled."
                    )),
            );
        }
        if self.wizard_open {
            content = content.child(self.render_wizard(cx));
        }
        content
    }
}

fn label(text: &'static str) -> impl IntoElement {
    div().text_color(rgb(0xB9C0CC)).child(text)
}

fn button(
    text: &'static str,
    color: gpui::Rgba,
    enabled: bool,
    handler: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let mut button = div()
        .id(text)
        .px(px(14.))
        .py(px(8.))
        .rounded(px(8.))
        .bg(if enabled { color } else { rgb(0x242933) })
        .text_color(if enabled {
            rgb(0xF2F4F8)
        } else {
            rgb(0x727A88)
        })
        .child(text);
    if enabled {
        button = button.focusable().on_click(handler);
    }
    button
}

fn world_card(world: &World) -> impl IntoElement {
    let (indicator, status_color) = match world.status {
        WorldStatus::Stopped => ("○", rgb(0xA1A8B5)),
        WorldStatus::Running => ("●", rgb(0x7BE0A2)),
        WorldStatus::Failed => ("!", rgb(0xFF9B9B)),
        _ => ("◌", rgb(0xF0C674)),
    };
    let status = format!("{indicator}  {:?}", world.status);
    div()
        .w_full()
        .p(px(18.))
        .rounded(px(12.))
        .bg(rgb(0x1A1E25))
        .flex()
        .justify_between()
        .items_center()
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(div().text_size(px(18.)).child(world.name.clone()))
                .child(
                    div()
                        .text_color(rgb(0x9CA3AF))
                        .child(format!("Vanilla · {}", world.server.version)),
                )
                .child(div().text_color(status_color).child(status))
                .child(
                    div()
                        .text_color(rgb(0x707987))
                        .child("Runtime unavailable in this build · Start disabled"),
                ),
        )
        .child(
            div()
                .px(px(14.))
                .py(px(8.))
                .rounded(px(8.))
                .bg(rgb(0x292E38))
                .text_color(rgb(0x777F8D))
                .child("START (unavailable)"),
        )
}

fn main() {
    Application::new().run(|cx: &mut App| {
        cx.bind_keys([
            KeyBinding::new("backspace", Backspace, Some("TextInput")),
            KeyBinding::new("delete", Delete, Some("TextInput")),
            KeyBinding::new("left", Left, Some("TextInput")),
            KeyBinding::new("right", Right, Some("TextInput")),
            KeyBinding::new("shift-left", SelectLeft, Some("TextInput")),
            KeyBinding::new("shift-right", SelectRight, Some("TextInput")),
            KeyBinding::new("home", Home, Some("TextInput")),
            KeyBinding::new("end", End, Some("TextInput")),
            KeyBinding::new("ctrl-a", SelectAll, Some("TextInput")),
            KeyBinding::new("ctrl-c", Copy, Some("TextInput")),
            KeyBinding::new("ctrl-v", Paste, Some("TextInput")),
            KeyBinding::new("ctrl-x", Cut, Some("TextInput")),
            KeyBinding::new("enter", CreateWorldAction, Some("TextInput")),
            KeyBinding::new("escape", CancelWizardAction, Some("MineDockWizard")),
        ]);
        let bounds = Bounds::centered(None, size(px(900.), px(650.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                focus: true,
                ..Default::default()
            },
            |_, cx| {
                let name_input = cx.new(TextInput::new);
                cx.new(|cx| MineDockView::new(name_input, cx))
            },
        )
        .expect("failed to open MineDock window");
        cx.on_window_closed(|cx| cx.quit()).detach();
        cx.activate(true);
    });
}

#[cfg(test)]
mod tests {
    use super::utf16_range_to_byte_range;

    #[test]
    fn ime_range_handles_non_ascii_prefixes() {
        assert_eq!(utf16_range_to_byte_range("é😀abc", 1..3), 2..6);
    }

    #[test]
    fn ime_range_handles_surrogate_pairs() {
        assert_eq!(utf16_range_to_byte_range("A😀B", 1..3), 1..5);
        // An offset inside the emoji is rounded to safe UTF-8 boundaries.
        assert_eq!(utf16_range_to_byte_range("A😀B", 2..3), 1..5);
    }

    #[test]
    fn ime_range_clamps_out_of_range_and_reversed_input() {
        assert_eq!(utf16_range_to_byte_range("abc", 90..120), 3..3);
        let reversed = std::ops::Range { start: 5, end: 2 };
        assert_eq!(utf16_range_to_byte_range("abcdef", reversed), 2..5);
    }
}
