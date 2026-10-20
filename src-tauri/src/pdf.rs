use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::ImageEncoder;
use pdfium_render::prelude::*;
use serde::{Deserialize, Serialize};
use tauri::ipc::Response;
use tauri::State;

/// Pdfium se inicializa una sola vez y vive toda la app, así los documentos
/// abiertos pueden ser `PdfDocument<'static>` y guardarse en el estado de Tauri.
static PDFIUM: OnceLock<Pdfium> = OnceLock::new();

/// Cantidad máxima de pasos de deshacer que se guardan en memoria.
const HISTORY_LIMIT: usize = 30;

/// Ancho de referencia (en píxeles) para convertir coordenadas relativas a puntos PDF.
const REF_WIDTH: Pixels = 10_000;

pub struct OpenDoc {
    pub(crate) doc: PdfDocument<'static>,
    path: PathBuf,
    /// Copias del documento previas a cada edición.
    undo: Vec<Vec<u8>>,
    redo: Vec<Vec<u8>>,
}

pub struct AppState {
    pub doc: Mutex<Option<OpenDoc>>,
}

#[derive(Serialize)]
pub struct PageSize {
    width: f32,
    height: f32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DocInfo {
    path: String,
    pages: Vec<PageSize>,
    can_undo: bool,
    can_redo: bool,
}

/// Punto relativo a la página tal como se muestra en pantalla:
/// (0, 0) es la esquina superior izquierda y (1, 1) la inferior derecha.
#[derive(Serialize, Deserialize, Clone, Copy)]
pub struct RelPoint {
    x: f32,
    y: f32,
}

/// Una línea de texto; `x`/`y` marcan el inicio de la línea base.
#[derive(Deserialize)]
pub struct TextLine {
    text: String,
    x: f32,
    y: f32,
}

/// Rectángulo en coordenadas relativas a la vista.
#[derive(Serialize)]
pub struct RelRect {
    from: RelPoint,
    to: RelPoint,
}

/// Texto agregado con la herramienta de texto, listo para volver a editarse.
#[derive(Serialize)]
pub struct TextInfo {
    lines: Vec<String>,
    size: f32,
    color: String,
    /// Inicio de la línea base de la primera línea.
    anchor: RelPoint,
}

#[derive(Serialize)]
pub struct AnnotationHit {
    annotation: PdfPageAnnotationIndex,
    rect: RelRect,
    /// Presente solo si la anotación es un texto editable.
    text: Option<TextInfo>,
}

/// Fragmento de texto original de la página: un renglón (o parte de uno) con la misma
/// fuente, tamaño y color. Muchos PDFs guardan cada letra como un objeto aparte, así que
/// un fragmento puede abarcar varios objetos.
#[derive(Serialize)]
pub struct TextObjectHit {
    /// Índices de los objetos, de izquierda a derecha.
    objects: Vec<PdfPageObjectIndex>,
    text: String,
    rect: RelRect,
    /// Tamaño de letra efectivo, en puntos.
    size: f32,
    color: String,
    font: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TextEditResult {
    doc: DocInfo,
    /// La fuente original no tenía todas las letras y se usó Helvetica.
    font_replaced: bool,
}

/// Busca `libpdfium` en los recursos del bundle y, en desarrollo, en `src-tauri/pdfium`.
pub fn init_pdfium(resource_dir: Option<PathBuf>) -> Result<(), String> {
    if PDFIUM.get().is_some() {
        return Ok(());
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(dir) = resource_dir {
        candidates.push(dir.join("pdfium"));
    }
    candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("pdfium"));

    let mut last_err = String::from("no se encontró libpdfium");
    for dir in candidates {
        let lib = Pdfium::pdfium_platform_library_name_at_path(&dir);
        if !lib.exists() {
            continue;
        }
        match Pdfium::bind_to_library(&lib) {
            Ok(bindings) => {
                let _ = PDFIUM.set(Pdfium::new(bindings));
                return Ok(());
            }
            Err(e) => last_err = format!("{}: {e}", lib.display()),
        }
    }
    Err(last_err)
}

pub(crate) fn pdfium() -> Result<&'static Pdfium, String> {
    PDFIUM.get().ok_or_else(|| "pdfium no está inicializado".into())
}

pub(crate) fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

pub(crate) fn load(bytes: Vec<u8>, password: Option<&str>) -> Result<PdfDocument<'static>, String> {
    pdfium()?
        .load_pdf_from_byte_vec(bytes, password)
        .map_err(|e| match e {
            PdfiumError::PdfiumLibraryInternalError(PdfiumInternalError::PasswordError) => {
                "el PDF está protegido con contraseña".into()
            }
            e => err(e),
        })
}

fn doc_info(open: &OpenDoc) -> DocInfo {
    let pages = open
        .doc
        .pages()
        .iter()
        .map(|p| PageSize {
            width: p.width().value,
            height: p.height().value,
        })
        .collect();
    DocInfo {
        path: open.path.to_string_lossy().into_owned(),
        pages,
        can_undo: !open.undo.is_empty(),
        can_redo: !open.redo.is_empty(),
    }
}



fn get_page(open: &OpenDoc, index: PdfPageIndex) -> Result<PdfPage<'static>, String> {
    open.doc.pages().get(index).map_err(err)
}

pub(crate) fn render(page: &PdfPage, width: u32) -> Result<image::RgbaImage, String> {
    let config = PdfRenderConfig::new()
        .set_target_width(width.clamp(16, 8000) as Pixels)
        .render_form_data(true)
        .render_annotations(true);
    Ok(page
        .render_with_config(&config)
        .map_err(err)?
        .as_image()
        .map_err(err)?
        .to_rgba8())
}

pub(crate) fn encode_png(image: &image::RgbaImage) -> Result<Vec<u8>, String> {
    let mut png = Vec::new();
    PngEncoder::new_with_quality(Cursor::new(&mut png), CompressionType::Fast, FilterType::Sub)
        .write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            image::ExtendedColorType::Rgba8,
        )
        .map_err(err)?;
    Ok(png)
}

/// Convierte un punto relativo a la vista en puntos PDF, respetando la rotación de la página.
fn to_points(page: &PdfPage, p: RelPoint) -> Result<(f32, f32), String> {
    let height = REF_WIDTH as f32 * page.height().value / page.width().value;
    let config = PdfRenderConfig::new().set_target_width(REF_WIDTH);
    let (x, y) = page
        .pixels_to_points(
            (p.x * REF_WIDTH as f32).round() as Pixels,
            (p.y * height).round() as Pixels,
            &config,
        )
        .map_err(err)?;
    Ok((x.value, y.value))
}

/// Rectángulo en puntos PDF a partir de dos esquinas opuestas en coordenadas relativas.
fn to_rect(page: &PdfPage, a: RelPoint, b: RelPoint) -> Result<PdfRect, String> {
    let (ax, ay) = to_points(page, a)?;
    let (bx, by) = to_points(page, b)?;
    Ok(PdfRect::new_from_values(
        ay.min(by),
        ax.min(bx),
        ay.max(by),
        ax.max(bx),
    ))
}

fn expand(rect: PdfRect, by: f32) -> PdfRect {
    PdfRect::new_from_values(
        rect.bottom().value - by,
        rect.left().value - by,
        rect.top().value + by,
        rect.right().value + by,
    )
}

