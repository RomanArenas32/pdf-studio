//! Generación en serie: rellena campos de texto de un PDF plantilla con los datos de cada
//! fila de una tabla (CSV o Excel), por ejemplo para emitir constancias con distintos nombres.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use pdfium_render::prelude::*;
use serde::{Deserialize, Serialize};
use tauri::ipc::Response;
use tauri::{AppHandle, Emitter, State};

use crate::pdf::{
    encode_png, err, load, objects_at, pdfium, render, replace_run, Align, AppState,
};

/// Se activa desde la interfaz para detener una generación en curso.
static CANCEL: AtomicBool = AtomicBool::new(false);

#[derive(Serialize, Debug)]
pub struct Table {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
}

/// Fragmento de texto de la plantilla que se reemplaza con una columna de la tabla.
#[derive(Deserialize, Clone)]
pub struct Field {
    page: PdfPageIndex,
    /// Objetos del fragmento, de izquierda a derecha (como los devuelve `text_object_at`).
    objects: Vec<PdfPageObjectIndex>,
    column: usize,
    align: Align,
}

#[derive(Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum Output {
    /// Un PDF por fila dentro de `dir`; `names[i]` es el nombre de archivo de la fila `i`.
    Files { dir: String, names: Vec<String> },
    /// Un solo PDF con las páginas de todas las filas.
    Single { path: String },
}

#[derive(Serialize, Clone)]
pub struct Progress {
    done: usize,
    total: usize,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct BatchResult {
    generated: usize,
    cancelled: bool,
    /// Filas (desde 0) en las que algún campo tuvo que usar Helvetica.
    font_replaced_rows: Vec<usize>,
    /// Archivos escritos.
    files: Vec<String>,
}

// ---------- Lectura de tablas ----------

/// Lee un CSV o una planilla de Excel/LibreOffice. La primera fila son los encabezados.
pub fn read_table(path: &Path) -> Result<Table, String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_lowercase();
    let mut rows = match ext.as_str() {
        "xlsx" | "xlsm" | "xlsb" | "xls" | "ods" => read_spreadsheet(path)?,
        _ => read_csv(&std::fs::read(path).map_err(err)?)?,
    };
    rows.retain(|r| r.iter().any(|c| !c.trim().is_empty()));
    if rows.is_empty() {
        return Err("el archivo no tiene datos".into());
    }

    let headers: Vec<String> = rows
        .remove(0)
        .into_iter()
        .enumerate()
        .map(|(i, h)| match h.trim() {
            "" => format!("Columna {}", i + 1),
            h => h.to_string(),
        })
        .collect();
    for row in &mut rows {
        row.resize(headers.len(), String::new());
    }
    Ok(Table { headers, rows })
}

/// Texto de un archivo respetando su BOM; sin BOM, UTF-8 si es válido y si no Windows-1252,
/// que es como Excel guarda los CSV en español.
fn decode(bytes: &[u8]) -> String {
    let encoding = match encoding_rs::Encoding::for_bom(bytes) {
        Some((encoding, _)) => encoding,
        None if std::str::from_utf8(bytes).is_ok() => encoding_rs::UTF_8,
        None => encoding_rs::WINDOWS_1252,
    };
    encoding.decode(bytes).0.into_owned()
}

/// Separador más frecuente en la primera línea (fuera de comillas): coma, punto y coma o
/// tabulación. Excel en español usa punto y coma.
fn sniff_delimiter(text: &str) -> u8 {
    let mut counts = [(b',', 0), (b';', 0), (b'\t', 0)];
    let mut in_quotes = false;
    for c in text.lines().next().unwrap_or_default().bytes() {
        if c == b'"' {
            in_quotes = !in_quotes;
        } else if !in_quotes {
            for (delimiter, count) in &mut counts {
                if c == *delimiter {
                    *count += 1;
                }
            }
        }
    }
    counts.iter().max_by_key(|(_, n)| *n).filter(|(_, n)| *n > 0).map_or(b',', |(d, _)| *d)
}

fn read_csv(bytes: &[u8]) -> Result<Vec<Vec<String>>, String> {
    let text = decode(bytes);
    csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .delimiter(sniff_delimiter(&text))
        .from_reader(text.as_bytes())
        .records()
        .map(|r| r.map(|record| record.iter().map(String::from).collect()).map_err(err))
        .collect()
}

fn read_spreadsheet(path: &Path) -> Result<Vec<Vec<String>>, String> {
    use calamine::{open_workbook_auto, Reader};

    let mut workbook = open_workbook_auto(path).map_err(err)?;
    let range = workbook
        .worksheet_range_at(0)
        .ok_or("el archivo no tiene hojas")?
        .map_err(err)?;
    Ok(range
        .rows()
        .map(|row| row.iter().map(cell_text).collect())
        .collect())
}

