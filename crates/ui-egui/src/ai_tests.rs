//! Edit › AI and Preferences › AI (desktop, `crate::ai`): where AI is on, the menu offers it and
//! the dialogs open; nothing is sent until Generate; what comes back goes in through the app's own
//! commands, one undo step, and the person's clipboard stays as it was. No request leaves this
//! computer: the end-to-end test talks to a local stand-in for the model.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::json;

use super::*;
use crate::{PhotocraftApp, Services};

fn plain_app() -> PhotocraftApp {
    PhotocraftApp::new(photocraft_engine::Session::new(), Services::default())
}

/// AI on, with a key of the person's own (never used: nothing is sent unless a test serves it),
/// and no settings file or publik credential of this computer's.
fn ai_on(app: &mut PhotocraftApp) {
    let mut ai = Ai::new(AppInfo { slug: "photocraft-tests", name: "PhotoCraft", version: "0.0.0", app_token: "", dialects: &["images"], config_dir: None });
    ai.skip_first_run();
    ai.state.settings.provider = "own".into();
    ai.state.settings.own_key = "sk-test".into();
    ai.state.settings.setup_done = true;
    app.ai = Some(ai);
}

/// A 64 × 48 white document with AI on.
fn doc_app() -> PhotocraftApp {
    let mut app = plain_app();
    ai_on(&mut app);
    app.session.execute("file.new", json!({"width": 64, "height": 48})).unwrap();
    app.sync_views();
    app
}

fn select(app: &mut PhotocraftApp, x: i32, y: i32, w: u32, h: u32) {
    app.run("select.rect", json!({"x": x, "y": y, "width": w, "height": h})).unwrap();
}

fn doc(app: &PhotocraftApp) -> &photocraft_doc::Document {
    &app.session.active().unwrap().doc
}

fn pixel(app: &PhotocraftApp, x: i32, y: i32) -> [u8; 4] {
    let px = photocraft_compose::render(doc(app), Rect::new(x, y, x + 1, y + 1)).to_rgba8().pixels;
    [px[0], px[1], px[2], px[3]]
}

/// A context with PhotoCraft's fonts and theme (named fonts apply from the second frame).
fn ui_ctx() -> egui::Context {
    let ctx = egui::Context::default();
    PhotocraftApp::setup_context(&ctx, crate::theme::ThemeKind::ALL[0]);
    ctx.run_ui(egui::RawInput::default(), |_| {}).textures_delta.clear();
    ctx
}

/// Draw the dialogs for a few frames.
fn draw_dialogs(app: &mut PhotocraftApp, ctx: &egui::Context) {
    for _ in 0..3 {
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| crate::dialogs::show(app, ui.ctx()));
        out.textures_delta.clear();
    }
}

fn solid(w: u32, h: u32, c: [u8; 4]) -> Vec<u8> {
    c.iter().copied().cycle().take(w as usize * h as usize * 4).collect()
}

#[test]
fn the_menu_offers_ai_only_where_it_is_on() {
    let mut off = plain_app();
    off.session.execute("file.new", json!({"width": 8, "height": 8})).unwrap();
    assert!(!crate::menus::menu_items(&off).iter().any(|i| i.id.starts_with("ai.")), "no Edit › AI without AI");
    assert!(menu(&mut off, GENERATIVE_FILL).unwrap().is_err());
    assert!(off.ui.dialogs.is_empty());
    assert!(menu(&mut off, "edit.fill").is_none(), "other ids are not ours");

    let mut on = plain_app();
    ai_on(&mut on);
    let items = crate::menus::menu_items(&on);
    let at = items.iter().position(|i| i.id == "edit.contentAwareFill").unwrap();
    let ours: Vec<(&str, &str, bool)> = items[at + 1..at + 5].iter().map(|i| (i.id.as_str(), i.label.as_str(), i.enabled)).collect();
    assert_eq!(
        ours,
        [(GENERATIVE_FILL, "Generative Fill…", false), (GENERATE_IMAGE, "Generate Image…", true), ("---", "---", false), (SETTINGS, "AI Settings…", true)],
        "Edit › AI after Content-Aware Fill; Generative Fill needs a document"
    );
    assert!(items[at + 1..at + 5].iter().all(|i| i.path == ["Edit", "AI"]));
    assert_eq!(menu(&mut on, GENERATIVE_FILL).unwrap(), Err("Open a document first".to_string()));
    on.session.execute("file.new", json!({"width": 8, "height": 8})).unwrap();
    assert!(crate::menus::is_enabled(&on, GENERATIVE_FILL));
}

