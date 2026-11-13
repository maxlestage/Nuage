//! Browser bundle: hydrates the islands (the live dashboard) and offers the new version of
//! the site when the service worker has one ready.

pub mod live;

use std::cell::Cell;
use std::rc::Rc;

use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{ServiceWorker, ServiceWorkerRegistration, ServiceWorkerState};

#[wasm_bindgen(start)]
pub fn start() {
    active::hydrate(&crate::ISLANDS);
    update_prompt();
}

pub fn window() -> web_sys::Window {
    web_sys::window().expect_throw("no window")
}

pub fn document() -> web_sys::Document {
    window().document().expect_throw("no document")
}

/// Adds an event listener for the page's lifetime.
pub fn on(target: &web_sys::EventTarget, event: &str, f: impl FnMut(web_sys::Event) + 'static) {
    let closure = Closure::<dyn FnMut(web_sys::Event)>::new(f);
    let _ = target.add_event_listener_with_callback(event, closure.as_ref().unchecked_ref());
    closure.forget();
}

/// Calls `f` once, after `ms` milliseconds.
pub fn after(ms: i32, f: impl FnOnce() + 'static) {
    let f = Closure::once_into_js(f);
    let _ = window().set_timeout_with_callback_and_timeout_and_arguments_0(f.unchecked_ref(), ms);
}

/// Calls `f` every `ms` milliseconds, for the page's lifetime.
pub fn every(ms: i32, f: impl FnMut() + 'static) {
    let closure = Closure::<dyn FnMut()>::new(f);
    let _ = window().set_interval_with_callback_and_timeout_and_arguments_0(closure.as_ref().unchecked_ref(), ms);
    closure.forget();
}

/// The update flow. The service worker of a new deployment installs in the background and
/// waits; the page shows "a new version is available" (`#update-toast`, rendered hidden by
/// the server). "Reload" tells the waiting worker to take over (`skip-waiting`), and the
/// page reloads once it controls it.
fn update_prompt() {
    let container = window().navigator().service_worker();
    // The first install takes control without asking: there is nothing to replace.
    let had_controller = container.controller().is_some();
    let reloading = Rc::new(Cell::new(false));
    on(&container, "controllerchange", move |_| {
        if had_controller && !reloading.replace(true) {
            let _ = window().location().reload();
        }
    });
    let Ok(ready) = container.ready() else { return };
    wasm_bindgen_futures::spawn_local(async move {
        let Ok(registration) = wasm_bindgen_futures::JsFuture::from(ready).await else { return };
        let registration: ServiceWorkerRegistration = registration.unchecked_into();
        if let Some(waiting) = registration.waiting() {
            offer(waiting);
        }
        let watched = registration.clone();
        on(&registration, "updatefound", move |_| {
            let Some(worker) = watched.installing() else { return };
            let installing = worker.clone();
            on(&worker, "statechange", move |_| {
                if installing.state() == ServiceWorkerState::Installed
                    && window().navigator().service_worker().controller().is_some()
                {
                    offer(installing.clone());
                }
            });
        });
        // Look for a new version when the visitor comes back to the tab.
        let checked = registration.clone();
        on(&document(), "visibilitychange", move |_| {
            if document().visibility_state() == web_sys::VisibilityState::Visible {
                let _ = checked.update();
            }
        });
    });
}

/// Shows the toast; its button activates `worker`.
fn offer(worker: ServiceWorker) {
    let doc = document();
    let (Some(toast), Some(button)) = (doc.get_element_by_id("update-toast"), doc.get_element_by_id("update-reload"))
    else {
        return;
    };
    let _ = toast.remove_attribute("hidden");
    on(&button, "click", move |_| {
        let _ = worker.post_message(&JsValue::from_str("skip-waiting"));
    });
}