/// Texto de una celda como lo vería una persona: sin ".0" en enteros y fechas dd/mm/aaaa.
fn cell_text(cell: &calamine::Data) -> String {
    use calamine::Data;

    match cell {
        Data::Empty => String::new(),
        Data::Float(f) if f.fract() == 0.0 && f.abs() < 1e15 => format!("{}", *f as i64),
        Data::DateTime(date) => match date.as_datetime() {
            Some(dt) if dt.format("%H:%M").to_string() == "00:00" => {
                dt.format("%d/%m/%Y").to_string()
            }
            Some(dt) => dt.format("%d/%m/%Y %H:%M").to_string(),
            None => cell.to_string(),
        },
        other => other.to_string(),
    }
}

// ---------- Generación ----------

/// Copia de la plantilla con los valores de una fila. Devuelve también si algún campo
/// tuvo que usar Helvetica.
fn fill(
    template: &[u8],
    fields: &[Field],
    row: &[String],
    known: &mut HashMap<String, HashSet<char>>,
) -> Result<(PdfDocument<'static>, bool), String> {
    let mut doc = load(template.to_vec(), None)?;
    let mut font_replaced = false;

    let mut pages: Vec<PdfPageIndex> = fields.iter().map(|f| f.page).collect();
    pages.sort_unstable();
    pages.dedup();
    for index in pages {
        let mut page = doc.pages().get(index).map_err(err)?;
        let on_page: Vec<&Field> = fields.iter().filter(|f| f.page == index).collect();
        // Primero se resuelven los objetos de todos los campos de la página: reemplazar
        // uno quita objetos y correría los índices de los demás.
        let resolved = on_page
            .iter()
            .map(|f| objects_at(&mut page, &f.objects))
            .collect::<Result<Vec<_>, _>>()?;
        for (field, objects) in on_page.into_iter().zip(resolved) {
            let value = row.get(field.column).map_or("", |v| v.trim());
            font_replaced |= replace_run(&mut doc, &mut page, objects, value, field.align, known)?;
        }
    }
    Ok((doc, font_replaced))
}

/// Nombre de archivo seguro y que no pisa otro archivo de la carpeta ni de este lote.
fn unique_file_name(name: &str, row: usize, dir: &Path, used: &mut HashSet<String>) -> String {
    let clean: String = name
        .chars()
        .map(|c| if "/\\:*?\"<>|".contains(c) || c.is_control() { '-' } else { c })
        .collect();
    let clean = clean.trim();
    let stem = match clean.len().checked_sub(4) {
        Some(i) if clean.is_char_boundary(i) && clean[i..].eq_ignore_ascii_case(".pdf") => {
            clean[..i].trim()
        }
        _ => clean,
    };
    let stem = if stem.is_empty() {
        format!("fila-{}", row + 1)
    } else {
        stem.to_string()
    };

    let mut candidate = format!("{stem}.pdf");
    let mut n = 2;
    while dir.join(&candidate).exists() || !used.insert(candidate.to_lowercase()) {
        candidate = format!("{stem} ({n}).pdf");
        n += 1;
    }
    candidate
}

fn generate(
    template: &[u8],
    fields: &[Field],
    rows: &[Vec<String>],
    output: Output,
    mut on_progress: impl FnMut(usize, usize),
) -> Result<BatchResult, String> {
    if fields.is_empty() {
        return Err("no hay campos marcados".into());
    }
    CANCEL.store(false, Ordering::SeqCst);
    let total = rows.len();
    let mut known = HashMap::new();
    let mut result = BatchResult {
        generated: 0,
        cancelled: false,
        font_replaced_rows: Vec::new(),
        files: Vec::new(),
    };

    match output {
        Output::Files { dir, names } => {
            let dir = PathBuf::from(dir);
            let mut used = HashSet::new();
            for (i, row) in rows.iter().enumerate() {
                if CANCEL.load(Ordering::SeqCst) {
                    result.cancelled = true;
                    break;
                }
                let (doc, replaced) = fill(template, fields, row, &mut known)?;
                let name = names.get(i).map_or("", String::as_str);
                let path = dir.join(unique_file_name(name, i, &dir, &mut used));
                doc.save_to_file(&path).map_err(err)?;
                if replaced {
                    result.font_replaced_rows.push(i);
                }
                result.files.push(path.to_string_lossy().into_owned());
                result.generated += 1;
                on_progress(i + 1, total);
            }
        }
        Output::Single { path } => {
            let mut combined = pdfium()?.create_new_pdf().map_err(err)?;
            for (i, row) in rows.iter().enumerate() {
                if CANCEL.load(Ordering::SeqCst) {
                    // Un archivo a medias confundiría más que ayudar: no se guarda.
                    result.cancelled = true;
                    return Ok(result);
                }
                let (doc, replaced) = fill(template, fields, row, &mut known)?;
                combined.pages_mut().append(&doc).map_err(err)?;
                if replaced {
                    result.font_replaced_rows.push(i);
                }
                result.generated += 1;
                on_progress(i + 1, total);
            }
            combined.save_to_file(&path).map_err(err)?;
            result.files.push(path);
        }
    }
    Ok(result)
}

