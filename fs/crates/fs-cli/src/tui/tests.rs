use super::*;
use fs_protocol::ipc_protocol::SearchHitDto;

fn make_app() -> App {
    App::new(std::path::Path::new("/tmp/test.sock"))
}

fn make_hit(path: &str, archive_path: Option<&str>) -> SearchHitDto {
    SearchHitDto {
        path: path.into(),
        name: path.rsplit('/').next().unwrap_or(path).into(),
        extension: Some("rs".into()),
        size_bytes: 1024,
        mtime: 0,
        ctime: 0,
        is_dir: false,
        attrib_bitmask: 0,
        archive_path: archive_path.map(str::to_string),
        snippet: None,
    }
}

// ── Mode ─────────────────────────────────────────────────────────────────

#[test]
fn mode_prefix_all_variants() {
    assert_eq!(Mode::Exact.prefix(), "");
    assert_eq!(Mode::Fuzzy.prefix(), "fuzzy:");
    assert_eq!(Mode::Regex.prefix(), "regex:");
    assert_eq!(Mode::Content.prefix(), "content:");
    assert_eq!(Mode::Archive.prefix(), "archive:");
}

#[test]
fn mode_label_all_variants() {
    assert_eq!(Mode::Exact.label(), "exact");
    assert_eq!(Mode::Fuzzy.label(), "fuzzy");
    assert_eq!(Mode::Regex.label(), "regex");
    assert_eq!(Mode::Content.label(), "content");
    assert_eq!(Mode::Archive.label(), "archive");
}

#[test]
fn mode_next_cycles_through_all_and_wraps() {
    let cycle = [
        Mode::Exact,
        Mode::Fuzzy,
        Mode::Regex,
        Mode::Content,
        Mode::Archive,
        Mode::Exact,
    ];
    for pair in cycle.windows(2) {
        assert_eq!(pair[0].next(), pair[1]);
    }
}

// ── App::full_query ───────────────────────────────────────────────────────

#[test]
fn full_query_exact_mode_returns_raw_input() {
    let mut app = make_app();
    app.input = "main.rs".into();
    assert_eq!(app.full_query(), "main.rs");
}

#[test]
fn full_query_fuzzy_mode_prepends_prefix() {
    let mut app = make_app();
    app.mode = Mode::Fuzzy;
    app.input = "mian".into();
    assert_eq!(app.full_query(), "fuzzy:mian");
}

#[test]
fn full_query_non_exact_mode_with_empty_input_returns_empty() {
    let mut app = make_app();
    app.mode = Mode::Content;
    app.input = String::new();
    assert_eq!(app.full_query(), "");
}

// ── Navigation ───────────────────────────────────────────────────────────

#[test]
fn select_next_clamps_at_last_row() {
    let mut app = make_app();
    app.load_hits(vec![make_hit("/a", None), make_hit("/b", None)], 2, 5);
    app.select_next();
    app.select_next();
    app.select_next(); // past end
    assert_eq!(app.list_state.selected(), Some(1)); // clamped at index 1
}

#[test]
fn select_next_empty_rows_is_noop() {
    let mut app = make_app();
    app.select_next();
    assert_eq!(app.list_state.selected(), None);
}

#[test]
fn select_prev_clamps_at_zero() {
    let mut app = make_app();
    app.load_hits(vec![make_hit("/a", None), make_hit("/b", None)], 2, 5);
    app.select_prev(); // already at 0 (auto-selected on load)
    app.select_prev();
    assert_eq!(app.list_state.selected(), Some(0));
}

#[test]
fn selected_path_none_when_nothing_selected() {
    let app = make_app();
    assert_eq!(app.selected_path(), None);
}

#[test]
fn selected_path_returns_path_of_selected_row() {
    let mut app = make_app();
    app.load_hits(
        vec![make_hit("/first.rs", None), make_hit("/second.rs", None)],
        2,
        5,
    );
    app.list_state.select(Some(1));
    assert_eq!(app.selected_path(), Some("/second.rs".into()));
}

// ── load_hits / clear_hits ───────────────────────────────────────────────

#[test]
fn load_hits_populates_rows_and_auto_selects() {
    let mut app = make_app();
    app.load_hits(
        vec![make_hit("/a.rs", None), make_hit("/b.rs", None)],
        42,
        7,
    );
    assert_eq!(app.rows.len(), 2);
    assert_eq!(app.total_hits, 42);
    assert_eq!(app.last_took_ms, Some(7));
    assert_eq!(app.list_state.selected(), Some(0)); // auto-selected
    assert!(app.status_msg.contains("2 results"));
    assert!(app.status_msg.contains("7ms"));
    assert!(app.status_msg.contains("exact"));
}

#[test]
fn load_hits_empty_clears_selection() {
    let mut app = make_app();
    app.load_hits(vec![make_hit("/a.rs", None)], 1, 5);
    app.load_hits(vec![], 0, 3);
    assert!(app.rows.is_empty());
    assert_eq!(app.list_state.selected(), None);
}