#[test]
fn the_dialogs_open_and_send_nothing_until_generate() {
    let mut app = doc_app();
    let ctx = ui_ctx();
    let r = crate::menus::invoke(&mut app, &ctx, GENERATIVE_FILL, json!({})).unwrap();
    let id = r["dialog"].as_u64().unwrap();
    let d = app.ui.dialogs.iter().find(|d| d.id == id).unwrap();
    assert_eq!(d.fields.get("__ai"), Some(&json!("fill")));
    assert_eq!(d.fields.get("__label"), Some(&json!("Generative Fill")));
    // the same dialog again, not a second one
    assert_eq!(crate::menus::invoke(&mut app, &ctx, GENERATIVE_FILL, json!({})).unwrap()["dialog"], json!(id));
    let image = crate::menus::invoke(&mut app, &ctx, GENERATE_IMAGE, json!({})).unwrap()["dialog"].as_u64().unwrap();
    assert_eq!(app.ui.dialogs.iter().find(|d| d.id == image).unwrap().fields.get("shape"), Some(&json!("square")));
    // drawing them sends nothing; confirming one only closes it
    draw_dialogs(&mut app, &ctx);
    assert!(!app.ai.as_ref().unwrap().generating());
    crate::dialogs::confirm(&mut app, image).unwrap();
    assert!(!app.ui.dialogs.iter().any(|d| d.id == image));
    assert!(!app.ai.as_ref().unwrap().generating());
}

#[test]
fn ai_settings_open_preferences_on_the_ai_section() {
    let mut app = plain_app();
    ai_on(&mut app);
    let id = menu(&mut app, SETTINGS).unwrap().unwrap()["dialog"].as_u64().unwrap();
    let d = app.ui.dialogs.iter().find(|d| d.id == id).unwrap();
    assert!(crate::prefs_ui::is_preferences(&d.fields));
    assert_eq!(d.fields.get("section"), Some(&json!("ai")));
    let ctx = ui_ctx();
    draw_dialogs(&mut app, &ctx);
    assert_eq!(app.ui.dialogs.iter().find(|d| d.id == id).unwrap().fields.get("section"), Some(&json!("ai")), "the AI section stays chosen");
}

#[test]
fn the_setup_dialog_opens_once_on_a_quiet_first_launch() {
    let mut app = plain_app();
    let ai = Ai::new(AppInfo { slug: "photocraft-tests", name: "PhotoCraft", version: "0.0.0", app_token: "", dialects: &["images"], config_dir: None });
    if !ai.state.wants_first_run() {
        // (PUBLIK_API_KEY is set where these tests run: AI is set up already)
        return;
    }
    app.ai = Some(ai);
    let ctx = egui::Context::default();
    // not while another dialog shows
    let other = app.ui.open_dialog(crate::state::DialogKind::About, Default::default());
    for _ in 0..6 {
        frame(&mut app, &ctx);
    }
    assert_eq!(app.ui.dialogs.len(), 1);
    app.ui.close_dialog(other);
    frame(&mut app, &ctx);
    let setup: Vec<_> = app.ui.dialogs.iter().filter(|d| d.fields.get("__ai") == Some(&json!("setup"))).collect();
    assert_eq!(setup.len(), 1, "the setup flow once the window is free");
    let id = setup[0].id;
    app.ui.close_dialog(id);
    for _ in 0..6 {
        frame(&mut app, &ctx);
    }
    assert!(app.ui.dialogs.is_empty(), "once per launch");

    // an agent driving the app sees no setup dialog
    let mut driven = plain_app();
    let mut ai = Ai::new(AppInfo { slug: "photocraft-tests", name: "PhotoCraft", version: "0.0.0", app_token: "", dialects: &["images"], config_dir: None });
    ai.skip_first_run();
    driven.ai = Some(ai);
    for _ in 0..6 {
        frame(&mut driven, &ctx);
    }
    assert!(driven.ui.dialogs.is_empty());
}

