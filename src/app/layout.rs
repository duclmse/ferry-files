//! Sizes the user drags: the sidebar, the two panes, a pane's files and preview, and columns.

use super::*;

/// A width in px kept in localStorage. 0 means "not set": the layout's own default.
pub(crate) fn stored_px(key: &str) -> RwSignal<f64> {
    RwSignal::new(stored(key).and_then(|v| v.parse().ok()).unwrap_or(0.0))
}

/// What a grip resizes.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Target {
    /// The element just before the grip.
    Prev,
    /// The element the grip is in (a table header cell). Dragging left makes it wider.
    ParentLeft,
    /// The element just after the grip. Dragging left makes it wider.
    Next,
}

/// A handle dragged sideways to change `size`, which is saved to `key` (unless empty) when the
/// drag ends.
/// Double-click sets it back to 0 (the default); with `half`, a double-click when it's already
/// the default makes it half of the grip's container instead. The size stays between `min` and `max`, and,
/// when `room` is set, leaves that much of the grip's container free on the side it grows to
/// (the right, or for `Target::Next` the left).
#[component]
pub(crate) fn Grip(
    size: RwSignal<f64>,
    key: String,
    #[prop(default = 80.0)] min: f64,
    #[prop(default = f64::INFINITY)] max: f64,
    #[prop(optional)] room: Option<f64>,
    #[prop(default = Target::Prev)] target: Target,
    #[prop(default = "grip")] class: &'static str,
    #[prop(optional)] half: bool,
) -> impl IntoView {
    let el = NodeRef::<html::Div>::new();
    let listeners = StoredValue::new_local(None::<(WindowListenerHandle, WindowListenerHandle)>);
    let key = StoredValue::new(key);
    let save = move |v: &str| {
        key.with_value(|k| {
            if !k.is_empty() {
                store(k, v)
            }
        })
    };
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
        let Some(grip) = el.get_untracked() else {
            return;
        };
        let grip: web_sys::Element = grip.into();
        let measured = match target {
            Target::Prev => grip.previous_element_sibling(),
            Target::ParentLeft => grip.parent_element(),
            Target::Next => grip.next_element_sibling(),
        };
        let Some(measured) = measured else { return };
        let start = measured.get_bounding_client_rect().width();
        let sign = if target == Target::Prev { 1.0 } else { -1.0 };
        // How far the grip may go before the space on the side it moves to is less than `room`.
        let max = match (room, grip.parent_element()) {
            (Some(room), Some(parent)) => {
                let (outer, g) = (
                    parent.get_bounding_client_rect(),
                    grip.get_bounding_client_rect(),
                );
                let free = if target == Target::Next {
                    g.left() - outer.left()
                } else {
                    outer.right() - g.right()
                } - room;
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
    // Half the container, leaving `room` on the other side; None when it can't be measured.
    let halved = move || {
        let outer = el.get_untracked()?.parent_element()?;
        let w = outer.get_bounding_client_rect().width();
        let most = room.map_or(max, |r| max.min(w - r));
        Some((w / 2.0).clamp(min, most.max(min)).round())
    };
    let title = if half {
        "Drag to resize · double-click for the default, again for half"
    } else {
        "Drag to resize · double-click for the default"
    };
    view! {
        <div class=class node_ref=el role="separator" aria-orientation="vertical"
            title=title
            on:mousedown=down
            on:click=|ev| ev.stop_propagation()
            on:dblclick=move |ev| {
                ev.stop_propagation();
                let w = match halved() {
                    Some(w) if half && size.get_untracked() == 0.0 => w,
                    _ => 0.0,
                };
                size.set(w);
                save(&w.to_string());
            }></div>
    }
}

/// One of a side pane's tabs, for `tab_pick`: its name, whether it's the one showing, and what
/// picking it does.
pub(crate) type SideTab = (&'static str, Signal<bool>, Callback<()>);

/// A side pane's tabs as a list to pick from, which the styles show in place of the tabs when
/// the pane is too narrow for them (`.outline-pick` in styles/views.css).
pub(crate) fn tab_pick(tabs: Vec<SideTab>) -> impl IntoView {
    let acts: Vec<Callback<()>> = tabs.iter().map(|t| t.2).collect();
    let pick = move |e: leptos::ev::Event| {
        if let Some(act) = event_target_value(&e)
            .parse::<usize>()
            .ok()
            .and_then(|i| acts.get(i))
        {
            act.run(());
        }
    };
    view! {
        <select class="outline-pick" title="What the pane shows" aria-label="What the pane shows" on:change=pick>
            {tabs.into_iter().enumerate().map(|(i, (name, on, _))| view! {
                <option value=i.to_string() prop:selected=move || on.get()>{name}</option>
            }).collect_view()}
        </select>
    }
}
