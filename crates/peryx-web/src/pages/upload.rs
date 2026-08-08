#![allow(
    clippy::must_use_candidate,
    reason = "the #[component] macro consumes attributes, so #[must_use] cannot reach the generated functions"
)]

use leptos::prelude::*;
use peryx_core::UiUploadSpec;

use crate::data::load_snapshot;
use crate::model::{UiIndex, UiSnapshot};

/// Upload one Python distribution through an upload-enabled index.
#[component]
pub fn Upload() -> impl IntoView {
    let snapshot = Resource::new(|| (), |()| load_snapshot());
    view! {
        <section class="page upload-page">
            <h1>"Upload artifact"</h1>
            <Suspense fallback=|| view! { <p class="dim">"Loading upload targets..."</p> }>
                {move || Suspend::new(async move { upload_form(snapshot.await) })}
            </Suspense>
        </section>
    }
}

fn upload_form(snapshot: UiSnapshot) -> AnyView {
    let indexes = snapshot
        .indexes
        .into_iter()
        .filter(|index| index.upload.is_some())
        .collect::<Vec<_>>();
    let Some(first) = indexes.first() else {
        return view! { <p class="dim">"No index exposes a browser upload."</p> }.into_any();
    };
    let (spec, set_spec) = signal(first.upload.clone().expect("filtered upload target"));
    let (token, set_token) = signal(String::new());
    let (filename, set_filename) = signal(String::new());
    let (outcome, set_outcome) = signal(String::new());
    let (progress, set_progress) = signal(0.0_f64);
    let (uploading, set_uploading) = signal(false);
    let ui = UploadUi {
        outcome: set_outcome,
        progress: set_progress,
        uploading: set_uploading,
    };
    let file_input = NodeRef::<leptos::html::Input>::new();
    let selectable = indexes.clone();
    let targets = indexes.into_iter().map(upload_target).collect_view();
    on_cleanup(cancel_active_upload);
    view! {
        <form class="upload-form" on:submit=move |event| {
            event.prevent_default();
            begin_upload(
                file_input,
                &token.get_untracked(),
                &filename.get_untracked(),
                spec.get_untracked(),
                ui,
            );
        }>
            <label for="upload-route">"Index"</label>
            <select id="upload-route" on:change:target=move |event| {
                let selected = event.target().value();
                if let Some(upload) = selectable
                    .iter()
                    .find(|index| index.route == selected)
                    .and_then(|index| index.upload.clone())
                {
                    set_spec.set(upload);
                }
            }>
                {targets}
            </select>
            <label for="upload-token">{move || spec.get().token_label}</label>
            <input
                id="upload-token"
                class="token"
                type="password"
                autocomplete="off"
                required
                on:input:target=move |event| set_token.set(event.target().value())
            />
            <label for="upload-file">{move || spec.get().file_label}</label>
            <input
                id="upload-file"
                node_ref=file_input
                type="file"
                accept=move || spec.get().accept
                required
                on:change=move |_| set_filename.set(selected_filename(file_input))
            />
            <p class="dim">{move || spec.get().help}</p>
            <div class="upload-actions">
                <button type="submit" disabled=move || uploading.get()>"Upload"</button>
                <button
                    type="button"
                    disabled=move || !uploading.get()
                    on:click=move |_| cancel_active_upload()
                >
                    "Cancel"
                </button>
            </div>
            <progress max="100" value=move || progress.get()></progress>
            <p class="upload-outcome" role="status" aria-live="polite">{move || outcome.get()}</p>
        </form>
    }
    .into_any()
}

fn upload_target(index: UiIndex) -> impl IntoView {
    let label = match index.upload_to.as_deref() {
        Some(target) if target != index.name => format!("{} (stores in {target})", index.route),
        _ => index.route.clone(),
    };
    view! { <option value=index.route>{label}</option> }
}

fn selected_filename(input: NodeRef<leptos::html::Input>) -> String {
    #[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
    {
        input
            .get()
            .and_then(|input| input.files())
            .and_then(|files| files.get(0))
            .map_or_else(String::new, |file| file.name())
    }
    #[cfg(any(feature = "ssr", not(feature = "hydrate")))]
    {
        let _ = input;
        String::new()
    }
}

#[derive(Clone, Copy)]
struct UploadUi {
    outcome: WriteSignal<String>,
    progress: WriteSignal<f64>,
    uploading: WriteSignal<bool>,
}

fn begin_upload(input: NodeRef<leptos::html::Input>, token: &str, filename: &str, spec: UiUploadSpec, ui: UploadUi) {
    if token.is_empty() {
        ui.outcome.set("Enter an upload token.".to_owned());
        return;
    }
    if filename.is_empty() {
        ui.outcome.set("Choose an artifact.".to_owned());
        return;
    }
    if !accepted_filename(filename, &spec.allowed_suffixes) {
        ui.outcome.set(format!("{filename}: unsupported artifact type"));
        return;
    }
    #[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
    browser_upload(input, spec, token.to_owned(), filename.to_owned(), ui);
    #[cfg(any(feature = "ssr", not(feature = "hydrate")))]
    let _ = (input, spec, ui.progress, ui.uploading);
}