#[test]
fn the_fill_area_is_the_selection_with_context_in_a_request_shape() {
    let canvas = Rect::new(0, 0, 1000, 1000);
    // a 100 × 50 selection: 32 px of margin, then grown to 3:2 around its middle
    let (r, size) = fill_region(Rect::new(100, 100, 200, 150), canvas).unwrap();
    assert_eq!(size, (1536, 1024, "1536x1024"));
    assert_eq!(r, Rect::new(65, 68, 236, 182));
    // the whole canvas of a 3:2 document is sent as it is
    let canvas = Rect::new(0, 0, 300, 200);
    assert_eq!(fill_region(canvas, canvas), Some((canvas, (1536, 1024, "1536x1024"))));
    // at an edge, it grows inwards
    let canvas = Rect::new(0, 0, 400, 400);
    let (r, size) = fill_region(Rect::new(0, 0, 40, 200), canvas).unwrap();
    assert_eq!(size, (1024, 1536, "1024x1536"));
    assert_eq!((r.x0, r.y0, r.y1), (0, 0, 250));
    assert!(canvas.contains_rect(&r));
    // a canvas too narrow for the shape keeps its width
    let canvas = Rect::new(0, 0, 100, 1000);
    let (r, _) = fill_region(canvas, canvas).unwrap();
    assert_eq!(r, canvas);
    // nothing selected on the canvas
    assert_eq!(fill_region(Rect::new(2000, 2000, 2100, 2100), canvas), None);
    assert_eq!(fill_region(Rect::EMPTY, canvas), None);
}

#[test]
fn the_mask_is_transparent_where_the_selection_is() {
    assert_eq!(mask_rgba8(&[1.0, 0.0, 0.5, 7.0, -1.0]), [0, 0, 0, 0, 0, 0, 0, 255, 0, 0, 0, 128, 0, 0, 0, 0, 0, 0, 0, 255]);
}

#[test]
fn pixels_scale_without_colour_bleeding_from_transparent_ones() {
    // the same size: unchanged
    let src: Vec<u8> = (0..4 * 3 * 4).map(|i| i as u8).collect();
    assert_eq!(resample_rgba8(&src, 4, 3, 4, 3).unwrap(), src);
    // a flat colour stays flat, up and down
    let flat = solid(5, 7, [10, 200, 30, 255]);
    for (w, h) in [(1, 1), (2, 3), (11, 13), (40, 9)] {
        let out = resample_rgba8(&flat, 5, 7, w, h).unwrap();
        assert_eq!(out.len(), (w * h * 4) as usize);
        assert!(out.chunks_exact(4).all(|p| p == [10, 200, 30, 255]), "{w}×{h}");
    }
    // opaque red beside transparent black: red at half cover, not dark red
    let out = resample_rgba8(&[255, 0, 0, 255, 0, 0, 0, 0], 2, 1, 1, 1).unwrap();
    assert_eq!(out, [255, 0, 0, 128]);
    // sizes that do not match the pixels, or are zero, are refused
    assert_eq!(resample_rgba8(&flat, 5, 6, 2, 2), None);
    assert_eq!(resample_rgba8(&flat, 5, 7, 0, 2), None);
    assert_eq!(resample_rgba8(&[], 0, 0, 2, 2), None);
}

#[test]
fn layer_names_come_from_the_prompt() {
    assert_eq!(layer_name("  a red\n bench / under: a tree.  ", "Generative Fill"), "a red bench under a tree");
    assert_eq!(layer_name(" \n ", "Generated Image"), "Generated Image");
    assert_eq!(layer_name(&"x".repeat(200), "F").chars().count(), MAX_NAME_CHARS);
}

/// A fill for the active document's selection, of one colour.
fn fill_out(app: &PhotocraftApp, colour: [u8; 4]) -> FillOut {
    let st = app.session.active().unwrap();
    let selection = selection_bounds(&st.doc).unwrap();
    let (region, _) = fill_region(selection, st.doc.bounds()).unwrap();
    let target = FillTarget { doc: st.doc.id, revision: content_revision(st), region, selection, name: "a green bench".into() };
    FillOut { rgba: solid(region.width(), region.height(), colour), target }
}