/// Convierte puntos PDF a coordenadas relativas a la vista (inversa de `to_points`).
fn to_rel(page: &PdfPage, x: f32, y: f32) -> Result<RelPoint, String> {
    let height = REF_WIDTH as f32 * page.height().value / page.width().value;
    let config = PdfRenderConfig::new().set_target_width(REF_WIDTH);
    let (px, py) = page
        .points_to_pixels(PdfPoints::new(x), PdfPoints::new(y), &config)
        .map_err(err)?;
    Ok(RelPoint {
        x: px as f32 / REF_WIDTH as f32,
        y: py as f32 / height,
    })
}

fn rect_to_rel(page: &PdfPage, rect: PdfRect) -> Result<RelRect, String> {
    let a = to_rel(page, rect.left().value, rect.top().value)?;
    let b = to_rel(page, rect.right().value, rect.bottom().value)?;
    Ok(RelRect {
        from: RelPoint {
            x: a.x.min(b.x),
            y: a.y.min(b.y),
        },
        to: RelPoint {
            x: a.x.max(b.x),
            y: a.y.max(b.y),
        },
    })
}

fn contains(rect: PdfRect, x: f32, y: f32, tolerance: f32) -> bool {
    x >= rect.left().value - tolerance
        && x <= rect.right().value + tolerance
        && y >= rect.bottom().value - tolerance
        && y <= rect.top().value + tolerance
}

/// Índice de la anotación de marcado que está más arriba en el punto (x, y).
fn hit_annotation(page: &PdfPage, x: f32, y: f32) -> Option<PdfPageAnnotationIndex> {
    use PdfPageAnnotationType::*;

    let annotations = page.annotations();
    // Se recorre de atrás hacia adelante: la última anotación es la que se dibuja encima.
    (0..annotations.len()).rev().find(|&i| {
        annotations.get(i).is_ok_and(|a| {
            matches!(
                a.annotation_type(),
                Ink | Highlight
                    | Square
                    | Circle
                    | Stamp
                    | FreeText
                    | Text
                    | Underline
                    | Strikeout
                    | Squiggly
                    | Line
                    | Polygon
                    | Polyline
                    | Caret
            ) && a.bounds().is_ok_and(|r| contains(r, x, y, 3.0))
        })
    })
}

/// Lee el texto de una anotación creada con `insert_text`: un sello que solo contiene texto.
fn text_info(page: &PdfPage, annotation: &PdfPageAnnotation) -> Option<TextInfo> {
    if annotation.annotation_type() != PdfPageAnnotationType::Stamp {
        return None;
    }
    let objects = annotation.objects();
    let all_text = (0..objects.len())
        .all(|i| objects.get(i).is_ok_and(|o| o.as_text_object().is_some()));
    let first = objects.get(0).ok()?;
    let first = first.as_text_object().filter(|_| all_text)?;
    let (x, y) = first.get_translation();
    // Pdfium no puede extraer el texto de objetos dentro de una anotación,
    // así que las líneas se leen del campo `contents` que guarda `insert_text`.
    let lines: Vec<String> = annotation.contents()?.split('\n').map(String::from).collect();
    if lines.len() != objects.len() {
        return None;
    }
    Some(TextInfo {
        lines,
        size: first.unscaled_font_size().value,
        color: color_hex(&first.fill_color().ok()?),
        anchor: to_rel(page, x.value, y.value).ok()?,
    })
}

/// Índice del objeto de texto visible que está más arriba en el punto (x, y).
fn hit_text_object(page: &PdfPage, x: f32, y: f32) -> Option<PdfPageObjectIndex> {
    let objects = page.objects();
    (0..objects.len()).rev().find(|&i| {
        objects.get(i).is_ok_and(|o| {
            o.as_text_object().is_some_and(|t| t.is_visible())
                && o.bounds().is_ok_and(|q| contains(q.to_rect(), x, y, 1.0))
        })
    })
}

/// Datos de un objeto de texto necesarios para agruparlo con sus vecinos.
struct Glyph {
    index: PdfPageObjectIndex,
    left: f32,
    right: f32,
    baseline: f32,
    size: f32,
    font: String,
    color: (u8, u8, u8),
}

fn glyph(index: PdfPageObjectIndex, object: &PdfPageObject) -> Option<Glyph> {
    let text = object.as_text_object().filter(|t| t.is_visible())?;
    let rect = object.bounds().ok()?.to_rect();
    let color = text.fill_color().unwrap_or(PdfColor::BLACK);
    Some(Glyph {
        index,
        left: rect.left().value,
        right: rect.right().value,
        baseline: text.get_vertical_translation().value,
        size: text.scaled_font_size().value,
        font: text.font().name(),
        color: (color.red(), color.green(), color.blue()),
    })
}

/// Objetos de texto contiguos al objeto `hit` en el mismo renglón y con el mismo estilo,
/// ordenados de izquierda a derecha.
fn text_run(page: &PdfPage, hit: PdfPageObjectIndex) -> Result<Vec<Glyph>, String> {
    let objects = page.objects();
    let target = glyph(hit, &objects.get(hit).map_err(err)?).ok_or("el objeto no es texto")?;
    let mut line: Vec<Glyph> = (0..objects.len())
        .filter_map(|i| glyph(i, &objects.get(i).ok()?))
        .filter(|g| {
            (g.baseline - target.baseline).abs() < 0.5
                && (g.size - target.size).abs() < 0.05
                && g.font == target.font
                && g.color == target.color
        })
        .collect();
    line.sort_by(|a, b| a.left.total_cmp(&b.left));

    // Dos objetos siguen en el mismo fragmento si el hueco entre ellos es menor
    // a ~1 espacio y medio; un hueco mayor separa columnas o bloques distintos.
    let max_gap = target.size * 0.6;
    let at = line.iter().position(|g| g.index == hit).unwrap_or(0);
    let mut start = at;
    while start > 0 && line[start].left - line[start - 1].right <= max_gap {
        start -= 1;
    }
    let mut end = at;
    while end + 1 < line.len() && line[end + 1].left - line[end].right <= max_gap {
        end += 1;
    }
    Ok(line.drain(start..=end).collect())
}

/// Texto de un fragmento. Algunos PDFs no guardan los espacios como caracteres, así que
/// se agrega uno cuando hay un hueco visible entre dos objetos.
fn run_text(page: &PdfPage, run: &[Glyph]) -> Result<String, String> {
    let page_text = page.text().map_err(err)?;
    let objects = page.objects();
    let mut text = String::new();
    let mut prev_right: Option<f32> = None;
    for g in run {
        let object = objects.get(g.index).map_err(err)?;
        let piece = page_text.for_object(object.as_text_object().ok_or("el objeto no es texto")?);
        if let Some(prev) = prev_right {
            let gap = g.left - prev;
            let has_space = text.ends_with(char::is_whitespace) || piece.starts_with(char::is_whitespace);
            if gap > g.size * 0.28 && !has_space {
                text.push(' ');
            }
        }
        text.push_str(&piece);
        prev_right = Some(prev_right.map_or(g.right, |p| p.max(g.right)));
    }
    Ok(text.trim_end().to_string())
}

/// Referencias a objetos de la página; siguen siendo válidas aunque se quiten otros objetos.
pub(crate) fn objects_at(
    page: &mut PdfPage<'static>,
    indices: &[PdfPageObjectIndex],
) -> Result<Vec<PdfPageObject<'static>>, String> {
    indices
        .iter()
        .map(|&i| page.objects_mut().get(i).map_err(err))
        .collect()
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Align {
    Left,
    Center,
    Right,
}

