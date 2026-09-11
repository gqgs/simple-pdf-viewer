use std::{
    collections::hash_map::DefaultHasher,
    ffi::CStr,
    fs::File,
    hash::{Hash, Hasher},
    io::{self, Read},
    path::{Path, PathBuf},
    thread,
};

use cairo::{Context, Format, ImageSurface};
use crossbeam_channel::{Receiver, Sender, unbounded};
use poppler::{Document, SelectionStyle, ffi};

use crate::annotations::NormalizedRect;

const PREVIEW_WIDTH: i32 = 420;
const PREVIEW_HEIGHT: i32 = 260;

#[derive(Clone, Debug, PartialEq)]
pub struct Destination {
    pub page_index: usize,
    pub x: Option<f32>,
    pub y_from_top: Option<f32>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum LinkTarget {
    Internal(Destination),
    Uri(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct LinkRegion {
    pub rect: NormalizedRect,
    pub target: LinkTarget,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PreviewKey {
    pub page_index: usize,
    x_bucket: i32,
    y_bucket: i32,
}

impl PreviewKey {
    pub fn for_destination(destination: &Destination) -> Self {
        Self {
            page_index: destination.page_index,
            x_bucket: destination
                .x
                .map_or(-1, |value| (value * 2.0).round() as i32),
            y_bucket: destination
                .y_from_top
                .map_or(-1, |value| (value * 2.0).round() as i32),
        }
    }
}

#[derive(Debug)]
pub struct RenderedImage {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

#[derive(Debug)]
pub enum PdfResponse {
    Loaded {
        generation: u64,
        path: PathBuf,
        title: Option<String>,
        fingerprint: String,
        page_sizes: Vec<[f32; 2]>,
    },
    LoadFailed {
        generation: u64,
        path: PathBuf,
        error: String,
    },
    PageRendered {
        generation: u64,
        page_index: usize,
        pixel_size: [usize; 2],
        image: RenderedImage,
        links: Vec<LinkRegion>,
    },
    PreviewRendered {
        generation: u64,
        key: PreviewKey,
        image: RenderedImage,
    },
    SelectionResolved {
        generation: u64,
        request_id: u64,
        page_index: usize,
        text: String,
        rects: Vec<NormalizedRect>,
    },
    OperationFailed {
        generation: u64,
        operation: &'static str,
        error: String,
    },
}

enum PdfCommand {
    Load {
        generation: u64,
        path: PathBuf,
    },
    RenderPage {
        generation: u64,
        page_index: usize,
        pixel_size: [usize; 2],
    },
    RenderPreview {
        generation: u64,
        destination: Destination,
        key: PreviewKey,
        pixels_per_point: f32,
    },
    ResolveSelection {
        generation: u64,
        request_id: u64,
        page_index: usize,
        selection: NormalizedRect,
    },
    Quit,
}

struct WorkerDocument {
    generation: u64,
    document: Document,
}

pub struct PdfBackend {
    command_tx: Sender<PdfCommand>,
    response_rx: Receiver<PdfResponse>,
    worker: Option<thread::JoinHandle<()>>,
}

impl PdfBackend {
    pub fn new() -> Self {
        let (command_tx, command_rx) = unbounded();
        let (response_tx, response_rx) = unbounded();
        let worker = thread::Builder::new()
            .name("pdf-renderer".into())
            .spawn(move || worker_loop(command_rx, response_tx))
            .expect("failed to start PDF rendering thread");
        Self {
            command_tx,
            response_rx,
            worker: Some(worker),
        }
    }

    pub fn load(&self, generation: u64, path: PathBuf) {
        let _ = self.command_tx.send(PdfCommand::Load { generation, path });
    }

    pub fn render_page(&self, generation: u64, page_index: usize, pixel_size: [usize; 2]) {
        let _ = self.command_tx.send(PdfCommand::RenderPage {
            generation,
            page_index,
            pixel_size,
        });
    }

    pub fn render_preview(&self, generation: u64, destination: Destination, pixels_per_point: f32) {
        let key = PreviewKey::for_destination(&destination);
        let _ = self.command_tx.send(PdfCommand::RenderPreview {
            generation,
            destination,
            key,
            pixels_per_point,
        });
    }

    pub fn resolve_selection(
        &self,
        generation: u64,
        request_id: u64,
        page_index: usize,
        selection: NormalizedRect,
    ) {
        let _ = self.command_tx.send(PdfCommand::ResolveSelection {
            generation,
            request_id,
            page_index,
            selection,
        });
    }

    pub fn try_recv(&self) -> Option<PdfResponse> {
        self.response_rx.try_recv().ok()
    }
}

impl Drop for PdfBackend {
    fn drop(&mut self) {
        let _ = self.command_tx.send(PdfCommand::Quit);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn worker_loop(command_rx: Receiver<PdfCommand>, response_tx: Sender<PdfResponse>) {
    let mut loaded: Option<WorkerDocument> = None;
    while let Ok(command) = command_rx.recv() {
        match command {
            PdfCommand::Load { generation, path } => match load_document(generation, &path) {
                Ok((worker_document, response)) => {
                    loaded = Some(worker_document);
                    let _ = response_tx.send(response);
                }
                Err(error) => {
                    let _ = response_tx.send(PdfResponse::LoadFailed {
                        generation,
                        path,
                        error,
                    });
                }
            },
            PdfCommand::RenderPage {
                generation,
                page_index,
                pixel_size,
            } => {
                let Some(state) = loaded
                    .as_ref()
                    .filter(|state| state.generation == generation)
                else {
                    continue;
                };
                let result = (|| {
                    let page = state
                        .document
                        .page(page_index as i32)
                        .ok_or_else(|| "page does not exist".to_owned())?;
                    let image = render_full_page(&page, pixel_size)?;
                    let links = extract_links(&state.document, &page)?;
                    Ok::<_, String>(PdfResponse::PageRendered {
                        generation,
                        page_index,
                        pixel_size,
                        image,
                        links,
                    })
                })();
                send_result(&response_tx, generation, "render page", result);
            }
            PdfCommand::RenderPreview {
                generation,
                destination,
                key,
                pixels_per_point,
            } => {
                let Some(state) = loaded
                    .as_ref()
                    .filter(|state| state.generation == generation)
                else {
                    continue;
                };
                let result =
                    render_preview(&state.document, &destination, pixels_per_point).map(|image| {
                        PdfResponse::PreviewRendered {
                            generation,
                            key,
                            image,
                        }
                    });
                send_result(&response_tx, generation, "render preview", result);
            }
            PdfCommand::ResolveSelection {
                generation,
                request_id,
                page_index,
                selection,
            } => {
                let Some(state) = loaded
                    .as_ref()
                    .filter(|state| state.generation == generation)
                else {
                    continue;
                };
                let result = resolve_selection(&state.document, page_index, selection).map(
                    |(text, rects)| PdfResponse::SelectionResolved {
                        generation,
                        request_id,
                        page_index,
                        text,
                        rects,
                    },
                );
                send_result(&response_tx, generation, "select text", result);
            }
            PdfCommand::Quit => break,
        }
    }
}

fn send_result(
    response_tx: &Sender<PdfResponse>,
    generation: u64,
    operation: &'static str,
    result: Result<PdfResponse, String>,
) {
    let response = result.unwrap_or_else(|error| PdfResponse::OperationFailed {
        generation,
        operation,
        error,
    });
    let _ = response_tx.send(response);
}

fn load_document(generation: u64, path: &Path) -> Result<(WorkerDocument, PdfResponse), String> {
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("could not open {}: {error}", path.display()))?;
    let uri = glib::filename_to_uri(&canonical, None)
        .map_err(|error| format!("could not convert path to URI: {error}"))?;
    let document = Document::from_file(uri.as_str(), None)
        .map_err(|error| format!("Poppler could not open the PDF: {error}"))?;
    let page_count = document.n_pages().max(0) as usize;
    if page_count == 0 {
        return Err("the PDF has no pages".into());
    }
    let mut page_sizes = Vec::with_capacity(page_count);
    for page_index in 0..page_count {
        let page = document
            .page(page_index as i32)
            .ok_or_else(|| format!("could not inspect page {}", page_index + 1))?;
        let (width, height) = page.size();
        page_sizes.push([width as f32, height as f32]);
    }
    let fingerprint = fingerprint_file(&canonical)
        .map_err(|error| format!("could not fingerprint the PDF: {error}"))?;
    let title = document.title().map(|title| title.to_string());
    let response = PdfResponse::Loaded {
        generation,
        path: canonical,
        title,
        fingerprint,
        page_sizes,
    };
    Ok((
        WorkerDocument {
            generation,
            document,
        },
        response,
    ))
}

fn fingerprint_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn render_full_page(page: &poppler::Page, pixel_size: [usize; 2]) -> Result<RenderedImage, String> {
    let (page_width, page_height) = page.size();
    let width = pixel_size[0].max(1).min(i32::MAX as usize) as i32;
    let height = pixel_size[1].max(1).min(i32::MAX as usize) as i32;
    let scale_x = f64::from(width) / page_width;
    let scale_y = f64::from(height) / page_height;
    render_surface(width, height, |context| {
        context.scale(scale_x, scale_y);
        page.render(context);
    })
}

fn render_preview(
    document: &Document,
    destination: &Destination,
    pixels_per_point: f32,
) -> Result<RenderedImage, String> {
    let page = document
        .page(destination.page_index as i32)
        .ok_or_else(|| "preview destination page does not exist".to_owned())?;
    let (page_width, page_height) = page.size();
    let output_width = ((PREVIEW_WIDTH as f32) * pixels_per_point.clamp(1.0, 2.0)).round() as i32;
    let output_height = ((PREVIEW_HEIGHT as f32) * pixels_per_point.clamp(1.0, 2.0)).round() as i32;

    match (destination.x, destination.y_from_top) {
        (Some(target_x), Some(target_y)) => {
            let crop_width = (page_width * 0.62).clamp(250.0, 380.0);
            let crop_height = crop_width * f64::from(output_height) / f64::from(output_width);
            let crop_x = (f64::from(target_x) - crop_width * 0.5)
                .clamp(0.0, (page_width - crop_width).max(0.0));
            let crop_y = (f64::from(target_y) - crop_height * 0.3)
                .clamp(0.0, (page_height - crop_height).max(0.0));
            let scale =
                (f64::from(output_width) / crop_width).min(f64::from(output_height) / crop_height);
            render_surface(output_width, output_height, |context| {
                context.scale(scale, scale);
                context.translate(-crop_x, -crop_y);
                page.render(context);
            })
        }
        _ => {
            let scale =
                (f64::from(output_width) / page_width).min(f64::from(output_height) / page_height);
            let x = (f64::from(output_width) - page_width * scale) * 0.5;
            let y = (f64::from(output_height) - page_height * scale) * 0.5;
            render_surface(output_width, output_height, |context| {
                context.translate(x, y);
                context.scale(scale, scale);
                page.render(context);
            })
        }
    }
}

fn render_surface(
    width: i32,
    height: i32,
    render: impl FnOnce(&Context),
) -> Result<RenderedImage, String> {
    let mut surface = ImageSurface::create(Format::ARgb32, width, height)
        .map_err(|error| format!("could not create render surface: {error}"))?;
    {
        let context = Context::new(&surface)
            .map_err(|error| format!("could not create Cairo context: {error}"))?;
        context.set_source_rgb(1.0, 1.0, 1.0);
        context
            .paint()
            .map_err(|error| format!("could not clear render surface: {error}"))?;
        render(&context);
    }
    surface.flush();
    let stride = surface.stride() as usize;
    let data = surface
        .data()
        .map_err(|error| format!("could not read render surface: {error}"))?;
    let mut rgba = vec![0_u8; width as usize * height as usize * 4];
    for y in 0..height as usize {
        for x in 0..width as usize {
            let source = y * stride + x * 4;
            let target = (y * width as usize + x) * 4;
            #[cfg(target_endian = "little")]
            {
                rgba[target] = data[source + 2];
                rgba[target + 1] = data[source + 1];
                rgba[target + 2] = data[source];
                rgba[target + 3] = data[source + 3];
            }
            #[cfg(target_endian = "big")]
            {
                rgba[target] = data[source + 1];
                rgba[target + 1] = data[source + 2];
                rgba[target + 2] = data[source + 3];
                rgba[target + 3] = data[source];
            }
        }
    }
    drop(data);
    Ok(RenderedImage {
        width: width as usize,
        height: height as usize,
        rgba,
    })
}

fn resolve_selection(
    document: &Document,
    page_index: usize,
    selection: NormalizedRect,
) -> Result<(String, Vec<NormalizedRect>), String> {
    let page = document
        .page(page_index as i32)
        .ok_or_else(|| "selection page does not exist".to_owned())?;
    let (page_width, page_height) = page.size();
    let mut pdf_rect = poppler::Rectangle::new();
    pdf_rect.set_x1(f64::from(selection.x) * page_width);
    pdf_rect.set_x2(f64::from(selection.x + selection.width) * page_width);
    pdf_rect.set_y1(f64::from(1.0 - selection.y - selection.height) * page_height);
    pdf_rect.set_y2(f64::from(1.0 - selection.y) * page_height);
    let text = page
        .selected_text(SelectionStyle::Word, &mut pdf_rect)
        .map(|text| text.trim().to_owned())
        .unwrap_or_default();
    let Some(region) = page.selected_region(1.0, SelectionStyle::Word, &mut pdf_rect) else {
        return Ok((text, Vec::new()));
    };
    let mut rects = Vec::with_capacity(region.num_rectangles().max(0) as usize);
    for index in 0..region.num_rectangles() {
        let rect = region.rectangle(index);
        let normalized = NormalizedRect {
            x: rect.x() as f32 / page_width as f32,
            y: rect.y() as f32 / page_height as f32,
            width: rect.width() as f32 / page_width as f32,
            height: rect.height() as f32 / page_height as f32,
        };
        if normalized.width > 0.0 && normalized.height > 0.0 {
            rects.push(normalized);
        }
    }
    Ok((text, rects))
}

fn extract_links(document: &Document, page: &poppler::Page) -> Result<Vec<LinkRegion>, String> {
    let (page_width, page_height) = page.size();
    let page_count = document.n_pages().max(0) as usize;
    let mut result = Vec::new();
    for mapping in page.link_mapping() {
        // SAFETY: `mapping` owns a valid PopplerLinkMapping for the duration of this loop. We
        // validate all nested pointers before reading them and immediately copy data into Rust
        // owned values; nothing borrowed escapes this block.
        let parsed = unsafe {
            let mapping_ptr = mapping.as_ptr();
            if mapping_ptr.is_null() {
                None
            } else {
                let mapping = &*mapping_ptr;
                let target = parse_action(document, mapping.action, page_count);
                target.map(|target| {
                    let x1 = mapping.area.x1.min(mapping.area.x2);
                    let x2 = mapping.area.x1.max(mapping.area.x2);
                    let y1 = mapping.area.y1.min(mapping.area.y2);
                    let y2 = mapping.area.y1.max(mapping.area.y2);
                    LinkRegion {
                        rect: NormalizedRect {
                            x: (x1 / page_width) as f32,
                            y: ((page_height - y2) / page_height) as f32,
                            width: ((x2 - x1) / page_width) as f32,
                            height: ((y2 - y1) / page_height) as f32,
                        },
                        target,
                    }
                })
            }
        };
        if let Some(link) = parsed {
            result.push(link);
        }
    }
    Ok(result)
}

unsafe fn parse_action(
    document: &Document,
    action: *mut ffi::PopplerAction,
    page_count: usize,
) -> Option<LinkTarget> {
    if action.is_null() {
        return None;
    }
    // SAFETY: caller guarantees `action` points to a live PopplerAction union.
    let action_type = unsafe { (*action).type_ };
    match action_type {
        ffi::POPPLER_ACTION_GOTO_DEST => {
            // SAFETY: PopplerAction's discriminator was checked above.
            let destination = unsafe { (*action).goto_dest.dest };
            // SAFETY: destination is owned by the live action.
            unsafe { parse_destination(document, destination, page_count, 0) }
                .map(LinkTarget::Internal)
        }
        ffi::POPPLER_ACTION_URI => {
            // SAFETY: PopplerAction's discriminator was checked above.
            let uri = unsafe { (*action).uri.uri };
            if uri.is_null() {
                None
            } else {
                // SAFETY: Poppler promises a NUL-terminated URI for this action type.
                let uri = unsafe { CStr::from_ptr(uri) }
                    .to_string_lossy()
                    .into_owned();
                (!uri.is_empty()).then_some(LinkTarget::Uri(uri))
            }
        }
        _ => None,
    }
}

unsafe fn parse_destination(
    document: &Document,
    destination: *mut ffi::PopplerDest,
    page_count: usize,
    depth: usize,
) -> Option<Destination> {
    if destination.is_null() || depth > 2 {
        return None;
    }
    // SAFETY: caller guarantees `destination` is owned by a live Poppler action or Dest wrapper.
    let destination = unsafe { &*destination };
    if destination.type_ == ffi::POPPLER_DEST_NAMED && !destination.named_dest.is_null() {
        // SAFETY: Poppler provides a NUL-terminated named destination.
        let name = unsafe { CStr::from_ptr(destination.named_dest) }
            .to_string_lossy()
            .into_owned();
        let resolved = document.find_dest(&name)?;
        // SAFETY: `resolved` remains live for this recursive call.
        return unsafe { parse_destination(document, resolved.as_ptr(), page_count, depth + 1) };
    }

    let page_index = destination.page_num.checked_sub(1)? as usize;
    if page_index >= page_count {
        return None;
    }
    let page = document.page(page_index as i32)?;
    let (page_width, page_height) = page.size();
    let coordinate_destination = matches!(
        destination.type_,
        ffi::POPPLER_DEST_XYZ | ffi::POPPLER_DEST_FITH | ffi::POPPLER_DEST_FITBH
    );
    Some(Destination {
        page_index,
        x: coordinate_destination.then_some(destination.left.clamp(0.0, page_width) as f32),
        y_from_top: coordinate_destination
            .then_some((page_height - destination.top).clamp(0.0, page_height) as f32),
    })
}

pub fn render_cache_hash(page_index: usize, pixel_size: [usize; 2]) -> u64 {
    let mut hasher = DefaultHasher::new();
    page_index.hash(&mut hasher);
    pixel_size.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_keys_are_stable_and_quantized() {
        let first = PreviewKey::for_destination(&Destination {
            page_index: 4,
            x: Some(10.01),
            y_from_top: Some(20.01),
        });
        let second = PreviewKey::for_destination(&Destination {
            page_index: 4,
            x: Some(10.02),
            y_from_top: Some(20.02),
        });
        assert_eq!(first, second);
    }

    #[test]
    fn sample_pdf_supports_rendering_links_previews_and_selection() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("sample.pdf");
        let (state, response) = load_document(7, &path).expect("sample PDF should load");
        let PdfResponse::Loaded { page_sizes, .. } = response else {
            panic!("expected a loaded response");
        };
        assert_eq!(page_sizes.len(), 53);

        let first_page = state.document.page(0).expect("first page");
        let image = render_full_page(&first_page, [320, 452]).expect("first page should render");
        assert_eq!(image.rgba.len(), 320 * 452 * 4);
        assert!(
            image
                .rgba
                .chunks_exact(4)
                .any(|pixel| pixel[0] < 245 || pixel[1] < 245 || pixel[2] < 245),
            "rendered page should contain non-white content"
        );

        let (text, rects) = resolve_selection(
            &state.document,
            0,
            NormalizedRect {
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: 1.0,
            },
        )
        .expect("selection should resolve");
        assert!(text.contains("Ingressing Minds"));
        assert!(!rects.is_empty());

        let internal_destination = (0..state.document.n_pages())
            .find_map(|page_index| {
                let page = state.document.page(page_index)?;
                extract_links(&state.document, &page)
                    .ok()?
                    .into_iter()
                    .find_map(|link| match link.target {
                        LinkTarget::Internal(destination) => Some(destination),
                        LinkTarget::Uri(_) => None,
                    })
            })
            .expect("sample PDF should contain an internal link");
        let preview = render_preview(&state.document, &internal_destination, 1.0)
            .expect("internal destination should render a preview");
        assert_eq!([preview.width, preview.height], [420, 260]);
    }
}
