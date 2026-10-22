//! AI through publik API or the person's own key, desktop only. The setup flow, the credential
//! file and the HTTP calls live in the shared `craft-publik` crate (one per Craft app). This module
//! is PhotoCraft's thin host:
//!
//! - the setup dialog on the first launch (once, after a few frames, when no other window shows;
//!   "Later" ends it for good),
//! - Edit › Preferences › AI (status, sign-in and key controls, the setup flow in place),
//! - Edit › AI › Generative Fill… and Generate Image…, two dialogs of the app's dialog system.
//!
//! Nothing is sent, and nothing is charged, until the person clicks Generate. Generative Fill sends
//! the visible image around the selection (a PNG, at most [`LONG_EDGE`] pixels on its long side)
//! and the selection as the mask, through `client::image_edit`. The answer goes back in through the
//! app's own Edit › Paste Special › Paste Into: a new layer whose mask is the selection, one undo
//! step. Generate Image sends only the prompt (`client::image_generate`); the image goes in through
//! File › Place Embedded's code (a new layer in the middle of the canvas, one undo step), or File ›
//! New from Clipboard's when no document is open. A paid image that cannot go in at once waits
//! behind Place image.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};

use craft_publik::client::{self, Fail, Reply};
use craft_publik::ui::{Action, AiState};
use craft_publik::{Line, Provider, WHY_IT_COSTS};
use egui::RichText;
use photocraft_doc::{DocId, Document};
use photocraft_geom::Rect;
use serde_json::{Map, Value, json};

pub use craft_publik::AppInfo;

use crate::PhotocraftApp;
use crate::menus::MenuItem;
use crate::state::DialogKind;
use crate::theme::Tokens;
use crate::widgets::{ButtonRole, DialogButton};

/// Edit › AI › Generative Fill…
pub const GENERATIVE_FILL: &str = "ai.generativeFill";
/// Edit › AI › Generate Image…
pub const GENERATE_IMAGE: &str = "ai.generateImage";
/// Edit › AI › AI Settings… (Preferences on its AI section).
pub const SETTINGS: &str = "ai.settings";

/// The submenu the items sit in, right after Edit › Content-Aware Fill… (Photoshop keeps
/// Generative Fill beside it).
const MENU: [&str; 2] = ["Edit", "AI"];
const AFTER: &str = "edit.contentAwareFill";
/// The dialog field that marks an AI dialog: `setup`, `fill` or `image`.
const MARK: &str = "__ai";

/// The long side of the image Generative Fill sends, in pixels (the largest request size).
pub const LONG_EDGE: u32 = 1536;
/// The most pixels Generative Fill reads from the canvas for one request (the selection with its
/// margin). A larger area is refused before anything is read or sent.
pub const MAX_REGION_PIXELS: u64 = 40_000_000;
/// The longest prompt a dialog takes, in characters.
const MAX_PROMPT_CHARS: usize = 4_000;
/// The longest layer name made from a prompt, in characters.
const MAX_NAME_CHARS: usize = 60;
/// Rows of canvas rendered at a time while the fill reads the visible image.
const BAND_ROWS: i32 = 512;

/// The three sizes the image line takes: (width, height, the `size` parameter).
const SIZES: [(u32, u32, &str); 3] = [(1024, 1024, "1024x1024"), (1536, 1024, "1536x1024"), (1024, 1536, "1024x1536")];

const NO_SELECTION: &str = "Make a selection first. Generative Fill changes only the selected area.";
const OFF: &str = "AI is off in this session.";
const BUSY: &str = "Generating… This can take a minute.";

/// AI state of the desktop app (`PhotocraftApp::ai`).
pub struct Ai {
    pub state: AiState,
    frames: u32,
    first_run_checked: bool,
    open_setup: bool,
    /// Preferences › AI shows the setup flow in place of its status.
    inline_setup: bool,
    fill: Task<FillJob, FillOut>,
    image: Task<ImageJob, ImageOut>,
}

/// One feature's request in flight, its result waiting to be placed, and what the dialog says.
struct Task<J, O> {
    job: Option<J>,
    waiting: Option<O>,
    /// The last prompt, so the dialog opens with it again.
    prompt: String,
    note: Option<String>,
    error: Option<String>,
}

impl<J, O> Default for Task<J, O> {
    fn default() -> Self {
        Task { job: None, waiting: None, prompt: String::new(), note: None, error: None }
    }
}

impl<J, O> Task<J, O> {
    fn busy(&self) -> bool {
        self.job.is_some()
    }
    fn start(&mut self, job: J) {
        self.job = Some(job);
        self.note = None;
        self.error = None;
    }
}

/// What a fill was made for. The pixels go back to this place.
#[derive(Clone, Debug, PartialEq, Eq)]
struct FillTarget {
    doc: DocId,
    /// The document's content revision when the request went out.
    revision: u64,
    /// The canvas area sent (the selection with a margin of context).
    region: Rect,
    /// The selection's bounds.
    selection: Rect,
    /// The new layer's name.
    name: String,
}

struct FillJob {
    rx: Receiver<Result<(Vec<u8>, Reply), Fail>>,
    target: FillTarget,
}

/// A finished fill: RGBA8 pixels the size of `target.region`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct FillOut {
    target: FillTarget,
    rgba: Vec<u8>,
}

struct ImageJob {
    rx: Receiver<Result<(ImageOut, Reply), Fail>>,
    /// The document the image goes into (the active one at Generate), if any.
    doc: Option<DocId>,
}

/// A finished image: the PNG the service sent, and its pixels for a new document.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ImageOut {
    png: Vec<u8>,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
    name: String,
}

impl Ai {
    pub fn new(app: AppInfo) -> Self {
        Ai {
            state: AiState::new(app),
            frames: 0,
            first_run_checked: false,
            open_setup: false,
            inline_setup: false,
            fill: Task::default(),
            image: Task::default(),
        }
    }

    /// Do not open the setup dialog on this launch (an agent drives the app over `--control`).
    /// Preferences › AI and the AI dialogs still offer it.
    pub fn skip_first_run(&mut self) {
        self.first_run_checked = true;
    }

    /// A Generative Fill or Generate Image request is on its way.
    pub fn generating(&self) -> bool {
        self.fill.busy() || self.image.busy()
    }
}

