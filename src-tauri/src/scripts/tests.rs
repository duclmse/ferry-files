use super::*;

static NEVER: AtomicBool = AtomicBool::new(false);

/// A script run for real, its progress dropped.
fn run(
    dev: Arc<Device>,
    dir: &str,
    items: &[String],
    name: &str,
    code: &str,
    stop: &'static AtomicBool,
) -> ScriptReport {
    super::run(dev, dir, items, name, code, stop, false, &|_| {})
}

/// A scratch folder on this computer, removed afterwards.
struct Scratch(std::path::PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn scratch(name: &str) -> (Scratch, String) {
    let dir = std::env::temp_dir().join(format!("ferry-script-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let app = crate::platform::app_path(&dir);
    (Scratch(dir), app)
}

#[test]
fn scripts_list_read_and_write_through_the_device() {
    let (_s, dir) = scratch("rw");
    let dev = Arc::new(Device::new(Box::new(crate::local::Local)));
    let a = device::join(&dir, "a.txt");
    dev.write(&a, b"hello", None, true).unwrap();
    let code = r#"
        for _, e in ipairs(ferry.list(ferry.dir)) do print(e.name, e.size) end
        local s = ferry.read(ferry.items[1])
        ferry.write(ferry.join(ferry.dir, "b.txt"), s:upper())
        ferry.mkdir(ferry.join(ferry.dir, "sub"))
        print(ferry.basename(ferry.rename(ferry.join(ferry.dir, "b.txt"), "c.txt")))
    "#;
    let r = run(dev.clone(), &dir, &[a.clone()], "t", code, &NEVER);
    assert_eq!(r.error, None);
    assert_eq!(r.output, ["a.txt\t5", "c.txt"]);
    assert_eq!(r.changed, 3);
    assert_eq!(dev.read(&device::join(&dir, "c.txt")).unwrap().0, b"HELLO");
    assert!(dev
        .stat(&device::join(&dir, "sub"))
        .unwrap()
        .is_some_and(|e| e.dir));
}

#[test]
fn scripts_get_no_io_or_os_and_say_what_went_wrong() {
    let (_s, dir) = scratch("sandbox");
    let dev = Arc::new(Device::new(Box::new(crate::local::Local)));
    for code in ["io.open('x')", "os.remove('x')", "require('x')"] {
        let r = run(dev.clone(), &dir, &[], "t", code, &NEVER);
        assert!(r.error.is_some(), "{code}");
    }
    let r = run(
        dev.clone(),
        &dir,
        &[],
        "t",
        "ferry.read(ferry.join(ferry.dir, 'none.txt'))",
        &NEVER,
    );
    assert!(
        r.error
            .as_deref()
            .is_some_and(|e| e.starts_with("t.lua:1: No such file") && e.contains("none.txt")),
        "{:?}",
        r.error
    );
    // Caught, the error is a string, with the script's line.
    let r = run(
        dev.clone(),
        &dir,
        &[],
        "t",
        "local ok, e = pcall(function() local s = ferry.read('x') return s end)
print('skipped: ' .. e)",
        &NEVER,
    );
    assert_eq!(r.error, None);
    assert!(
        r.output[0].starts_with("skipped: t.lua:1: "),
        "{:?}",
        r.output
    );
    // A device error can be caught.
    let r = run(
        dev,
        &dir,
        &[],
        "t",
        "print(pcall(ferry.remove, ferry.join(ferry.dir, 'none')))",
        &NEVER,
    );
    assert_eq!(r.error, None);
    assert!(r.output[0].starts_with("false"), "{:?}", r.output);
}

#[test]
fn scripts_make_copy_move_and_delete_files_and_folders() {
    let (_s, dir) = scratch("tree");
    let dev = Arc::new(Device::new(Box::new(crate::local::Local)));
    let code = r#"
        local d = ferry.dir
        ferry.mkdir(ferry.join(d, "a/b/c"), true)
        ferry.create(ferry.join(d, "a/b/c/one.txt"), "1")
        ferry.create(ferry.join(d, "a/two.txt"))
        ferry.append(ferry.join(d, "a/two.txt"), "2")
        ferry.append(ferry.join(d, "a/two.txt"), "2")
        print(pcall(ferry.create, ferry.join(d, "a/two.txt"), "again"))
        ferry.copy(ferry.join(d, "a"), ferry.join(d, "copy"))
        print(pcall(ferry.copy, ferry.join(d, "a"), ferry.join(d, "copy")))
        print(pcall(ferry.copy, ferry.join(d, "a"), ferry.join(d, "a/b/in")))
        ferry.move(ferry.join(d, "copy"), ferry.join(d, "moved"))
        print(ferry.exists(ferry.join(d, "copy")), ferry.read(ferry.join(d, "moved/b/c/one.txt")), ferry.read(ferry.join(d, "moved/two.txt")))
        print(pcall(ferry.remove, ferry.join(d, "moved")))
        ferry.remove(ferry.join(d, "moved"), true)
        print(ferry.exists(ferry.join(d, "moved")))
    "#;
    let r = run(dev.clone(), &dir, &[], "t", code, &NEVER);
    assert_eq!(r.error, None, "{:?}", r.output);
    assert!(
        r.output[0].starts_with("false\t") && r.output[0].contains("already exists"),
        "{:?}",
        r.output
    );
    assert!(
        r.output[1].starts_with("false\t") && r.output[1].contains("Not replaced"),
        "{:?}",
        r.output
    );
    assert!(r.output[2].contains("into itself"), "{:?}", r.output);
    assert_eq!(r.output[3], "false\t1\t22");
    assert!(
        r.output[4].starts_with("false\t"),
        "a folder with things in it isn't deleted without `true`: {:?}",
        r.output
    );
    assert_eq!(r.output[5], "false");
    // The original is as it was.
    assert_eq!(
        dev.read(&device::join(&dir, "a/b/c/one.txt")).unwrap().0,
        b"1"
    );
}

/// The start of a PNG `w` × `h`: enough for its size to be read.
fn png(w: u32, h: u32) -> Vec<u8> {
    let mut b = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    b.extend(w.to_be_bytes());
    b.extend(h.to_be_bytes());
    b.extend([8, 6, 0, 0, 0, 0, 0, 0, 0]);
    b
}

#[test]
fn the_cleanup_script_keeps_only_big_media() {
    let (_s, dir) = scratch("clean");
    let dev = Arc::new(Device::new(Box::new(crate::local::Local)));
    let put = |name: &str, data: &[u8]| {
        dev.write(&device::join(&dir, name), data, None, true)
            .unwrap()
    };
    put("big.png", &png(800, 600));
    put("wide.png", &png(1200, 300));
    put("small.png", &png(120, 90));
    put("notes.txt", b"hello");
    put("song.mp3", b"ID3");
    dev.mkdir(&device::join(&dir, "sub")).unwrap();
    put("sub/tiny.png", &png(10, 10));
    let code = include_str!("../../../docs/scripts/delete non-media and small media.lua");
    let r = run(dev.clone(), &dir, &[], "t", code, &NEVER);
    assert_eq!(r.error, None, "{:?}", r.output);
    let left: Vec<String> = dev
        .listdir(&dir)
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    // Folders aren't gone into unless asked, and sounds are media.
    assert_eq!(left, ["big.png", "song.mp3", "sub"], "{:?}", r.output);
    assert!(dev
        .stat(&device::join(&dir, "sub/tiny.png"))
        .unwrap()
        .is_some());
    assert_eq!(r.changed, 3);
}

#[test]
fn scripts_tell_gray_pages_from_colour_ones() {
    let (_s, dir) = scratch("colour");
    let dev = Arc::new(Device::new(Box::new(crate::local::Local)));
    let put = |name: &str, img: image::RgbImage| {
        let mut b = std::io::Cursor::new(Vec::new());
        img.write_to(&mut b, image::ImageFormat::Png).unwrap();
        dev.write(&device::join(&dir, name), b.get_ref(), None, true)
            .unwrap()
    };
    let gray = image::RgbImage::from_fn(200, 200, |x, _| {
        image::Rgb(if x % 10 < 2 { [0; 3] } else { [240; 3] })
    });
    let mut corner = gray.clone();
    for (x, y, p) in corner.enumerate_pixels_mut() {
        if x < 50 && y < 50 {
            *p = image::Rgb([200, 30, 30]);
        }
    }
    put("gray.png", gray);
    put("corner.png", corner);
    put(
        "photo.png",
        image::RgbImage::from_pixel(80, 60, image::Rgb([40, 120, 220])),
    );
    dev.write(&device::join(&dir, "notes.txt"), b"hi", None, true)
        .unwrap();
    let code = r#"
        for _, n in ipairs({"gray.png", "corner.png", "photo.png", "notes.txt"}) do
            local c = ferry.color(ferry.join(ferry.dir, n))
            if c then print(n, c.share > 0.5, c.peak > 0.5) else print(n, "nil") end
        end
    "#;
    let r = run(dev.clone(), &dir, &[], "t", code, &NEVER);
    assert_eq!(r.error, None, "{:?}", r.output);
    assert_eq!(
        r.output,
        [
            "gray.png\tfalse\tfalse",
            "corner.png\tfalse\ttrue",
            "photo.png\ttrue\ttrue",
            "notes.txt\tnil"
        ]
    );
    assert_eq!(r.changed, 0);
}

#[test]
fn the_grayscale_script_moves_gray_pictures_three_levels_down() {
    let (s, dir) = scratch("gray");
    let dev = Arc::new(Device::new(Box::new(crate::local::Local)));
    let put = |name: &str, img: image::RgbImage| {
        let mut b = std::io::Cursor::new(Vec::new());
        img.write_to(&mut b, image::ImageFormat::Png).unwrap();
        let path = device::join(&dir, name);
        std::fs::create_dir_all(s.0.join(name).parent().unwrap()).unwrap();
        dev.write(&path, b.get_ref(), None, true).unwrap()
    };
    let gray = image::RgbImage::from_fn(120, 120, |x, _| {
        image::Rgb(if x % 10 < 2 { [0; 3] } else { [240; 3] })
    });
    let mut corner = gray.clone();
    for (x, y, p) in corner.enumerate_pixels_mut() {
        if x < 30 && y < 30 {
            *p = image::Rgb([200, 30, 30]);
        }
    }
    put("a.png", gray.clone());
    put("b.png", corner);
    put(
        "c.png",
        image::RgbImage::from_pixel(40, 40, image::Rgb([40, 120, 220])),
    );
    put("x/a.png", gray.clone());
    put("x/y/z/deep.png", gray.clone());
    put("x/y/z/w/too deep.png", gray);
    dev.write(&device::join(&dir, "notes.txt"), b"hi", None, true)
        .unwrap();
    let code = include_str!("../../../docs/scripts/move grayscale pictures.lua");
    // Tried first: it says what it would move, and moves nothing.
    let r = super::run(dev.clone(), &dir, &[], "t", code, &NEVER, true, &|_| {});
    assert_eq!(r.error, None, "{:?}", r.output);
    assert!(r
        .output
        .contains(&"Would move x/a.png to Grayscale/x/a.png".to_string()));
    assert_eq!(
        r.output.last().unwrap(),
        "Would move 3 grayscale pictures to Grayscale."
    );
    assert!(dev
        .stat(&device::join(&dir, "Grayscale"))
        .unwrap()
        .is_none());
    let r = run(dev.clone(), &dir, &[], "t", code, &NEVER);
    assert_eq!(r.error, None, "{:?}", r.output);
    let is = |p: &str| dev.stat(&device::join(&dir, p)).unwrap().is_some();
    for p in [
        "Grayscale/a.png",
        "Grayscale/x/a.png",
        "Grayscale/x/y/z/deep.png",
    ] {
        assert!(is(p), "{p} {:?}", r.output);
    }
    for p in ["b.png", "c.png", "notes.txt", "x/y/z/w/too deep.png"] {
        assert!(is(p), "{p} {:?}", r.output);
    }
    assert!(!is("a.png") && !is("x/a.png"));
    assert_eq!(
        r.output.last().unwrap(),
        "Moved 3 grayscale pictures to Grayscale."
    );
}

#[test]
fn scripts_read_whats_in_a_zip_a_range_at_a_time() {
    use std::io::Write;
    let (_s, dir) = scratch("zip");
    let dev = Arc::new(Device::new(Box::new(crate::local::Local)));
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut z = zip::ZipWriter::new(&mut buf);
        let o = |how| zip::write::SimpleFileOptions::default().compression_method(how);
        z.add_directory("ch1/", o(zip::CompressionMethod::Stored))
            .unwrap();
        z.start_file("ch1/stored.txt", o(zip::CompressionMethod::Stored))
            .unwrap();
        z.write_all(b"plain text").unwrap();
        z.start_file("ch1/packed.txt", o(zip::CompressionMethod::Deflated))
            .unwrap();
        z.write_all(&b"abc".repeat(100)).unwrap();
        z.finish().unwrap();
    }
    let a = device::join(&dir, "a.cbz");
    dev.write(&a, buf.get_ref(), None, true).unwrap();
    let code = r#"
        local zip = ferry.items[1]
        local items, total = ferry.zip(zip)
        print(total)
        for _, e in ipairs(items) do print(e.name, e.size, e.dir, e.stored) end
        print(ferry.unzip(zip, "ch1/stored.txt"))
        print(ferry.unzip(zip, "/ch1/stored.txt", 6))
        print(ferry.unzip(zip, "ch1/packed.txt", 298, 10))
        print(#ferry.unzip(zip, "ch1/packed.txt"))
        print(pcall(ferry.unzip, zip, "nope.txt"))
        print(pcall(ferry.zip, ferry.dir))
    "#;
    let stop = Box::leak(Box::new(AtomicBool::new(false)));
    let r = run(dev, &dir, &[a.clone()], "zip", code, stop);
    assert_eq!(r.error, None);
    assert_eq!(
        r.output[..8],
        [
            "3",
            "ch1/\t0\ttrue\ttrue",
            "ch1/stored.txt\t10\tfalse\ttrue",
            "ch1/packed.txt\t300\tfalse\tfalse",
            "plain text\t10",
            "text\t10",
            "bc\t300",
            "300",
        ]
    );
    assert!(
        r.output[8].starts_with("false\t") && r.output[8].contains("nope.txt: not in the ZIP."),
        "{}",
        r.output[8]
    );
    assert!(
        r.output[9].contains("is a folder, not a ZIP."),
        "{}",
        r.output[9]
    );
}

#[test]
fn a_trial_tells_its_changes_and_makes_none() {
    let (_s, dir) = scratch("trial");
    let dev = Arc::new(Device::new(Box::new(crate::local::Local)));
    let a = device::join(&dir, "a.txt");
    dev.write(&a, b"hello", None, true).unwrap();
    let code = r#"
        ferry.write(ferry.join(ferry.dir, "b.txt"), "new")
        ferry.mkdir(ferry.join(ferry.dir, "sub/deeper"), true)
        print(ferry.exists(ferry.join(ferry.dir, "sub")))
        ferry.move(ferry.items[1], ferry.join(ferry.dir, "sub/a.txt"))
        print(ferry.exists(ferry.items[1]), ferry.stat(ferry.items[1]))
        print(pcall(ferry.create, ferry.join(ferry.dir, "sub/a.txt")))
        ferry.remove(ferry.join(ferry.dir, "sub"), true)
    "#;
    let r = super::run(
        dev.clone(),
        &dir,
        &[a.clone()],
        "t",
        code,
        &NEVER,
        true,
        &|_| {},
    );
    assert_eq!(r.error, None, "{:?}", r.output);
    let sub_a = device::join(&dir, "sub/a.txt");
    assert_eq!(
        r.output,
        [
            "Would write b.txt".to_string(),
            "Would make folder sub/deeper".into(),
            "true".into(),
            "Would move a.txt to sub/a.txt".into(),
            "false\tnil".into(),
            format!("false\t{sub_a} is already there. Not replaced."),
            "Would delete sub".into(),
        ]
    );
    assert_eq!(r.changed, 4);
    let names: Vec<String> = dev
        .listdir(&dir)
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(names, ["a.txt"]);
    assert_eq!(dev.read(&a).unwrap().0, b"hello");
}

#[test]
fn what_a_script_prints_comes_while_it_runs() {
    let (s, dir) = scratch("live");
    let dev = Arc::new(Device::new(Box::new(crate::local::Local)));
    let flag = s.0.join("seen");
    let got = Mutex::new(Vec::<ScriptProgress>::new());
    let tick = |p: ScriptProgress| {
        if p.lines.iter().any(|l| l == "first") {
            let _ = std::fs::write(&flag, b"");
        }
        got.lock().unwrap().push(p);
    };
    // Waits (for a while at most) until the first line has been sent.
    let code = r#"
        print("first")
        local seen = ferry.join(ferry.dir, "seen")
        for _ = 1, 1000000 do
          if ferry.exists(seen) then break end
        end
        print("second")
    "#;
    let r = super::run(dev, &dir, &[], "t", code, &NEVER, false, &tick);
    assert_eq!(r.error, None);
    assert_eq!(r.output, ["first", "second"]);
    let got = got.into_inner().unwrap();
    assert_eq!(
        got.first().map(|p| p.lines.clone()),
        Some(vec!["first".to_string()])
    );
}

#[test]
fn new_lines_are_told_once_even_past_the_last_kept() {
    let out = Out::default();
    out.push("a".into());
    let (lines, seen) = out.since(0);
    assert_eq!((lines, seen), (vec!["a".to_string()], 1));
    for i in 0..OUTPUT_MAX + 5 {
        out.push(i.to_string());
    }
    let (lines, seen) = out.since(seen);
    assert_eq!(lines.len(), OUTPUT_MAX);
    assert_eq!(lines.last().map(String::as_str), Some("2004"));
    assert_eq!(out.since(seen).0, Vec::<String>::new());
}

#[test]
fn stop_ends_a_script() {
    let (_s, dir) = scratch("stop");
    let dev = Arc::new(Device::new(Box::new(crate::local::Local)));
    static STOPPED: AtomicBool = AtomicBool::new(true);
    let r = run(dev, &dir, &[], "t", "while true do end", &STOPPED);
    assert_eq!(
        r.error.as_deref().map(|e| e.contains("Stopped.")),
        Some(true),
        "{:?}",
        r.error
    );
}

#[test]
fn about_is_the_first_comment() {
    assert_eq!(about("-- Lists sizes.\nprint(1)"), "Lists sizes.");
    assert_eq!(about("print(1)"), "");
}

#[test]
fn scripts_read_json_toml_yaml_and_xml() {
    let (_s, dir) = scratch("config");
    let dev = Arc::new(Device::new(Box::new(crate::local::Local)));
    let code = r#"
        local j = ferry.json('{"name": "x", "list": [1, null, 3], "n": null}')
        print(j.name, #j.list, j.list[2] == ferry.null, j.n == ferry.null)
        print(ferry.json("null"), math.type(ferry.json("2")), ferry.json("2.5"))
        local t = ferry.toml('[server]\nport = 8080\nhosts = ["a", "b"]\n')
        print(t.server.port, t.server.hosts[2])
        local a, b = ferry.yaml("x: 1\n---\n- y\n- ~\n")
        print(a.x, b[1], b[2] == ferry.null)
        local x = ferry.xml('<conf v="2"><name>A &amp; B</name><empty/></conf>')
        print(x.tag, x.attr.v, #x, x[1].tag, x[1][1], x[2].tag, #x[2])
        print(pcall(ferry.yaml, "a: 1\na: 2\n"))
        print(pcall(ferry.json, "{"))
        print(pcall(ferry.xml, "<a><b></a>"))
    "#;
    let r = run(dev, &dir, &[], "t", code, &NEVER);
    assert_eq!(r.error, None);
    assert_eq!(
        r.output[..5],
        [
            "x\t3\ttrue\ttrue",
            "nil\tinteger\t2.5",
            "8080\tb",
            "1\ty\ttrue",
            "conf\t2\t2\tname\tA & B\tempty\t0",
        ]
    );
    assert_eq!(
        r.output[5],
        "false\tYAML line 2: the key \"a\" is there twice"
    );
    assert!(
        r.output[6].starts_with("false\tJSON line 1: "),
        "{}",
        r.output[6]
    );
    assert!(
        r.output[7].starts_with("false\tXML line 1: "),
        "{}",
        r.output[7]
    );
}
