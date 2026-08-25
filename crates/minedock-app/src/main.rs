use std::{
    collections::HashMap,
    ops::Range,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Sender},
    },
    time::Duration,
};

use gpui::{
    App, Application, Bounds, Context, Element, ElementId, ElementInputHandler, Entity,
    EntityInputHandler, FocusHandle, Focusable, GlobalElementId, KeyBinding, LayoutId, MouseButton,
    PaintQuad, Pixels, Point, ScrollHandle, ShapedLine, SharedString, Style, TextRun,
    UTF16Selection, Window, WindowBounds, WindowOptions, actions, div, fill, prelude::*, px,
    relative, rgb, rgba, size,
};
use minedock_core::{
    CreateWorldRequest, EulaAcceptanceRepository, FileEulaAcceptanceRepository, JavaReadiness,
    JavaRuntime, JsonWorldRepository, LaunchSpec, LifecycleSupervisor, MineDockError,
    OFFICIAL_EULA_URL, ProcessExit, SessionId, StopEscalationToken, StopOutcome, TemplateCatalog,
    TemplateId, World, WorldId, WorldLibrary, WorldStatus, normalize_world_name,
};

#[allow(dead_code)]
mod http_transport;
mod java_adapter;
mod lifecycle_adapter;
mod localization;
mod native_process;
mod native_safety;
mod network;

use localization::{JavaStatus, Language, UiAction, UiText};
use network::LanAddressState;

type AppLifecycle = LifecycleSupervisor<
    lifecycle_adapter::JsonLifecyclePersistence,
    native_process::NativeProcessFactory,
    lifecycle_adapter::AppDataLeaseProvider,
>;
type LifecyclePollResult = (WorldId, minedock_core::Result<Option<ProcessExit>>);
type LifecyclePollResults = Vec<LifecyclePollResult>;

struct PreparedLaunch {
    launch_spec: LaunchSpec,
    java_status: JavaStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartProgress {
    Starting,
}

const STOP_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LifecycleOperation {
    Starting(WorldId),
    Stopping(WorldId),
    ForceStopping(WorldId),
    Polling,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorldAction {
    Start,
    Stop,
    ForceStop,
    None,
}

fn prepare_launch(
    app_data_root: &Path,
    world: &World,
    eula: &FileEulaAcceptanceRepository,
) -> minedock_core::Result<PreparedLaunch> {
    let provider = http_transport::production_vanilla_provider(app_data_root.join("downloads"))?;
    let resolved = provider.resolve_version(&world.server.version_selector()?)?;
    let readiness = java_adapter::discover_java_off_event_loop(resolved.java_requirement)
        .recv()
        .map_err(|_| {
            MineDockError::JavaUnavailable("Java readiness probe did not return a result".into())
        })?;
    let (runtime, java_status) = select_java_runtime(readiness)?;
    let artifact = provider.acquire_server(&resolved, eula)?;
    let provisioned =
        provider.provision_server_at(app_data_root, world, &resolved, &artifact, eula)?;
    let launch_spec = LaunchSpec::new(&runtime, &provisioned, &world.server, world.id)?;
    Ok(PreparedLaunch {
        launch_spec,
        java_status,
    })
}

fn select_java_runtime(
    readiness: JavaReadiness,
) -> minedock_core::Result<(JavaRuntime, JavaStatus)> {
    let java_status = JavaStatus::from_readiness(&readiness);
    let runtime = readiness.runtime.clone().ok_or_else(|| {
        let status_text = readiness.status_text();
        MineDockError::JavaUnavailable(format!(
            "{status_text}. Choose a compatible Java executable and retry."
        ))
    })?;
    Ok((runtime, java_status))
}

fn eula_requires_confirmation<E: EulaAcceptanceRepository>(
    repository: &E,
) -> minedock_core::Result<bool> {
    Ok(!repository.is_accepted()?)
}

fn execute_start(
    lifecycle: AppLifecycle,
    app_data_root: PathBuf,
    world: World,
    eula: FileEulaAcceptanceRepository,
    progress: Sender<StartProgress>,
) -> (
    AppLifecycle,
    Option<JavaStatus>,
    minedock_core::Result<SessionId>,
) {
    let mut java_status = None;
    let (lifecycle, result) = execute_lifecycle_start(lifecycle, &app_data_root, world.id, || {
        let prepared = prepare_launch(&app_data_root, &world, &eula)?;
        java_status = Some(prepared.java_status);
        let _ = progress.send(StartProgress::Starting);
        Ok(prepared.launch_spec)
    });
    (lifecycle, java_status, result)
}

fn start_allowed(
    status: WorldStatus,
    lifecycle_available: bool,
    operation_active: bool,
    session_active: bool,
) -> bool {
    lifecycle_available && !operation_active && !session_active && status == WorldStatus::Stopped
}

fn stop_allowed(
    status: WorldStatus,
    lifecycle_available: bool,
    operation_active: bool,
    session_active: bool,
) -> bool {
    lifecycle_available && !operation_active && session_active && status == WorldStatus::Running
}

fn poll_exit_allowed(
    active_operation: Option<LifecycleOperation>,
    pending_force_stop_world: Option<WorldId>,
) -> bool {
    match active_operation {
        None => true,
        Some(LifecycleOperation::Stopping(world_id)) => pending_force_stop_world == Some(world_id),
        Some(LifecycleOperation::Starting(_))
        | Some(LifecycleOperation::ForceStopping(_))
        | Some(LifecycleOperation::Polling) => false,
    }
}

fn execute_lifecycle_start<P, F, L, Resolve>(
    mut lifecycle: LifecycleSupervisor<P, F, L>,
    app_data_root: &Path,
    world_id: WorldId,
    resolve_launch: Resolve,
) -> (
    LifecycleSupervisor<P, F, L>,
    minedock_core::Result<SessionId>,
)
where
    P: minedock_core::LifecyclePersistence,
    F: minedock_core::ProcessFactory,
    L: minedock_core::LifecycleLeaseProvider,
    Resolve: FnOnce() -> minedock_core::Result<LaunchSpec>,
{
    let result = lifecycle.start_checked(
        app_data_root,
        world_id,
        WorldStatus::Stopped,
        resolve_launch,
    );
    (lifecycle, result)
}

fn execute_lifecycle_stop<P, F, L>(
    mut lifecycle: LifecycleSupervisor<P, F, L>,
    world_id: WorldId,
) -> (
    LifecycleSupervisor<P, F, L>,
    minedock_core::Result<Option<StopOutcome>>,
)
where
    P: minedock_core::LifecyclePersistence,
    F: minedock_core::ProcessFactory,
    L: minedock_core::LifecycleLeaseProvider,
{
    let result = lifecycle.stop(world_id, STOP_TIMEOUT);
    (lifecycle, result)
}

fn execute_lifecycle_force_stop<P, F, L>(
    mut lifecycle: LifecycleSupervisor<P, F, L>,
    world_id: WorldId,
    token: StopEscalationToken,
) -> (
    LifecycleSupervisor<P, F, L>,
    minedock_core::Result<ProcessExit>,
)
where
    P: minedock_core::LifecyclePersistence,
    F: minedock_core::ProcessFactory,
    L: minedock_core::LifecycleLeaseProvider,
{
    let result = lifecycle.force_terminate(world_id, token);
    (lifecycle, result)
}

fn execute_lifecycle_poll<P, F, L>(
    mut lifecycle: LifecycleSupervisor<P, F, L>,
    world_ids: impl IntoIterator<Item = WorldId>,
) -> (LifecycleSupervisor<P, F, L>, LifecyclePollResults)
where
    P: minedock_core::LifecyclePersistence,
    F: minedock_core::ProcessFactory,
    L: minedock_core::LifecycleLeaseProvider,
{
    let results = world_ids
        .into_iter()
        .map(|world_id| {
            let result = lifecycle.poll_exit(world_id);
            (world_id, result)
        })
        .collect();
    (lifecycle, results)
}

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
        CancelEulaAction,
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

const EULA_ACCEPTANCE_FILE: &str = "eula-acceptance.json";

fn eula_acceptance_path(app_data_root: &Path) -> PathBuf {
    app_data_root.join(EULA_ACCEPTANCE_FILE)
}

fn open_external_url(url: &str) -> std::io::Result<()> {
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = Command::new("rundll32.exe");
        command.args(["url.dll,FileProtocolHandler", url]);
        command
    };

    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = Command::new("open");
        command.arg(url);
        command
    };