/// Reemplaza el texto de un fragmento (objetos de izquierda a derecha): el texto completo
/// queda en el primer objeto y se quitan los demás. Si la fuente incrustada no tiene alguna
/// de las letras nuevas, se vuelve a crear con Helvetica. El resultado se alinea respecto
/// del ancho que ocupaba el fragmento original. Un texto vacío elimina el fragmento.
///
/// `objects` se obtiene con `objects_at` antes de modificar la página, para que quitar
/// objetos de un campo no cambie los índices de otro.
/// `known` guarda las letras conocidas de cada fuente entre llamadas.
/// Devuelve `true` si se tuvo que usar Helvetica.
pub(crate) fn replace_run(
    doc: &mut PdfDocument<'static>,
    page: &mut PdfPage<'static>,
    mut objects: Vec<PdfPageObject<'static>>,
    text: &str,
    align: Align,
    known: &mut HashMap<String, HashSet<char>>,
) -> Result<bool, String> {
    if objects.is_empty() || objects.iter().any(|o| o.as_text_object().is_none()) {
        return Err("el objeto no es texto".into());
    }
    let (mut left, mut right) = (f32::MAX, f32::MIN);
    for o in &objects {
        let r = o.bounds().map_err(err)?.to_rect();
        left = left.min(r.left().value);
        right = right.max(r.right().value);
    }

    // Pdfium falla con textos vacíos, así que vaciar un fragmento lo elimina.
    if text.trim().is_empty() {
        for o in objects {
            page.objects_mut().remove_object(o).map_err(err)?;
        }
        return Ok(false);
    }

    let mut first = objects.remove(0);
    for o in objects {
        page.objects_mut().remove_object(o).map_err(err)?;
    }
    let (embedded, font_name, matrix, size, color) = {
        let original = first.as_text_object().ok_or("el objeto no es texto")?;
        let font = original.font();
        (
            font.is_embedded().unwrap_or(false),
            font.name(),
            original.matrix().map_err(err)?,
            original.unscaled_font_size(),
            original.fill_color().unwrap_or(PdfColor::BLACK),
        )
    };
    let missing = embedded && {
        let chars = known
            .entry(font_name)
            .or_insert_with_key(|name| known_chars(doc, name));
        text.chars().any(|c| !c.is_whitespace() && !chars.contains(&c))
    };

    let mut result = if missing {
        page.objects_mut().remove_object(first).map_err(err)?;
        let helvetica = doc.fonts_mut().helvetica();
        let mut replacement = PdfPageTextObject::new(doc, text, helvetica, size).map_err(err)?;
        replacement.set_fill_color(color).map_err(err)?;
        replacement.apply_matrix(matrix).map_err(err)?;
        page.objects_mut()
            .add_text_object(replacement)
            .map_err(err)?
    } else {
        first
            .as_text_object_mut()
            .ok_or("el objeto no es texto")?
            .set_text(text)
            .map_err(err)?;
        first
    };

    let r = result.bounds().map_err(err)?.to_rect();
    let dx = match align {
        Align::Left => left - r.left().value,
        Align::Center => (left + right - r.left().value - r.right().value) / 2.0,
        Align::Right => right - r.right().value,
    };
    if dx.abs() > 0.01 {
        result
            .as_text_object_mut()
            .ok_or("el objeto no es texto")?
            .translate(PdfPoints::new(dx), PdfPoints::new(0.0))
            .map_err(err)?;
    }
    page.regenerate_content().map_err(err)?;
    Ok(missing)
}

/// Letras que se sabe que tiene una fuente incrustada: las que ya se usan con ella en el
/// documento. Las fuentes incrustadas suelen venir recortadas a solo esas letras.
fn known_chars(doc: &PdfDocument, font_name: &str) -> HashSet<char> {
    let mut chars = HashSet::new();
    for page in doc.pages().iter() {
        let Ok(text) = page.text() else { continue };
        for c in text.chars().iter() {
            if c.font_name() == font_name {
                chars.extend(c.unicode_char());
            }
        }
    }
    chars
}

fn color_hex(color: &PdfColor) -> String {
    format!("#{:02x}{:02x}{:02x}", color.red(), color.green(), color.blue())
}

/// Agrega texto como una anotación de sello, para que se pueda mover, editar o borrar
/// sin tocar el contenido original de la página.
fn insert_text(
    doc: &mut PdfDocument<'static>,
    page: &mut PdfPage<'static>,
    lines: &[TextLine],
    size: f32,
    color: PdfColor,
) -> Result<(), String> {
    let font = doc.fonts_mut().helvetica();
    let degrees = rotation_degrees(page);

    let mut objects = Vec::new();
    let mut bounds: Option<PdfRect> = None;
    for line in lines {
        let (x, y) = to_points(page, RelPoint { x: line.x, y: line.y })?;
        // Las líneas vacías se guardan como un espacio para conservar el interlineado.
        let text = if line.text.is_empty() { " " } else { &line.text };
        let mut object =
            PdfPageTextObject::new(doc, text, font, PdfPoints::new(size)).map_err(err)?;
        object.set_fill_color(color).map_err(err)?;
        // Se gira en sentido contrario a la página para que el texto se lea derecho.
        if degrees != 0.0 {
            object
                .rotate_counter_clockwise_degrees(degrees)
                .map_err(err)?;
        }
        object
            .translate(PdfPoints::new(x), PdfPoints::new(y))
            .map_err(err)?;

        let rect = object.bounds().map_err(err)?.to_rect();
        bounds = Some(match bounds {
            None => rect,
            Some(b) => PdfRect::new_from_values(
                b.bottom().value.min(rect.bottom().value),
                b.left().value.min(rect.left().value),
                b.top().value.max(rect.top().value),
                b.right().value.max(rect.right().value),
            ),
        });
        objects.push(object);
    }
    let bounds = bounds.ok_or("el texto está vacío")?;

    let text: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
    let mut annotation = page
        .annotations_mut()
        .create_stamp_annotation()
        .map_err(err)?;
    annotation.set_bounds(expand(bounds, 2.0)).map_err(err)?;
    annotation.set_contents(&text.join("\n")).map_err(err)?;
    for object in objects {
        annotation
            .objects_mut()
            .add_text_object(object)
            .map_err(err)?;
    }
    Ok(())
}

/// Quita líneas vacías al final; `None` si no queda texto.
fn trim_lines(mut lines: Vec<TextLine>) -> Option<Vec<TextLine>> {
    while lines.last().is_some_and(|l| l.text.trim().is_empty()) {
        lines.pop();
    }
    (!lines.is_empty()).then_some(lines)
}

fn parse_color(hex: &str) -> Result<PdfColor, String> {
    let hex = hex.trim_start_matches('#');
    let value = u32::from_str_radix(hex, 16)
        .ok()
        .filter(|_| hex.len() == 6)
        .ok_or_else(|| format!("color inválido: #{hex}"))?;
    Ok(PdfColor::new(
        (value >> 16) as u8,
        (value >> 8) as u8,
        value as u8,
        255,
    ))
}

fn rotation_degrees(page: &PdfPage) -> f32 {
    match page.rotation() {
        Ok(PdfPageRenderRotation::Degrees90) => 90.0,
        Ok(PdfPageRenderRotation::Degrees180) => 180.0,
        Ok(PdfPageRenderRotation::Degrees270) => 270.0,
        _ => 0.0,
    }
}

impl AppState {
    pub(crate) fn with_doc<T>(
        &self,
        f: impl FnOnce(&mut OpenDoc) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut guard = self.doc.lock().map_err(err)?;
        let open = guard.as_mut().ok_or("no hay documento abierto")?;
        f(open)
    }