#[test]
fn load_hits_marks_archive_entries() {
    let mut app = make_app();
    app.load_hits(
        vec![
            make_hit("/archive.zip!/main.rs", Some("/archive.zip")),
            make_hit("/real.rs", None),
        ],
        2,
        1,
    );
    assert!(app.rows[0].is_archive);
    assert!(!app.rows[1].is_archive);
}

#[test]
fn clear_hits_resets_all_state() {
    let mut app = make_app();
    app.load_hits(vec![make_hit("/a.rs", None)], 1, 5);
    app.clear_hits();
    assert!(app.rows.is_empty());
    assert_eq!(app.total_hits, 0);
    assert_eq!(app.last_took_ms, None);
    assert_eq!(app.list_state.selected(), None);
    assert!(app.status_msg.is_empty());
}

// ── humanize_size ─────────────────────────────────────────────────────────

#[test]
fn humanize_size_boundaries() {
    assert_eq!(humanize_size(0), "0B");
    assert_eq!(humanize_size(1023), "1023B");
    assert_eq!(humanize_size(1024), "1.0KB");
    assert_eq!(humanize_size(1024 * 1024 - 1), "1024.0KB");
    assert_eq!(humanize_size(1024 * 1024), "1.0MB");
    assert_eq!(humanize_size(1024 * 1024 * 1024), "1.0GB");
}

// ── cycle_mode ────────────────────────────────────────────────────────────

#[test]
fn cycle_mode_updates_title_and_sets_pending() {
    let mut app = make_app();
    assert_eq!(app.mode, Mode::Exact);
    app.cycle_mode();
    assert_eq!(app.mode, Mode::Fuzzy);
    assert!(app.mode_title.contains("fuzzy"));
    assert!(app.search_pending);
    assert!(app.last_key_at.is_some());
}

// ── humanize_count ────────────────────────────────────────────────────────

#[test]
fn humanize_count_below_thousand() {
    assert_eq!(humanize_count(0), "0");
    assert_eq!(humanize_count(1), "1");
    assert_eq!(humanize_count(999), "999");
}

#[test]
fn humanize_count_thousands() {
    assert_eq!(humanize_count(1_000), "1.0k");
    assert_eq!(humanize_count(1_500), "1.5k");
    assert_eq!(humanize_count(999_999), "1000.0k");
}

#[test]
fn humanize_count_millions() {
    assert_eq!(humanize_count(1_000_000), "1.0M");
    assert_eq!(humanize_count(3_200_000), "3.2M");
}

// ── load_stats / rebuild_status ───────────────────────────────────────────

#[test]
fn load_stats_updates_counters_and_rebuilds_status() {
    let mut app = make_app();
    app.load_hits(vec![make_hit("/a.rs", None)], 1, 10);
    app.load_stats(3_200_000, 4_000);
    assert_eq!(app.stats_total_files, 3_200_000);
    assert_eq!(app.stats_zip_entries, 4_000);
    assert!(app.status_msg.contains("3.2M idx"));
    assert!(app.status_msg.contains("4.0k zip"));
}

#[test]
fn rebuild_status_shows_result_count_took_and_mode() {
    let mut app = make_app();
    app.load_hits(
        vec![make_hit("/a.rs", None), make_hit("/b.rs", None)],
        99,
        42,
    );
    assert!(app.status_msg.contains("2 results"));
    assert!(app.status_msg.contains("total: 99"));
    assert!(app.status_msg.contains("42ms"));
    assert!(app.status_msg.contains("exact"));
}

#[test]
fn stats_persist_across_clear_hits() {
    let mut app = make_app();
    app.load_stats(5_000, 200);
    app.load_hits(vec![make_hit("/a.rs", None)], 1, 5);
    app.clear_hits();
    // Stats counters survive clear_hits (they're daemon-side facts)
    assert_eq!(app.stats_total_files, 5_000);
    assert_eq!(app.stats_zip_entries, 200);
}

#[test]
fn reset_stats_zeroes_counters() {
    let mut app = make_app();
    app.load_stats(1_000, 50);
    app.reset_stats();
    assert_eq!(app.stats_total_files, 0);
    assert_eq!(app.stats_zip_entries, 0);
}

#[test]
fn render_tags_only_archive_rows_with_zip() {
    use ratatui::{backend::TestBackend, Terminal};

    let mut app = make_app();
    app.load_hits(
        vec![
            make_hit("/archive.zip!/main.rs", Some("/archive.zip")),
            make_hit("/real.rs", None),
        ],
        2,
        1,
    );

    let mut term = Terminal::new(TestBackend::new(80, 10)).unwrap();
    term.draw(|f| super::render::render(f, &mut app)).unwrap();

    let buf = term.backend().buffer();
    let line = |y: u16| {
        (0..buf.area.width)
            .map(|x| buf[(x, y)].symbol())
            .collect::<String>()
    };
    let lines: Vec<String> = (0..buf.area.height).map(line).collect();
    let archive = lines
        .iter()
        .find(|l| l.contains("/archive.zip!/main.rs"))
        .unwrap();
    let real = lines.iter().find(|l| l.contains("/real.rs")).unwrap();
    assert!(archive.contains("[zip]"), "{archive}");
    assert!(!real.contains("[zip]"), "{real}");
}
