//! Verto: cell fonts served next to the web build.
//!
//! A browser gives the app no system fonts, so on its own a bold or italic cell is drawn in the
//! bundled regular face. A site can put a family's four faces in `fonts/` beside `index.html`;
//! Verto serves Carlito (SIL OFL 1.1, the metrics of Calibri, Excel's default font), from Google
//! Fonts. They are fetched before the app starts and used for cell text; a missing file is logged
//! and its style falls back as before.

use wasm_bindgen::JsCast as _;

/// Where the fonts are, relative to the page.
const DIR: &str = "./fonts";

/// The cell role each served file is used for.
const SERVED: &[(&str, &str)] =
    &[("cell", "Carlito-Regular.ttf"), ("cell-bold", "Carlito-Bold.ttf"), ("cell-italic", "Carlito-Italic.ttf"), ("cell-bold-italic", "Carlito-BoldItalic.ttf")];

/// Fetch and register every served cell face the site has.
pub async fn load_all() {
    for (role, file) in SERVED {
        match bytes(&format!("{DIR}/{file}")).await {
            Ok(b) => {
                log::info!("cell font {file}: {} KB", b.len() / 1024);
                gridcraft_ui_egui::theme::register_served_cell_face(role, b);
            }
            Err(e) => log::info!("cell font {file}: not served ({e})"),
        }
    }
}

/// The body of `url` as bytes, or why not.
async fn bytes(url: &str) -> Result<Vec<u8>, String> {
    let window = web_sys::window().ok_or("no window")?;
    let response = wasm_bindgen_futures::JsFuture::from(window.fetch_with_str(url)).await.map_err(|e| format!("{e:?}"))?;
    let response: web_sys::Response = response.dyn_into().map_err(|_| "not a response".to_string())?;
    if !response.ok() {
        return Err(format!("HTTP {}", response.status()));
    }
    let body = response.array_buffer().map_err(|e| format!("{e:?}"))?;
    let body = wasm_bindgen_futures::JsFuture::from(body).await.map_err(|e| format!("{e:?}"))?;
    Ok(js_sys::Uint8Array::new(&body).to_vec())
}