    /// Ejecuta una edición guardando antes una copia para poder deshacerla.
    /// Si la edición falla, el documento vuelve al estado anterior.
    fn edit(
        &self,
        f: impl FnOnce(&mut OpenDoc) -> Result<(), String>,
    ) -> Result<DocInfo, String> {
        self.with_doc(|open| {
            let snapshot = open.doc.save_to_bytes().map_err(err)?;
            if let Err(e) = f(open) {
                open.doc = load(snapshot, None)?;
                return Err(e);
            }
            open.undo.push(snapshot);
            if open.undo.len() > HISTORY_LIMIT {
                open.undo.remove(0);
            }
            open.redo.clear();
            Ok(doc_info(open))
        })
    }

    // ---------- Documento ----------

    pub fn open_pdf(&self, path: String, password: Option<String>) -> Result<DocInfo, String> {
        // Se carga en memoria para poder sobrescribir el mismo archivo al guardar.
        let bytes = std::fs::read(&path).map_err(err)?;
        let open = OpenDoc {
            doc: load(bytes, password.as_deref())?,
            path: PathBuf::from(path),
            undo: Vec::new(),
            redo: Vec::new(),
        };
        let info = doc_info(&open);
        *self.doc.lock().map_err(err)? = Some(open);
        Ok(info)
    }

    /// Guarda el documento. Sin `path`, sobrescribe el archivo original.
    pub fn save_pdf(&self, path: Option<String>) -> Result<DocInfo, String> {
        self.with_doc(|open| {
            let target = path.map(PathBuf::from).unwrap_or_else(|| open.path.clone());
            let bytes = open.doc.save_to_bytes().map_err(err)?;
            std::fs::write(&target, bytes).map_err(err)?;
            open.path = target;
            Ok(doc_info(open))
        })
    }

    /// Renderiza una página a PNG con el ancho indicado en píxeles.
    pub fn render_page(&self, index: PdfPageIndex, width: u32) -> Result<Response, String> {
        let image = self.with_doc(|open| render(&get_page(open, index)?, width))?;
        // La codificación se hace fuera del lock para no bloquear otros comandos.
        Ok(Response::new(encode_png(&image)?))
    }

    pub fn undo(&self) -> Result<DocInfo, String> {
        self.with_doc(|open| {
            let previous = open.undo.pop().ok_or("no hay nada que deshacer")?;
            let current = open.doc.save_to_bytes().map_err(err)?;
            open.doc = load(previous, None)?;
            open.redo.push(current);
            Ok(doc_info(open))
        })
    }

    pub fn redo(&self) -> Result<DocInfo, String> {
        self.with_doc(|open| {
            let next = open.redo.pop().ok_or("no hay nada que rehacer")?;
            let current = open.doc.save_to_bytes().map_err(err)?;
            open.doc = load(next, None)?;
            open.undo.push(current);
            Ok(doc_info(open))
        })
    }

    // ---------- Páginas ----------

    pub fn rotate_page(&self, index: PdfPageIndex, clockwise: bool) -> Result<DocInfo, String> {
        use PdfPageRenderRotation::*;

        self.edit(|open| {
            let mut page = get_page(open, index)?;
            let next = match (page.rotation().map_err(err)?, clockwise) {
                (None, true) | (Degrees180, false) => Degrees90,
                (Degrees90, true) | (Degrees270, false) => Degrees180,
                (Degrees180, true) | (None, false) => Degrees270,
                (Degrees270, true) | (Degrees90, false) => None,
            };
            page.set_rotation(next);
            Ok(())
        })
    }

    pub fn delete_page(&self, index: PdfPageIndex) -> Result<DocInfo, String> {
        self.edit(|open| {
            if open.doc.pages().len() <= 1 {
                return Err("el documento debe tener al menos una página".into());
            }
            get_page(open, index)?.delete().map_err(err)
        })
    }

    /// Reordena las páginas. `order[i]` es el índice actual de la página que pasa a la posición `i`.
    pub fn reorder_pages(&self, order: Vec<PdfPageIndex>) -> Result<DocInfo, String> {
        self.edit(|open| {
            let count = open.doc.pages().len();
            let mut seen = vec![false; count as usize];
            let valid = order.len() == count as usize
                && order.iter().all(|&i| {
                    (0..count).contains(&i) && !std::mem::replace(&mut seen[i as usize], true)
                });
            if !valid {
                return Err("orden de páginas inválido".into());
            }

            // Pdfium no puede mover páginas dentro de un documento, así que se copian todas
            // en el nuevo orden a un documento nuevo. Una sola importación comparte los
            // recursos (fuentes, imágenes) entre páginas en vez de duplicarlos.
            let range = order
                .iter()
                .map(|i| (i + 1).to_string())
                .collect::<Vec<_>>()
                .join(",");
            let mut doc = pdfium()?.create_new_pdf().map_err(err)?;
            doc.pages_mut()
                .copy_pages_from_document(&open.doc, &range, 0)
                .map_err(err)?;
            open.doc = doc;
            Ok(())
        })
    }

    /// Inserta todas las páginas de otro PDF en la posición `at`.
    pub fn insert_pdf(&self, path: String, at: PdfPageIndex) -> Result<DocInfo, String> {
        let other = load(std::fs::read(&path).map_err(err)?, None)?;
        self.edit(|open| {
            let count = other.pages().len();
            let at = at.clamp(0, open.doc.pages().len());
            open.doc
                .pages_mut()
                .copy_page_range_from_document(&other, 0..=count - 1, at)
                .map_err(err)
        })
    }

    /// Inserta una página en blanco del mismo tamaño que la página `like`.
    pub fn insert_blank_page(
        &self,
        at: PdfPageIndex,
        like: PdfPageIndex,
    ) -> Result<DocInfo, String> {
        self.edit(|open| {
            let model = get_page(open, like)?;
            let size = PdfPagePaperSize::new_custom(model.width(), model.height());
            let at = at.clamp(0, open.doc.pages().len());
            open.doc
                .pages_mut()
                .create_page_at_index(size, at)
                .map(|_| ())
                .map_err(err)
        })
    }

    // ---------- Anotaciones ----------

    /// Agrega texto como una anotación de sello, para que se pueda borrar o deshacer
    /// sin tocar el contenido original de la página.
    pub fn add_text(
        &self,
        index: PdfPageIndex,
        lines: Vec<TextLine>,
        size: f32,
        color: String,
    ) -> Result<DocInfo, String> {
        self.edit(|open| {
            let color = parse_color(&color)?;
            let lines = trim_lines(lines).ok_or("el texto está vacío")?;
            let mut page = get_page(open, index)?;
            insert_text(&mut open.doc, &mut page, &lines, size, color)
        })
    }

    /// Reemplaza un texto agregado con la app (para editarlo o moverlo).
    /// Sin líneas, lo borra.
    pub fn update_text(
        &self,
        index: PdfPageIndex,
        annotation: PdfPageAnnotationIndex,
        lines: Vec<TextLine>,
        size: f32,
        color: String,
    ) -> Result<DocInfo, String> {
        self.edit(|open| {
            let color = parse_color(&color)?;
            let mut page = get_page(open, index)?;
            {
                let old = page.annotations().get(annotation).map_err(err)?;
                if text_info(&page, &old).is_none() {
                    return Err("la anotación no es un texto".into());
                }
            }
            let annotations = page.annotations_mut();
            let old = annotations.get(annotation).map_err(err)?;
            annotations.delete_annotation(old).map_err(err)?;
            match trim_lines(lines) {
                Some(lines) => insert_text(&mut open.doc, &mut page, &lines, size, color),
                None => Ok(()),
            }
        })
    }