#[test]
fn a_fill_goes_in_as_one_paste_into_step_and_keeps_the_clipboard() {
    let mut app = doc_app();
    // something the person copied
    select(&mut app, 0, 0, 4, 4);
    app.run("edit.copy", json!({})).unwrap();
    let copied = app.session.clipboard.as_ref().unwrap().bounds;
    select(&mut app, 20, 10, 16, 12);
    let layers = doc(&app).layers.len();
    let steps = app.session.active().unwrap().history.past_len();
    let out = fill_out(&app, [0, 200, 0, 255]);
    // it came back to an unchanged document: it goes in at once
    arrived_fill(&mut app, out);
    let ai = app.ai.as_ref().unwrap();
    assert!(ai.fill.waiting.is_none());
    assert!(ai.fill.note.as_deref().unwrap().starts_with("The fill is a new layer"));
    let d = doc(&app);
    assert_eq!(d.layers.len(), layers + 1);
    let new = d.layers.last().unwrap();
    assert_eq!(new.name, "a green bench");
    assert!(new.mask.is_some(), "masked to the selection");
    assert_eq!(pixel(&app, 25, 15), [0, 200, 0, 255], "inside the selection");
    assert_eq!(pixel(&app, 5, 40), [255, 255, 255, 255], "outside it, the image as it was");
    assert_eq!(app.session.active().unwrap().history.past_len(), steps + 1, "one step");
    assert_eq!(app.session.clipboard.as_ref().unwrap().bounds, copied, "the clipboard is the person's again");
    app.run("edit.undo", json!({})).unwrap();
    assert_eq!(doc(&app).layers.len(), layers);
    assert_eq!(pixel(&app, 25, 15), [255, 255, 255, 255]);
}

#[test]
fn a_fill_for_a_changed_document_waits_for_place_image() {
    let mut app = doc_app();
    select(&mut app, 20, 10, 16, 12);
    let out = fill_out(&app, [0, 0, 255, 255]);
    // the person moved on: another selection
    select(&mut app, 2, 30, 10, 10);
    let layers = doc(&app).layers.len();
    arrived_fill(&mut app, out);
    assert_eq!(doc(&app).layers.len(), layers, "not placed behind the person's back");
    let ai = app.ai.as_ref().unwrap();
    assert!(ai.fill.waiting.is_some(), "kept, not lost");
    assert!(ai.fill.note.as_deref().unwrap().contains("Place image"));
    assert_eq!(app.ui.notices.len(), 1, "the dialog is closed: a notice says where it waits");
    // Place image: into the selection there now
    place_waiting_fill(&mut app);
    assert!(app.ai.as_ref().unwrap().fill.waiting.is_none());
    assert_eq!(doc(&app).layers.len(), layers + 1);
    assert_eq!(pixel(&app, 6, 34), [0, 0, 255, 255]);
    assert_eq!(pixel(&app, 25, 15), [255, 255, 255, 255]);
}

#[test]
fn place_image_needs_a_selection_for_a_fill() {
    let mut app = doc_app();
    select(&mut app, 20, 10, 16, 12);
    let out = fill_out(&app, [0, 0, 255, 255]);
    app.run("select.deselect", json!({})).unwrap();
    app.ai.as_mut().unwrap().fill.waiting = Some(out);
    place_waiting_fill(&mut app);
    let ai = app.ai.as_ref().unwrap();
    assert!(ai.fill.waiting.is_some());
    assert_eq!(ai.fill.error.as_deref(), Some(NO_SELECTION));
}

fn image_out(w: u32, h: u32) -> ImageOut {
    let rgba = solid(w, h, [200, 20, 20, 255]);
    ImageOut { png: encode_png(w, h, rgba.clone()).unwrap(), width: w, height: h, rgba, name: "a red square".into() }
}

