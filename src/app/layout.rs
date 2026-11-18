//! Sizes the user drags: the sidebar, the two panes, a pane's files and preview, and columns.

use super::*;

/// A width in px kept in localStorage. 0 means "not set": the layout's own default.
pub(super) fn stored_px(key: &str) -> RwSignal<f64> {
    RwSignal::new(stored(key).and_then(|v| v.parse().ok()).unwrap_or(0.0))
}

/// What a grip resizes.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum Target {
    /// The element just before the grip.
    Prev,
    /// The element the grip is in (a table header cell). Dragging left makes it wider.
    ParentLeft,
}

/// A handle dragged sideways to change `size`, which is saved to `key` (unless empty) when the
/// drag ends.
/// Double-click sets it back to 0 (the default). The size stays between `min` and `max`, and,
/// when `room` is set, leaves that much of the grip's container free to its right.
#[component]
pub(super) fn Grip(
    size: RwSignal<f64>,
    key: String,
    #[prop(default = 80.0)] min: f64,
    #[prop(default = f64::INFINITY)] max: f64,
    #[prop(optional)] room: Option<f64>,
    #[prop(default = Target::Prev)] target: Target,
    #[prop(default = "grip")] class: &'static str,
) -> impl IntoView {
    let el = NodeRef::<html::Div>::new();
    let listeners = StoredValue::new_local(None::<(WindowListenerHandle, WindowListenerHandle)>);
    let key = StoredValue::new(key);
    let save = move |v: &str| key.with_value(|k| if !k.is_empty() { store(k, v) });
    let stop = move || {
        if let Some((a, b)) = listeners.try_update_value(Option::take).flatten() {
            a.remove();
            b.remove();
        }
        if let Some(b) = document().body() {
            let _ = b.class_list().remove_1("resizing");
        }
    };
    on_cleanup(stop);
    let down = move |ev: web_sys::MouseEvent| {
        if ev.button() != 0 {
            return;
        }
        ev.prevent_default();
        ev.stop_propagation();
        let Some(grip) = el.get_untracked() else { return };
        let grip: web_sys::Element = grip.into();
        let measured = match target {
            Target::Prev => grip.previous_element_sibling(),
            Target::ParentLeft => grip.parent_element(),
        };
        let Some(measured) = measured else { return };
        let start = measured.get_bounding_client_rect().width();
        let sign = if target == Target::ParentLeft { -1.0 } else { 1.0 };
        // How far right the grip may go before the space after it is less than `room`.
        let max = match (room, grip.parent_element()) {
            (Some(room), Some(parent)) => {
                let free = parent.get_bounding_client_rect().right() - grip.get_bounding_client_rect().right() - room;
                max.min(start + free.max(0.0))
            }
            _ => max,
        };
        let x0 = ev.client_x() as f64;
        let moved = window_event_listener(ev::mousemove, move |e| {
            let w = start + sign * (e.client_x() as f64 - x0);
            size.set(w.clamp(min, max.max(min)).round());
        });
        let up = window_event_listener(ev::mouseup, move |_| {
            stop();
            save(&size.get_untracked().to_string());
        });
        stop();
        listeners.set_value(Some((moved, up)));
        if let Some(b) = document().body() {
            let _ = b.class_list().add_1("resizing");
        }
    };
    view! {
        <div class=class node_ref=el role="separator" aria-orientation="vertical"
            title="Drag to resize · double-click for the default"
            on:mousedown=down
            on:click=|ev| ev.stop_propagation()
            on:dblclick=move |ev| {
                ev.stop_propagation();
                size.set(0.0);
                save("0");
            }></div>
    }
}
