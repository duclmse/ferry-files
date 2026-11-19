//! The buttons' icons: 16×16 line drawings in the text's colour, sized by `.ico` in base.css.

use leptos::prelude::*;

#[derive(Clone, Copy)]
pub(super) enum Icon {
    Up,
    Sidebar,
    Split,
    Close,
    Backups,
    History,
    Settings,
    Trash,
    Folder,
    File,
    Book,
    Edit,
    Check,
    Stop,
    Save,
    Copy,
    Plus,
    Prev,
    Next,
    ZoomIn,
    ZoomOut,
    TurnLeft,
    TurnRight,
    Restore,
    Eye,
    Refresh,
    Text,
    Binary,
    Wifi,
    Search,
    Eject,
    /// Points right; turned down (in CSS) when what it opens is open.
    Chevron,
    Play,
    Pause,
    SoundOn,
    SoundOff,
    PopOut,
    FullScreen,
}

impl Icon {
    fn paths(self) -> &'static str {
        match self {
            Icon::Up => r#"<path d="M8 13V3M3.5 7.5 8 3l4.5 4.5"/>"#,
            Icon::Sidebar => r#"<rect x="1.5" y="2.5" width="13" height="11" rx="1.5"/><path d="M6 2.5v11M3.5 5h1M3.5 7h1"/>"#,
            Icon::Split => r#"<rect x="1.5" y="2.5" width="13" height="11" rx="1.5"/><path d="M8 2.5v11"/>"#,
            Icon::Close => r#"<path d="M4 4l8 8M12 4l-8 8"/>"#,
            Icon::Backups => r#"<path d="M2.5 8a5.5 5.5 0 1 0 1.6-3.9"/><path d="M2.5 2.5v3h3M8 5v3l2 1.5"/>"#,
            Icon::History => r#"<path d="M5.5 4h8M5.5 8h8M5.5 12h8M2.5 4h.01M2.5 8h.01M2.5 12h.01"/>"#,
            Icon::Settings => r#"<path d="M2 4.5h7M12 4.5h2M2 11.5h2M7 11.5h7"/><circle cx="10.5" cy="4.5" r="1.5"/><circle cx="5.5" cy="11.5" r="1.5"/>"#,
            Icon::Trash => r#"<path d="M2.5 4.5h11M6.5 4.5V3h3v1.5M4 4.5l.7 9h6.6l.7-9"/>"#,
            Icon::Folder => r#"<path d="M1.5 12.5v-9a1 1 0 0 1 1-1h3.5l1.5 2h6a1 1 0 0 1 1 1v7a1 1 0 0 1-1 1h-11a1 1 0 0 1-1-1z"/>"#,
            Icon::Book => r#"<path d="M8 4.5C6.5 3.5 4.5 3 2 3v9.5c2.5 0 4.5.5 6 1.5 1.5-1 3.5-1.5 6-1.5V3c-2.5 0-4.5.5-6 1.5zM8 4.5V14"/>"#,
            Icon::File => r#"<path d="M3.5 1.5h6l3 3v10h-9z"/><path d="M9.5 1.5v3h3"/>"#,
            Icon::Edit => r#"<path d="M10.5 2.5l3 3-8 8h-3v-3z"/>"#,
            Icon::Check => r#"<path d="M3 8.5l3 3 7-7"/>"#,
            Icon::Stop => r#"<rect x="4" y="4" width="8" height="8" rx="1"/>"#,
            Icon::Save => r#"<path d="M8 2.5v7.5M5 7l3 3 3-3M2.5 10.5v3h11v-3"/>"#,
            Icon::Copy => r#"<rect x="5.5" y="5.5" width="8" height="8" rx="1.5"/><path d="M10.5 5.5v-2a1 1 0 0 0-1-1h-6a1 1 0 0 0-1 1v6a1 1 0 0 0 1 1h2"/>"#,
            Icon::Plus => r#"<path d="M8 3v10M3 8h10"/>"#,
            Icon::Prev => r#"<path d="M10 3.5 5.5 8l4.5 4.5"/>"#,
            Icon::Next => r#"<path d="M6 3.5 10.5 8 6 12.5"/>"#,
            Icon::ZoomIn => r#"<circle cx="7" cy="7" r="4.5"/><path d="M10.5 10.5 14 14M5 7h4M7 5v4"/>"#,
            Icon::ZoomOut => r#"<circle cx="7" cy="7" r="4.5"/><path d="M10.5 10.5 14 14M5 7h4"/>"#,
            Icon::TurnLeft => r#"<path d="M3.5 8a4.5 4.5 0 1 0 1.3-3.2"/><path d="M4.5 2v3h3"/>"#,
            Icon::TurnRight => r#"<path d="M12.5 8a4.5 4.5 0 1 1-1.3-3.2"/><path d="M11.5 2v3h-3"/>"#,
            Icon::Restore => r#"<path d="M5.5 3.5 2.5 6.5l3 3"/><path d="M2.5 6.5h7a4 4 0 0 1 0 8h-3"/>"#,
            Icon::Eye => r#"<path d="M1.5 8S4 3.5 8 3.5 14.5 8 14.5 8 12 12.5 8 12.5 1.5 8 1.5 8z"/><circle cx="8" cy="8" r="2"/>"#,
            Icon::Refresh => r#"<path d="M13 8a5 5 0 1 1-1.5-3.6"/><path d="M13 2.5v3h-3"/>"#,
            Icon::Text => r#"<path d="M3 3.5h10M8 3.5v9.5"/>"#,
            Icon::Wifi => r#"<path d="M1.5 6a9.5 9.5 0 0 1 13 0M3.8 8.6a6.2 6.2 0 0 1 8.4 0M6.1 11.1a2.9 2.9 0 0 1 3.8 0M8 13.5h.01"/>"#,
            Icon::Eject => r#"<path d="M3.5 9.5 8 4.5l4.5 5z"/><path d="M3.5 12.5h9"/>"#,
            Icon::Chevron => r#"<path d="M6 3.5 10.5 8 6 12.5"/>"#,
            Icon::Play => r#"<path d="M5 3.5v9l7.5-4.5z"/>"#,
            Icon::Pause => r#"<path d="M5.5 3.5v9M10.5 3.5v9"/>"#,
            Icon::SoundOn => r#"<path d="M2.5 6v4h2.5l3.5 3V3L5 6z"/><path d="M10.5 6a2.5 2.5 0 0 1 0 4M12 4.5a5 5 0 0 1 0 7"/>"#,
            Icon::SoundOff => r#"<path d="M2.5 6v4h2.5l3.5 3V3L5 6z"/><path d="M10.5 6.5l3 3M13.5 6.5l-3 3"/>"#,
            Icon::PopOut => r#"<rect x="1.5" y="3" width="13" height="10" rx="1"/><rect x="8" y="8" width="5" height="3.5"/>"#,
            Icon::FullScreen => r#"<path d="M2.5 6V2.5H6M10 2.5h3.5V6M13.5 10v3.5H10M6 13.5H2.5V10"/>"#,
            Icon::Search => r#"<circle cx="7" cy="7" r="4.5"/><path d="M10.5 10.5 14 14"/>"#,
            Icon::Binary => r#"<rect x="2" y="3.5" width="4" height="9" rx="2"/><path d="M10 5l2-1.5v9"/>"#,
        }
    }
}

/// The icon, hidden from screen readers: the button's label or title says what it does.
pub(super) fn ico(i: Icon) -> impl IntoView {
    view! { <svg class="ico" viewBox="0 0 16 16" aria-hidden="true" inner_html=i.paths()></svg> }
}
