use std::{
    collections::{HashSet, VecDeque},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use eframe::egui::{
    self, Align, Align2, Color32, ColorImage, CursorIcon, Id, Key, Modifiers, OpenUrl, Order,
    PointerButton, Pos2, Rect, RichText, Sense, TextureHandle, TextureOptions, Vec2, pos2, vec2,
};

use crate::{
    annotations::{AnnotationStore, NormalizedRect},
    pdf::{
        Destination, LinkRegion, LinkTarget, PdfBackend, PdfResponse, PreviewKey, RenderedImage,
        render_cache_hash,
    },
};

const PDF_POINT_TO_LOGICAL: f32 = 96.0 / 72.0;
const MIN_ZOOM: f32 = 0.25;
const MAX_ZOOM: f32 = 4.0;
const PAGE_MARGIN: f32 = 24.0;
const PAGE_GAP: f32 = 18.0;
const MAX_RENDER_PIXELS: f64 = 24_000_000.0;
const MAX_TEXTURE_SIDE: f64 = 8_192.0;
const RENDER_DEBOUNCE: Duration = Duration::from_millis(110);
const HOVER_DELAY: Duration = Duration::from_millis(300);
const SAVE_DELAY: Duration = Duration::from_millis(450);

#[derive(Clone, Copy, Debug, PartialEq)]
enum ZoomMode {
    Manual(f32),
    FitWidth,
    FitPage,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum LayoutMode {
    #[default]
    SinglePage,
    Continuous,
}

#[derive(Clone, Copy, Debug)]
struct ViewLocation {
    page_index: usize,
    scroll: Vec2,
    zoom: ZoomMode,
    layout: LayoutMode,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct RenderKey {
    generation: u64,
    page_index: usize,
    pixel_size: [usize; 2],
}

struct CachedPage {
    key: RenderKey,
    texture: TextureHandle,
    links: Vec<LinkRegion>,
}

struct CachedPreview {
    key: PreviewKey,
    texture: TextureHandle,
}

struct DocumentState {
    generation: u64,
    page_sizes: Vec<[f32; 2]>,
    annotations: AnnotationStore,
}

struct SelectionDraft {
    page_index: usize,
    text: String,
    rects: Vec<NormalizedRect>,
}

#[derive(Clone, Copy)]
enum SelectionAction {
    Copy,
    Highlight,
    AddNote,
    Clear,
}

struct HoverState {
    target: LinkTarget,
    started: Instant,
}

struct StatusMessage {
    text: String,
    is_error: bool,
    created: Instant,
}

pub struct PdfViewerApp {
    backend: PdfBackend,
    document: Option<DocumentState>,
    next_generation: u64,
    loading: Option<(u64, PathBuf)>,
    current_page: usize,
    page_input: String,
    zoom: ZoomMode,
    layout: LayoutMode,
    last_viewport: Vec2,
    current_scroll: Vec2,
    pending_scroll: Option<Vec2>,
    pending_page_scroll: Option<usize>,
    pending_destination: Option<Destination>,
    back_history: Vec<ViewLocation>,
    forward_history: Vec<ViewLocation>,
    page_cache: VecDeque<CachedPage>,
    pending_renders: HashSet<RenderKey>,
    desired_render: Option<RenderKey>,
    render_due: Instant,
    preview_cache: VecDeque<CachedPreview>,
    pending_previews: HashSet<PreviewKey>,
    hover: Option<HoverState>,
    drag_start: Option<[f32; 2]>,
    drag_current: Option<[f32; 2]>,
    drag_page: Option<usize>,
    selection_request: u64,
    selection_waiting: Option<u64>,
    selection_draft: Option<SelectionDraft>,
    show_notes: bool,
    selected_annotation: Option<u64>,
    status: Option<StatusMessage>,
}

impl PdfViewerApp {
    pub fn new(cc: &eframe::CreationContext<'_>, initial_path: Option<PathBuf>) -> Self {
        cc.egui_ctx.set_theme(egui::Theme::Dark);
        cc.egui_ctx.style_mut_of(egui::Theme::Dark, |style| {
            style.spacing.button_padding = vec2(8.0, 5.0);
            style.visuals.panel_fill = Color32::from_rgb(35, 37, 42);
            style.scroll_animation = egui::style::ScrollAnimation::duration(0.18);
        });
        let mut app = Self {
            backend: PdfBackend::new(),
            document: None,
            next_generation: 1,
            loading: None,
            current_page: 0,
            page_input: "1".into(),
            zoom: ZoomMode::FitPage,
            layout: LayoutMode::SinglePage,
            last_viewport: vec2(900.0, 700.0),
            current_scroll: Vec2::ZERO,
            pending_scroll: None,
            pending_page_scroll: None,
            pending_destination: None,
            back_history: Vec::new(),
            forward_history: Vec::new(),
            page_cache: VecDeque::new(),
            pending_renders: HashSet::new(),
            desired_render: None,
            render_due: Instant::now(),
            preview_cache: VecDeque::new(),
            pending_previews: HashSet::new(),
            hover: None,
            drag_start: None,
            drag_current: None,
            drag_page: None,
            selection_request: 0,
            selection_waiting: None,
            selection_draft: None,
            show_notes: true,
            selected_annotation: None,
            status: None,
        };
        if let Some(path) = initial_path {
            app.open_path(path);
        }
        app
    }

    fn open_path(&mut self, path: PathBuf) {
        if !path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
        {
            self.set_status("Please choose a PDF file.", true);
            return;
        }
        let generation = self.next_generation;
        self.next_generation = self.next_generation.saturating_add(1);
        self.loading = Some((generation, path.clone()));
        self.backend.load(generation, path);
    }

    fn choose_file(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("PDF document", &["pdf"])
            .pick_file()
        {
            self.open_path(path);
        }
    }

    fn set_status(&mut self, text: impl Into<String>, is_error: bool) {
        self.status = Some(StatusMessage {
            text: text.into(),
            is_error,
            created: Instant::now(),
        });
    }

    fn process_backend(&mut self, ctx: &egui::Context) {
        while let Some(response) = self.backend.try_recv() {
            match response {
                PdfResponse::Loaded {
                    generation,
                    path,
                    title,
                    fingerprint,
                    page_sizes,
                } => {
                    if self.loading.as_ref().map(|loading| loading.0) != Some(generation) {
                        continue;
                    }
                    let (annotations, warning) =
                        AnnotationStore::load(&path, fingerprint, page_sizes.len());
                    self.document = Some(DocumentState {
                        generation,
                        page_sizes,
                        annotations,
                    });
                    self.loading = None;
                    self.current_page = 0;
                    self.page_input = "1".into();
                    self.zoom = ZoomMode::FitPage;
                    self.current_scroll = Vec2::ZERO;
                    self.pending_scroll = Some(Vec2::ZERO);
                    self.pending_page_scroll = None;
                    self.pending_destination = None;
                    self.back_history.clear();
                    self.forward_history.clear();
                    self.page_cache.clear();
                    self.preview_cache.clear();
                    self.pending_renders.clear();
                    self.pending_previews.clear();
                    self.desired_render = None;
                    self.selection_draft = None;
                    self.selected_annotation = None;
                    self.render_due = Instant::now();
                    let display_name = title
                        .filter(|title| !title.trim().is_empty())
                        .or_else(|| {
                            path.file_name()
                                .map(|name| name.to_string_lossy().into_owned())
                        })
                        .unwrap_or_else(|| "PDF".into());
                    ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!(
                        "{display_name} — Simple PDF Viewer"
                    )));
                    if let Some(warning) = warning {
                        self.set_status(warning.to_string(), true);
                    } else {
                        self.set_status(format!("Opened {}", path.display()), false);
                    }
                }
                PdfResponse::LoadFailed {
                    generation,
                    path,
                    error,
                } => {
                    if self.loading.as_ref().map(|loading| loading.0) == Some(generation) {
                        self.loading = None;
                        self.set_status(
                            format!("Could not open {}: {error}", path.display()),
                            true,
                        );
                    }
                }
                PdfResponse::PageRendered {
                    generation,
                    page_index,
                    pixel_size,
                    image,
                    links,
                } => {
                    let key = RenderKey {
                        generation,
                        page_index,
                        pixel_size,
                    };
                    self.pending_renders.remove(&key);
                    if self.active_generation() != Some(generation) {
                        continue;
                    }
                    let texture = load_texture(
                        ctx,
                        format!(
                            "page-{}-{}",
                            generation,
                            render_cache_hash(page_index, pixel_size)
                        ),
                        image,
                    );
                    self.page_cache.retain(|entry| entry.key != key);
                    self.page_cache.push_front(CachedPage {
                        key,
                        texture,
                        links,
                    });
                    self.page_cache
                        .truncate(if self.layout == LayoutMode::Continuous {
                            12
                        } else {
                            3
                        });
                }
                PdfResponse::PreviewRendered {
                    generation,
                    key,
                    image,
                } => {
                    self.pending_previews.remove(&key);
                    if self.active_generation() != Some(generation) {
                        continue;
                    }
                    let texture = load_texture(ctx, format!("preview-{generation}-{key:?}"), image);
                    self.preview_cache.retain(|entry| entry.key != key);
                    self.preview_cache
                        .push_front(CachedPreview { key, texture });
                    self.preview_cache.truncate(12);
                }
                PdfResponse::SelectionResolved {
                    generation,
                    request_id,
                    page_index,
                    text,
                    rects,
                } => {
                    if self.active_generation() == Some(generation)
                        && self.selection_waiting == Some(request_id)
                    {
                        self.selection_waiting = None;
                        if text.is_empty() || rects.is_empty() {
                            self.set_status("No selectable text was found in that area.", true);
                        } else {
                            self.selection_draft = Some(SelectionDraft {
                                page_index,
                                text,
                                rects,
                            });
                            self.set_status(
                                "Text selected — right-click it for Copy, Highlight, or Add note.",
                                false,
                            );
                        }
                    }
                }
                PdfResponse::OperationFailed {
                    generation,
                    operation,
                    error,
                } => {
                    if self.active_generation() == Some(generation) {
                        self.set_status(format!("Could not {operation}: {error}"), true);
                    }
                }
            }
        }
    }

    fn active_generation(&self) -> Option<u64> {
        self.document.as_ref().map(|document| document.generation)
    }

    fn page_count(&self) -> usize {
        self.document
            .as_ref()
            .map_or(0, |document| document.page_sizes.len())
    }

    fn current_location(&self) -> ViewLocation {
        ViewLocation {
            page_index: self.current_page,
            scroll: self.current_scroll,
            zoom: self.zoom,
            layout: self.layout,
        }
    }

    fn go_to_page(&mut self, page_index: usize) {
        let page_count = self.page_count();
        if page_count == 0 {
            return;
        }
        self.current_page = page_index.min(page_count - 1);
        self.page_input = (self.current_page + 1).to_string();
        if self.layout == LayoutMode::Continuous {
            self.pending_page_scroll = Some(self.current_page);
            self.pending_scroll = None;
        } else {
            self.pending_scroll = Some(Vec2::ZERO);
            self.pending_page_scroll = None;
        }
        self.pending_destination = None;
        self.forward_history.clear();
        self.selection_draft = None;
        self.selected_annotation = None;
        self.schedule_render(true);
    }

    fn navigate_to_destination(&mut self, destination: Destination) {
        self.back_history.push(self.current_location());
        self.forward_history.clear();
        self.current_page = destination.page_index;
        self.page_input = (self.current_page + 1).to_string();
        self.pending_destination = Some(destination);
        self.pending_scroll = None;
        self.pending_page_scroll = None;
        self.selection_draft = None;
        self.selected_annotation = None;
        self.schedule_render(true);
    }

    fn navigate_back(&mut self) {
        if let Some(location) = self.back_history.pop() {
            self.forward_history.push(self.current_location());
            self.restore_location(location);
        }
    }

    fn navigate_forward(&mut self) {
        if let Some(location) = self.forward_history.pop() {
            self.back_history.push(self.current_location());
            self.restore_location(location);
        }
    }

    fn restore_location(&mut self, location: ViewLocation) {
        self.current_page = location.page_index.min(self.page_count().saturating_sub(1));
        self.page_input = (self.current_page + 1).to_string();
        self.zoom = location.zoom;
        if self.layout == location.layout {
            self.pending_scroll = Some(location.scroll);
        } else {
            self.pending_page_scroll = Some(self.current_page);
            self.pending_scroll = None;
        }
        self.layout = location.layout;
        self.pending_destination = None;
        self.selection_draft = None;
        self.schedule_render(true);
    }

    fn schedule_render(&mut self, immediate: bool) {
        self.desired_render = None;
        self.render_due = if immediate {
            Instant::now()
        } else {
            Instant::now() + RENDER_DEBOUNCE
        };
    }

    fn set_manual_zoom(&mut self, zoom: f32) {
        self.zoom = ZoomMode::Manual(zoom.clamp(MIN_ZOOM, MAX_ZOOM));
        self.schedule_render(false);
    }

    fn display_scale(&self, page_size: [f32; 2], viewport: Vec2) -> f32 {
        match self.zoom {
            ZoomMode::Manual(zoom) => PDF_POINT_TO_LOGICAL * zoom,
            ZoomMode::FitWidth => {
                ((viewport.x - PAGE_MARGIN * 2.0).max(40.0) / page_size[0]).max(0.02)
            }
            ZoomMode::FitPage => {
                let width = (viewport.x - PAGE_MARGIN * 2.0).max(40.0) / page_size[0];
                let height = (viewport.y - PAGE_MARGIN * 2.0).max(40.0) / page_size[1];
                width.min(height).max(0.02)
            }
        }
    }

    fn effective_zoom(&self) -> f32 {
        let Some(document) = self.document.as_ref() else {
            return 1.0;
        };
        let page_size = document.page_sizes[self.current_page];
        self.display_scale(page_size, self.last_viewport) / PDF_POINT_TO_LOGICAL
    }

    fn process_shortcuts(&mut self, ctx: &egui::Context) {
        let dropped_path = ctx.input(|input| {
            input
                .raw
                .dropped_files
                .iter()
                .map(|file| file.path().to_path_buf())
                .next()
        });
        if let Some(path) = dropped_path {
            self.open_path(path);
        }

        let shortcut = |modifiers, key| {
            ctx.input(|input| input.modifiers.matches_exact(modifiers) && input.key_pressed(key))
        };
        if shortcut(Modifiers::CTRL, Key::O) {
            self.choose_file();
        }
        if shortcut(Modifiers::ALT, Key::ArrowLeft) {
            self.navigate_back();
        }
        if shortcut(Modifiers::ALT, Key::ArrowRight) {
            self.navigate_forward();
        }
        if shortcut(Modifiers::CTRL, Key::Plus) || shortcut(Modifiers::CTRL, Key::Equals) {
            self.set_manual_zoom(self.effective_zoom() + 0.1);
        }
        if shortcut(Modifiers::CTRL, Key::Minus) {
            self.set_manual_zoom(self.effective_zoom() - 0.1);
        }
        if shortcut(Modifiers::CTRL, Key::Num0) {
            self.set_manual_zoom(1.0);
        }

        if !ctx.egui_wants_keyboard_input() {
            if ctx.input(|input| input.key_pressed(Key::PageUp)) {
                self.go_to_page(self.current_page.saturating_sub(1));
            }
            if ctx.input(|input| input.key_pressed(Key::PageDown)) {
                self.go_to_page(self.current_page.saturating_add(1));
            }
            if ctx.input(|input| input.key_pressed(Key::Home)) {
                self.go_to_page(0);
            }
            if ctx.input(|input| input.key_pressed(Key::End)) {
                self.go_to_page(self.page_count().saturating_sub(1));
            }
        }

        let zoom_delta = ctx.input(|input| {
            if input.modifiers.ctrl {
                input.zoom_delta()
            } else {
                1.0
            }
        });
        if (zoom_delta - 1.0).abs() > 0.001 {
            self.set_manual_zoom(self.effective_zoom() * zoom_delta);
        }
    }

    fn toolbar(&mut self, root_ui: &mut egui::Ui) {
        egui::Panel::top("toolbar").show(root_ui, |ui| {
            let compact = ui.available_width() < 900.0;
            egui::ScrollArea::horizontal()
                .id_salt("toolbar-scroll")
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        if ui
                            .button("Open…")
                            .on_hover_text("Open PDF (Ctrl+O)")
                            .clicked()
                        {
                            self.choose_file();
                        }
                        ui.separator();

                        let has_document = self.document.is_some();
                        if ui
                            .add_enabled(
                                !self.back_history.is_empty(),
                                egui::Button::new(if compact { "↶" } else { "← Back" }),
                            )
                            .on_hover_text("Return from a followed reference (Alt+Left)")
                            .clicked()
                        {
                            self.navigate_back();
                        }
                        if ui
                            .add_enabled(
                                !self.forward_history.is_empty(),
                                egui::Button::new(if compact { "↷" } else { "Forward →" }),
                            )
                            .on_hover_text("Go forward again (Alt+Right)")
                            .clicked()
                        {
                            self.navigate_forward();
                        }
                        ui.separator();

                        if ui
                            .add_enabled(
                                has_document && self.current_page > 0,
                                egui::Button::new("◀"),
                            )
                            .on_hover_text("Previous page (Page Up)")
                            .clicked()
                        {
                            self.go_to_page(self.current_page - 1);
                        }
                        let page_response = ui.add_enabled(
                            has_document,
                            egui::TextEdit::singleline(&mut self.page_input).desired_width(46.0),
                        );
                        if page_response.lost_focus()
                            && ui.input(|input| input.key_pressed(Key::Enter))
                        {
                            if let Ok(page) = self.page_input.trim().parse::<usize>() {
                                self.go_to_page(page.saturating_sub(1));
                            } else {
                                self.page_input = (self.current_page + 1).to_string();
                            }
                        }
                        ui.label(format!("/ {}", self.page_count().max(1)));
                        if ui
                            .add_enabled(
                                has_document && self.current_page + 1 < self.page_count(),
                                egui::Button::new("▶"),
                            )
                            .on_hover_text("Next page (Page Down)")
                            .clicked()
                        {
                            self.go_to_page(self.current_page + 1);
                        }
                        ui.separator();

                        if ui
                            .add_enabled(has_document, egui::Button::new("−"))
                            .clicked()
                        {
                            self.set_manual_zoom(self.effective_zoom() - 0.1);
                        }
                        let mut percent = (self.effective_zoom() * 100.0).round();
                        if ui
                            .add_enabled(
                                has_document,
                                egui::DragValue::new(&mut percent)
                                    .range(25.0..=400.0)
                                    .speed(1.0)
                                    .suffix("%"),
                            )
                            .changed()
                        {
                            self.set_manual_zoom(percent / 100.0);
                        }
                        if ui
                            .add_enabled(has_document, egui::Button::new("+"))
                            .clicked()
                        {
                            self.set_manual_zoom(self.effective_zoom() + 0.1);
                        }
                        if ui
                            .add_enabled(
                                has_document,
                                egui::Button::selectable(
                                    self.zoom == ZoomMode::FitWidth,
                                    if compact { "Width" } else { "Fit width" },
                                ),
                            )
                            .clicked()
                        {
                            self.zoom = ZoomMode::FitWidth;
                            self.schedule_render(false);
                        }
                        if ui
                            .add_enabled(
                                has_document,
                                egui::Button::selectable(
                                    self.zoom == ZoomMode::FitPage,
                                    if compact { "Page" } else { "Fit page" },
                                ),
                            )
                            .clicked()
                        {
                            self.zoom = ZoomMode::FitPage;
                            self.schedule_render(false);
                        }
                        ui.separator();
                        let previous_layout = self.layout;
                        ui.add_enabled_ui(has_document, |ui| {
                            ui.selectable_value(
                                &mut self.layout,
                                LayoutMode::SinglePage,
                                if compact { "1 page" } else { "Single" },
                            );
                            ui.selectable_value(
                                &mut self.layout,
                                LayoutMode::Continuous,
                                if compact { "Scroll" } else { "Continuous" },
                            );
                        });
                        if self.layout != previous_layout {
                            if self.layout == LayoutMode::Continuous {
                                self.pending_page_scroll = Some(self.current_page);
                                self.pending_scroll = None;
                            } else {
                                self.pending_page_scroll = None;
                                self.pending_scroll = Some(Vec2::ZERO);
                            }
                            self.selection_draft = None;
                            self.drag_page = None;
                            self.schedule_render(true);
                        }
                        ui.separator();
                        if ui
                            .add_enabled(
                                has_document,
                                egui::Button::selectable(self.show_notes, "Notes"),
                            )
                            .clicked()
                        {
                            self.show_notes = !self.show_notes;
                        }

                        if let Some((_, path)) = self.loading.as_ref() {
                            ui.separator();
                            ui.spinner();
                            if !compact {
                                ui.label(format!("Opening {}", file_name(path)));
                            }
                        } else if let Some(status) = self
                            .status
                            .as_ref()
                            .filter(|status| !compact || status.is_error)
                        {
                            let color = if status.is_error {
                                Color32::from_rgb(255, 145, 135)
                            } else {
                                Color32::from_gray(180)
                            };
                            ui.separator();
                            ui.label(RichText::new(&status.text).color(color));
                        }
                    });
                });
        });
    }

    fn notes_panel(&mut self, root_ui: &mut egui::Ui) {
        if !self.show_notes || self.document.is_none() {
            return;
        }
        egui::Panel::right("notes-panel")
            .default_size(285.0)
            .min_size(220.0)
            .resizable(true)
            .show(root_ui, |ui| {
                ui.heading(format!("Notes — page {}", self.current_page + 1));
                ui.label(
                    RichText::new("Select text, then right-click and choose Add note.")
                        .small()
                        .weak(),
                );
                ui.separator();

                let summaries: Vec<_> = self
                    .document
                    .as_ref()
                    .expect("document checked above")
                    .annotations
                    .annotations()
                    .iter()
                    .filter(|annotation| annotation.page_index == self.current_page)
                    .map(|annotation| {
                        (
                            annotation.id,
                            compact_text(&annotation.selected_text, 70),
                            !annotation.note.trim().is_empty(),
                        )
                    })
                    .collect();

                if summaries.is_empty() {
                    ui.label("No highlights on this page.");
                } else {
                    egui::ScrollArea::vertical()
                        .max_height(190.0)
                        .show(ui, |ui| {
                            for (id, text, has_note) in summaries {
                                let label = if has_note {
                                    format!("📝 {text}")
                                } else {
                                    text
                                };
                                if ui
                                    .selectable_label(self.selected_annotation == Some(id), label)
                                    .clicked()
                                {
                                    self.selected_annotation = Some(id);
                                }
                            }
                        });
                }

                let Some(id) = self.selected_annotation else {
                    return;
                };
                let selected_page = self
                    .document
                    .as_ref()
                    .and_then(|document| {
                        document
                            .annotations
                            .annotations()
                            .iter()
                            .find(|annotation| annotation.id == id)
                    })
                    .map(|annotation| annotation.page_index);
                if selected_page != Some(self.current_page) {
                    self.selected_annotation = None;
                    return;
                }

                ui.separator();
                let mut note_changed = false;
                if let Some(annotation) = self
                    .document
                    .as_mut()
                    .and_then(|document| document.annotations.annotation_mut(id))
                {
                    ui.label(RichText::new(&annotation.selected_text).italics().weak());
                    ui.add_space(6.0);
                    ui.label("Note");
                    note_changed = ui
                        .add(
                            egui::TextEdit::multiline(&mut annotation.note)
                                .desired_rows(8)
                                .hint_text("Write a note…"),
                        )
                        .changed();
                }
                if note_changed && let Some(document) = self.document.as_mut() {
                    document.annotations.mark_dirty();
                }
                ui.add_space(6.0);
                if ui.button("Delete highlight").clicked() {
                    if let Some(document) = self.document.as_mut() {
                        document.annotations.remove(id);
                    }
                    self.selected_annotation = None;
                }
            });
    }

    fn central_panel(&mut self, root_ui: &mut egui::Ui, ctx: &egui::Context) {
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(Color32::from_rgb(27, 29, 33)))
            .show(root_ui, |ui| {
                self.last_viewport = ui.available_size();
                if self.document.is_none() {
                    ui.centered_and_justified(|ui| {
                        ui.vertical_centered(|ui| {
                            ui.heading("Simple PDF Viewer");
                            ui.label("Open a PDF or drop one into this window.");
                            if ui.button("Open PDF…").clicked() {
                                self.choose_file();
                            }
                        });
                    });
                    return;
                }

                if self.layout == LayoutMode::Continuous {
                    self.continuous_document(ui, ctx);
                    return;
                }

                let document = self.document.as_ref().expect("document checked above");

                let generation = document.generation;
                let page_size = document.page_sizes[self.current_page];
                let annotations: Vec<_> = document
                    .annotations
                    .annotations()
                    .iter()
                    .filter(|annotation| annotation.page_index == self.current_page)
                    .cloned()
                    .collect();
                let display_scale = self.display_scale(page_size, self.last_viewport);
                let display_size = vec2(page_size[0] * display_scale, page_size[1] * display_scale);
                let desired_key = render_key(
                    generation,
                    self.current_page,
                    display_size,
                    ctx.pixels_per_point(),
                );
                self.update_desired_render(desired_key);

                let cache_index = self
                    .page_cache
                    .iter()
                    .position(|entry| entry.key == desired_key)
                    .or_else(|| {
                        self.page_cache.iter().position(|entry| {
                            entry.key.generation == generation
                                && entry.key.page_index == self.current_page
                        })
                    });
                let (texture, links) = cache_index.map_or((None, Vec::new()), |index| {
                    (
                        Some(self.page_cache[index].texture.clone()),
                        self.page_cache[index].links.clone(),
                    )
                });
                let selected_annotation = self.selected_annotation;
                let drag_rect = self
                    .drag_start
                    .zip(self.drag_current)
                    .filter(|_| self.drag_page == Some(self.current_page))
                    .map(|(start, current)| NormalizedRect::from_points(start, current));
                let selected_rects = self
                    .selection_draft
                    .as_ref()
                    .filter(|draft| draft.page_index == self.current_page)
                    .map(|draft| draft.rects.clone())
                    .unwrap_or_default();

                let mut requested_offset = self.pending_scroll.take();
                if let Some(destination) = self.pending_destination.take()
                    && destination.page_index == self.current_page
                {
                    let page_offset = vec2(
                        ((self.last_viewport.x - display_size.x) * 0.5).max(PAGE_MARGIN),
                        ((self.last_viewport.y - display_size.y) * 0.5).max(PAGE_MARGIN),
                    );
                    let x = destination.x.unwrap_or(0.0) * display_scale + page_offset.x
                        - self.last_viewport.x * 0.35;
                    let y = destination.y_from_top.unwrap_or(0.0) * display_scale + page_offset.y
                        - 36.0;
                    requested_offset = Some(vec2(x.max(0.0), y.max(0.0)));
                }

                let mut scroll_area = egui::ScrollArea::both()
                    .id_salt("document-scroll-single")
                    .auto_shrink([false, false]);
                if let Some(offset) = requested_offset {
                    scroll_area = scroll_area.scroll_offset(offset);
                }

                let output = scroll_area.show_viewport(ui, |ui, viewport| {
                    let canvas_size = vec2(
                        display_size.x.max(viewport.width()) + PAGE_MARGIN * 2.0,
                        display_size.y.max(viewport.height()) + PAGE_MARGIN * 2.0,
                    );
                    ui.set_min_size(canvas_size);
                    let page_min = ui.min_rect().min
                        + vec2(
                            ((viewport.width() - display_size.x) * 0.5).max(PAGE_MARGIN),
                            ((viewport.height() - display_size.y) * 0.5).max(PAGE_MARGIN),
                        );
                    let page_rect = Rect::from_min_size(page_min, display_size);
                    let response =
                        ui.interact(page_rect, Id::new("pdf-page"), Sense::click_and_drag());
                    let painter = ui.painter();
                    painter.rect_filled(
                        page_rect.translate(vec2(4.0, 5.0)),
                        2.0,
                        Color32::from_black_alpha(90),
                    );
                    painter.rect_filled(page_rect, 1.0, Color32::WHITE);
                    if let Some(texture) = texture.as_ref() {
                        painter.image(
                            texture.id(),
                            page_rect,
                            Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)),
                            Color32::WHITE,
                        );
                    } else {
                        painter.text(
                            page_rect.center(),
                            Align2::CENTER_CENTER,
                            "Rendering…",
                            egui::FontId::proportional(16.0),
                            Color32::DARK_GRAY,
                        );
                    }

                    for annotation in &annotations {
                        let color = if selected_annotation == Some(annotation.id) {
                            Color32::from_rgba_unmultiplied(255, 190, 20, 105)
                        } else {
                            Color32::from_rgba_unmultiplied(255, 224, 50, 72)
                        };
                        for rect in &annotation.rects {
                            painter.rect_filled(rect_to_screen(*rect, page_rect), 1.0, color);
                        }
                    }
                    for rect in &selected_rects {
                        painter.rect_filled(
                            rect_to_screen(*rect, page_rect),
                            1.0,
                            Color32::from_rgba_unmultiplied(70, 140, 255, 82),
                        );
                    }
                    if let Some(rect) = drag_rect {
                        painter.rect_filled(
                            rect_to_screen(rect, page_rect),
                            1.0,
                            Color32::from_rgba_unmultiplied(70, 140, 255, 55),
                        );
                    }

                    (response, page_rect)
                });
                self.current_scroll = output.state.offset;
                let (response, page_rect) = output.inner;
                self.handle_page_interaction(
                    ctx,
                    self.current_page,
                    &response,
                    page_rect,
                    &links,
                    &annotations,
                );
            });
    }

    fn update_desired_render(&mut self, key: RenderKey) {
        if self.desired_render != Some(key) {
            let same_page_cached = self.page_cache.iter().any(|entry| {
                entry.key.generation == key.generation && entry.key.page_index == key.page_index
            });
            self.desired_render = Some(key);
            self.render_due = if same_page_cached {
                Instant::now() + RENDER_DEBOUNCE
            } else {
                Instant::now()
            };
        }
    }

    fn continuous_document(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let document = self.document.as_ref().expect("document checked above");
        let generation = document.generation;
        let page_sizes = document.page_sizes.clone();
        let annotations = document.annotations.annotations().to_vec();
        let display_sizes: Vec<Vec2> = page_sizes
            .iter()
            .map(|page_size| {
                let scale = self.display_scale(*page_size, self.last_viewport);
                vec2(page_size[0] * scale, page_size[1] * scale)
            })
            .collect();
        let mut page_tops = Vec::with_capacity(display_sizes.len());
        let mut next_top = PAGE_MARGIN;
        for size in &display_sizes {
            page_tops.push(next_top);
            next_top += size.y + PAGE_GAP;
        }
        let canvas_width = (display_sizes.iter().map(|size| size.x).fold(0.0, f32::max)
            + PAGE_MARGIN * 2.0)
            .max(self.last_viewport.x);
        let canvas_height = (next_top - PAGE_GAP + PAGE_MARGIN).max(self.last_viewport.y);

        let requested_offset = self.pending_scroll.take();
        let requested_page = self.pending_page_scroll.take();
        let requested_destination = self.pending_destination.take();
        let selected_annotation = self.selected_annotation;
        let selected_page = self.selection_draft.as_ref().map(|draft| draft.page_index);
        let selected_rects = self
            .selection_draft
            .as_ref()
            .map(|draft| draft.rects.clone())
            .unwrap_or_default();
        let drag_page = self.drag_page;
        let drag_rect = self
            .drag_start
            .zip(self.drag_current)
            .map(|(start, current)| NormalizedRect::from_points(start, current));

        let mut scroll_area = egui::ScrollArea::both()
            .id_salt("document-scroll-continuous")
            .auto_shrink([false, false]);
        if let Some(offset) = requested_offset {
            scroll_area = scroll_area.scroll_offset(offset);
        }

        let mut requested_renders = Vec::new();
        let mut interactions = Vec::new();
        let output = scroll_area.show_viewport(ui, |ui, viewport| {
            ui.set_min_size(vec2(canvas_width, canvas_height));
            let origin = ui.min_rect().min;
            let prefetch_viewport = viewport.expand2(vec2(0.0, viewport.height()));

            for (page_index, display_size) in display_sizes.iter().copied().enumerate() {
                let page_min = origin
                    + vec2(
                        ((canvas_width - display_size.x) * 0.5).max(PAGE_MARGIN),
                        page_tops[page_index],
                    );
                let page_rect = Rect::from_min_size(page_min, display_size);
                let logical_page_rect = Rect::from_min_size(
                    pos2(page_min.x - origin.x, page_tops[page_index]),
                    display_size,
                );

                if requested_page == Some(page_index) {
                    let target = Rect::from_min_size(
                        pos2(origin.x, page_rect.top()),
                        vec2(canvas_width, 1.0),
                    );
                    ui.scroll_to_rect(target, Some(Align::Min));
                }
                if let Some(destination) = requested_destination.as_ref()
                    && destination.page_index == page_index
                {
                    let page_scale = display_size.x / page_sizes[page_index][0];
                    let y = page_rect.top() + destination.y_from_top.unwrap_or(0.0) * page_scale;
                    let target = Rect::from_min_size(pos2(origin.x, y), vec2(canvas_width, 1.0));
                    ui.scroll_to_rect(target, Some(Align::Center));
                }

                if !logical_page_rect.intersects(prefetch_viewport) {
                    continue;
                }

                let key = render_key(generation, page_index, display_size, ctx.pixels_per_point());
                requested_renders.push(key);
                let cache = self
                    .page_cache
                    .iter()
                    .find(|entry| entry.key == key)
                    .or_else(|| {
                        self.page_cache.iter().find(|entry| {
                            entry.key.generation == generation && entry.key.page_index == page_index
                        })
                    });
                let texture = cache.map(|entry| entry.texture.clone());
                let links = cache.map(|entry| entry.links.clone()).unwrap_or_default();
                let page_annotations: Vec<_> = annotations
                    .iter()
                    .filter(|annotation| annotation.page_index == page_index)
                    .cloned()
                    .collect();

                let response = ui.interact(
                    page_rect,
                    Id::new(("pdf-page", page_index)),
                    Sense::click_and_drag(),
                );
                let painter = ui.painter();
                painter.rect_filled(
                    page_rect.translate(vec2(4.0, 5.0)),
                    2.0,
                    Color32::from_black_alpha(90),
                );
                painter.rect_filled(page_rect, 1.0, Color32::WHITE);
                if let Some(texture) = texture.as_ref() {
                    painter.image(
                        texture.id(),
                        page_rect,
                        Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)),
                        Color32::WHITE,
                    );
                } else {
                    painter.text(
                        page_rect.center(),
                        Align2::CENTER_CENTER,
                        format!("Rendering page {}…", page_index + 1),
                        egui::FontId::proportional(16.0),
                        Color32::DARK_GRAY,
                    );
                }

                for annotation in &page_annotations {
                    let color = if selected_annotation == Some(annotation.id) {
                        Color32::from_rgba_unmultiplied(255, 190, 20, 105)
                    } else {
                        Color32::from_rgba_unmultiplied(255, 224, 50, 72)
                    };
                    for rect in &annotation.rects {
                        painter.rect_filled(rect_to_screen(*rect, page_rect), 1.0, color);
                    }
                }
                if selected_page == Some(page_index) {
                    for rect in &selected_rects {
                        painter.rect_filled(
                            rect_to_screen(*rect, page_rect),
                            1.0,
                            Color32::from_rgba_unmultiplied(70, 140, 255, 82),
                        );
                    }
                }
                if drag_page == Some(page_index)
                    && let Some(rect) = drag_rect
                {
                    painter.rect_filled(
                        rect_to_screen(rect, page_rect),
                        1.0,
                        Color32::from_rgba_unmultiplied(70, 140, 255, 55),
                    );
                }
                interactions.push((page_index, response, page_rect, links, page_annotations));
            }
        });

        self.current_scroll = output.state.offset;
        let reading_position = self.current_scroll.y + self.last_viewport.y * 0.35;
        if let Some((page_index, _)) = page_tops.iter().enumerate().min_by(
            |(left_index, left_top), (right_index, right_top)| {
                let left_center = **left_top + display_sizes[*left_index].y * 0.5;
                let right_center = **right_top + display_sizes[*right_index].y * 0.5;
                (left_center - reading_position)
                    .abs()
                    .total_cmp(&(right_center - reading_position).abs())
            },
        ) && page_index != self.current_page
        {
            self.current_page = page_index;
            self.page_input = (page_index + 1).to_string();
            self.selected_annotation = None;
        }

        for key in requested_renders {
            self.request_render(key);
        }
        if !interactions
            .iter()
            .any(|(_, response, _, _, _)| response.hovered())
        {
            self.hover = None;
        }
        for (page_index, response, page_rect, links, page_annotations) in interactions {
            self.handle_page_interaction(
                ctx,
                page_index,
                &response,
                page_rect,
                &links,
                &page_annotations,
            );
        }
    }

    fn request_render(&mut self, key: RenderKey) {
        let already_cached = self.page_cache.iter().any(|entry| entry.key == key);
        if !already_cached && !self.pending_renders.contains(&key) && self.pending_renders.len() < 4
        {
            self.pending_renders.insert(key);
            self.backend
                .render_page(key.generation, key.page_index, key.pixel_size);
        }
    }

    fn handle_page_interaction(
        &mut self,
        ctx: &egui::Context,
        page_index: usize,
        response: &egui::Response,
        page_rect: Rect,
        links: &[LinkRegion],
        annotations: &[crate::annotations::Annotation],
    ) {
        let pointer = response.interact_pointer_pos();
        if response.drag_started_by(PointerButton::Primary)
            && let Some(pointer) = pointer
        {
            let normalized = point_to_normalized(pointer, page_rect);
            self.drag_start = Some(normalized);
            self.drag_current = Some(normalized);
            self.drag_page = Some(page_index);
            self.selection_draft = None;
            self.hover = None;
        }
        if response.dragged_by(PointerButton::Primary)
            && self.drag_page == Some(page_index)
            && let Some(pointer) = pointer
        {
            self.drag_current = Some(point_to_normalized(pointer, page_rect));
        }
        if response.drag_stopped_by(PointerButton::Primary) && self.drag_page == Some(page_index) {
            if let Some((start, end)) = self.drag_start.zip(self.drag_current) {
                let selection = NormalizedRect::from_points(start, end);
                if selection.width * page_rect.width() > 3.0
                    || selection.height * page_rect.height() > 3.0
                {
                    self.selection_request = self.selection_request.saturating_add(1);
                    self.selection_waiting = Some(self.selection_request);
                    if let Some(generation) = self.active_generation() {
                        self.backend.resolve_selection(
                            generation,
                            self.selection_request,
                            page_index,
                            start,
                            end,
                        );
                    }
                }
            }
            self.drag_start = None;
            self.drag_current = None;
            self.drag_page = None;
        }

        let hover_point = response
            .hover_pos()
            .map(|pointer| point_to_normalized(pointer, page_rect));
        let hovered_link = hover_point
            .and_then(|point| links.iter().find(|link| link.rect.contains(point)).cloned());
        if response.dragged_by(PointerButton::Primary) {
            self.hover = None;
        } else if let Some(link) = hovered_link.as_ref() {
            ctx.set_cursor_icon(CursorIcon::PointingHand);
            let unchanged = self
                .hover
                .as_ref()
                .is_some_and(|hover| hover.target == link.target);
            if !unchanged {
                self.hover = Some(HoverState {
                    target: link.target.clone(),
                    started: Instant::now(),
                });
            }
        } else if response.hovered() {
            self.hover = None;
            ctx.set_cursor_icon(CursorIcon::Text);
        }

        if response.clicked()
            && let Some(point) = hover_point
        {
            if let Some(link) = links.iter().find(|link| link.rect.contains(point)) {
                match &link.target {
                    LinkTarget::Internal(destination) => {
                        self.navigate_to_destination(destination.clone());
                    }
                    LinkTarget::Uri(uri) => ctx.open_url(OpenUrl::new_tab(uri)),
                }
            } else if let Some(annotation) = annotations
                .iter()
                .find(|annotation| annotation.rects.iter().any(|rect| rect.contains(point)))
            {
                self.current_page = page_index;
                self.page_input = (page_index + 1).to_string();
                self.selected_annotation = Some(annotation.id);
                self.show_notes = true;
            } else {
                self.selected_annotation = None;
            }
        }

        let selected_text = self
            .selection_draft
            .as_ref()
            .filter(|draft| draft.page_index == page_index)
            .map(|draft| draft.text.clone());
        let mut selection_action = None;
        response.context_menu(|ui| {
            if let Some(text) = selected_text.as_ref() {
                ui.set_max_width(360.0);
                ui.label(RichText::new(compact_text(text, 140)).italics().weak());
                ui.separator();
                if ui.button("Copy text").clicked() {
                    selection_action = Some(SelectionAction::Copy);
                    ui.close();
                }
                if ui.button("Highlight").clicked() {
                    selection_action = Some(SelectionAction::Highlight);
                    ui.close();
                }
                if ui.button("Add note").clicked() {
                    selection_action = Some(SelectionAction::AddNote);
                    ui.close();
                }
                ui.separator();
                if ui.button("Clear selection").clicked() {
                    selection_action = Some(SelectionAction::Clear);
                    ui.close();
                }
            } else if self.selection_waiting.is_some() {
                ui.spinner();
                ui.label("Reading selected text…");
            } else {
                ui.label("Drag across text first, then right-click it.");
            }
        });
        self.apply_selection_action(ctx, selection_action);
    }

    fn apply_selection_action(&mut self, ctx: &egui::Context, action: Option<SelectionAction>) {
        match action {
            Some(SelectionAction::Copy) => {
                if let Some(draft) = self.selection_draft.as_ref() {
                    ctx.copy_text(draft.text.clone());
                    self.set_status("Copied selected text.", false);
                }
            }
            Some(SelectionAction::Highlight | SelectionAction::AddNote) => {
                let add_note = matches!(action, Some(SelectionAction::AddNote));
                if let Some(draft) = self.selection_draft.take()
                    && let Some(document) = self.document.as_mut()
                {
                    let id = document
                        .annotations
                        .add(draft.page_index, draft.rects, draft.text);
                    if add_note {
                        self.current_page = draft.page_index;
                        self.page_input = (draft.page_index + 1).to_string();
                        self.selected_annotation = Some(id);
                        self.show_notes = true;
                    }
                }
            }
            Some(SelectionAction::Clear) => self.selection_draft = None,
            None => {}
        }
    }

    fn hover_preview(&mut self, ctx: &egui::Context) {
        let Some(hover) = self.hover.as_ref() else {
            return;
        };
        if hover.started.elapsed() < HOVER_DELAY {
            ctx.request_repaint_after(HOVER_DELAY - hover.started.elapsed());
            return;
        }
        let target = hover.target.clone();
        if let LinkTarget::Internal(destination) = &target {
            let key = PreviewKey::for_destination(destination);
            let cached = self.preview_cache.iter().find(|entry| entry.key == key);
            if cached.is_none()
                && !self.pending_previews.contains(&key)
                && let Some(generation) = self.active_generation()
            {
                self.pending_previews.insert(key);
                self.backend.render_preview(
                    generation,
                    destination.clone(),
                    ctx.pixels_per_point(),
                );
            }
        }

        let pointer = ctx.pointer_hover_pos().unwrap_or(pos2(20.0, 70.0));
        let screen = ctx.content_rect();
        let size = match target {
            LinkTarget::Internal(_) => vec2(438.0, 294.0),
            LinkTarget::Uri(_) => vec2(360.0, 56.0),
        };
        let mut position = pointer + vec2(16.0, 18.0);
        if position.x + size.x > screen.right() {
            position.x = pointer.x - size.x - 12.0;
        }
        if position.y + size.y > screen.bottom() {
            position.y = (screen.bottom() - size.y - 8.0).max(screen.top() + 8.0);
        }
        egui::Area::new(Id::new("link-preview"))
            .order(Order::Tooltip)
            .fixed_pos(position)
            .interactable(false)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| match &target {
                    LinkTarget::Internal(destination) => {
                        ui.label(
                            RichText::new(format!("Page {}", destination.page_index + 1)).strong(),
                        );
                        let key = PreviewKey::for_destination(destination);
                        if let Some(preview) =
                            self.preview_cache.iter().find(|entry| entry.key == key)
                        {
                            ui.add(
                                egui::Image::new((preview.texture.id(), vec2(420.0, 260.0)))
                                    .maintain_aspect_ratio(true),
                            );
                        } else {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label("Rendering reference preview…");
                            });
                        }
                    }
                    LinkTarget::Uri(uri) => {
                        ui.label(RichText::new("Open external link").strong());
                        ui.label(compact_text(uri, 80));
                    }
                });
            });
    }

    fn send_render_if_due(&mut self) {
        let Some(key) = self.desired_render else {
            return;
        };
        let already_cached = self.page_cache.iter().any(|entry| entry.key == key);
        if !already_cached
            && !self.pending_renders.contains(&key)
            && Instant::now() >= self.render_due
        {
            self.pending_renders.insert(key);
            self.backend
                .render_page(key.generation, key.page_index, key.pixel_size);
        }
    }

    fn save_annotations(&mut self, ctx: &egui::Context) {
        let mut error = None;
        let mut dirty = false;
        if let Some(document) = self.document.as_mut() {
            dirty = document.annotations.is_dirty();
            if let Err(save_error) = document.annotations.save_if_due(SAVE_DELAY) {
                error = Some(save_error.to_string());
            }
        }
        if let Some(error) = error {
            self.set_status(format!("Could not save annotations: {error}"), true);
        } else if dirty {
            ctx.request_repaint_after(SAVE_DELAY);
        }
    }
}

