//! Tells an element's owner when it scrolls into or out of view, through one IntersectionObserver
//! shared by all of them (thousands of thumbnails would otherwise need one each).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{Element, IntersectionObserver, IntersectionObserverEntry};

/// Which element an observer entry is about: its number in `WATCHED`.
const ATTR: &str = "data-near";

type Seen = Rc<dyn Fn(bool)>;

thread_local! {
    static OBSERVER: RefCell<Option<IntersectionObserver>> = const { RefCell::new(None) };
    static WATCHED: RefCell<HashMap<u32, Seen>> = RefCell::new(HashMap::new());
    static NEXT: Cell<u32> = const { Cell::new(0) };
}

fn observer() -> Option<IntersectionObserver> {
    OBSERVER.with(|o| {
        if o.borrow().is_none() {
            let cb = Closure::<dyn FnMut(js_sys::Array)>::new(|entries: js_sys::Array| {
                // Collected first: a callback may stop watching (and so borrow WATCHED) itself.
                let calls: Vec<(Seen, bool)> = entries
                    .iter()
                    .filter_map(|e| {
                        let e: IntersectionObserverEntry = e.unchecked_into();
                        let id: u32 = e.target().get_attribute(ATTR)?.parse().ok()?;
                        WATCHED.with(|w| w.borrow().get(&id).cloned()).map(|f| (f, e.is_intersecting()))
                    })
                    .collect();
                for (f, on) in calls {
                    f(on);
                }
            });
            *o.borrow_mut() = IntersectionObserver::new(cb.as_ref().unchecked_ref()).ok();
            cb.forget(); // Lives as long as the page.
        }
        o.borrow().clone()
    })
}

/// Calls `seen(true)` when `el` comes into view (soon after this, if it's already there), and
/// `seen(false)` when it leaves. Returns what `unwatch` takes.
pub(super) fn watch(el: &Element, seen: impl Fn(bool) + 'static) -> u32 {
    let id = NEXT.with(|n| {
        let id = n.get();
        n.set(id.wrapping_add(1));
        id
    });
    WATCHED.with(|w| w.borrow_mut().insert(id, Rc::new(seen)));
    let _ = el.set_attribute(ATTR, &id.to_string());
    if let Some(o) = observer() {
        o.observe(el);
    }
    id
}

pub(super) fn unwatch(el: &Element, id: u32) {
    if let Some(o) = OBSERVER.with(|o| o.borrow().clone()) {
        o.unobserve(el);
    }
    WATCHED.with(|w| w.borrow_mut().remove(&id));
}