/// `PUBLIK_SELF_TEST=1`: provision for real and print one line (publikhq.com/skills/setup.md).
pub fn self_test(app: &AppInfo) -> String {
    craft_publik::self_test(app)
}

fn local(message: impl Into<String>) -> Fail {
    Fail { kind: "local".into(), message: message.into(), ..Fail::default() }
}

// ---- once a frame -------------------------------------------------------------------------------

/// Once a frame: take finished requests (and place what they made), and open the setup dialog on
/// the first launch once nothing else asks for the person's attention.
pub fn frame(app: &mut PhotocraftApp, _ctx: &egui::Context) {
    let quiet = app.ui.dialogs.is_empty() && app.discard.is_none() && app.camera_raw.is_none() && app.file_dialog.is_none();
    let Some(ai) = app.ai.as_mut() else { return };
    ai.state.poll();
    let fill = take_fill(ai);
    let image = take_image(ai);
    ai.frames = ai.frames.saturating_add(1);
    if !ai.first_run_checked && ai.frames > 3 {
        ai.first_run_checked = true;
        ai.open_setup = ai.state.wants_first_run() || ai.state.owes_first_card();
    }
    let open_setup = std::mem::take(&mut ai.open_setup);
    if let Some(out) = fill {
        arrived_fill(app, out);
    }
    if let Some((out, doc)) = image {
        arrived_image(app, out, doc);
    }
    if open_setup {
        if quiet {
            open_setup_dialog(app);
        } else if let Some(ai) = app.ai.as_mut() {
            // ask again once the window is free
            ai.open_setup = true;
        }
    }
}

/// A finished fill, if one came in: its charge noted, its failure shown.
fn take_fill(ai: &mut Ai) -> Option<FillOut> {
    let job = ai.fill.job.as_ref()?;
    let r = match job.rx.try_recv() {
        Ok(r) => r,
        Err(TryRecvError::Empty) => return None,
        Err(TryRecvError::Disconnected) => Err(local("The request stopped unexpectedly. Try again.")),
    };
    let job = ai.fill.job.take()?;
    match r {
        Ok((rgba, reply)) => {
            ai.state.note_reply(&reply);
            Some(FillOut { target: job.target, rgba })
        }
        Err(f) => {
            fail(&mut ai.state, &mut ai.fill.error, &f);
            None
        }
    }
}

fn take_image(ai: &mut Ai) -> Option<(ImageOut, Option<DocId>)> {
    let job = ai.image.job.as_ref()?;
    let r = match job.rx.try_recv() {
        Ok(r) => r,
        Err(TryRecvError::Empty) => return None,
        Err(TryRecvError::Disconnected) => Err(local("The request stopped unexpectedly. Try again.")),
    };
    let job = ai.image.job.take()?;
    match r {
        Ok((out, reply)) => {
            ai.state.note_reply(&reply);
            Some((out, job.doc))
        }
        Err(f) => {
            fail(&mut ai.state, &mut ai.image.error, &f);
            None
        }
    }
}

/// A failed request: a 402 or a retired key is the result bar's (one message, one link); anything
/// else is the dialog's error line.
fn fail(state: &mut AiState, error: &mut Option<String>, f: &Fail) {
    state.note_fail(f);
    *error = state.notice.is_none().then(|| f.message.clone());
}

/// A fill came in: it goes in at once when its document is in front and unchanged since the
/// request (the selection is the one it was made for); else it waits behind Place image.
fn arrived_fill(app: &mut PhotocraftApp, out: FillOut) {
    let unchanged = app.session.active().is_some_and(|st| {
        st.doc.id == out.target.doc && content_revision(st) == out.target.revision && selection_bounds(&st.doc) == Some(out.target.selection)
    });
    let placed = if unchanged { place_fill(app, &out, true) } else { Err(String::new()) };
    let dialog_open = ai_dialog(app, "fill").is_some();
    let Some(ai) = app.ai.as_mut() else { return };
    match placed {
        Ok(note) => ai.fill.note = Some(note),
        Err(e) => {
            ai.fill.waiting = Some(out);
            ai.fill.note = Some(waiting_note(&e, "Select where it goes, then click Place image."));
            if !dialog_open {
                crate::notices::post(app, "The generated fill is ready", vec!["Open Edit › AI › Generative Fill and click Place image.".into()], false, None);
            }
        }
    }
}

fn arrived_image(app: &mut PhotocraftApp, out: ImageOut, doc: Option<DocId>) {
    let placed = place_image(app, &out, doc);
    let dialog_open = ai_dialog(app, "image").is_some();
    let Some(ai) = app.ai.as_mut() else { return };
    match placed {
        Ok(note) => ai.image.note = Some(note),
        Err(e) => {
            ai.image.waiting = Some(out);
            ai.image.note = Some(waiting_note(&e, "Click Place image to put it in."));
            if !dialog_open {
                crate::notices::post(app, "The generated image is ready", vec!["Open Edit › AI › Generate Image and click Place image.".into()], false, None);
            }
        }
    }
}

/// "The image is ready. <why it is not in yet> <what to do>".
fn waiting_note(why: &str, todo: &str) -> String {
    let mut s = String::from("The image is ready, but it is not in the document yet.");
    if !why.is_empty() {
        s.push(' ');
        s.push_str(why.trim_end_matches('.'));
        s.push('.');
    }
    s.push(' ');
    s.push_str(todo);
    s
}

// ---- the menu -----------------------------------------------------------------------------------

/// Edit › AI (Generative Fill…, Generate Image…, AI Settings…) after Content-Aware Fill…, where AI
/// is on.
pub fn insert_menu_items(app: &PhotocraftApp, items: &mut Vec<MenuItem>) {
    if app.ai.is_none() {
        return;
    }
    let path: Vec<String> = MENU.iter().map(|s| s.to_string()).collect();
    let at = items
        .iter()
        .position(|i| i.id == AFTER)
        .or_else(|| items.iter().rposition(|i| i.path.first().map(String::as_str) == Some("Edit")))
        .map_or(items.len(), |i| i + 1);
    let rows = [(GENERATIVE_FILL, "Generative Fill…"), (GENERATE_IMAGE, "Generate Image…"), ("---", "---"), (SETTINGS, "AI Settings…")];
    for (k, (id, label)) in rows.into_iter().enumerate() {
        let enabled = is_enabled(app, id).unwrap_or(false);
        let item = MenuItem { id: id.into(), label: label.into(), path: path.clone(), shortcut: None, enabled, checked: None, color: None };
        items.insert(at + k, item);
    }
}