    /// Anotación de marcado en el punto indicado, si hay alguna.
    pub fn annotation_at(
        &self,
        index: PdfPageIndex,
        at: RelPoint,
    ) -> Result<Option<AnnotationHit>, String> {
        self.with_doc(|open| {
            let page = get_page(open, index)?;
            let (x, y) = to_points(&page, at)?;
            let Some(i) = hit_annotation(&page, x, y) else {
                return Ok(None);
            };
            let annotation = page.annotations().get(i).map_err(err)?;
            Ok(Some(AnnotationHit {
                annotation: i,
                rect: rect_to_rel(&page, annotation.bounds().map_err(err)?)?,
                text: text_info(&page, &annotation),
            }))
        })
    }

    pub fn delete_annotation(
        &self,
        index: PdfPageIndex,
        annotation: PdfPageAnnotationIndex,
    ) -> Result<DocInfo, String> {
        self.edit(|open| {
            let mut page = get_page(open, index)?;
            let annotations = page.annotations_mut();
            let target = annotations.get(annotation).map_err(err)?;
            annotations.delete_annotation(target).map_err(err)
        })
    }

    /// Fragmento de texto original de la página en el punto indicado, si hay alguno.
    pub fn text_object_at(
        &self,
        index: PdfPageIndex,
        at: RelPoint,
    ) -> Result<Option<TextObjectHit>, String> {
        self.with_doc(|open| {
            let page = get_page(open, index)?;
            let (x, y) = to_points(&page, at)?;
            let Some(hit) = hit_text_object(&page, x, y) else {
                return Ok(None);
            };
            let run = text_run(&page, hit)?;
            let objects = page.objects();
            let mut bounds: Option<PdfRect> = None;
            for g in &run {
                let r = objects.get(g.index).map_err(err)?.bounds().map_err(err)?.to_rect();
                bounds = Some(match bounds {
                    None => r,
                    Some(b) => PdfRect::new_from_values(
                        b.bottom().value.min(r.bottom().value),
                        b.left().value.min(r.left().value),
                        b.top().value.max(r.top().value),
                        b.right().value.max(r.right().value),
                    ),
                });
            }
            let first = &run[0];
            Ok(Some(TextObjectHit {
                text: run_text(&page, &run)?,
                rect: rect_to_rel(&page, bounds.ok_or("fragmento vacío")?)?,
                size: first.size,
                color: format!(
                    "#{:02x}{:02x}{:02x}",
                    first.color.0, first.color.1, first.color.2
                ),
                font: first.font.clone(),
                objects: run.iter().map(|g| g.index).collect(),
            }))
        })
    }

    /// Mueve un fragmento de texto original según el desplazamiento entre dos puntos.
    pub fn move_text_object(
        &self,
        index: PdfPageIndex,
        objects: Vec<PdfPageObjectIndex>,
        from: RelPoint,
        to: RelPoint,
    ) -> Result<DocInfo, String> {
        self.edit(|open| {
            let mut page = get_page(open, index)?;
            let (x0, y0) = to_points(&page, from)?;
            let (x1, y1) = to_points(&page, to)?;
            for &object in &objects {
                page.objects_mut()
                    .get(object)
                    .map_err(err)?
                    .as_text_object_mut()
                    .ok_or("el objeto no es texto")?
                    .translate(PdfPoints::new(x1 - x0), PdfPoints::new(y1 - y0))
                    .map_err(err)?;
            }
            page.regenerate_content().map_err(err)
        })
    }

    /// Cambia el contenido de un fragmento de texto original (ver `replace_run`).
    pub fn edit_text_object(
        &self,
        index: PdfPageIndex,
        objects: Vec<PdfPageObjectIndex>,
        text: String,
    ) -> Result<TextEditResult, String> {
        let mut font_replaced = false;
        let doc = self.edit(|open| {
            let mut page = get_page(open, index)?;
            let mut known = HashMap::new();
            let objects = objects_at(&mut page, &objects)?;
            font_replaced = replace_run(
                &mut open.doc,
                &mut page,
                objects,
                &text,
                Align::Left,
                &mut known,
            )?;
            Ok(())
        })?;
        Ok(TextEditResult { doc, font_replaced })
    }

    pub fn add_highlight(
        &self,
        index: PdfPageIndex,
        from: RelPoint,
        to: RelPoint,
        color: String,
    ) -> Result<DocInfo, String> {
        self.edit(|open| {
            let color = parse_color(&color)?;
            let mut page = get_page(open, index)?;
            let r = to_rect(&page, from, to)?;
            let (l, b, rt, t) = (r.left().value, r.bottom().value, r.right().value, r.top().value);

            let mut annotation = page
                .annotations_mut()
                .create_highlight_annotation()
                .map_err(err)?;
            // Pdfium espera los puntos en orden: arriba-izq, arriba-der, abajo-izq, abajo-der.
            // (`PdfQuadPoints::from_rect` usa otro orden y genera un "moño".)
            annotation
                .attachment_points_mut()
                .create_attachment_point_at_end(PdfQuadPoints::new_from_values(
                    l, t, rt, t, l, b, rt, b,
                ))
                .map_err(err)?;
            annotation.set_bounds(r).map_err(err)?;
            annotation.set_stroke_color(color).map_err(err)
        })
    }

    /// Rectángulo sin relleno.
    pub fn add_rect(
        &self,
        index: PdfPageIndex,
        from: RelPoint,
        to: RelPoint,
        width: f32,
        color: String,
    ) -> Result<DocInfo, String> {
        self.edit(|open| {
            let color = parse_color(&color)?;
            let mut page = get_page(open, index)?;
            let rect = to_rect(&page, from, to)?;
            let path = PdfPagePathObject::new_rect(
                &open.doc,
                rect,
                Some(color),
                Some(PdfPoints::new(width)),
                None,
            )
            .map_err(err)?;
            add_drawing(&mut page, rect, width, color, path)
        })
    }

    /// Trazo a mano alzada.
    pub fn add_ink(
        &self,
        index: PdfPageIndex,
        points: Vec<RelPoint>,
        width: f32,
        color: String,
    ) -> Result<DocInfo, String> {
        self.edit(|open| {
            let color = parse_color(&color)?;
            let mut page = get_page(open, index)?;
            let points = points
                .into_iter()
                .map(|p| to_points(&page, p))
                .collect::<Result<Vec<_>, _>>()?;
            let (&(x0, y0), rest) = points.split_first().ok_or("el trazo está vacío")?;

            let mut path = PdfPagePathObject::new(
                &open.doc,
                PdfPoints::new(x0),
                PdfPoints::new(y0),
                Some(color),
                Some(PdfPoints::new(width)),
                None,
            )
            .map_err(err)?;
            // Un clic sin movimiento se dibuja como un punto.
            let rest = if rest.is_empty() {
                &[(x0 + 0.01, y0)][..]
            } else {
                rest
            };
            let (mut min_x, mut min_y, mut max_x, mut max_y) = (x0, y0, x0, y0);
            for &(x, y) in rest {
                path.line_to(PdfPoints::new(x), PdfPoints::new(y))
                    .map_err(err)?;
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
            }
            path.set_line_cap(PdfPageObjectLineCap::Round)
                .map_err(err)?;
            path.set_line_join(PdfPageObjectLineJoin::Round)
                .map_err(err)?;

            let bounds = PdfRect::new_from_values(min_y, min_x, max_y, max_x);
            add_drawing(&mut page, bounds, width, color, path)
        })
    }

    /// Borra la anotación de marcado que esté más arriba en el punto indicado.
    pub fn erase_at(&self, index: PdfPageIndex, at: RelPoint) -> Result<DocInfo, String> {
        self.edit(|open| {
            let mut page = get_page(open, index)?;
            let (x, y) = to_points(&page, at)?;
            let i = hit_annotation(&page, x, y).ok_or("no hay ninguna anotación en ese punto")?;
            let annotations = page.annotations_mut();
            let target = annotations.get(i).map_err(err)?;
            annotations.delete_annotation(target).map_err(err)
        })
    }
}