#[test]
fn an_image_becomes_a_layer_or_a_new_document() {
    let mut app = doc_app();
    let layers = doc(&app).layers.len();
    let steps = app.session.active().unwrap().history.past_len();
    let id = doc(&app).id;
    arrived_image(&mut app, image_out(16, 8), Some(id));
    assert_eq!(doc(&app).layers.len(), layers + 1);
    assert_eq!(doc(&app).layers.last().unwrap().name, "a red square");
    assert_eq!(pixel(&app, 32, 24), [200, 20, 20, 255], "in the middle of the canvas");
    assert_eq!(pixel(&app, 2, 2), [255, 255, 255, 255]);
    assert_eq!(app.session.active().unwrap().history.past_len(), steps + 1, "one step");
    assert!(app.ai.as_ref().unwrap().image.waiting.is_none());
    app.run("edit.undo", json!({})).unwrap();
    assert_eq!(doc(&app).layers.len(), layers);

    // no document: a new one the image's size, which asks before it closes
    let mut empty = plain_app();
    ai_on(&mut empty);
    arrived_image(&mut empty, image_out(12, 9), None);
    assert_eq!(empty.session.documents().len(), 1);
    let st = empty.session.active().unwrap();
    assert_eq!((st.doc.size.width, st.doc.size.height), (12, 9));
    assert!(st.is_dirty(), "unsaved");
    assert_eq!(pixel(&empty, 5, 5), [200, 20, 20, 255]);
}

#[test]
fn a_402_shows_once_through_the_result_bar() {
    let mut app = doc_app();
    select(&mut app, 20, 10, 16, 12);
    let target = fill_out(&app, [0; 4]).target;
    let (tx, rx) = mpsc::channel();
    let ai = app.ai.as_mut().unwrap();
    ai.fill.start(FillJob { rx, target });
    assert!(ai.generating());
    let paid = Fail {
        status: 402,
        message: "Not enough publik API balance for this request.".into(),
        link_label: craft_publik::CTA_SIGN_IN.into(),
        link_url: Some("https://publikhq.com/claim/AB12".into()),
        ..Fail::default()
    };
    tx.send(Err(paid)).unwrap();
    let ctx = egui::Context::default();
    frame(&mut app, &ctx);
    let ai = app.ai.as_ref().unwrap();
    assert!(!ai.generating());
    assert_eq!(ai.state.notice.as_ref().map(|n| n.link_url.clone()), Some(Some("https://publikhq.com/claim/AB12".into())));
    assert_eq!(ai.fill.error, None, "the message is the result bar's, not a second copy");

    // any other failure is the dialog's own line
    let (tx, rx) = mpsc::channel();
    let target = fill_out(&app, [0; 4]).target;
    let ai = app.ai.as_mut().unwrap();
    ai.state.dismiss_notice();
    ai.fill.start(FillJob { rx, target });
    tx.send(Err(local("The service is down."))).unwrap();
    frame(&mut app, &ctx);
    assert_eq!(app.ai.as_ref().unwrap().fill.error.as_deref(), Some("The service is down."));
}

// ---- end to end, against a local stand-in for the model -----------------------------------------

/// A local stand-in for an OpenAI-compatible endpoint: it answers one request with `body` and
/// hands back what the request carried.
fn fake_model(body: String) -> (String, mpsc::Receiver<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else { return };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
        let request = read_request(&mut stream);
        let reply = format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
        let _ = stream.write_all(reply.as_bytes());
        let _ = tx.send(request);
    });
    (url, rx)
}