/// Enabled state of an Edit › AI item (`None` for other ids).
pub fn is_enabled(app: &PhotocraftApp, id: &str) -> Option<bool> {
    let on = app.ai.is_some();
    match id {
        GENERATIVE_FILL => Some(on && app.session.active().is_some()),
        GENERATE_IMAGE | SETTINGS => Some(on),
        _ => None,
    }
}

/// Run an Edit › AI item: open its dialog (`None` for other ids). Nothing is sent here.
pub fn menu(app: &mut PhotocraftApp, id: &str) -> Option<Result<Value, String>> {
    if !matches!(id, GENERATIVE_FILL | GENERATE_IMAGE | SETTINGS) {
        return None;
    }
    if app.ai.is_none() {
        return Some(Err(OFF.into()));
    }
    Some(match id {
        GENERATIVE_FILL if app.session.active().is_none() => Err("Open a document first".into()),
        GENERATIVE_FILL => Ok(json!({"dialog": open_dialog(app, "fill", "Generative Fill")})),
        GENERATE_IMAGE => Ok(json!({"dialog": open_dialog(app, "image", "Generate Image")})),
        _ => Ok(json!({"dialog": open_settings(app)})),
    })
}

/// The open AI dialog of `kind`, if any.
fn ai_dialog(app: &PhotocraftApp, kind: &str) -> Option<u64> {
    app.ui.dialogs.iter().find(|d| d.fields.get(MARK).and_then(Value::as_str) == Some(kind)).map(|d| d.id)
}

/// Open the AI dialog of `kind` (or find the one open), with the last prompt.
fn open_dialog(app: &mut PhotocraftApp, kind: &str, label: &str) -> u64 {
    if let Some(id) = ai_dialog(app, kind) {
        return id;
    }
    let mut fields = Map::new();
    fields.insert(MARK.into(), json!(kind));
    fields.insert("__label".into(), json!(label));
    if let Some(ai) = app.ai.as_ref() {
        let prompt = if kind == "fill" { &ai.fill.prompt } else { &ai.image.prompt };
        fields.insert("prompt".into(), json!(prompt));
    }
    if kind == "image" {
        fields.insert("shape".into(), json!(Shape::Square.id()));
    }
    app.ui.open_dialog(DialogKind::Command, fields)
}

/// The setup flow in a dialog of its own, over whatever is open (an AI dialog comes back when it
/// ends).
pub fn open_setup_dialog(app: &mut PhotocraftApp) {
    if ai_dialog(app, "setup").is_some() {
        return;
    }
    let Some(ai) = app.ai.as_mut() else { return };
    ai.state.begin();
    open_dialog(app, "setup", "AI");
}

/// Edit › Preferences on its AI section.
pub fn open_settings(app: &mut PhotocraftApp) -> u64 {
    let id = crate::prefs_ui::open_preferences(app, "general");
    if let Some(d) = app.ui.dialog_mut(id) {
        d.fields.insert("section".into(), json!(crate::prefs_ui::AI_SECTION.0));
    }
    id
}

// ---- the dialogs --------------------------------------------------------------------------------

/// The dialog is one of this module's.
pub fn owns(fields: &Map<String, Value>) -> bool {
    fields.contains_key(MARK)
}

/// An AI dialog's body, with its own buttons. `Some(false)` closes it.
pub fn body(app: &mut PhotocraftApp, ui: &mut egui::Ui, fields: &mut Map<String, Value>) -> Option<bool> {
    if app.ai.is_none() {
        let t = Tokens::get(ui.ctx());
        ui.label(RichText::new(OFF).color(t.text_dim));
        return close_row(ui);
    }
    match fields.get(MARK).and_then(Value::as_str).unwrap_or("") {
        "setup" => setup_body(app, ui),
        "fill" => fill_body(app, ui, fields),
        "image" => image_body(app, ui, fields),
        _ => Some(false),
    }
}

/// A button row with Close alone.
fn close_row(ui: &mut egui::Ui) -> Option<bool> {
    ui.add_space(8.0);
    let hit = ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
        crate::widgets::dialog_buttons(ui, &[DialogButton::new(ButtonRole::Cancel, "Close", 84.0)])
    });
    hit.inner.map(|_| false)
}

fn setup_body(app: &mut PhotocraftApp, ui: &mut egui::Ui) -> Option<bool> {
    let ctx = ui.ctx().clone();
    let action = app.ai.as_mut().map_or(Action::Close, |ai| ai.state.setup_ui(ui));
    match action {
        Action::None => None,
        Action::OpenUrl(url) => {
            crate::links::open(app, &ctx, &url);
            None
        }
        Action::Close => Some(false),
    }
}

/// The cost sentence shown before a button that sends: publik's (from the credential, or the
/// crate's own), or who bills an own key.
fn cost_line(state: &AiState, provider: &Provider) -> String {
    match provider {
        Provider::Publik { .. } => match &state.credential {
            Some(c) if !c.cost_sentence.trim().is_empty() => c.cost_sentence.clone(),
            _ => WHY_IT_COSTS.to_string(),
        },
        Provider::Own { service, .. } => format!("Your own key's service ({}) bills this request, not publik.", service.name()),
        Provider::NotSetUp => String::new(),
    }
}

/// The part of a dialog every feature shares when AI is not ready: why, and Set Up AI….
/// `Some(outcome)` when it drew that instead of the feature.
fn not_ready(app: &mut PhotocraftApp, ui: &mut egui::Ui, what: &str) -> Option<Option<bool>> {
    let provider = app.ai.as_ref()?.state.provider();
    if provider.is_set_up() {
        return None;
    }
    ui.label(format!("{what} needs AI. Set it up first: publik API (no key to paste) or your own key."));
    ui.add_space(8.0);
    let mut setup = false;
    let hit = ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
        crate::widgets::dialog_buttons(ui, &[DialogButton::new(ButtonRole::Default, "Set Up AI…", 96.0), DialogButton::new(ButtonRole::Cancel, "Close", 84.0)])
    });
    match hit.inner {
        Some(ButtonRole::Default) => setup = true,
        Some(_) => return Some(Some(false)),
        None => {}
    }
    if setup {
        open_setup_dialog(app);
    }
    Some(None)
}

