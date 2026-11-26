//! How colourful a picture is, for scripts (`ferry.colour`): how much of it is in colour, and
//! the most of any one part of it, so a gray manga page with a colour corner can be told from
//! both a gray page and a colour one.
//!
//! A pixel counts as colour when its red, green and blue are far enough apart. Two things that
//! aren't colour are kept out: the faint fringes JPEG leaves along black lines on white (below
//! `COLOURED`), and a tint over the whole page, like yellowed paper, which is balanced away
//! first (as a camera's white balance does) when it's no stronger than `TINT_MAX`.

use image::imageops::FilterType;
use image::RgbImage;

/// Longest side the picture is shrunk to first: big enough that a small colour patch isn't
/// blended away into the gray around it.
const SIDE: u32 = 640;
/// How far apart (the largest of red, green and blue less the smallest, 0 to 255) a pixel's
/// values must be for it to count as colour: about a tenth of the way to full colour.
const COLOURED: f32 = 26.0;
/// The strongest tint (as `COLOURED` measures it) that's taken to be the paper's, not the
/// picture's: a page that's mostly sky blue keeps its blue.
const TINT_MAX: f32 = 60.0;
/// The picture is cut into GRID × GRID parts for `peak`.
const GRID: u32 = 4;

/// How colourful a picture is.
#[derive(Debug, PartialEq)]
pub struct Colour {
    /// The share of the picture in colour, 0 to 1.
    pub share: f64,
    /// The share in colour of the most colourful of its 16 parts (a 4 × 4 grid), 0 to 1.
    pub peak: f64,
}

/// How colourful the picture in `data` is; None when it isn't a picture that can be read
/// (JPEG, PNG, GIF, WebP and BMP can).
pub fn colour(data: &[u8]) -> Option<Colour> {
    let img = image::load_from_memory(data).ok()?;
    let img = if img.width().max(img.height()) > SIDE {
        img.resize(SIDE, SIDE, FilterType::Triangle)
    } else {
        img
    };
    Some(measure(&img.to_rgb8()))
}

/// The largest of three values less the smallest.
fn spread(v: [f32; 3]) -> f32 {
    v[0].max(v[1]).max(v[2]) - v[0].min(v[1]).min(v[2])
}

/// The middle value of each of red, green and blue over the picture: on a page, its paper,
/// which a colour patch on less than half of it doesn't move.
fn middle(img: &RgbImage) -> [f32; 3] {
    let mut counts = [[0u32; 256]; 3];
    for p in img.pixels() {
        for c in 0..3 {
            counts[c][usize::from(p.0[c])] += 1;
        }
    }
    // Past half, so a picture split evenly between paper and ink takes the brighter.
    let half = img.width() * img.height() / 2;
    counts.map(|n| {
        let mut seen = 0;
        n.iter()
            .position(|&k| {
                seen += k;
                seen > half
            })
            .unwrap_or(0) as f32
    })
}

fn measure(img: &RgbImage) -> Colour {
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return Colour {
            share: 0.0,
            peak: 0.0,
        };
    }
    // Each channel turned up or down so the paper comes out gray. Scaling (not subtracting)
    // keeps black lines black on yellowed paper.
    let mid = middle(img);
    let gain = if spread(mid) > TINT_MAX || mid.contains(&0.0) {
        [1.0; 3]
    } else {
        let avg = (mid[0] + mid[1] + mid[2]) / 3.0;
        mid.map(|m| avg / m)
    };
    // (coloured, all) for each part.
    let mut parts = [(0u32, 0u32); (GRID * GRID) as usize];
    let mut coloured = 0u64;
    for (x, y, p) in img.enumerate_pixels() {
        let v = [0, 1, 2].map(|c| f32::from(p.0[c]) * gain[c]);
        let part = &mut parts[((y * GRID / h) * GRID + x * GRID / w) as usize];
        part.1 += 1;
        if spread(v) > COLOURED {
            part.0 += 1;
            coloured += 1;
        }
    }
    let peak = parts
        .iter()
        .filter(|p| p.1 > 0)
        .map(|p| f64::from(p.0) / f64::from(p.1))
        .fold(0.0, f64::max);
    Colour {
        share: coloured as f64 / (u64::from(w) * u64::from(h)) as f64,
        peak,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;

    /// A 400 × 400 page of `paper`, with black lines, and `patch` (a colour) over the square
    /// from (x0, y0) to (x1, y1).
    fn page(paper: [u8; 3], patch: Option<([u8; 3], (u32, u32, u32, u32))>) -> RgbImage {
        RgbImage::from_fn(400, 400, |x, y| {
            if let Some((c, (x0, y0, x1, y1))) = patch {
                if (x0..x1).contains(&x) && (y0..y1).contains(&y) {
                    return Rgb(c);
                }
            }
            if x % 20 < 2 || y % 50 < 3 {
                Rgb([10, 10, 10])
            } else {
                Rgb(paper)
            }
        })
    }

    #[test]
    fn a_gray_page_has_no_colour() {
        let c = measure(&page([250, 250, 250], None));
        assert_eq!((c.share, c.peak), (0.0, 0.0));
    }

    #[test]
    fn faint_fringes_and_yellowed_paper_are_not_colour() {
        // JPEG's fringes: a little apart, under COLOURED.
        let fringes = RgbImage::from_fn(200, 200, |x, _| {
            if x % 2 == 0 {
                Rgb([140, 128, 120])
            } else {
                Rgb([240, 240, 240])
            }
        });
        assert_eq!(measure(&fringes).share, 0.0);
        // Old paper: the whole page a pale yellow, with black lines and gray shading.
        let old = page([245, 232, 200], Some(([170, 160, 130], (0, 0, 400, 100))));
        let c = measure(&old);
        assert!(c.peak < 0.05, "{c:?}");
    }

    #[test]
    fn a_colour_corner_shows_in_peak_not_share() {
        let c = measure(&page(
            [250, 250, 250],
            Some(([220, 40, 40], (0, 0, 95, 95))),
        ));
        assert!(c.share > 0.03 && c.share < 0.08, "{c:?}");
        assert!(c.peak > 0.8, "{c:?}");
    }

    #[test]
    fn a_colour_page_is_colour_all_over() {
        let photo = RgbImage::from_fn(300, 300, |x, y| {
            Rgb([(x % 256) as u8, 120, (y % 256) as u8])
        });
        let c = measure(&photo);
        assert!(c.share > 0.5 && c.peak > 0.5, "{c:?}");
        // Mostly sky: not taken for a tint and taken off.
        let sky = page([110, 170, 235], None);
        assert!(measure(&sky).share > 0.7);
    }

    #[test]
    fn what_isnt_a_picture_has_no_answer() {
        assert_eq!(colour(b"not a picture"), None);
    }
}