impl eframe::App for PdfViewerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.process_backend(&ctx);
        self.process_shortcuts(&ctx);
        self.toolbar(ui);
        self.notes_panel(ui);
        self.central_panel(ui, &ctx);
        self.hover_preview(&ctx);
        if self.layout == LayoutMode::SinglePage {
            self.send_render_if_due();
        }
        self.save_annotations(&ctx);

        if self.loading.is_some()
            || !self.pending_renders.is_empty()
            || !self.pending_previews.is_empty()
            || self.selection_waiting.is_some()
        {
            ctx.request_repaint_after(Duration::from_millis(30));
        }
        if self
            .status
            .as_ref()
            .is_some_and(|status| status.created.elapsed() > Duration::from_secs(7))
        {
            self.status = None;
        } else if let Some(status) = self.status.as_ref() {
            ctx.request_repaint_after(
                Duration::from_secs(7).saturating_sub(status.created.elapsed()),
            );
        }
    }
}

impl Drop for PdfViewerApp {
    fn drop(&mut self) {
        if let Some(document) = self.document.as_mut() {
            let _ = document.annotations.save();
        }
    }
}

fn render_key(
    generation: u64,
    page_index: usize,
    display_size: Vec2,
    pixels_per_point: f32,
) -> RenderKey {
    let mut width = f64::from(display_size.x * pixels_per_point).max(1.0);
    let mut height = f64::from(display_size.y * pixels_per_point).max(1.0);
    let side_scale = (MAX_TEXTURE_SIDE / width)
        .min(MAX_TEXTURE_SIDE / height)
        .min(1.0);
    let pixel_scale = (MAX_RENDER_PIXELS / (width * height)).sqrt().min(1.0);
    let scale = side_scale.min(pixel_scale);
    width *= scale;
    height *= scale;
    RenderKey {
        generation,
        page_index,
        pixel_size: [width.round() as usize, height.round() as usize],
    }
}