/// Agrega un dibujo como anotación "ink", para que se pueda borrar sin tocar el contenido
/// original de la página.
fn add_drawing(
    page: &mut PdfPage<'static>,
    bounds: PdfRect,
    width: f32,
    color: PdfColor,
    path: PdfPagePathObject<'static>,
) -> Result<(), String> {
    let mut annotation = page
        .annotations_mut()
        .create_ink_annotation()
        .map_err(err)?;
    // Los límites deben fijarse antes de agregar el trazo o Pdfium no lo dibuja.
    annotation
        .set_bounds(expand(bounds, width))
        .map_err(err)?;
    annotation.set_stroke_color(color).map_err(err)?;
    annotation
        .objects_mut()
        .add_path_object(path)
        .map(|_| ())
        .map_err(err)
}

// ---------- Comandos de Tauri ----------

#[tauri::command]
pub async fn open_pdf(
    path: String,
    password: Option<String>,
    state: State<'_, AppState>,
) -> Result<DocInfo, String> {
    state.open_pdf(path, password)
}

#[tauri::command]
pub async fn save_pdf(
    path: Option<String>,
    state: State<'_, AppState>,
) -> Result<DocInfo, String> {
    state.save_pdf(path)
}

#[tauri::command]
pub async fn render_page(
    index: PdfPageIndex,
    width: u32,
    state: State<'_, AppState>,
) -> Result<Response, String> {
    state.render_page(index, width)
}

#[tauri::command]
pub async fn undo(
    state: State<'_, AppState>,
) -> Result<DocInfo, String> {
    state.undo()
}

#[tauri::command]
pub async fn redo(
    state: State<'_, AppState>,
) -> Result<DocInfo, String> {
    state.redo()
}

#[tauri::command]
pub async fn rotate_page(
    index: PdfPageIndex,
    clockwise: bool,
    state: State<'_, AppState>,
) -> Result<DocInfo, String> {
    state.rotate_page(index, clockwise)
}

#[tauri::command]
pub async fn delete_page(
    index: PdfPageIndex,
    state: State<'_, AppState>,
) -> Result<DocInfo, String> {
    state.delete_page(index)
}

#[tauri::command]
pub async fn reorder_pages(
    order: Vec<PdfPageIndex>,
    state: State<'_, AppState>,
) -> Result<DocInfo, String> {
    state.reorder_pages(order)
}

#[tauri::command]
pub async fn insert_pdf(
    path: String,
    at: PdfPageIndex,
    state: State<'_, AppState>,
) -> Result<DocInfo, String> {
    state.insert_pdf(path, at)
}

#[tauri::command]
pub async fn insert_blank_page(
    at: PdfPageIndex,
    like: PdfPageIndex,
    state: State<'_, AppState>,
) -> Result<DocInfo, String> {
    state.insert_blank_page(at, like)
}

#[tauri::command]
pub async fn add_text(
    index: PdfPageIndex,
    lines: Vec<TextLine>,
    size: f32,
    color: String,
    state: State<'_, AppState>,
) -> Result<DocInfo, String> {
    state.add_text(index, lines, size, color)
}

#[tauri::command]
pub async fn add_highlight(
    index: PdfPageIndex,
    from: RelPoint,
    to: RelPoint,
    color: String,
    state: State<'_, AppState>,
) -> Result<DocInfo, String> {
    state.add_highlight(index, from, to, color)
}

#[tauri::command]
pub async fn add_rect(
    index: PdfPageIndex,
    from: RelPoint,
    to: RelPoint,
    width: f32,
    color: String,
    state: State<'_, AppState>,
) -> Result<DocInfo, String> {
    state.add_rect(index, from, to, width, color)
}

#[tauri::command]
pub async fn add_ink(
    index: PdfPageIndex,
    points: Vec<RelPoint>,
    width: f32,
    color: String,
    state: State<'_, AppState>,
) -> Result<DocInfo, String> {
    state.add_ink(index, points, width, color)
}

#[tauri::command]
pub async fn erase_at(
    index: PdfPageIndex,
    at: RelPoint,
    state: State<'_, AppState>,
) -> Result<DocInfo, String> {
    state.erase_at(index, at)
}

#[tauri::command]
pub async fn update_text(
    index: PdfPageIndex,
    annotation: PdfPageAnnotationIndex,
    lines: Vec<TextLine>,
    size: f32,
    color: String,
    state: State<'_, AppState>,
) -> Result<DocInfo, String> {
    state.update_text(index, annotation, lines, size, color)
}

#[tauri::command]
pub async fn annotation_at(
    index: PdfPageIndex,
    at: RelPoint,
    state: State<'_, AppState>,
) -> Result<Option<AnnotationHit>, String> {
    state.annotation_at(index, at)
}

#[tauri::command]
pub async fn delete_annotation(
    index: PdfPageIndex,
    annotation: PdfPageAnnotationIndex,
    state: State<'_, AppState>,
) -> Result<DocInfo, String> {
    state.delete_annotation(index, annotation)
}

#[tauri::command]
pub async fn text_object_at(
    index: PdfPageIndex,
    at: RelPoint,
    state: State<'_, AppState>,
) -> Result<Option<TextObjectHit>, String> {
    state.text_object_at(index, at)
}

#[tauri::command]
pub async fn move_text_object(
    index: PdfPageIndex,
    objects: Vec<PdfPageObjectIndex>,
    from: RelPoint,
    to: RelPoint,
    state: State<'_, AppState>,
) -> Result<DocInfo, String> {
    state.move_text_object(index, objects, from, to)
}