fn accepted_filename(filename: &str, suffixes: &[String]) -> bool {
    let filename = filename.to_ascii_lowercase();
    suffixes
        .iter()
        .any(|suffix| filename.ends_with(&suffix.to_ascii_lowercase()))
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
thread_local! {
    static ACTIVE_UPLOAD: std::cell::RefCell<Option<web_sys::XmlHttpRequest>> = const { std::cell::RefCell::new(None) };
    static UPLOAD_CANCELLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
fn browser_upload(
    input: NodeRef<leptos::html::Input>,
    spec: UiUploadSpec,
    token: String,
    filename: String,
    ui: UploadUi,
) {
    use base64::Engine as _;
    use wasm_bindgen::JsCast as _;

    let Some(file) = input
        .get()
        .and_then(|input| input.files())
        .and_then(|files| files.get(0))
    else {
        ui.outcome.set("Choose an artifact.".to_owned());
        return;
    };
    let Some(window) = web_sys::window() else {
        browser_unavailable(ui, &filename);
        return;
    };
    let Ok(origin) = window.location().origin() else {
        browser_unavailable(ui, &filename);
        return;
    };
    let Ok(form) = web_sys::FormData::new() else {
        browser_unavailable(ui, &filename);
        return;
    };
    if form
        .append_with_blob_and_filename(&spec.form_field, &file, &filename)
        .is_err()
    {
        browser_unavailable(ui, &filename);
        return;
    }
    let Ok(request) = web_sys::XmlHttpRequest::new() else {
        browser_unavailable(ui, &filename);
        return;
    };
    let open_failed = request.open_with_async("POST", &spec.endpoint, true).is_err();
    let authorization_failed = !open_failed
        && spec.authorization_username.as_ref().is_some_and(|username| {
            request
                .set_request_header(
                    "authorization",
                    &format!(
                        "Basic {}",
                        base64::engine::general_purpose::STANDARD.encode(format!("{username}:{token}"))
                    ),
                )
                .is_err()
        });
    if open_failed || authorization_failed || request.set_request_header("x-peryx-csrf", &origin).is_err() {
        browser_unavailable(ui, &filename);
        return;
    }
    let Ok(upload) = request.upload() else {
        browser_unavailable(ui, &filename);
        return;
    };
    let progress_callback = wasm_bindgen::closure::Closure::<dyn FnMut(web_sys::ProgressEvent)>::new(
        move |event: web_sys::ProgressEvent| {
            if event.length_computable() && event.total() > 0.0 {
                ui.progress.set(((event.loaded() / event.total()) * 100.0).min(100.0));
            }
        },
    );
    upload.set_onprogress(Some(progress_callback.as_ref().unchecked_ref()));
    drop(progress_callback.into_js_value());

    let completed_request = request.clone();
    let completed_upload = upload;
    let completed_filename = filename.clone();
    let completion = wasm_bindgen::closure::Closure::<dyn FnMut()>::new(move || {
        completed_upload.set_onprogress(None);
        completed_request.set_onloadend(None);
        let cancelled = UPLOAD_CANCELLED.with(std::cell::Cell::get);
        let status = completed_request.status().unwrap_or_default();
        let body = completed_request.response_text().ok().flatten().unwrap_or_default();
        ui.outcome
            .set(upload_outcome(&completed_filename, status, &body, cancelled));
        ui.progress.set(if status < 400 && status != 0 { 100.0 } else { 0.0 });
        ui.uploading.set(false);
        drop(ACTIVE_UPLOAD.with(|active| active.borrow_mut().take()));
    });
    request.set_onloadend(Some(completion.as_ref().unchecked_ref()));
    drop(completion.into_js_value());
    UPLOAD_CANCELLED.with(|cancelled| cancelled.set(false));
    drop(ACTIVE_UPLOAD.with(|active| active.borrow_mut().replace(request.clone())));
    ui.progress.set(0.0);
    ui.outcome.set(format!("{filename}: uploading"));
    ui.uploading.set(true);
    if request.send_with_opt_form_data(Some(&form)).is_err() {
        cancel_active_upload();
        ui.outcome.set(format!("{filename}: request could not start"));
        ui.uploading.set(false);
    }
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
fn browser_unavailable(ui: UploadUi, filename: &str) {
    ui.outcome.set(format!("{filename}: browser upload is unavailable"));
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
fn upload_outcome(filename: &str, status: u16, body: &str, cancelled: bool) -> String {
    if cancelled {
        return format!("{filename}: upload cancelled");
    }
    if (200..300).contains(&status) {
        return format!("{filename}: uploaded");
    }
    if (400..500).contains(&status) {
        let rule = body.trim();
        return if rule.is_empty() {
            format!("{filename}: request rejected ({status})")
        } else {
            format!("{filename}: {rule}")
        };
    }
    if status >= 500 {
        return format!("{filename}: server could not store the upload");
    }
    format!("{filename}: connection ended before the upload completed")
}

fn cancel_active_upload() {
    #[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
    {
        UPLOAD_CANCELLED.with(|cancelled| cancelled.set(true));
        ACTIVE_UPLOAD.with(|active| {
            if let Some(request) = active.borrow().as_ref() {
                drop(request.abort());
            }
        });
    }
}

#[cfg(all(test, not(feature = "ssr"), feature = "hydrate"))]
#[path = "../../tests/unit/pages/upload/tests.rs"]
mod tests;