/// The prompt box. Returns the prompt as typed.
fn prompt_box(ui: &mut egui::Ui, fields: &mut Map<String, Value>, salt: &str, label: &str, hint: &str, enabled: bool) -> String {
    let t = Tokens::get(ui.ctx());
    ui.label(RichText::new(label).color(t.text));
    ui.add_space(2.0);
    let mut prompt = fields.get("prompt").and_then(Value::as_str).unwrap_or("").to_string();
    let edit = egui::TextEdit::multiline(&mut prompt)
        .id(egui::Id::new(("ai-prompt", salt)))
        .hint_text(hint)
        .desired_rows(3)
        .desired_width(f32::INFINITY)
        .char_limit(MAX_PROMPT_CHARS);
    ui.add_enabled(enabled, edit);
    fields.insert("prompt".into(), json!(prompt));
    prompt
}

/// What the dialog says under its buttons: the request in flight, its note or error, the result
/// bar (a 402's one link, or the charge), and what is in use.
fn status_rows<J, O>(app: &mut PhotocraftApp, ui: &mut egui::Ui, task: impl Fn(&Ai) -> &Task<J, O>) {
    let t = Tokens::get(ui.ctx());
    let ctx = ui.ctx().clone();
    let Some(ai) = app.ai.as_mut() else { return };
    let (busy, note, error) = {
        let task = task(ai);
        (task.busy(), task.note.clone(), task.error.clone())
    };
    if busy {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(RichText::new(BUSY).color(t.text_dim));
        });
    }
    if let Some(note) = note {
        ui.add_space(6.0);
        ui.add(egui::Label::new(RichText::new(note).color(t.text)).wrap());
    }
    if let Some(error) = error {
        ui.add_space(6.0);
        ui.add(egui::Label::new(RichText::new(error).color(t.danger)).wrap());
    }
    ui.add_space(6.0);
    let action = ai.state.result_bar(ui);
    ui.label(RichText::new(ai.state.status_line()).size(11.0).color(t.text_faint));
    if let Action::OpenUrl(url) = action {
        crate::links::open(app, &ctx, &url);
    }
}

/// The buttons of a feature dialog: Generate (or Place image and Discard while a result waits) and
/// Close. Returns the role clicked.
fn feature_buttons(ui: &mut egui::Ui, waiting: bool, can_generate: bool, can_place: bool) -> Option<ButtonRole> {
    ui.add_space(8.0);
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
        let close = DialogButton::new(ButtonRole::Cancel, "Close", 84.0);
        if waiting {
            crate::widgets::dialog_buttons(
                ui,
                &[
                    DialogButton::new(ButtonRole::Default, "Place image", 96.0).enabled(can_place),
                    close,
                    DialogButton::new(ButtonRole::Alternate, "Discard", 84.0),
                ],
            )
        } else {
            crate::widgets::dialog_buttons(ui, &[DialogButton::new(ButtonRole::Default, "Generate", 96.0).enabled(can_generate), close])
        }
    })
    .inner
}

fn fill_body(app: &mut PhotocraftApp, ui: &mut egui::Ui, fields: &mut Map<String, Value>) -> Option<bool> {
    if let Some(outcome) = not_ready(app, ui, "Generative Fill") {
        return outcome;
    }
    let t = Tokens::get(ui.ctx());
    let ctx = ui.ctx().clone();
    let selection = app.session.active().and_then(|st| selection_bounds(&st.doc));
    let ai = app.ai.as_mut()?;
    let provider = ai.state.provider();
    let images = provider.supports(Line::Images);
    let (busy, waiting) = (ai.fill.busy(), ai.fill.waiting.is_some());
    ui.add(egui::Label::new("Generative Fill fills the selection with what you describe. The result is a new layer with a mask from the selection.").wrap());
    ui.add_space(4.0);
    ui.add(
        egui::Label::new(
            RichText::new(format!(
                "PhotoCraft sends the visible image around the selection (at most {LONG_EDGE} pixels on its long side) and the selection as a mask. Nothing is sent until you click Generate."
            ))
            .size(11.0)
            .color(t.text_dim),
        )
        .wrap(),
    );
    if selection.is_none() {
        ui.add_space(4.0);
        ui.add(egui::Label::new(RichText::new(NO_SELECTION).color(t.warning)).wrap());
    }
    ui.add_space(8.0);
    let prompt = prompt_box(ui, fields, "fill", "Describe what goes in the selection", "A wooden bench under a tree, soft light", !busy && !waiting);
    ai.fill.prompt = prompt.clone();
    ui.add_space(8.0);
    if images {
        let cost = cost_line(&ai.state, &provider);
        if !cost.is_empty() {
            ui.add(egui::Label::new(RichText::new(cost).size(11.0).color(t.text_dim)).wrap());
        }
    } else {
        ui.add(egui::Label::new(RichText::new(Fail::unsupported(Line::Images).message).color(t.warning)).wrap());
    }
    let can_generate = images && !busy && selection.is_some() && !prompt.trim().is_empty();
    let hit = feature_buttons(ui, waiting, can_generate, selection.is_some());
    status_rows(app, ui, |ai| &ai.fill);
    match hit {
        Some(ButtonRole::Cancel) => return Some(false),
        Some(ButtonRole::Default) if waiting => place_waiting_fill(app),
        Some(ButtonRole::Default) => {
            if let Err(e) = start_fill(app, &ctx, &prompt)
                && let Some(ai) = app.ai.as_mut()
            {
                ai.fill.error = Some(e);
            }
        }
        Some(ButtonRole::Alternate) => {
            if let Some(ai) = app.ai.as_mut() {
                ai.fill.waiting = None;
                ai.fill.note = None;
            }
        }
        _ => {}
    }
    None
}

/// Generate Image's shapes and the request size of each.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shape {
    Square,
    Landscape,
    Portrait,
}

impl Shape {
    const ALL: [Shape; 3] = [Shape::Square, Shape::Landscape, Shape::Portrait];