fn load_texture(ctx: &egui::Context, name: String, image: RenderedImage) -> TextureHandle {
    ctx.load_texture(
        name,
        ColorImage::from_rgba_unmultiplied([image.width, image.height], &image.rgba),
        TextureOptions::LINEAR,
    )
}

fn rect_to_screen(rect: NormalizedRect, page_rect: Rect) -> Rect {
    Rect::from_min_size(
        pos2(
            page_rect.left() + rect.x * page_rect.width(),
            page_rect.top() + rect.y * page_rect.height(),
        ),
        vec2(
            rect.width * page_rect.width(),
            rect.height * page_rect.height(),
        ),
    )
}

fn point_to_normalized(point: Pos2, page_rect: Rect) -> [f32; 2] {
    [
        ((point.x - page_rect.left()) / page_rect.width()).clamp(0.0, 1.0),
        ((point.y - page_rect.top()) / page_rect.height()).clamp(0.0, 1.0),
    ]
}

fn compact_text(text: &str, max_chars: usize) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = normalized.chars();
    let compact: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{compact}…")
    } else {
        compact
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_size_respects_pixel_budget() {
        let key = render_key(1, 0, vec2(20_000.0, 20_000.0), 2.0);
        assert!(key.pixel_size[0] <= MAX_TEXTURE_SIDE as usize);
        assert!(key.pixel_size[1] <= MAX_TEXTURE_SIDE as usize);
        assert!(key.pixel_size[0] * key.pixel_size[1] <= MAX_RENDER_PIXELS as usize + 20_000);
    }

    #[test]
    fn compact_text_collapses_whitespace_and_truncates() {
        assert_eq!(compact_text("a\n  b c", 20), "a b c");
        assert_eq!(compact_text("abcdefgh", 4), "abcd…");
    }

    #[test]
    fn coordinate_round_trip_is_stable() {
        let page = Rect::from_min_size(pos2(20.0, 30.0), vec2(600.0, 800.0));
        let point = pos2(170.0, 230.0);
        let normalized = point_to_normalized(point, page);
        assert!((normalized[0] - 0.25).abs() < f32::EPSILON);
        assert!((normalized[1] - 0.25).abs() < f32::EPSILON);
    }
}