fn template_bytes(state: &AppState) -> Result<Vec<u8>, String> {
    state.with_doc(|open| open.doc.save_to_bytes().map_err(err))
}

// ---------- Comandos de Tauri ----------

#[tauri::command]
pub async fn read_table_file(path: String) -> Result<Table, String> {
    read_table(Path::new(&path))
}

/// Renderiza una página de la plantilla con los valores de una fila, sin modificar el documento.
#[tauri::command]
pub async fn batch_preview(
    fields: Vec<Field>,
    row: Vec<String>,
    page: PdfPageIndex,
    width: u32,
    state: State<'_, AppState>,
) -> Result<Response, String> {
    let template = template_bytes(&state)?;
    let (doc, _) = fill(&template, &fields, &row, &mut HashMap::new())?;
    let image = render(&doc.pages().get(page).map_err(err)?, width)?;
    Ok(Response::new(encode_png(&image)?))
}

#[tauri::command]
pub async fn batch_generate(
    fields: Vec<Field>,
    rows: Vec<Vec<String>>,
    output: Output,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<BatchResult, String> {
    let template = template_bytes(&state)?;
    tauri::async_runtime::spawn_blocking(move || {
        generate(&template, &fields, &rows, output, |done, total| {
            let _ = app.emit("batch-progress", Progress { done, total });
        })
    })
    .await
    .map_err(err)?
}

#[tauri::command]
pub fn batch_cancel() {
    CANCEL.store(true, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pdf-studio-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn csv_from_spanish_excel() {
        // Excel en español: punto y coma y Windows-1252 ("Pérez" con é = 0xE9).
        let bytes = b"Nombre;Curso\r\nAna P\xe9rez;\"Nutrici\xf3n; nivel 1\"\r\n;\r\nLuis;Qu\xedmica\r\n";
        let path = tmp_dir("csv").join("datos.csv");
        std::fs::write(&path, bytes).unwrap();
        let table = read_table(&path).unwrap();
        assert_eq!(table.headers, ["Nombre", "Curso"]);
        assert_eq!(
            table.rows,
            [["Ana Pérez", "Nutrición; nivel 1"], ["Luis", "Química"]]
        );
    }

    #[test]
    fn csv_with_utf8_bom_and_commas() {
        let bytes = "\u{feff}Nombre,Fecha\nJosé Ñúñez,26/08/2026\n".as_bytes();
        let path = tmp_dir("bom").join("datos.csv");
        std::fs::write(&path, bytes).unwrap();
        let table = read_table(&path).unwrap();
        assert_eq!(table.headers, ["Nombre", "Fecha"]);
        assert_eq!(table.rows, [["José Ñúñez", "26/08/2026"]]);
    }

    #[test]
    fn excel_numbers_and_dates() {
        let path = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/test-data/datos.xlsx"));
        let table = read_table(path).unwrap();
        assert_eq!(table.headers, ["Nombre", "Horas", "Fecha"]);
        assert_eq!(
            table.rows,
            [
                ["José Ñúñez", "2", "26/08/2026"],
                ["Ana", "1.5", "01/09/2026 14:30"]
            ]
        );
    }

    #[test]
    fn file_names_are_safe_and_unique() {
        let dir = tmp_dir("names");
        std::fs::write(dir.join("Ana.pdf"), b"").unwrap();
        let mut used = HashSet::new();
        assert_eq!(unique_file_name("Ana", 0, &dir, &mut used), "Ana (2).pdf");
        assert_eq!(unique_file_name("Ana.PDF", 1, &dir, &mut used), "Ana (3).pdf");
        assert_eq!(unique_file_name("a/b:c", 2, &dir, &mut used), "a-b-c.pdf");
        assert_eq!(unique_file_name("  ", 3, &dir, &mut used), "fila-4.pdf");
    }

    /// Plantilla con un nombre centrado en x = 300 y una fecha a la derecha.
    fn template() -> (Vec<u8>, Vec<Field>) {
        crate::pdf::init_pdfium(None).unwrap();
        let mut doc = pdfium().unwrap().create_new_pdf().unwrap();
        let size = PdfPagePaperSize::new_custom(PdfPoints::new(600.0), PdfPoints::new(400.0));
        let mut page = doc.pages_mut().create_page_at_end(size).unwrap();
        let font = doc.fonts_mut().times_bold();
        for (text, y) in [("Nombre Apellido", 200.0), ("01/01/2026", 100.0)] {
            let mut object = PdfPageTextObject::new(&doc, text, font, PdfPoints::new(24.0)).unwrap();
            let width = object.bounds().unwrap().to_rect().width().value;
            object
                .translate(PdfPoints::new(300.0 - width / 2.0), PdfPoints::new(y))
                .unwrap();
            page.objects_mut().add_text_object(object).unwrap();
        }
        let fields = vec![
            Field { page: 0, objects: vec![0], column: 0, align: Align::Center },
            Field { page: 0, objects: vec![1], column: 1, align: Align::Right },
        ];
        (doc.save_to_bytes().unwrap(), fields)
    }

    fn texts(doc: &PdfDocument) -> Vec<(String, f32, f32)> {
        let page = doc.pages().get(0).unwrap();
        let objects = page.objects();
        (0..objects.len())
            .filter_map(|i| {
                let o = objects.get(i).ok()?;
                let r = o.bounds().ok()?.to_rect();
                Some((o.as_text_object()?.text(), r.left().value, r.right().value))
            })
            .collect()
    }

    #[test]
    fn fill_keeps_alignment() {
        let (template, fields) = template();
        let original = texts(&load(template.clone(), None).unwrap());
        let row = vec!["Ana".to_string(), "26/08/2026".to_string()];
        let (doc, replaced) = fill(&template, &fields, &row, &mut HashMap::new()).unwrap();
        assert!(!replaced);
        let filled = texts(&doc);
        let find = |t: &str| filled.iter().find(|(text, ..)| text == t).cloned().unwrap();

        // Centrado: el centro no se mueve.
        let (_, l, r) = find("Ana");
        assert!(((l + r) / 2.0 - 300.0).abs() < 0.5, "centro {}", (l + r) / 2.0);
        // A la derecha: el borde derecho no se mueve.
        let (_, _, r) = find("26/08/2026");
        assert!((r - original[1].2).abs() < 0.5, "derecha {r} vs {}", original[1].2);
    }

    #[test]
    fn generate_files_and_single_pdf() {
        let (template, fields) = template();
        let rows: Vec<Vec<String>> = [["Ana", "1"], ["Ana", "2"], ["Luis", "3"]]
            .iter()
            .map(|r| r.iter().map(|s| s.to_string()).collect())
            .collect();

        let dir = tmp_dir("generate");
        let names = rows.iter().map(|r| r[0].clone()).collect();
        let mut progress = Vec::new();
        let output = Output::Files { dir: dir.to_string_lossy().into(), names };
        let result = generate(&template, &fields, &rows, output, |d, t| progress.push((d, t))).unwrap();
        assert_eq!(result.generated, 3);
        assert_eq!(progress.last(), Some(&(3, 3)));
        let mut files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        files.sort();
        assert_eq!(files, ["Ana (2).pdf", "Ana.pdf", "Luis.pdf"]);
        let luis = pdfium().unwrap().load_pdf_from_file(&dir.join("Luis.pdf"), None).unwrap();
        assert!(texts(&luis).iter().any(|(t, ..)| t == "Luis"));

        let single = dir.join("todas.pdf");
        let output = Output::Single { path: single.to_string_lossy().into() };
        generate(&template, &fields, &rows, output, |_, _| {}).unwrap();
        let combined = pdfium().unwrap().load_pdf_from_file(&single, None).unwrap();
        assert_eq!(combined.pages().len(), 3);
    }

    #[test]
    fn missing_letters_are_reported_per_row() {
        crate::pdf::init_pdfium(None).unwrap();
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/test-data/subset-font.pdf");
        let template = std::fs::read(path).unwrap();
        let doc = load(template.clone(), None).unwrap();
        let page = doc.pages().get(0).unwrap();
        let objects = page.objects();
        let first_text = (0..objects.len())
            .find(|&i| objects.get(i).unwrap().as_text_object().is_some())
            .unwrap();
        let fields = vec![Field { page: 0, objects: vec![first_text], column: 0, align: Align::Left }];
        let rows = vec![vec!["Hola".to_string()], vec!["Ñandú".to_string()]];
        let dir = tmp_dir("missing");
        let output = Output::Files { dir: dir.to_string_lossy().into(), names: vec![] };
        let result = generate(&template, &fields, &rows, output, |_, _| {}).unwrap();
        assert_eq!(result.font_replaced_rows, [1]);
    }
}