    /// The shapes a provider can draw: publik API draws 1024x1024 only; a person's own key has all three.
    fn offered(publik: bool) -> &'static [Shape] {
        if publik { &Self::ALL[..1] } else { &Self::ALL[..] }
    }

    fn id(self) -> &'static str {
        match self {
            Shape::Square => "square",
            Shape::Landscape => "landscape",
            Shape::Portrait => "portrait",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Shape::Square => "Square",
            Shape::Landscape => "Landscape",
            Shape::Portrait => "Portrait",
        }
    }
    fn size(self) -> &'static str {
        match self {
            Shape::Square => SIZES[0].2,
            Shape::Landscape => SIZES[1].2,
            Shape::Portrait => SIZES[2].2,
        }
    }
    fn from_id(id: &str) -> Shape {
        Shape::ALL.into_iter().find(|s| s.id() == id).unwrap_or(Shape::Square)
    }
}

fn image_body(app: &mut PhotocraftApp, ui: &mut egui::Ui, fields: &mut Map<String, Value>) -> Option<bool> {
    if let Some(outcome) = not_ready(app, ui, "Generate Image") {
        return outcome;
    }
    let t = Tokens::get(ui.ctx());
    let ctx = ui.ctx().clone();
    let has_doc = app.session.active().is_some();
    let ai = app.ai.as_mut()?;
    let provider = ai.state.provider();
    let images = provider.supports(Line::Images);
    let (busy, waiting) = (ai.image.busy(), ai.image.waiting.is_some());
    ui.add(egui::Label::new("Generate Image makes a new image from what you describe. Nothing is sent until you click Generate.").wrap());
    ui.add_space(4.0);
    let place = if has_doc { "The image becomes a new layer in the middle of the canvas." } else { "No document is open: the image becomes a new document." };
    ui.add(egui::Label::new(RichText::new(place).size(11.0).color(t.text_dim)).wrap());
    ui.add_space(8.0);
    let prompt = prompt_box(ui, fields, "image", "Describe the image", "A lighthouse on a cliff at dawn, soft colors", !busy && !waiting);
    ai.image.prompt = prompt.clone();
    ui.add_space(8.0);
    let mut shape = Shape::from_id(fields.get("shape").and_then(Value::as_str).unwrap_or(""));
    let shapes = Shape::offered(provider.is_publik());
    if !shapes.contains(&shape) {
        shape = Shape::Square;
    }
    ui.horizontal(|ui| {
        ui.label(RichText::new("Shape").color(t.text));
        ui.add_enabled_ui(!busy, |ui| {
            for s in shapes.iter().copied() {
                if ui.radio(shape == s, s.label()).clicked() {
                    shape = s;
                }
            }
        });
    });
    fields.insert("shape".into(), json!(shape.id()));
    ui.add_space(8.0);
    if images {
        let cost = cost_line(&ai.state, &provider);
        if !cost.is_empty() {
            ui.add(egui::Label::new(RichText::new(cost).size(11.0).color(t.text_dim)).wrap());
        }
    } else {
        ui.add(egui::Label::new(RichText::new(Fail::unsupported(Line::Images).message).color(t.warning)).wrap());
    }
    let can_generate = images && !busy && !prompt.trim().is_empty();
    let hit = feature_buttons(ui, waiting, can_generate, true);
    status_rows(app, ui, |ai| &ai.image);
    match hit {
        Some(ButtonRole::Cancel) => return Some(false),
        Some(ButtonRole::Default) if waiting => place_waiting_image(app),
        Some(ButtonRole::Default) => {
            if let Err(e) = start_image(app, &ctx, &prompt, shape)
                && let Some(ai) = app.ai.as_mut()
            {
                ai.image.error = Some(e);
            }
        }
        Some(ButtonRole::Alternate) => {
            if let Some(ai) = app.ai.as_mut() {
                ai.image.waiting = None;
                ai.image.note = None;
            }
        }
        _ => {}
    }
    None
}

// ---- Preferences › AI ---------------------------------------------------------------------------

/// Preferences › AI: what is in use, sign-in and key controls, and the setup flow in place. AI
/// settings take effect at once (OK and Cancel do not apply to them).
pub fn prefs_section(app: &mut PhotocraftApp, ui: &mut egui::Ui) {
    let t = Tokens::get(ui.ctx());
    let ctx = ui.ctx().clone();
    let Some(ai) = app.ai.as_mut() else {
        ui.label(RichText::new(OFF).color(t.text_dim));
        return;
    };
    ui.add(
        egui::Label::new(
            RichText::new("Edit › AI › Generative Fill and Generate Image use publik API or your own key. Changes here take effect at once.").color(t.text_dim),
        )
        .wrap(),
    );
    ui.add_space(6.0);
    let action = if ai.inline_setup {
        let action = ai.state.setup_ui(ui);
        if action == Action::Close {
            ai.inline_setup = false;
        }
        action
    } else {
        let mut open_setup = false;
        let action = ai.state.options_ui(ui, &mut open_setup);
        ai.inline_setup = open_setup;
        action
    };
    if let Action::OpenUrl(url) = action {
        crate::links::open(app, &ctx, &url);
    }
}

// ---- Generative Fill: the request ---------------------------------------------------------------

/// The selection's bounds on the canvas, when there is a selection with anything in it.
fn selection_bounds(doc: &Document) -> Option<Rect> {
    let b = doc.selection.as_ref()?.content_bounds().intersect(&doc.bounds());
    (!b.is_empty()).then_some(b)
}

/// The document's revision without view-only changes (channel eyes, targeting).
fn content_revision(st: &photocraft_engine::DocState) -> u64 {
    st.revision.saturating_sub(st.view_revision)
}

/// The request size nearest the shape of `w × h`.
fn nearest_size(w: u32, h: u32) -> (u32, u32, &'static str) {
    let aspect = |w: u32, h: u32| (f64::from(w.max(1)) / f64::from(h.max(1))).ln();
    let want = aspect(w, h);
    let mut best = SIZES[0];
    for s in SIZES {
        if (aspect(s.0, s.1) - want).abs() < (aspect(best.0, best.1) - want).abs() {
            best = s;
        }
    }
    best
}