    #[cfg(target_os = "linux")]
    let mut command = {
        let mut command = Command::new("xdg-open");
        command.arg(url);
        command
    };

    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    return Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "opening external URLs is unsupported on this platform",
    ));

    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
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
    _lifecycle: Option<AppLifecycle>,
    catalog: Option<TemplateCatalog>,
    startup_error: Option<String>,
    name_input: Entity<TextInput>,
    wizard_open: bool,
    selected_template: Option<TemplateId>,
    wizard_error: Option<String>,
    java_status: JavaStatus,
    eula_open: bool,
    pending_start: Option<WorldId>,
    eula_error: Option<String>,
    active_operation: Option<LifecycleOperation>,
    pending_force_stop: Option<(WorldId, StopEscalationToken)>,
    status_overrides: HashMap<WorldId, WorldStatus>,
    lifecycle_error: Option<String>,
    lan_address_state: LanAddressState,
    clipboard_notice: Option<String>,
    language: Language,
    settings_error: Option<String>,
    world_scroll: ScrollHandle,
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
                    java_status: JavaStatus::Unavailable { reason: None },
                    eula_open: false,
                    pending_start: None,
                    eula_error: None,
                    active_operation: None,
                    pending_force_stop: None,
                    status_overrides: HashMap::new(),
                    lifecycle_error: None,
                    lan_address_state: LanAddressState::Checking,
                    clipboard_notice: None,
                    language: Language::English,
                    settings_error: None,
                    world_scroll: ScrollHandle::new(),
                };
            }
        };
        let mut startup_error = None;
        let mut language = Language::English;
        let mut settings_error = None;
        let mut app_data_lease = None;
        let mut lifecycle = None;
        let library = match resolve_app_data_root() {
            Ok(root) => {
                let repository = JsonWorldRepository::new(root);
                match lifecycle_adapter::AppDataLease::acquire(repository.root()) {
                    Ok(lease) => {
                        app_data_lease = Some(lease);
                        match localization::load_language(repository.root()) {
                            Ok(value) => language = value,
                            Err(error) => settings_error = Some(error),
                        }
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
        name_input.update(cx, |input, _| {
            input.placeholder = language.text(UiText::NamePlaceholder).into();
        });
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
                    view.java_status = JavaStatus::detected_from_readiness(&readiness);
                    cx.notify();
                });
            }
        })
        .detach();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                if this.update(cx, |view, cx| view.poll_exit_once(cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        cx.spawn(async move |this, cx| {
            loop {
                let state = cx
                    .background_executor()
                    .spawn(async { network::discover_lan_address_state() })
                    .await;
                if this
                    .update(cx, |view, cx| {
                        view.lan_address_state = state;
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
                cx.background_executor().timer(Duration::from_secs(5)).await;
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
            java_status: JavaStatus::Checking,
            eula_open: false,
            pending_start: None,
            eula_error: None,
            active_operation: None,
            pending_force_stop: None,
            status_overrides: HashMap::new(),
            lifecycle_error: None,
            lan_address_state: LanAddressState::Checking,
            clipboard_notice: None,
            language,
            settings_error,
            world_scroll: ScrollHandle::new(),
        }
    }

    fn eula_repository(&self) -> Option<FileEulaAcceptanceRepository> {
        self.library.as_ref().map(|library| {
            FileEulaAcceptanceRepository::new(eula_acceptance_path(library.repository().root()))
        })
    }

    /// Open the explicit EULA step for a pending Start request. The actual
    /// acceptance is recorded only by `accept_eula` after the user clicks
    /// `I Agree`.
    fn request_eula_confirmation(&mut self, world_id: WorldId, cx: &mut Context<Self>) {
        self.pending_start = Some(world_id);
        self.eula_open = true;
        self.eula_error = None;
        self.lifecycle_error = None;
        cx.notify();
    }

    fn open_eula_link(&mut self, _: &gpui::ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        if let Err(error) = open_external_url(OFFICIAL_EULA_URL) {
            self.eula_error = Some(format!(
                "Could not open the official Minecraft EULA: {error}. Visit {OFFICIAL_EULA_URL}"
            ));
            cx.notify();
        }
    }

    fn cancel_eula(&mut self, _: &gpui::ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.eula_open = false;
        self.pending_start = None;
        self.eula_error = None;
        cx.notify();
    }

    fn cancel_eula_action(&mut self, _: &CancelEulaAction, _: &mut Window, cx: &mut Context<Self>) {
        self.eula_open = false;
        self.pending_start = None;
        self.eula_error = None;
        cx.notify();
    }

    fn accept_eula(&mut self, _: &gpui::ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(repository) = self.eula_repository() else {
            self.eula_error = Some("MineDock's app-data directory is unavailable.".into());
            cx.notify();
            return;
        };
        match repository.record_explicit_acceptance(true) {
            Ok(Some(_)) => {
                self.eula_open = false;
                self.eula_error = None;
                if let Some(world_id) = self.pending_start.take() {
                    self.begin_start(world_id, cx);
                } else {
                    cx.notify();
                }
            }
            Ok(None) => {
                self.eula_error = Some(
                    "EULA acceptance was not recorded because no affirmative action was provided."
                        .into(),
                );
                cx.notify();
            }
            Err(error) => {
                self.eula_error = Some(format!(
                    "MineDock could not save your EULA acceptance: {error}"
                ));
                cx.notify();
            }
        }
    }

    fn displayed_status(&self, world: &World) -> WorldStatus {
        self.status_overrides
            .get(&world.id)
            .copied()
            .unwrap_or(world.status)
    }

    fn can_start_world(&self, world_id: WorldId) -> bool {
        let Some(world) = self
            .library
            .as_ref()
            .and_then(|library| library.get(world_id))
        else {
            return false;
        };
        let session_active = self
            ._lifecycle
            .as_ref()
            .is_some_and(|lifecycle| lifecycle.session_id(world_id).is_some());
        start_allowed(
            world.status,
            self.can_mutate() && self._lifecycle.is_some(),
            self.active_operation.is_some(),
            session_active,
        )
    }

    fn can_stop_world(&self, world_id: WorldId) -> bool {
        let Some(world) = self
            .library
            .as_ref()
            .and_then(|library| library.get(world_id))
        else {
            return false;
        };
        let session_active = self
            ._lifecycle
            .as_ref()
            .is_some_and(|lifecycle| lifecycle.session_id(world_id).is_some());
        stop_allowed(
            world.status,
            self.can_mutate() && self._lifecycle.is_some(),
            self.active_operation.is_some(),
            session_active,
        )
    }

    fn can_force_stop_world(&self, world_id: WorldId) -> bool {
        self.pending_force_stop
            .as_ref()
            .is_some_and(|(pending_world_id, _)| *pending_world_id == world_id)
            && self.active_operation == Some(LifecycleOperation::Stopping(world_id))
            && self._lifecycle.is_some()
    }

    fn poll_exit_once(&mut self, cx: &mut Context<Self>) {
        let pending_force_stop_world = self
            .pending_force_stop
            .as_ref()
            .map(|(world_id, _)| *world_id);
        if !poll_exit_allowed(self.active_operation, pending_force_stop_world) {
            return;
        }
        let Some(lifecycle) = self._lifecycle.take() else {
            return;
        };
        let world_ids: Vec<_> = self
            .library
            .as_ref()
            .map(|library| {
                library
                    .list()
                    .iter()
                    .filter(|world| {
                        world.status == WorldStatus::Running
                            || pending_force_stop_world == Some(world.id)
                    })
                    .filter(|world| lifecycle.session_id(world.id).is_some())
                    .map(|world| world.id)
                    .collect()
            })
            .unwrap_or_default();
        if world_ids.is_empty() {
            self._lifecycle = Some(lifecycle);
            return;
        }

        self.active_operation = Some(LifecycleOperation::Polling);
        let worker = cx
            .background_executor()
            .spawn(async move { execute_lifecycle_poll(lifecycle, world_ids) });
        cx.spawn(async move |this, cx| {
            let (lifecycle, results) = worker.await;
            let _ = this.update(cx, |view, cx| {
                view._lifecycle = Some(lifecycle);
                let mut errors = Vec::new();
                let mut should_reload = false;
                let mut late_stop_completed = false;
                let mut pending_force_stop_is_active = false;
                let pending_force_stop_world = view
                    .pending_force_stop
                    .as_ref()
                    .map(|(world_id, _)| *world_id);
                for (world_id, result) in results {
                    match result {
                        Ok(Some(exit)) => {
                            should_reload = true;
                            view.status_overrides.remove(&world_id);
                            if pending_force_stop_world == Some(world_id) {
                                view.pending_force_stop = None;
                                if exit.success {
                                    late_stop_completed = true;
                                } else {
                                    errors.push(format!(
                                        "World process exited after graceful stop timed out (code {:?}); it was marked Failed.",
                                        exit.code
                                    ));
                                }
                            } else {
                                errors.push(format!(
                                    "World process exited unexpectedly (code {:?}; success={}); it was marked Failed.",
                                    exit.code, exit.success
                                ));
                            }
                        }
                        Ok(None) => {
                            if pending_force_stop_world == Some(world_id) {
                                pending_force_stop_is_active = true;
                            }
                        }
                        Err(error) => {
                            should_reload = true;
                            if pending_force_stop_world == Some(world_id) {
                                pending_force_stop_is_active = true;
                            }
                            errors.push(format!(
                                "Could not observe world process state: {error}"
                            ));
                        }
                    }
                }
                if pending_force_stop_is_active {
                    if let Some((world_id, _)) = view.pending_force_stop {
                        view.active_operation = Some(LifecycleOperation::Stopping(world_id));
                    } else {
                        view.active_operation = None;
                    }
                } else {
                    view.active_operation = None;
                }
                if !errors.is_empty() {
                    view.lifecycle_error = Some(errors.join(" "));
                } else if late_stop_completed {
                    view.lifecycle_error = None;
                }
                if should_reload {
                    if let Some(library) = view.library.as_mut() {
                        if let Err(error) = library.reload() {
                            let refresh_error = format!(
                                "MineDock could not refresh the persisted world state: {error}"
                            );
                            view.lifecycle_error = Some(match view.lifecycle_error.take() {
                                Some(existing) => format!("{existing} {refresh_error}"),
                                None => refresh_error,
                            });
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn start_world(
        &mut self,
        world_id: WorldId,
        _: &gpui::ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.can_start_world(world_id) {
            return;
        }
        let Some(repository) = self.eula_repository() else {
            self.lifecycle_error = Some("MineDock's app-data directory is unavailable.".into());
            cx.notify();
            return;
        };
        match eula_requires_confirmation(&repository) {
            Ok(false) => self.begin_start(world_id, cx),
            Ok(true) => self.request_eula_confirmation(world_id, cx),
            Err(error) => {
                self.lifecycle_error = Some(format!(
                    "MineDock could not check Minecraft EULA acceptance: {error}"
                ));
                cx.notify();
            }
        }
    }

    fn begin_start(&mut self, world_id: WorldId, cx: &mut Context<Self>) {
        if !self.can_start_world(world_id) {
            self.lifecycle_error = Some(
                "This world is no longer stopped or already has an active lifecycle operation."
                    .into(),
            );
            cx.notify();
            return;
        }
        let Some(library) = self.library.as_ref() else {
            self.lifecycle_error = Some("MineDock's world library is unavailable.".into());
            cx.notify();
            return;
        };
        let Some(world) = library.get(world_id).cloned() else {
            self.lifecycle_error = Some("The selected world no longer exists.".into());
            cx.notify();
            return;
        };
        let app_data_root = library.repository().root().to_path_buf();
        let Some(eula) = self.eula_repository() else {
            self.lifecycle_error = Some("MineDock's app-data directory is unavailable.".into());
            cx.notify();
            return;
        };
        let Some(lifecycle) = self._lifecycle.take() else {
            self.lifecycle_error = Some("The lifecycle runtime is unavailable.".into());
            cx.notify();
            return;
        };

        self.active_operation = Some(LifecycleOperation::Starting(world_id));
        self.status_overrides
            .insert(world_id, WorldStatus::Preparing);
        self.lifecycle_error = None;
        cx.notify();

        let (progress_sender, progress_receiver) = mpsc::channel();
        let completed = Arc::new(AtomicBool::new(false));
        let completed_by_worker = completed.clone();
        let worker = cx.background_executor().spawn(async move {
            let result = execute_start(lifecycle, app_data_root, world, eula, progress_sender);
            completed_by_worker.store(true, Ordering::Release);
            result
        });

        let progress_completed = completed.clone();
        cx.spawn(async move |this, cx| {
            loop {
                let mut drained = false;
                while let Ok(progress) = progress_receiver.try_recv() {
                    drained = true;
                    let _ = this.update(cx, |view, cx| {
                        if view.active_operation != Some(LifecycleOperation::Starting(world_id)) {
                            return;
                        }
                        match progress {
                            StartProgress::Starting => {
                                view.status_overrides
                                    .insert(world_id, WorldStatus::Starting);
                            }
                        }
                        cx.notify();
                    });
                }
                if progress_completed.load(Ordering::Acquire) && !drained {
                    break;
                }
                cx.background_executor()
                    .timer(Duration::from_millis(50))
                    .await;
            }
        })
        .detach();

        cx.spawn(async move |this, cx| {
            let (lifecycle, java_status, result) = worker.await;
            let _ = this.update(cx, |view, cx| {
                view._lifecycle = Some(lifecycle);
                view.active_operation = None;
                view.status_overrides.remove(&world_id);
                if let Some(java_status) = java_status {
                    view.java_status = java_status;
                }
                match result {
                    Ok(_) => {
                        view.lifecycle_error = None;
                    }
                    Err(error) => {
                        view.lifecycle_error = Some(format!("Could not start this world: {error}"));
                    }
                }
                if let Some(library) = view.library.as_mut() {
                    if let Err(error) = library.reload() {
                        let refresh_error = format!(
                            "MineDock could not refresh the persisted world state: {error}"
                        );
                        view.lifecycle_error = Some(match view.lifecycle_error.take() {
                            Some(existing) => format!("{existing} {refresh_error}"),
                            None => refresh_error,
                        });
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn stop_world(
        &mut self,
        world_id: WorldId,
        _: &gpui::ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.can_stop_world(world_id) {
            return;
        }
        if self.library.is_none() {
            self.lifecycle_error = Some("MineDock's world library is unavailable.".into());
            cx.notify();
            return;
        }
        let Some(lifecycle) = self._lifecycle.take() else {
            self.lifecycle_error = Some("The lifecycle runtime is unavailable.".into());
            cx.notify();
            return;
        };

        self.active_operation = Some(LifecycleOperation::Stopping(world_id));
        self.pending_force_stop = None;
        self.status_overrides
            .insert(world_id, WorldStatus::Stopping);
        self.lifecycle_error = None;
        cx.notify();

        let worker = cx
            .background_executor()
            .spawn(async move { execute_lifecycle_stop(lifecycle, world_id) });
        cx.spawn(async move |this, cx| {
            let (lifecycle, result) = worker.await;
            let _ = this.update(cx, |view, cx| {
                view._lifecycle = Some(lifecycle);
                match result {
                    Ok(Some(StopOutcome::Exited { exit })) => {
                        view.active_operation = None;
                        view.pending_force_stop = None;
                        view.status_overrides.remove(&world_id);
                        view.lifecycle_error = if exit.success {
                            None
                        } else {
                            Some(format!(
                                "World stop completed with a failure (exit code {:?}); it was marked Failed.",
                                exit.code
                            ))
                        };
                    }
                    Ok(Some(StopOutcome::TimedOut(token))) => {
                        view.active_operation = Some(LifecycleOperation::Stopping(world_id));
                        view.pending_force_stop = Some((world_id, token));
                        view.status_overrides
                            .insert(world_id, WorldStatus::Stopping);
                        view.lifecycle_error = Some(
                            "Graceful stop timed out; the server is still running. Use Force Stop only if you accept possible data loss.".into(),
                        );
                    }
                    Ok(None) => {
                        view.active_operation = None;
                        view.pending_force_stop = None;
                        view.status_overrides.remove(&world_id);
                        view.lifecycle_error = Some(
                            "MineDock could not stop this world because no active lifecycle session was found.".into(),
                        );
                    }
                    Err(error) => {
                        view.active_operation = None;
                        view.pending_force_stop = None;
                        view.status_overrides.remove(&world_id);
                        view.lifecycle_error =
                            Some(format!("Could not stop this world: {error}"));
                    }
                }
                if let Some(library) = view.library.as_mut() {
                    if let Err(error) = library.reload() {
                        let refresh_error = format!(
                            "MineDock could not refresh the persisted world state: {error}"
                        );
                        view.lifecycle_error = Some(match view.lifecycle_error.take() {
                            Some(existing) => format!("{existing} {refresh_error}"),
                            None => refresh_error,
                        });
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn force_stop_world(
        &mut self,
        world_id: WorldId,
        _: &gpui::ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.can_force_stop_world(world_id) {
            return;
        }
        let Some((_, token)) = self.pending_force_stop else {
            return;
        };
        let Some(lifecycle) = self._lifecycle.take() else {
            self.lifecycle_error = Some("The lifecycle runtime is unavailable.".into());
            cx.notify();
            return;
        };

        self.active_operation = Some(LifecycleOperation::ForceStopping(world_id));
        self.lifecycle_error = Some("Force stop is terminating the server process…".into());
        cx.notify();

        let worker = cx
            .background_executor()
            .spawn(async move { execute_lifecycle_force_stop(lifecycle, world_id, token) });
        cx.spawn(async move |this, cx| {
            let (lifecycle, result) = worker.await;
            let _ = this.update(cx, |view, cx| {
                view._lifecycle = Some(lifecycle);
                match result {
                    Ok(exit) => {
                        view.active_operation = None;
                        view.pending_force_stop = None;
                        view.status_overrides.remove(&world_id);
                        view.lifecycle_error = Some(format!(
                            "The server was force-stopped and marked Failed (exit code {:?}).",
                            exit.code
                        ));
                    }
                    Err(error) => {
                        view.active_operation = Some(LifecycleOperation::Stopping(world_id));
                        view.lifecycle_error =
                            Some(format!("Could not force-stop this world: {error}"));
                    }
                }
                if let Some(library) = view.library.as_mut() {
                    if let Err(error) = library.reload() {
                        let refresh_error = format!(
                            "MineDock could not refresh the persisted world state: {error}"
                        );
                        view.lifecycle_error = Some(match view.lifecycle_error.take() {
                            Some(existing) => format!("{existing} {refresh_error}"),
                            None => refresh_error,
                        });
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn copy_address(
        &mut self,
        endpoint: String,
        _: &gpui::ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(endpoint.clone()));
        self.clipboard_notice = Some(self.language.copy_notice(&endpoint));
        cx.notify();
    }

    fn set_language(
        &mut self,
        language: Language,
        _: &gpui::ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.can_change_language() {
            return;
        }
        if self.language == language && self.settings_error.is_none() {
            return;
        }
        if self.language != language {
            self.language = language;
            self.name_input.update(cx, |input, _| {
                input.placeholder = language.text(UiText::NamePlaceholder).into();
            });
        }
        if let (Some(library), Some(_lease)) =
            (self.library.as_ref(), self._app_data_lease.as_ref())
        {
            match localization::save_language(library.repository().root(), language) {
                Ok(()) => self.settings_error = None,
                Err(error) => self.settings_error = Some(error),
            }
        }
        cx.notify();
    }

    fn can_change_language(&self) -> bool {
        self.library.is_some() && self._app_data_lease.is_some()
    }

    fn render_connection(
        &mut self,
        world_id: WorldId,
        status: WorldStatus,
        port: u16,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let mut section = div().flex().flex_col().gap(px(5.));
        if status != WorldStatus::Running {
            return section;
        }

        section = section.child(
            div()
                .text_color(rgb(0xB9C0CC))
                .child(self.language.text(UiText::LanConnection)),
        );
        if port == 0 {
            return section.child(
                div()
                    .text_color(rgb(0xFFB4B4))
                    .child(self.language.text(UiText::EndpointInvalid)),
            );
        }

        match self.lan_address_state.clone() {
            LanAddressState::Checking => section.child(
                div()
                    .text_color(rgb(0x91A0B7))
                    .child(self.language.text(UiText::CheckingLan)),
            ),
            LanAddressState::Unavailable(reason) => section.child(
                div()
                    .text_color(rgb(0xFFB4B4))
                    .child(self.language.lan_unavailable(&reason)),
            ),
            LanAddressState::Available(candidate) => {
                let endpoint = candidate
                    .endpoint(port)
                    .expect("nonzero port was checked above");
                let endpoint_for_copy = endpoint.clone();
                let handler = cx.listener(move |view, event, window, cx| {
                    view.copy_address(endpoint_for_copy.clone(), event, window, cx)
                });
                section.child(connection_endpoint_row(
                    world_id,
                    &endpoint,
                    &candidate.interface_name,
                    self.language,
                    handler,
                ))
            }
            LanAddressState::Ambiguous(candidates) => {
                section = section.child(
                    div()
                        .text_color(rgb(0xF0C674))
                        .child(self.language.text(UiText::MultipleLan)),
                );
                for candidate in candidates {
                    let endpoint = candidate
                        .endpoint(port)
                        .expect("nonzero port was checked above");
                    let endpoint_for_copy = endpoint.clone();
                    let handler = cx.listener(move |view, event, window, cx| {
                        view.copy_address(endpoint_for_copy.clone(), event, window, cx)
                    });
                    section = section.child(connection_endpoint_row(
                        world_id,
                        &endpoint,
                        &candidate.interface_name,
                        self.language,
                        handler,
                    ));
                }
                section
            }
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
        let language = self.language;
        let can_mutate = self.can_mutate();
        let can_change_language = self.can_change_language();
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
            .child(div().text_color(rgb(0x9CA3AF)).child(format!(
                "{}  ·  {}",
                language.text(UiText::LocalLibrary),
                language.java_status(&self.java_status)
            )))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(4.))
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(rgb(0x9CA3AF))
                                    .child(language.text(UiText::Language)),
                            )
                            .child(
                                div()
                                    .id("language-english")
                                    .px(px(7.))
                                    .py(px(5.))
                                    .rounded(px(6.))
                                    .bg(if language == Language::English {
                                        rgb(0x3D5A88)
                                    } else if can_change_language {
                                        rgb(0x292E38)
                                    } else {
                                        rgb(0x252932)
                                    })
                                    .text_color(if can_change_language {
                                        rgb(0xE7EEFF)
                                    } else {
                                        rgb(0x737985)
                                    })
                                    .child(language.text(UiText::English))
                                    .when(can_change_language, |element| {
                                        element.focusable().on_click(cx.listener(
                                            |view, event, window, cx| {
                                                view.set_language(
                                                    Language::English,
                                                    event,
                                                    window,
                                                    cx,
                                                )
                                            },
                                        ))
                                    }),
                            )
                            .child(
                                div()
                                    .id("language-japanese")
                                    .px(px(7.))
                                    .py(px(5.))
                                    .rounded(px(6.))
                                    .bg(if language == Language::Japanese {
                                        rgb(0x3D5A88)
                                    } else if can_change_language {
                                        rgb(0x292E38)
                                    } else {
                                        rgb(0x252932)
                                    })
                                    .text_color(if can_change_language {
                                        rgb(0xE7EEFF)
                                    } else {
                                        rgb(0x737985)
                                    })
                                    .child(language.text(UiText::Japanese))
                                    .when(can_change_language, |element| {
                                        element.focusable().on_click(cx.listener(
                                            |view, event, window, cx| {
                                                view.set_language(
                                                    Language::Japanese,
                                                    event,
                                                    window,
                                                    cx,
                                                )
                                            },
                                        ))
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .id("new-world")
                            .px(px(14.))
                            .py(px(8.))
                            .rounded(px(8.))
                            .bg(if can_mutate {
                                rgb(0x34415C)
                            } else {
                                rgb(0x252932)
                            })
                            .text_color(if can_mutate {
                                rgb(0xE7EEFF)
                            } else {
                                rgb(0x737985)
                            })
                            .child(language.text(UiText::NewWorld))
                            .when(can_mutate, |element| {
                                element.focusable().on_click(cx.listener(Self::open_wizard))
                            }),
                    ),
            )
    }

    fn render_library(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut cards = div().flex().flex_col().gap(px(12.));
        if let Some(library) = &self.library {
            let worlds: Vec<_> = library.list().to_vec();
            for world in worlds {
                let world_id = world.id;
                let status = self.displayed_status(&world);
                let start_enabled = self.can_start_world(world_id);
                let stop_enabled = self.can_stop_world(world_id);
                let force_stop_enabled = self.can_force_stop_world(world_id);
                let connection = self.render_connection(world_id, status, world.server.port, cx);
                let action = match status {
                    WorldStatus::Stopped => WorldAction::Start,
                    WorldStatus::Running => WorldAction::Stop,
                    WorldStatus::Stopping if force_stop_enabled => WorldAction::ForceStop,
                    _ => WorldAction::None,
                };
                let action_enabled = match action {
                    WorldAction::Start => start_enabled,
                    WorldAction::Stop => stop_enabled,
                    WorldAction::ForceStop => true,
                    WorldAction::None => false,
                };
                cards = cards.child(world_card(
                    &world,
                    status,
                    action,
                    action_enabled,
                    connection,
                    self.language,
                    cx.listener(move |view, event, window, cx| match action {
                        WorldAction::Start => view.start_world(world_id, event, window, cx),
                        WorldAction::Stop => view.stop_world(world_id, event, window, cx),
                        WorldAction::ForceStop => {
                            view.force_stop_world(world_id, event, window, cx)
                        }
                        WorldAction::None => {}
                    }),
                ));
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
                    .child(self.language.text(UiText::NoWorlds)),
            );
        }
        div()
            .id("world-library")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .track_scroll(&self.world_scroll)
            .child(cards)
    }

    fn render_wizard(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let language = self.language;
        let mut templates = div().flex().gap(px(8.));
        if let Some(catalog) = &self.catalog {
            for template in catalog.iter() {
                let id = template.id.clone();
                let selected = self.selected_template.as_ref() == Some(&id);
                let template_name = language.template_name(template.id.as_str(), &template.name);
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
                        .child(template_name)
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
                let template_name = language.template_name(template.id.as_str(), &template.name);
                language.safe_defaults(
                    template.server.whitelist,
                    template.server.max_players,
                    &template_name,
                )
            })
            .unwrap_or_else(|| language.text(UiText::SelectTemplate).into());
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
                    .child(
                        div()
                            .text_size(px(22.))
                            .child(language.text(UiText::CreateWorld)),
                    )
                    .child(label(language.text(UiText::Name)))
                    .child(
                        div()
                            .w_full()
                            .rounded(px(7.))
                            .bg(rgb(0x11151D))
                            .border_1()
                            .border_color(rgb(0x424B5B))
                            .child(self.name_input.clone()),
                    )
                    .child(label(language.text(UiText::Template)))
                    .child(templates)
                    .child(label(language.text(UiText::Minecraft)))
                    .child(
                        div()
                            .text_color(rgb(0xB9C0CC))
                            .child(language.text(UiText::VanillaCurrentRelease)),
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
                                language.text(UiText::Cancel),
                                rgb(0x303744),
                                true,
                                cx.listener(Self::cancel_wizard),
                            ))
                            .child(button(
                                language.text(UiText::Create),
                                rgb(0x3D5A88),
                                create_enabled,
                                cx.listener(Self::create_world),
                            )),
                    ),
            )
    }

    fn render_eula_dialog(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let language = self.language;
        div()
            .absolute()
            .inset_0()
            .bg(rgba(0xCC0D1016))
            .key_context("MineDockEula")
            .on_action(cx.listener(Self::cancel_eula_action))
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
                    .child(
                        div()
                            .text_size(px(22.))
                            .child(language.text(UiText::EulaTitle)),
                    )
                    .child(
                        div()
                            .text_color(rgb(0xD3D8E2))
                            .child(language.text(UiText::EulaBody)),
                    )
                    .child(div().text_color(rgb(0x91A0B7)).child(format!(
                        "{}: {OFFICIAL_EULA_URL}",
                        language.text(UiText::OfficialEula)
                    )))
                    .child(button(
                        language.text(UiText::ViewOfficialEula),
                        rgb(0x34415C),
                        true,
                        cx.listener(Self::open_eula_link),
                    ))
                    .when_some(self.eula_error.clone(), |element, error| {
                        element.child(div().text_color(rgb(0xFF9B9B)).child(error))
                    })
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap(px(10.))
                            .child(button(
                                language.text(UiText::Cancel),
                                rgb(0x303744),
                                true,
                                cx.listener(Self::cancel_eula),
                            ))
                            .child(button(
                                language.text(UiText::Agree),
                                rgb(0x3D5A88),
                                true,
                                cx.listener(Self::accept_eula),
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
            .child(
                div()
                    .text_size(px(20.))
                    .child(self.language.text(UiText::Worlds)),
            )
            .child(self.render_library(cx));
        if let Some(error) = &self.startup_error {
            content = content.child(
                div()
                    .p(px(12.))
                    .rounded(px(8.))
                    .bg(rgb(0x42262B))
                    .text_color(rgb(0xFFB4B4))
                    .child(self.language.startup_error(error)),
            );
        }
        if let Some(error) = &self.settings_error {
            content = content.child(
                div()
                    .p(px(12.))
                    .rounded(px(8.))
                    .bg(rgb(0x42262B))
                    .text_color(rgb(0xFFB4B4))
                    .child(self.language.settings_error(error)),
            );
        }
        if let Some(error) = &self.lifecycle_error {
            content = content.child(
                div()
                    .p(px(12.))
                    .rounded(px(8.))
                    .bg(rgb(0x42262B))
                    .text_color(rgb(0xFFB4B4))
                    .child(error.clone()),
            );
        }
        if let Some(notice) = &self.clipboard_notice {
            content = content.child(
                div()
                    .p(px(12.))
                    .rounded(px(8.))
                    .bg(rgb(0x1D3A2A))
                    .text_color(rgb(0xA7F3C0))
                    .child(notice.clone()),
            );
        }
        if self.wizard_open {
            content = content.child(self.render_wizard(cx));
        }
        if self.eula_open {
            content = content.child(self.render_eula_dialog(cx));
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

fn connection_endpoint_row(
    world_id: WorldId,
    endpoint: &str,
    interface_name: &str,
    language: Language,
    handler: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .flex()
        .justify_between()
        .items_center()
        .gap(px(10.))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(div().text_color(rgb(0xE7EEFF)).child(endpoint.to_owned()))
                .child(
                    div()
                        .text_color(rgb(0x707987))
                        .child(language.via(interface_name)),
                ),
        )
        .child(
            div()
                .id(SharedString::from(format!(
                    "copy-address-{world_id}-{endpoint}"
                )))
                .px(px(10.))
                .py(px(6.))
                .rounded(px(7.))
                .bg(rgb(0x34415C))
                .text_color(rgb(0xE7EEFF))
                .child(language.text(UiText::Copy))
                .focusable()
                .on_click(handler),
        )
}

fn world_card(
    world: &World,
    status: WorldStatus,
    action: WorldAction,
    action_enabled: bool,
    connection: impl IntoElement,
    language: Language,
    action_handler: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let (indicator, status_color) = match status {
        WorldStatus::Stopped => ("○", rgb(0xA1A8B5)),
        WorldStatus::Running => ("●", rgb(0x7BE0A2)),
        WorldStatus::Failed => ("!", rgb(0xFF9B9B)),
        _ => ("◌", rgb(0xF0C674)),
    };
    let status_text = format!("{indicator}  {}", language.status(status));
    let action_text = match action {
        WorldAction::Start => language.action(UiAction::Start),
        WorldAction::Stop => language.action(UiAction::Stop),
        WorldAction::ForceStop => language.action(UiAction::ForceStop),
        WorldAction::None => match status {
            WorldStatus::Stopped => language.action(UiAction::Start),
            WorldStatus::Preparing => language.text(UiText::Preparing),
            WorldStatus::Starting => language.text(UiText::Starting),
            WorldStatus::Running => language.action(UiAction::Stop),
            WorldStatus::Stopping => language.text(UiText::Stopping),
            WorldStatus::BackingUp => language.text(UiText::BackingUp),
            WorldStatus::Failed => language.text(UiText::RecoverRequired),
        },
    };
    let action_color = match action {
        WorldAction::Start => rgb(0x3D5A88),
        WorldAction::Stop => rgb(0x8B4A4A),
        WorldAction::ForceStop => rgb(0xA33E3E),
        WorldAction::None => rgb(0x292E38),
    };
    let mut action = div()
        .id(SharedString::from(format!("world-action-{}", world.id)))
        .px(px(14.))
        .py(px(8.))
        .rounded(px(8.))
        .bg(if action_enabled {
            action_color
        } else {
            rgb(0x292E38)
        })
        .text_color(if action_enabled {
            rgb(0xF2F4F8)
        } else {
            rgb(0x777F8D)
        })
        .child(action_text);
    if action_enabled {
        action = action.focusable().on_click(action_handler);
    }
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
                        .child(language.vanilla_version(&world.server.version)),
                )
                .child(div().text_color(status_color).child(status_text))
                .child(connection)
                .child(
                    div()
                        .text_color(rgb(0x707987))
                        .child(language.text(UiText::WorldDescription)),
                ),
        )
        .child(action)
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
            KeyBinding::new("escape", CancelEulaAction, Some("MineDockEula")),
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
    use super::{
        EULA_ACCEPTANCE_FILE, LifecycleOperation, eula_acceptance_path, eula_requires_confirmation,
        execute_lifecycle_force_stop, execute_lifecycle_poll, execute_lifecycle_start,
        execute_lifecycle_stop, poll_exit_allowed, select_java_runtime, start_allowed,
        stop_allowed, utf16_range_to_byte_range,
    };
    use minedock_core::{
        EulaAcceptanceRepository, FileEulaAcceptanceRepository, GracefulStopResult,
        InMemoryLifecyclePersistence, JavaMajor, JavaReadiness, JavaRequirement,
        JavaUnavailableReason, LaunchSpec, LifecycleLeaseProvider, ProcessExit, ProcessFactory,
        ServerEvent, ServerProcess, SessionId, StopEscalationToken, ValidatedServerCommand,
        WorldId, WorldStatus,
    };
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    #[derive(Debug, Default)]
    struct TestLease;

    #[derive(Debug, Default)]
    struct TestLeaseProvider;

    impl LifecycleLeaseProvider for TestLeaseProvider {
        type Lease = TestLease;

        fn acquire(&mut self, _: &Path) -> minedock_core::Result<Self::Lease> {
            Ok(TestLease)
        }
    }

    #[derive(Debug)]
    struct TestProcess {
        world_id: WorldId,
        session_id: SessionId,
        stop_times_out: bool,
        exits_on_poll: bool,
        poll_exit_success: bool,
    }

    impl ServerProcess for TestProcess {
        fn world_id(&self) -> WorldId {
            self.world_id
        }

        fn session_id(&self) -> SessionId {
            self.session_id
        }

        fn pid(&self) -> u32 {
            42
        }

        fn send_command(&mut self, _: &ValidatedServerCommand) -> minedock_core::Result<()> {
            Ok(())
        }

        fn graceful_stop(
            &mut self,
            _: std::time::Duration,
        ) -> minedock_core::Result<GracefulStopResult> {
            if self.stop_times_out {
                return Ok(GracefulStopResult::TimedOut);
            }
            Ok(GracefulStopResult::Exited {
                exit: ProcessExit {
                    code: Some(0),
                    success: true,
                },
            })
        }

        fn force_terminate(
            &mut self,
            _: StopEscalationToken,
        ) -> minedock_core::Result<ProcessExit> {
            Ok(ProcessExit {
                code: Some(1),
                success: false,
            })
        }

        fn force_cleanup_after_start_failure(&mut self) -> minedock_core::Result<ProcessExit> {
            Ok(ProcessExit {
                code: Some(1),
                success: false,
            })
        }

        fn try_wait(&mut self) -> minedock_core::Result<Option<ProcessExit>> {
            Ok(self.exits_on_poll.then_some(if self.poll_exit_success {
                ProcessExit {
                    code: Some(0),
                    success: true,
                }
            } else {
                ProcessExit {
                    code: Some(1),
                    success: false,
                }
            }))
        }

        fn drain_events(&self, _: usize) -> Vec<ServerEvent> {
            Vec::new()
        }
    }

    #[derive(Debug, Default)]
    struct TestProcessFactory {
        stop_times_out: bool,
        exits_on_poll: bool,
        poll_exit_success: bool,
    }

    impl ProcessFactory for TestProcessFactory {
        type Process = TestProcess;

        fn spawn(
            &mut self,
            spec: &LaunchSpec,
            session_id: SessionId,
        ) -> minedock_core::Result<Self::Process> {
            Ok(TestProcess {
                world_id: spec.world_id,
                session_id,
                stop_times_out: self.stop_times_out,
                exits_on_poll: self.exits_on_poll,
                poll_exit_success: self.poll_exit_success,
            })
        }
    }

    fn test_launch_spec(world_id: WorldId) -> LaunchSpec {
        LaunchSpec {
            executable: PathBuf::from("/java"),
            args: vec![OsString::from("-version")],
            current_dir: PathBuf::from("/world"),
            world_id,
        }
    }

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

    #[test]
    fn missing_eula_record_requires_confirmation() {
        let root = TempDir::new().expect("temporary app data");
        let repository = FileEulaAcceptanceRepository::new(eula_acceptance_path(root.path()));

        assert!(
            !repository
                .is_accepted()
                .expect("missing record is readable")
        );
        assert!(eula_requires_confirmation(&repository).expect("EULA state is readable"));
        assert_eq!(EULA_ACCEPTANCE_FILE, "eula-acceptance.json");
    }

    #[test]
    fn only_affirmative_eula_action_persists_acceptance() {
        let root = TempDir::new().expect("temporary app data");
        let repository = FileEulaAcceptanceRepository::new(eula_acceptance_path(root.path()));

        assert!(
            repository
                .record_explicit_acceptance(false)
                .expect("cancel is handled")
                .is_none()
        );
        assert!(!repository.is_accepted().expect("cancel has no side effect"));

        assert!(
            repository
                .record_explicit_acceptance(true)
                .expect("affirmative action is persisted")
                .is_some()
        );
        assert!(repository.is_accepted().expect("record is readable"));
        assert!(!eula_requires_confirmation(&repository).expect("EULA state is readable"));
    }

    #[test]
    fn java_not_ready_becomes_an_actionable_start_error() {
        let readiness = JavaReadiness {
            required: JavaRequirement::from_major(JavaMajor::new(21).expect("major")),
            runtime: None,
            reasons: vec![JavaUnavailableReason::NoCandidates],
        };

        let error = select_java_runtime(readiness).expect_err("Java must be required");
        let message = error.to_string();
        assert!(message.contains("No Java executable candidates were found"));
        assert!(message.contains("Choose a compatible Java executable"));
    }

    #[test]
    fn start_is_enabled_only_for_a_stopped_unreserved_world() {
        use minedock_core::WorldStatus;

        assert!(start_allowed(WorldStatus::Stopped, true, false, false));
        for status in [
            WorldStatus::Preparing,
            WorldStatus::Starting,
            WorldStatus::Running,
            WorldStatus::Stopping,
            WorldStatus::BackingUp,
            WorldStatus::Failed,
        ] {
            assert!(!start_allowed(status, true, false, false));
        }
        assert!(!start_allowed(WorldStatus::Stopped, false, false, false));
        assert!(!start_allowed(WorldStatus::Stopped, true, true, false));
        assert!(!start_allowed(WorldStatus::Stopped, true, false, true));
    }

    #[test]
    fn stop_is_enabled_only_for_a_running_session_without_an_active_operation() {
        assert!(stop_allowed(WorldStatus::Running, true, false, true));
        for status in [
            WorldStatus::Stopped,
            WorldStatus::Preparing,
            WorldStatus::Starting,
            WorldStatus::Stopping,
            WorldStatus::BackingUp,
            WorldStatus::Failed,
        ] {
            assert!(!stop_allowed(status, true, false, true));
        }
        assert!(!stop_allowed(WorldStatus::Running, false, false, true));
        assert!(!stop_allowed(WorldStatus::Running, true, true, true));
        assert!(!stop_allowed(WorldStatus::Running, true, false, false));
    }

    #[test]
    fn exit_polling_remains_available_while_waiting_for_force_escalation() {
        let world_id = WorldId::new();
        let other_world_id = WorldId::new();

        assert!(poll_exit_allowed(None, None));
        assert!(!poll_exit_allowed(
            Some(LifecycleOperation::Starting(world_id)),
            None
        ));
        assert!(!poll_exit_allowed(
            Some(LifecycleOperation::Stopping(world_id)),
            None
        ));
        assert!(poll_exit_allowed(
            Some(LifecycleOperation::Stopping(world_id)),
            Some(world_id)
        ));
        assert!(!poll_exit_allowed(
            Some(LifecycleOperation::Stopping(world_id)),
            Some(other_world_id)
        ));
        assert!(!poll_exit_allowed(
            Some(LifecycleOperation::Polling),
            Some(world_id)
        ));
    }

    #[test]
    fn start_orchestration_reaches_running_with_test_doubles() {
        let root = TempDir::new().expect("temporary app data");
        let world_id = WorldId::new();
        let lifecycle = minedock_core::LifecycleSupervisor::new(
            InMemoryLifecyclePersistence::default(),
            TestProcessFactory::default(),
            TestLeaseProvider,
        );
        let (lifecycle, result) = execute_lifecycle_start(lifecycle, root.path(), world_id, || {
            Ok(test_launch_spec(world_id))
        });

        assert!(result.is_ok());
        assert_eq!(
            lifecycle.persistence().status(world_id),
            Some(WorldStatus::Running)
        );
        assert!(lifecycle.session_id(world_id).is_some());
    }

    #[test]
    fn start_orchestration_rejects_a_second_start_for_the_same_world() {
        let root = TempDir::new().expect("temporary app data");
        let world_id = WorldId::new();
        let lifecycle = minedock_core::LifecycleSupervisor::new(
            InMemoryLifecyclePersistence::default(),
            TestProcessFactory::default(),
            TestLeaseProvider,
        );
        let (lifecycle, first) = execute_lifecycle_start(lifecycle, root.path(), world_id, || {
            Ok(test_launch_spec(world_id))
        });
        assert!(first.is_ok());

        let (_, second) = execute_lifecycle_start(lifecycle, root.path(), world_id, || {
            Ok(test_launch_spec(world_id))
        });
        assert!(second.is_err());
    }

    #[test]
    fn stop_orchestration_reaches_stopped_after_graceful_exit() {
        let root = TempDir::new().expect("temporary app data");
        let world_id = WorldId::new();
        let lifecycle = minedock_core::LifecycleSupervisor::new(
            InMemoryLifecyclePersistence::default(),
            TestProcessFactory::default(),
            TestLeaseProvider,
        );
        let (lifecycle, started) =
            execute_lifecycle_start(lifecycle, root.path(), world_id, || {
                Ok(test_launch_spec(world_id))
            });
        assert!(started.is_ok());

        let (lifecycle, stopped) = execute_lifecycle_stop(lifecycle, world_id);
        assert!(matches!(
            stopped,
            Ok(Some(minedock_core::StopOutcome::Exited { exit })) if exit.success
        ));
        assert_eq!(
            lifecycle.persistence().status(world_id),
            Some(WorldStatus::Stopped)
        );
        assert!(lifecycle.session_id(world_id).is_none());
    }

    #[test]
    fn stop_timeout_keeps_session_for_explicit_force_escalation() {
        let root = TempDir::new().expect("temporary app data");
        let world_id = WorldId::new();
        let lifecycle = minedock_core::LifecycleSupervisor::new(
            InMemoryLifecyclePersistence::default(),
            TestProcessFactory {
                stop_times_out: true,
                exits_on_poll: false,
                poll_exit_success: false,
            },
            TestLeaseProvider,
        );
        let (lifecycle, started) =
            execute_lifecycle_start(lifecycle, root.path(), world_id, || {
                Ok(test_launch_spec(world_id))
            });
        assert!(started.is_ok());

        let (lifecycle, stopped) = execute_lifecycle_stop(lifecycle, world_id);
        let token = match stopped.expect("stop result") {
            Some(minedock_core::StopOutcome::TimedOut(token)) => token,
            other => panic!("expected timeout, got {other:?}"),
        };
        assert_eq!(
            lifecycle.persistence().status(world_id),
            Some(WorldStatus::Stopping)
        );
        assert!(lifecycle.session_id(world_id).is_some());

        let (lifecycle, forced) = execute_lifecycle_force_stop(lifecycle, world_id, token);
        assert!(!forced.expect("force stop").success);
        assert_eq!(
            lifecycle.persistence().status(world_id),
            Some(WorldStatus::Failed)
        );
        assert!(lifecycle.session_id(world_id).is_none());
    }

    #[test]
    fn late_successful_exit_after_stop_timeout_reaches_stopped() {
        let root = TempDir::new().expect("temporary app data");
        let world_id = WorldId::new();
        let lifecycle = minedock_core::LifecycleSupervisor::new(
            InMemoryLifecyclePersistence::default(),
            TestProcessFactory {
                stop_times_out: true,
                exits_on_poll: true,
                poll_exit_success: true,
            },
            TestLeaseProvider,
        );
        let (lifecycle, started) =
            execute_lifecycle_start(lifecycle, root.path(), world_id, || {
                Ok(test_launch_spec(world_id))
            });
        assert!(started.is_ok());

        let (lifecycle, stopped) = execute_lifecycle_stop(lifecycle, world_id);
        let token = match stopped.expect("stop result") {
            Some(minedock_core::StopOutcome::TimedOut(token)) => token,
            other => panic!("expected timeout, got {other:?}"),
        };
        assert_eq!(token.world_id(), world_id);
        assert_eq!(
            lifecycle.persistence().status(world_id),
            Some(WorldStatus::Stopping)
        );

        let (lifecycle, results) = execute_lifecycle_poll(lifecycle, [world_id]);
        assert!(matches!(
            results.as_slice(),
            [(id, Ok(Some(ProcessExit { success: true, .. })))] if *id == world_id
        ));
        assert_eq!(
            lifecycle.persistence().status(world_id),
            Some(WorldStatus::Stopped)
        );
        assert!(lifecycle.session_id(world_id).is_none());
    }

    #[test]
    fn polling_an_unexpected_exit_updates_failed_state() {
        let root = TempDir::new().expect("temporary app data");
        let world_id = WorldId::new();
        let lifecycle = minedock_core::LifecycleSupervisor::new(
            InMemoryLifecyclePersistence::default(),
            TestProcessFactory {
                stop_times_out: false,
                exits_on_poll: true,
                poll_exit_success: false,
            },
            TestLeaseProvider,
        );
        let (lifecycle, started) =
            execute_lifecycle_start(lifecycle, root.path(), world_id, || {
                Ok(test_launch_spec(world_id))
            });
        assert!(started.is_ok());

        let (lifecycle, results) = execute_lifecycle_poll(lifecycle, [world_id]);
        assert!(matches!(
            results.as_slice(),
            [(id, Ok(Some(ProcessExit { success: false, .. })))] if *id == world_id
        ));
        assert_eq!(
            lifecycle.persistence().status(world_id),
            Some(WorldStatus::Failed)
        );
        assert!(lifecycle.session_id(world_id).is_none());
    }
}