/// The request on `stream`, head and body (by its Content-Length; a 100-continue is answered).
fn read_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 65536];
    let mut continued = false;
    loop {
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..end]).to_lowercase();
            if !continued && head.contains("expect: 100-continue") {
                let _ = stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
                continued = true;
            }
            let len = head.lines().find_map(|l| l.strip_prefix("content-length:")).and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(0);
            if buf.len() >= end + 4 + len {
                return buf;
            }
        }
        let n = stream.read(&mut chunk).unwrap_or(0);
        if n == 0 {
            return buf;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

#[test]
fn a_fill_request_carries_the_image_and_the_mask_and_its_answer_fits_the_selection() {
    // the stand-in paints the whole request blue
    let blue = encode_png(1536, 1024, solid(1536, 1024, [0, 0, 255, 255])).unwrap();
    let (url, request) = fake_model(json!({"data": [{"b64_json": craft_publik::client::base64(&blue)}]}).to_string());
    let provider = Provider::Own { service: craft_publik::own_key::Service::OpenAi, base_url: url, api_key: "sk-test".into(), model: "test".into() };
    let mut app = doc_app();
    select(&mut app, 20, 10, 16, 12);
    let st = app.session.active().unwrap();
    let selection = selection_bounds(&st.doc).unwrap();
    let (region, size) = fill_region(selection, st.doc.bounds()).unwrap();
    let (rgba, _reply) = fill_work(&provider, &st.doc, region, size, "a blue pond").unwrap();
    assert_eq!(rgba.len(), (region.width() * region.height() * 4) as usize, "the answer fits the area sent");
    assert!(rgba.chunks_exact(4).all(|p| p == [0, 0, 255, 255]));

    let sent = request.recv_timeout(Duration::from_secs(60)).unwrap();
    let text = String::from_utf8_lossy(&sent);
    assert!(text.starts_with("POST /v1/images/edits"), "{}", &text[..text.len().min(200)]);
    for part in ["name=\"image\"", "name=\"mask\"", "name=\"prompt\"", "a blue pond", "1536x1024"] {
        assert!(text.contains(part), "the request carries {part}");
    }
    // two PNGs of the request size: the visible image (white, opaque) and the mask
    let pngs: Vec<(u32, u32, Vec<u8>)> = sent
        .windows(8)
        .enumerate()
        .filter(|(_, w)| *w == [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'])
        .map(|(i, _)| decode_rgba8(&sent[i..]).unwrap())
        .collect();
    assert_eq!(pngs.len(), 2);
    let (iw, ih, image) = &pngs[0];
    assert_eq!((*iw, *ih), (1536, 1024));
    assert!(image.chunks_exact(4).all(|p| p == [255, 255, 255, 255]));
    let (mw, mh, mask) = &pngs[1];
    assert_eq!((*mw, *mh), (1536, 1024));
    // transparent in the selection, opaque around it (the margin)
    let at = |x: f64, y: f64| {
        let mx = ((x - f64::from(region.x0)) / f64::from(region.width()) * f64::from(*mw)) as u32;
        let my = ((y - f64::from(region.y0)) / f64::from(region.height()) * f64::from(*mh)) as u32;
        mask[((my * mw + mx) * 4 + 3) as usize]
    };
    assert_eq!(at(28.0, 16.0), 0, "the selection's middle");
    assert_eq!(at(2.0, 2.0), 255);
    assert_eq!(at(60.0, 44.0), 255);
}

/// The AI dialogs and Preferences › AI as PNGs in `CRAFT_UI_VISUAL_FIXTURES` (skipped without it),
/// to look at.
#[test]
fn capture_ai_visual_fixtures() {
    let Some(directory) = std::env::var_os("CRAFT_UI_VISUAL_FIXTURES").map(std::path::PathBuf::from) else { return };
    let _gpu = photocraft_testkit::gpu_lock();
    std::fs::create_dir_all(&directory).unwrap();
    for kind in [crate::theme::ThemeKind::ALL[0], *crate::theme::ThemeKind::ALL.last().unwrap()] {
        for (name, open) in [("fill", GENERATIVE_FILL), ("image", GENERATE_IMAGE), ("prefs", SETTINGS), ("setup", "")] {
            let mut app = doc_app();
            select(&mut app, 20, 10, 16, 12);
            if open.is_empty() {
                open_setup_dialog(&mut app);
            } else {
                menu(&mut app, open).unwrap().unwrap();
            }
            if let Some(d) = app.ui.dialogs.last_mut() {
                d.fields.insert("prompt".into(), json!("A wooden bench under an old oak tree"));
            }
            let mut ready = false;
            let mut harness = egui_kittest::Harness::builder().with_size(egui::vec2(1100.0, 760.0)).with_pixels_per_point(1.0).wgpu().build_ui_state(
                move |ui, app: &mut PhotocraftApp| {
                    if !ready {
                        crate::theme::install_fonts(ui.ctx());
                        crate::theme::apply(ui.ctx(), kind);
                        ready = true;
                        return;
                    }
                    crate::dialogs::show(app, ui.ctx());
                },
                app,
            );
            harness.run_steps(6);
            harness.render().unwrap().save(directory.join(format!("photocraft-ai-{name}-{}.png", kind.id()))).unwrap();
        }
    }
}