/// What a fill sends: the selection's bounds with a margin of context (a quarter of their long
/// side, at least 32 pixels), grown towards the shape of the nearest request size where the canvas
/// has room, and kept on the canvas. With the request size it is scaled to.
fn fill_region(selection: Rect, canvas: Rect) -> Option<(Rect, (u32, u32, &'static str))> {
    let sel = selection.intersect(&canvas);
    if sel.is_empty() {
        return None;
    }
    let margin = i32::try_from((sel.width().max(sel.height()) / 4).max(32)).unwrap_or(i32::MAX);
    let r = sel.inflate(margin).intersect(&canvas);
    let size = nearest_size(r.width(), r.height());
    let target = f64::from(size.0) / f64::from(size.1);
    let (w, h) = (f64::from(r.width()), f64::from(r.height()));
    let r = if w / h < target {
        let want = (h * target).round().min(f64::from(canvas.width())) as i32;
        let (x0, x1) = grow(r.x0, r.x1, want, canvas.x0, canvas.x1);
        Rect::new(x0, r.y0, x1, r.y1)
    } else {
        let want = (w / target).round().min(f64::from(canvas.height())) as i32;
        let (y0, y1) = grow(r.y0, r.y1, want, canvas.y0, canvas.y1);
        Rect::new(r.x0, y0, r.x1, y1)
    };
    Some((r, size))
}

/// `a0..a1` grown to `want` long around its middle, kept inside `lo..hi`.
fn grow(a0: i32, a1: i32, want: i32, lo: i32, hi: i32) -> (i32, i32) {
    let len = a1.saturating_sub(a0);
    if want <= len {
        return (a0, a1);
    }
    let extra = want.saturating_sub(len);
    let mut n0 = a0.saturating_sub(extra / 2);
    let mut n1 = n0.saturating_add(want);
    if n0 < lo {
        n1 = n1.saturating_add(lo.saturating_sub(n0));
        n0 = lo;
    }
    if n1 > hi {
        n0 = n0.saturating_sub(n1.saturating_sub(hi));
        n1 = hi;
    }
    (n0.max(lo), n1)
}

/// A layer name from a prompt: one line, at most [`MAX_NAME_CHARS`] characters, none that a file
/// name cannot hold; `fallback` when nothing is left.
fn layer_name(prompt: &str, fallback: &str) -> String {
    let clean: String = prompt.chars().map(|c| if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { ' ' } else { c }).collect();
    let one: String = clean.split_whitespace().collect::<Vec<_>>().join(" ");
    let name: String = one.chars().take(MAX_NAME_CHARS).collect();
    let name = name.trim().trim_end_matches('.').trim().to_string();
    if name.is_empty() { fallback.to_string() } else { name }
}

/// Start a fill of the active document's selection on a worker.
fn start_fill(app: &mut PhotocraftApp, ctx: &egui::Context, prompt: &str) -> Result<(), String> {
    let prompt = prompt.trim();
    if prompt.is_empty() {
        return Err("Describe what goes in the selection first.".into());
    }
    let st = app.session.active().ok_or("Open a document first.")?;
    let selection = selection_bounds(&st.doc).ok_or(NO_SELECTION)?;
    let (region, size) = fill_region(selection, st.doc.bounds()).ok_or(NO_SELECTION)?;
    if u64::from(region.width()) * u64::from(region.height()) > MAX_REGION_PIXELS {
        return Err(format!(
            "The selection is too large for one request. Select a smaller area (at most {} megapixels with its margin).",
            MAX_REGION_PIXELS / 1_000_000
        ));
    }
    let target = FillTarget { doc: st.doc.id, revision: content_revision(st), region, selection, name: layer_name(prompt, "Generative Fill") };
    let doc = Arc::clone(&st.doc);
    let ai = app.ai.as_mut().ok_or(OFF)?;
    if ai.fill.busy() {
        return Ok(());
    }
    let provider = ai.state.provider();
    let prompt = prompt.to_string();
    // publik API draws 1024x1024 only; the fill is scaled to the selection afterwards either way.
    let size = if provider.is_publik() { SIZES[0] } else { size };
    let rx = craft_publik::ui::spawn(ctx, move || fill_work(&provider, &doc, region, size, &prompt));
    ai.fill.start(FillJob { rx, target });
    Ok(())
}

/// The fill request, on a worker: read the visible image over `region`, scale it and the selection
/// (as a mask: transparent where the service may paint) to the request size, send both, and scale
/// the answer back to `region`. RGBA8 pixels the size of `region`, with the reply.
fn fill_work(provider: &Provider, doc: &Document, region: Rect, size: (u32, u32, &str), prompt: &str) -> Result<(Vec<u8>, Reply), Fail> {
    let (w, h) = (region.width(), region.height());
    let (sw, sh, size_param) = size;
    let image = visible_rgba8(doc, region).ok_or_else(|| local("Not enough memory to read the image."))?;
    let cover = photocraft_algo::selection::mask_from_surface(doc.selection.as_ref(), region);
    let mask = mask_rgba8(&cover);
    let image = resample_rgba8(&image, w, h, sw, sh).ok_or_else(|| local("The image could not be scaled for the request."))?;
    let mask = resample_rgba8(&mask, w, h, sw, sh).ok_or_else(|| local("The mask could not be scaled for the request."))?;
    let scratch = Scratch::new().map_err(local)?;
    let image_path = scratch.write("image.png", &encode_png(sw, sh, image).map_err(local)?).map_err(local)?;
    let mask_path = scratch.write("mask.png", &encode_png(sw, sh, mask).map_err(local)?).map_err(local)?;
    let reply = client::image_edit(provider, prompt, &image_path, Some(&mask_path), size_param)?;
    drop(scratch);
    let (iw, ih, rgba) = decode_rgba8(&reply.bytes).map_err(local)?;
    let back = resample_rgba8(&rgba, iw, ih, w, h).ok_or_else(|| local("The image from the service could not be scaled to the selection."))?;
    Ok((back, reply))
}

/// The document's visible image over `area` as straight RGBA8, rendered in bands (no full-size
/// float buffer is held). `None` when the area is too large to hold.
fn visible_rgba8(doc: &Document, area: Rect) -> Option<Vec<u8>> {
    let len = usize::try_from(u64::from(area.width()) * u64::from(area.height()) * 4).ok()?;
    let mut out = Vec::new();
    out.try_reserve_exact(len).ok()?;
    let mut y = area.y0;
    while y < area.y1 {
        let y1 = y.saturating_add(BAND_ROWS).min(area.y1);
        let band = photocraft_compose::render(doc, Rect::new(area.x0, y, area.x1, y1));
        let px = band.to_rgba8().pixels;
        let want = (area.width() as usize) * (y1 - y) as usize * 4;
        if px.len() != want {
            return None;
        }
        out.extend_from_slice(&px);
        y = y1;
    }
    Some(out)
}

/// A request mask from selection coverage: black, transparent where the selection is (the service
/// paints there), opaque elsewhere.
fn mask_rgba8(cover: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(cover.len() * 4);
    for c in cover {
        let keep = ((1.0 - c.clamp(0.0, 1.0)) * 255.0 + 0.5) as u8;
        out.extend_from_slice(&[0, 0, 0, keep]);
    }
    out
}

/// Start an image request on a worker: the prompt only.
fn start_image(app: &mut PhotocraftApp, ctx: &egui::Context, prompt: &str, shape: Shape) -> Result<(), String> {
    let prompt = prompt.trim();
    if prompt.is_empty() {
        return Err("Describe the image first.".into());
    }
    let doc = app.session.active().map(|st| st.doc.id);
    let ai = app.ai.as_mut().ok_or(OFF)?;
    if ai.image.busy() {
        return Ok(());
    }
    let provider = ai.state.provider();
    let name = layer_name(prompt, "Generated Image");
    let prompt = prompt.to_string();
    let size = if provider.is_publik() { Shape::Square.size() } else { shape.size() };
    let rx = craft_publik::ui::spawn(ctx, move || {
        let reply = client::image_generate(&provider, &prompt, size)?;
        let (width, height, rgba) = decode_rgba8(&reply.bytes).map_err(local)?;
        Ok((ImageOut { png: reply.bytes.clone(), width, height, rgba, name }, reply))
    });
    ai.image.start(ImageJob { rx, doc });
    Ok(())
}

// ---- placing what came back --------------------------------------------------------------------

/// Run `id` with `clip` on the session clipboard, then put back what was there: the pixels go in
/// through the app's own paste commands, and the person's clipboard stays as it was. The OS
/// clipboard is neither read nor written.
fn with_clip(app: &mut PhotocraftApp, clip: photocraft_engine::edit_cmds::Clip, id: &str, params: Value) -> Result<Value, String> {
    let saved = app.session.clipboard.replace(clip);
    let external = std::mem::replace(&mut app.clip_external, false);
    app.clip_read_for_paste = true;
    let r = app.run(id, params);
    app.clip_read_for_paste = false;
    app.session.clipboard = saved;
    app.clip_external = external;
    r
}

/// RGBA8 pixels as a clip at `at` (the size of `at`).
fn clip_of(at: Rect, rgba: &[u8]) -> Result<photocraft_engine::edit_cmds::Clip, String> {
    let want = (at.width() as usize).saturating_mul(at.height() as usize).saturating_mul(4);
    if at.is_empty() || rgba.len() != want {
        return Err("The image from the service has the wrong size.".into());
    }
    let mut surface = photocraft_raster::Surface::from_interleaved(photocraft_color::PixelFormat::RGBA8, at, rgba);
    surface.prune();
    Ok(photocraft_engine::edit_cmds::Clip { surface, bounds: at, layers: None, resolution: None })
}

/// A fill into the active document, through Edit › Paste Special › Paste Into: a new layer whose
/// mask is the selection, named after the prompt, one undo step. `in_place` puts it where it was
/// made for; else it is centred on the selection there now.
fn place_fill(app: &mut PhotocraftApp, out: &FillOut, in_place: bool) -> Result<String, String> {
    let st = app.session.active().ok_or("Open a document first")?;
    let selection = selection_bounds(&st.doc).ok_or(NO_SELECTION)?;
    let r = out.target.region;
    let centre = |b: Rect| json!([(f64::from(b.x0) + f64::from(b.x1)) / 2.0, (f64::from(b.y0) + f64::from(b.y1)) / 2.0]);
    let center = if in_place { centre(r) } else { centre(selection) };
    let clip = clip_of(r, &out.rgba)?;
    let pasted = with_clip(app, clip, "edit.pasteSpecial.pasteInto", json!({"center": center, "target": "pixels"}))?;
    // The layer takes the prompt's name in the same history step (the step's coalescing key).
    let key = app.session.active().and_then(|st| st.coalesce.clone());
    if let (Some(layer), Some(key)) = (pasted.get("layer").and_then(Value::as_u64), key) {
        let _ = app.run("layer.renameLayer", json!({"layer": layer, "name": out.target.name, "coalesce": key}));
    }
    Ok("The fill is a new layer with a mask from the selection. Undo removes it.".into())
}

/// Place Image for a waiting fill: where it was made for when that document is in front with the
/// same selection, else in the selection there now.
fn place_waiting_fill(app: &mut PhotocraftApp) {
    let Some(out) = app.ai.as_ref().and_then(|ai| ai.fill.waiting.clone()) else { return };
    let in_place = app.session.active().is_some_and(|st| st.doc.id == out.target.doc && selection_bounds(&st.doc) == Some(out.target.selection));
    let placed = place_fill(app, &out, in_place);
    if let Some(ai) = app.ai.as_mut() {
        match placed {
            Ok(note) => {
                ai.fill.waiting = None;
                ai.fill.note = Some(note);
                ai.fill.error = None;
            }
            Err(e) => ai.fill.error = Some(e),
        }
    }
}

/// An image into the document it was made for (else the one in front), through File › Place
/// Embedded's code: a new layer in the middle of the canvas, one undo step. With no document open,
/// a new one the image's size, through File › New from Clipboard.
fn place_image(app: &mut PhotocraftApp, out: &ImageOut, doc: Option<DocId>) -> Result<String, String> {
    if app.ui.transform.is_some() {
        return Err("Commit or cancel the transform first".into());
    }
    let open = |id: &DocId| app.session.documents().iter().any(|d| d.doc.id == *id);
    let target = doc.filter(open).or_else(|| app.session.active().map(|st| st.doc.id));
    match target {
        Some(id) => {
            let name = format!("{}.png", out.name);
            let png = out.png.clone();
            app.with_document(id, |app| app.place_bytes(&name, png, None))?;
            Ok("The image is a new layer in the middle of the canvas. Undo removes it.".into())
        }
        None => {
            let at = Rect::new(0, 0, i32::try_from(out.width).unwrap_or(0), i32::try_from(out.height).unwrap_or(0));
            let clip = clip_of(at, &out.rgba)?;
            with_clip(app, clip, "file.newFromClipboard", json!({}))?;
            Ok("The image is a new document.".into())
        }
    }
}

fn place_waiting_image(app: &mut PhotocraftApp) {
    let Some(out) = app.ai.as_ref().and_then(|ai| ai.image.waiting.clone()) else { return };
    let placed = place_image(app, &out, None);
    if let Some(ai) = app.ai.as_mut() {
        match placed {
            Ok(note) => {
                ai.image.waiting = None;
                ai.image.note = Some(note);
                ai.image.error = None;
            }
            Err(e) => ai.image.error = Some(e),
        }
    }
}

// ---- pixels -------------------------------------------------------------------------------------

/// Scale RGBA8 pixels from `sw × sh` to `dw × dh` (a tent filter as wide as the scale, alpha
/// premultiplied). `None` when a size is zero or the pixels do not match it.
fn resample_rgba8(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Option<Vec<u8>> {
    let (sw, sh, dw, dh) = (sw as usize, sh as usize, dw as usize, dh as usize);
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 || src.len() != sw.checked_mul(sh)?.checked_mul(4)? {
        return None;
    }
    if (sw, sh) == (dw, dh) {
        return Some(src.to_vec());
    }
    let pre: Vec<[f32; 4]> = src
        .chunks_exact(4)
        .map(|p| {
            let a = f32::from(p[3]) / 255.0;
            [f32::from(p[0]) / 255.0 * a, f32::from(p[1]) / 255.0 * a, f32::from(p[2]) / 255.0 * a, a]
        })
        .collect();
    let wx = tent_weights(sw, dw);
    let wy = tent_weights(sh, dh);
    // across, then down
    let mut across = vec![[0.0f32; 4]; dw * sh];
    for y in 0..sh {
        let row = pre.get(y * sw..(y + 1) * sw)?;
        for (x, (first, weights)) in wx.iter().enumerate() {
            let mut acc = [0.0f32; 4];
            for (k, w) in weights.iter().enumerate() {
                let p = row.get(first + k)?;
                for (a, v) in acc.iter_mut().zip(p) {
                    *a += v * w;
                }
            }
            *across.get_mut(y * dw + x)? = acc;
        }
    }
    let mut out = Vec::with_capacity(dw * dh * 4);
    for (first, weights) in &wy {
        for x in 0..dw {
            let mut acc = [0.0f32; 4];
            for (k, w) in weights.iter().enumerate() {
                let p = across.get((first + k) * dw + x)?;
                for (a, v) in acc.iter_mut().zip(p) {
                    *a += v * w;
                }
            }
            let a = acc[3].clamp(0.0, 1.0);
            let un = |v: f32| if a > 0.0 { (v / a).clamp(0.0, 1.0) } else { 0.0 };
            let q = |v: f32| (v * 255.0 + 0.5) as u8;
            out.extend_from_slice(&[q(un(acc[0])), q(un(acc[1])), q(un(acc[2])), q(a)]);
        }
    }
    Some(out)
}

/// For each of `dst` samples: the first source sample it reads and the weights of the ones from
/// there (normalised to 1).
fn tent_weights(src: usize, dst: usize) -> Vec<(usize, Vec<f32>)> {
    let scale = src as f64 / dst as f64;
    let radius = scale.max(1.0);
    (0..dst)
        .map(|d| {
            let centre = (d as f64 + 0.5) * scale;
            let lo = (centre - radius).floor().max(0.0) as usize;
            let hi = ((centre + radius).ceil() as usize).min(src).max(lo + 1);
            let mut w: Vec<f32> = (lo..hi).map(|s| (1.0 - ((s as f64 + 0.5) - centre).abs() / radius).max(0.0) as f32).collect();
            let sum: f32 = w.iter().sum();
            if sum > 0.0 {
                for v in &mut w {
                    *v /= sum;
                }
                (lo, w)
            } else {
                // (cannot happen with a radius of at least one sample: the nearest one)
                ((centre.floor() as usize).min(src.saturating_sub(1)), vec![1.0])
            }
        })
        .collect()
}

fn encode_png(w: u32, h: u32, rgba: Vec<u8>) -> Result<Vec<u8>, String> {
    let img = photocraft_codecs::Image::from_u8(w, h, photocraft_codecs::ChannelLayout::Rgba, rgba).map_err(|e| e.to_string())?;
    photocraft_codecs::encode(&img, photocraft_codecs::Format::Png, &photocraft_codecs::EncodeOptions::default()).map_err(|e| e.to_string())
}

/// An image file's pixels as RGBA8: (width, height, pixels).
fn decode_rgba8(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    let img = photocraft_codecs::decode(bytes).map_err(|e| format!("The service sent an image that PhotoCraft cannot read: {e}"))?;
    Ok((img.width(), img.height(), img.to_rgba8()))
}

/// A folder of its own for the files of one request, removed with everything in it when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Result<Self, String> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("photocraft-ai-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).map_err(|e| format!("Could not make a folder for the request: {e}"))?;
        Ok(Scratch(dir))
    }

    fn write(&self, name: &str, bytes: &[u8]) -> Result<PathBuf, String> {
        let path = self.0.join(name);
        std::fs::write(&path, bytes).map_err(|e| format!("Could not write the request's image: {e}"))?;
        Ok(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
#[path = "ai_tests.rs"]
mod tests;

#[cfg(test)]
mod publik_shape_tests {
    use super::Shape;

    #[test]
    fn publik_api_offers_square_only() {
        assert_eq!(Shape::offered(true).len(), 1);
        assert_eq!(Shape::offered(true)[0].size(), "1024x1024");
        assert_eq!(Shape::offered(false).len(), 3);
    }
}