#[tauri::command]
pub async fn edit_text_object(
    index: PdfPageIndex,
    objects: Vec<PdfPageObjectIndex>,
    text: String,
    state: State<'_, AppState>,
) -> Result<TextEditResult, String> {
    state.edit_text_object(index, objects, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RED: &str = "#e01e1e";

    /// Documento de prueba en memoria; cada página tiene un ancho distinto para identificarla.
    fn state_with_pages(count: usize) -> AppState {
        static INIT: std::sync::Once = std::sync::Once::new();
        INIT.call_once(|| init_pdfium(None).unwrap());
        let mut doc = pdfium().unwrap().create_new_pdf().unwrap();
        for i in 0..count {
            let size = PdfPagePaperSize::new_custom(
                PdfPoints::new(400.0 + i as f32 * 10.0),
                PdfPoints::new(600.0),
            );
            doc.pages_mut().create_page_at_end(size).unwrap();
        }
        AppState {
            doc: Mutex::new(Some(OpenDoc {
                doc,
                path: PathBuf::new(),
                undo: Vec::new(),
                redo: Vec::new(),
            })),
        }
    }

    fn widths(info: &DocInfo) -> Vec<f32> {
        info.pages.iter().map(|p| p.width.round()).collect()
    }

    fn info(state: &AppState) -> DocInfo {
        state.with_doc(|open| Ok(doc_info(open))).unwrap()
    }

    /// Píxeles no blancos de la página renderizada.
    fn inked_pixels(state: &AppState, index: PdfPageIndex) -> usize {
        state
            .with_doc(|open| {
                let image = get_page(open, index)?
                    .render_with_config(&PdfRenderConfig::new().set_target_width(400))
                    .map_err(err)?
                    .as_image()
                    .map_err(err)?
                    .to_rgba8();
                Ok(image
                    .pixels()
                    .filter(|p| p[0] < 200 || p[1] < 200 || p[2] < 200)
                    .count())
            })
            .unwrap()
    }

    fn p(x: f32, y: f32) -> RelPoint {
        RelPoint { x, y }
    }

    #[test]
    fn rotation_updates_page_size() {
        let state = state_with_pages(1);
        let info = state.rotate_page(0, true).unwrap();
        assert!(info.pages[0].width > info.pages[0].height);
        let info = state.rotate_page(0, false).unwrap();
        assert!(info.pages[0].width < info.pages[0].height);
    }

    #[test]
    fn reorder_delete_and_history() {
        let state = state_with_pages(3);
        assert_eq!(widths(&info(&state)), [400.0, 410.0, 420.0]);

        let info1 = state.reorder_pages(vec![2, 0, 1]).unwrap();
        assert_eq!(widths(&info1), [420.0, 400.0, 410.0]);
        assert!(state.reorder_pages(vec![0, 0, 1]).is_err());
        assert!(state.reorder_pages(vec![0, 1]).is_err());

        let info2 = state.delete_page(0).unwrap();
        assert_eq!(widths(&info2), [400.0, 410.0]);

        let undone = state.undo().unwrap();
        assert_eq!(widths(&undone), [420.0, 400.0, 410.0]);
        let undone = state.undo().unwrap();
        assert_eq!(widths(&undone), [400.0, 410.0, 420.0]);
        assert!(!undone.can_undo && undone.can_redo);
        assert!(state.undo().is_err());

        let redone = state.redo().unwrap();
        assert_eq!(widths(&redone), [420.0, 400.0, 410.0]);
    }

    #[test]
    fn failed_edit_does_not_touch_history() {
        let state = state_with_pages(1);
        assert!(state.delete_page(0).is_err());
        assert!(!info(&state).can_undo);
    }

    #[test]
    fn insert_blank_page_copies_size() {
        let state = state_with_pages(2);
        let info = state.insert_blank_page(1, 1).unwrap();
        assert_eq!(widths(&info), [400.0, 410.0, 410.0]);
        assert_eq!(inked_pixels(&state, 1), 0);
    }

    #[test]
    fn insert_pdf_from_file() {
        let other = state_with_pages(2);
        let path = std::env::temp_dir().join("pdf-studio-insert-test.pdf");
        other.save_pdf(Some(path.to_string_lossy().into())).unwrap();

        let state = state_with_pages(1);
        let info = state
            .insert_pdf(path.to_string_lossy().into(), 1)
            .unwrap();
        assert_eq!(widths(&info), [400.0, 400.0, 410.0]);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn annotations_render_and_erase() {
        let state = state_with_pages(1);
        assert_eq!(inked_pixels(&state, 0), 0);

        state.add_rect(0, p(0.2, 0.2), p(0.6, 0.4), 3.0, RED.into()).unwrap();
        let after_rect = inked_pixels(&state, 0);
        assert!(after_rect > 100, "rectángulo: {after_rect}");

        state
            .add_ink(0, vec![p(0.1, 0.8), p(0.5, 0.9), p(0.9, 0.8)], 4.0, RED.into())
            .unwrap();
        let after_ink = inked_pixels(&state, 0);
        assert!(after_ink > after_rect + 100, "trazo: {after_ink}");

        state
            .add_highlight(0, p(0.1, 0.5), p(0.9, 0.55), "#ffd400".into())
            .unwrap();
        let after_highlight = inked_pixels(&state, 0);
        assert!(after_highlight > after_ink + 1000, "resaltado: {after_highlight}");

        let lines = vec![TextLine { text: "Ñandú, ¿qué tal?".into(), x: 0.1, y: 0.1 }];
        state.add_text(0, lines, 18.0, "#000000".into()).unwrap();
        let after_text = inked_pixels(&state, 0);
        assert!(after_text > after_highlight + 100, "texto: {after_text}");

        // El texto sigue presente al guardar y volver a abrir.
        let reloaded = state
            .with_doc(|open| load(open.doc.save_to_bytes().map_err(err)?, None))
            .unwrap();
        assert_eq!(reloaded.pages().get(0).unwrap().annotations().len(), 4);

        // El borrador quita la anotación del punto y deja las demás.
        state.erase_at(0, p(0.5, 0.52)).unwrap();
        assert_eq!(inked_pixels(&state, 0), after_text - (after_highlight - after_ink));
        assert!(state.erase_at(0, p(0.95, 0.05)).is_err());
    }

    #[test]
    fn text_on_rotated_page_reads_upright() {
        let state = state_with_pages(1);
        state.rotate_page(0, true).unwrap();
        let lines = vec![TextLine { text: "Hola mundo".into(), x: 0.1, y: 0.2 }];
        state.add_text(0, lines, 24.0, "#000000".into()).unwrap();

        // En la vista, el texto debe ser más ancho que alto.
        let (w, h) = state
            .with_doc(|open| {
                let image = get_page(open, 0)?
                    .render_with_config(&PdfRenderConfig::new().set_target_width(600))
                    .map_err(err)?
                    .as_image()
                    .map_err(err)?
                    .to_rgba8();
                let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
                for (x, y, px) in image.enumerate_pixels() {
                    if px[0] < 128 {
                        (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
                    }
                }
                Ok((x1 - x0, y1 - y0))
            })
            .unwrap();
        assert!(w > h * 3, "texto girado: {w}x{h}");
    }

    fn line(text: &str, x: f32, y: f32) -> TextLine {
        TextLine {
            text: text.into(),
            x,
            y,
        }
    }

    fn close(a: RelPoint, b: RelPoint) -> bool {
        (a.x - b.x).abs() < 0.002 && (a.y - b.y).abs() < 0.002
    }

    #[test]
    fn own_text_can_be_read_back_moved_and_deleted() {
        for rotated in [false, true] {
            let state = state_with_pages(1);
            if rotated {
                state.rotate_page(0, true).unwrap();
            }
            let lines = vec![line("Hola", 0.2, 0.3), line("", 0.2, 0.35), line("mundo", 0.2, 0.4)];
            state.add_text(0, lines, 20.0, "#123456".into()).unwrap();

            let hit = state.annotation_at(0, p(0.21, 0.29)).unwrap().expect("sin anotación");
            let text = hit.text.expect("no se reconoció como texto");
            assert_eq!(text.lines, ["Hola", "", "mundo"]);
            assert_eq!(text.size, 20.0);
            assert_eq!(text.color, "#123456");
            assert!(close(text.anchor, p(0.2, 0.3)), "ancla {:?}", (text.anchor.x, text.anchor.y));

            // Mover = reemplazar en otra posición.
            let moved = vec![line("Hola", 0.5, 0.6), line("mundo", 0.5, 0.7)];
            state
                .update_text(0, hit.annotation, moved, 20.0, "#123456".into())
                .unwrap();
            assert!(state.annotation_at(0, p(0.21, 0.29)).unwrap().is_none());
            let hit = state.annotation_at(0, p(0.51, 0.59)).unwrap().unwrap();
            assert!(close(hit.text.unwrap().anchor, p(0.5, 0.6)));

            state.delete_annotation(0, hit.annotation).unwrap();
            assert!(state.annotation_at(0, p(0.51, 0.59)).unwrap().is_none());

            // Deshacer el borrado lo recupera.
            state.undo().unwrap();
            assert!(state.annotation_at(0, p(0.51, 0.59)).unwrap().is_some());
        }
    }

    #[test]
    fn drawings_are_not_editable_text() {
        let state = state_with_pages(1);
        state.add_rect(0, p(0.2, 0.2), p(0.4, 0.4), 2.0, RED.into()).unwrap();
        let hit = state.annotation_at(0, p(0.2, 0.3)).unwrap().unwrap();
        assert!(hit.text.is_none());
        assert!(state
            .update_text(0, hit.annotation, vec![line("x", 0.1, 0.1)], 12.0, RED.into())
            .is_err());
    }

    fn open_fixture() -> AppState {
        let state = state_with_pages(1);
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/test-data/subset-font.pdf");
        state.open_pdf(path.into(), None).unwrap();
        state
    }

    /// Punto cerca del inicio del primer fragmento de texto que contiene `needle`.
    fn find_text(state: &AppState, needle: &str) -> RelPoint {
        state
            .with_doc(|open| {
                let page = get_page(open, 0)?;
                let objects = page.objects();
                for i in 0..objects.len() {
                    let object = objects.get(i).map_err(err)?;
                    if object.as_text_object().is_some_and(|t| t.text().contains(needle)) {
                        let r = rect_to_rel(&page, object.bounds().map_err(err)?.to_rect())?;
                        return Ok(p(r.from.x + 0.01, (r.from.y + r.to.y) / 2.0));
                    }
                }
                Err(format!("no se encontró {needle:?}"))
            })
            .unwrap()
    }

    fn page_text(state: &AppState) -> String {
        state
            .with_doc(|open| Ok(get_page(open, 0)?.text().map_err(err)?.all()))
            .unwrap()
    }

    #[test]
    fn edit_original_text_with_known_letters() {
        let state = open_fixture();
        let at = find_text(&state, "Hola");
        let hit = state.text_object_at(0, at).unwrap().expect("sin texto");
        assert!(hit.text.contains("Hola"), "{:?}", hit.text);
        assert!(hit.font.contains("Monaco"), "{}", hit.font);

        let before = inked_pixels(&state, 0);
        let new_text = hit.text.replace("Hola", "Hela");
        let result = state.edit_text_object(0, hit.objects, new_text).unwrap();
        assert!(!result.font_replaced);
        assert!(page_text(&state).contains("Hela"));
        assert_ne!(inked_pixels(&state, 0), before);

        // Sigue igual al guardar y volver a abrir.
        let path = std::env::temp_dir().join("pdf-studio-edit-test.pdf");
        state.save_pdf(Some(path.to_string_lossy().into())).unwrap();
        state.open_pdf(path.to_string_lossy().into(), None).unwrap();
        assert!(page_text(&state).contains("Hela"));
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn edit_original_text_with_missing_letters_uses_helvetica() {
        let state = open_fixture();
        let at = find_text(&state, "Hola");
        let hit = state.text_object_at(0, at).unwrap().unwrap();
        // La fuente recortada no tiene "Ñ" ni "ú".
        let result = state
            .edit_text_object(0, hit.objects, "Ñandú".into())
            .unwrap();
        assert!(result.font_replaced);
        assert!(page_text(&state).contains("Ñandú"));

        let hit = state.text_object_at(0, at).unwrap().expect("se perdió el fragmento");
        assert_eq!(hit.text, "Ñandú");
        assert!(hit.font.contains("Helvetica"), "{}", hit.font);
    }

    #[test]
    fn move_original_text() {
        let state = open_fixture();
        let at = find_text(&state, "Hola");
        let hit = state.text_object_at(0, at).unwrap().unwrap();
        let to = p(at.x + 0.2, at.y + 0.3);
        state.move_text_object(0, hit.objects.clone(), at, to).unwrap();
        assert!(state.text_object_at(0, at).unwrap().is_none());
        let moved = state.text_object_at(0, to).unwrap().expect("no se movió");
        assert_eq!(moved.text, hit.text);
        assert!((moved.rect.from.x - hit.rect.from.x - 0.2).abs() < 0.002);
        assert!((moved.rect.from.y - hit.rect.from.y - 0.3).abs() < 0.002);
    }

    /// Página con un renglón escrito letra por letra, como lo exportan muchos generadores.
    fn state_with_letter_by_letter_text() -> AppState {
        let state = state_with_pages(1);
        state
            .with_doc(|open| {
                let font = open.doc.fonts_mut().helvetica();
                let mut page = get_page(open, 0)?;
                let mut put = |text: &str, x: f32, y: f32, size: f32| -> Result<f32, String> {
                    let mut object = PdfPageTextObject::new(&open.doc, text, font, PdfPoints::new(size))
                        .map_err(err)?;
                    object.translate(PdfPoints::new(x), PdfPoints::new(y)).map_err(err)?;
                    let right = object.bounds().map_err(err)?.to_rect().right().value;
                    page.objects_mut().add_text_object(object).map_err(err)?;
                    Ok(right)
                };
                // "Hola mundo" letra por letra, sin guardar el espacio como carácter.
                let mut x = 50.0;
                for c in "Hola".chars() {
                    x = put(&c.to_string(), x, 500.0, 12.0)? + 0.5;
                }
                x += 3.0;
                for c in "mundo".chars() {
                    x = put(&c.to_string(), x, 500.0, 12.0)? + 0.5;
                }
                // Otro bloque lejos en el mismo renglón y un título con otro tamaño.
                put("Lejos", 300.0, 500.0, 12.0)?;
                put("Titulo", 50.0, 550.0, 20.0)?;
                Ok(())
            })
            .unwrap();
        state
    }

    #[test]
    fn letter_by_letter_text_is_edited_as_one_fragment() {
        let state = state_with_letter_by_letter_text();
        let (x, y) = (60.0 / 400.0, 1.0 - 503.0 / 600.0);
        let hit = state.text_object_at(0, p(x, y)).unwrap().expect("sin texto");
        assert_eq!(hit.text, "Hola mundo");
        assert_eq!(hit.objects.len(), 9);

        state
            .edit_text_object(0, hit.objects, "Chau mundo".into())
            .unwrap();
        let text = page_text(&state);
        assert!(text.contains("Chau mundo"), "{text:?}");
        assert!(text.contains("Lejos") && text.contains("Titulo"), "{text:?}");
        let hit = state.text_object_at(0, p(x, y)).unwrap().unwrap();
        assert_eq!((hit.text.as_str(), hit.objects.len()), ("Chau mundo", 1));
    }

    #[test]
    fn letter_by_letter_text_moves_together() {
        let state = state_with_letter_by_letter_text();
        let at = p(60.0 / 400.0, 1.0 - 503.0 / 600.0);
        let hit = state.text_object_at(0, at).unwrap().unwrap();
        let to = p(at.x, at.y + 0.2);
        state.move_text_object(0, hit.objects, at, to).unwrap();
        let moved = state.text_object_at(0, to).unwrap().expect("no se movió");
        assert_eq!(moved.text, "Hola mundo");
        // "Lejos" se quedó en su lugar.
        let far = state.text_object_at(0, p(310.0 / 400.0, at.y)).unwrap().unwrap();
        assert_eq!(far.text, "Lejos");
    }

    #[test]
    fn emptying_original_text_removes_it() {
        let state = open_fixture();
        let at = find_text(&state, "Hola");
        let hit = state.text_object_at(0, at).unwrap().unwrap();
        state.edit_text_object(0, hit.objects, "  ".into()).unwrap();
        assert!(!page_text(&state).contains("Hola"));
        state.undo().unwrap();
        assert!(page_text(&state).contains("Hola"));
    }

    #[test]
    fn color_parsing() {
        assert!(parse_color("#ff8800").is_ok());
        assert!(parse_color("ff8800").is_ok());
        assert!(parse_color("#f80").is_err());
        assert!(parse_color("#zzzzzz").is_err());
    }
}
