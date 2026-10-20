mod batch;
mod pdf;

use std::sync::Mutex;

use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(pdf::AppState {
            doc: Mutex::new(None),
        })
        .setup(|app| {
            let resource_dir = app.path().resource_dir().ok();
            pdf::init_pdfium(resource_dir)?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            pdf::open_pdf,
            pdf::save_pdf,
            pdf::render_page,
            pdf::undo,
            pdf::redo,
            pdf::rotate_page,
            pdf::delete_page,
            pdf::reorder_pages,
            pdf::insert_pdf,
            pdf::insert_blank_page,
            pdf::add_text,
            pdf::add_highlight,
            pdf::add_rect,
            pdf::add_ink,
            pdf::erase_at,
            pdf::update_text,
            pdf::annotation_at,
            pdf::delete_annotation,
            pdf::text_object_at,
            pdf::move_text_object,
            pdf::edit_text_object,
            batch::read_table_file,
            batch::batch_preview,
            batch::batch_generate,
            batch::batch_cancel,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
