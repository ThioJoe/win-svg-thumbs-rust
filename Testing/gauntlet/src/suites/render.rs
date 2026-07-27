//! Rendering correctness and determinism.
//!
//! The existing unload harness proves the real Direct2D path executed by
//! sampling three pixels of one SVG at one size. That is exactly right for its
//! purpose and far too narrow to catch a rendering regression, so this suite
//! covers the properties a user would actually notice: that the same input
//! always produces the same image, that scaling works across the full size
//! range, that CSS is applied with the right precedence, and that alpha survives
//! the premultiplied-to-straight conversion.
//!
//! Exact output hashes are deliberately *not* asserted. WARP is deterministic
//! for a given Windows build, but pinning hashes would turn every Windows image
//! update into a spurious failure. Determinism is instead asserted relative to
//! the same process and the same run, which catches the real bug (a renderer
//! that is not reproducible) without the false positives.

use crate::corpus::BASE_SVG;
use crate::dll::{self, Dll, Rendering, Thumb};
use crate::report::Report;

/// Sizes covering icon sizes the shell actually requests, both sides of the
/// provider's 1..=4096 clamp, and awkward non-power-of-two values.
const SIZES: &[u32] = &[1, 2, 3, 7, 16, 24, 32, 48, 64, 96, 128, 129, 255, 256, 257, 512, 1024, 2048, 4096];

pub fn run(dll_handle: &Dll, report: &mut Report) {
    size_sweep(dll_handle, report);
    determinism(dll_handle, report);
    alpha_handling(dll_handle, report);
    css_semantics(dll_handle, report);
    scaling_behaviour(dll_handle, report);
    fallback_detection(dll_handle, report);
}

// ---------------------------------------------------------------
//                          Size sweep
// ---------------------------------------------------------------

fn size_sweep(dll_handle: &Dll, report: &mut Report) {
    for &size in SIZES {
        let name = format!("size_sweep_{size}");
        report.begin_case(&name);
        match dll::try_render(dll_handle, BASE_SVG.as_bytes(), size) {
            Ok(t) => {
                let geometry = t.width == size && t.height == size;
                let alpha = dll::declares_argb(&t);
                let real = t.classify() == Rendering::Real;
                // At 1x1 and 2x2 there is not enough resolution to make claims
                // about content, only about structure.
                let content_ok = if size <= 2 { true } else { real };
                report.check(
                    name,
                    geometry && alpha && content_ok,
                    format!(
                        "{}x{} declared_argb={alpha} classification={:?} coverage={:.2}",
                        t.width,
                        t.height,
                        t.classify(),
                        t.coverage()
                    ),
                );
            }
            Err(hr) => report.fail(
                name,
                format!("a valid SVG failed to render at {size}x{size}: hr=0x{:08X}", hr.0 as u32),
            ),
        }
    }
}

// ---------------------------------------------------------------
//                         Determinism
// ---------------------------------------------------------------

fn determinism(dll_handle: &Dll, report: &mut Report) {
    report.begin_case("repeated_renders_are_identical");
    // Same bytes, same size, same thread: any difference means the pipeline has
    // an uninitialised buffer or a race, which would show up to users as
    // thumbnails that change when refreshed.
    let mut baseline: Option<Thumb> = None;
    let mut mismatch_at = None;
    for i in 0..25 {
        match dll::try_render(dll_handle, BASE_SVG.as_bytes(), 128) {
            Ok(t) => match &baseline {
                None => baseline = Some(t),
                Some(b) => {
                    if b.pixels != t.pixels {
                        mismatch_at = Some(i);
                        break;
                    }
                }
            },
            Err(hr) => {
                report.fail(
                    "repeated_renders_are_identical",
                    format!("render {i} failed: hr=0x{:08X}", hr.0 as u32),
                );
                return;
            }
        }
    }
    report.check(
        "repeated_renders_are_identical",
        mismatch_at.is_none(),
        match mismatch_at {
            None => "25 consecutive renders of the same input produced byte-identical output".to_string(),
            Some(i) => format!("render {i} differed from the first - rendering is not deterministic"),
        },
    );

    report.begin_case("interleaved_inputs_do_not_contaminate");
    // Rendering a different document between two renders of the same document
    // must not change the result. A leaked device-context state or a reused
    // buffer would show up here and nowhere else.
    let other = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><circle cx="32" cy="32" r="30" fill="#ff00ff"/></svg>"##;
    let first = dll::try_render(dll_handle, BASE_SVG.as_bytes(), 128);
    let _ = dll::try_render(dll_handle, other.as_bytes(), 200);
    let _ = dll::try_render(dll_handle, b"garbage not svg", 64);
    let second = dll::try_render(dll_handle, BASE_SVG.as_bytes(), 128);
    match (first, second) {
        (Ok(a), Ok(b)) => report.check(
            "interleaved_inputs_do_not_contaminate",
            a.pixels == b.pixels,
            "rendering other documents in between left the result unchanged".to_string(),
        ),
        _ => report.fail("interleaved_inputs_do_not_contaminate", "one of the renders failed"),
    }

    report.begin_case("empty_regions_are_fully_transparent");
    // The provider clears to transparent black before drawing. If the clear were
    // skipped, uncovered pixels would contain whatever the previous render left
    // in the reused device context - the classic "ghost of the last thumbnail"
    // bug. Render a large opaque document, then a small centred one, and check
    // the corners really are empty.
    let big = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><rect width="64" height="64" fill="#ff0000"/></svg>"##;
    let small = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><rect x="28" y="28" width="8" height="8" fill="#00ff00"/></svg>"##;
    let _ = dll::try_render(dll_handle, big.as_bytes(), 128);
    match dll::try_render(dll_handle, small.as_bytes(), 128) {
        Ok(t) => {
            let corners = [t.pixel(1, 1), t.pixel(126, 1), t.pixel(1, 126), t.pixel(126, 126)];
            let clean = corners.iter().all(|p| p[3] == 0);
            report.check(
                "empty_regions_are_fully_transparent",
                clean,
                format!("corner pixels (BGRA) after a full-bleed red render: {corners:?}"),
            );
        }
        Err(hr) => report.fail(
            "empty_regions_are_fully_transparent",
            format!("render failed: hr=0x{:08X}", hr.0 as u32),
        ),
    }
}

// ---------------------------------------------------------------
//                        Alpha handling
// ---------------------------------------------------------------

fn alpha_handling(dll_handle: &Dll, report: &mut Report) {
    report.begin_case("opaque_fill_is_fully_opaque");
    let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><rect width="64" height="64" fill="#2d89ef"/></svg>"##;
    match dll::try_render(dll_handle, svg.as_bytes(), 64) {
        Ok(t) => {
            let p = t.pixel(32, 32);
            // BGRA, straight (non-premultiplied) alpha.
            let colour_ok = near(p[0], 0xEF) && near(p[1], 0x89) && near(p[2], 0x2D);
            report.check(
                "opaque_fill_is_fully_opaque",
                p[3] == 255 && colour_ok,
                format!("centre pixel BGRA={p:?}, expected ~[0xEF,0x89,0x2D,0xFF]"),
            );
        }
        Err(hr) => report.fail("opaque_fill_is_fully_opaque", format!("hr=0x{:08X}", hr.0 as u32)),
    }

    report.begin_case("half_alpha_is_unpremultiplied");
    // A pure white fill at 50% opacity. Premultiplied it would be ~128,128,128;
    // after correct un-premultiplication the colour channels must be back at
    // ~255 with alpha ~128. Leaving them at 128 is the classic symptom of a
    // missing un-premultiply step and shows up as thumbnails that look
    // washed-out or too dark over a light background.
    let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><rect width="64" height="64" fill="#ffffff" fill-opacity="0.5"/></svg>"##;
    match dll::try_render(dll_handle, svg.as_bytes(), 64) {
        Ok(t) => {
            let p = t.pixel(32, 32);
            let alpha_halfish = (100..=160).contains(&p[3]);
            let colour_restored = p[0] >= 240 && p[1] >= 240 && p[2] >= 240;
            report.check(
                "half_alpha_is_unpremultiplied",
                alpha_halfish && colour_restored,
                format!(
                    "centre pixel BGRA={p:?}; expected colour channels restored to ~255 with \
                     alpha ~128 (premultiplied output would read ~128 in every channel)"
                ),
            );
        }
        Err(hr) => report.fail("half_alpha_is_unpremultiplied", format!("hr=0x{:08X}", hr.0 as u32)),
    }

    report.begin_case("fully_transparent_stays_transparent");
    let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><rect width="64" height="64" fill="#ffffff" fill-opacity="0"/></svg>"##;
    match dll::try_render(dll_handle, svg.as_bytes(), 64) {
        Ok(t) => report.check(
            "fully_transparent_stays_transparent",
            t.is_fully_transparent(),
            format!("coverage={:.3} (expected 0)", t.coverage()),
        ),
        Err(hr) => report.fail("fully_transparent_stays_transparent", format!("hr=0x{:08X}", hr.0 as u32)),
    }

    report.begin_case("gradient_produces_varied_pixels");
    // A gradient that came back uniform would mean the render collapsed to a
    // solid colour - a failure mode that "did it return a bitmap" cannot see.
    let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><defs><linearGradient id="g" x1="0" y1="0" x2="1" y2="0"><stop offset="0" stop-color="#ff0000"/><stop offset="1" stop-color="#0000ff"/></linearGradient></defs><rect width="64" height="64" fill="url(#g)"/></svg>"##;
    match dll::try_render(dll_handle, svg.as_bytes(), 64) {
        Ok(t) => {
            let left = t.pixel(4, 32);
            let right = t.pixel(59, 32);
            // Left should be red-dominant, right blue-dominant (BGRA).
            let varied = !t.is_uniform() && left[2] > left[0] && right[0] > right[2];
            report.check(
                "gradient_produces_varied_pixels",
                varied,
                format!("left BGRA={left:?} right BGRA={right:?} uniform={}", t.is_uniform()),
            );
        }
        Err(hr) => report.fail("gradient_produces_varied_pixels", format!("hr=0x{:08X}", hr.0 as u32)),
    }
}

// ---------------------------------------------------------------
//                        CSS semantics
// ---------------------------------------------------------------
//
// The provider's whole reason for touching the DOM is that Direct2D does not
// apply <style> rules itself. These checks verify the rewriting actually
// produces the colours CSS says it should, which no amount of "did it render"
// testing can establish.

fn css_semantics(dll_handle: &Dll, report: &mut Report) {
    // Each case: (name, svg, expected BGR at the centre).
    let green = [0x00u8, 0xA0, 0x00];
    let cases: Vec<(&str, String, [u8; 3])> = vec![
        (
            "css_class_selector_applied",
            style_svg(".a{fill:#00a000}", r#"<rect class="a" width="64" height="64"/>"#),
            green,
        ),
        (
            "css_element_selector_applied",
            style_svg("rect{fill:#00a000}", r#"<rect width="64" height="64"/>"#),
            green,
        ),
        (
            "css_selector_list_applied",
            style_svg(".x,.a,.y{fill:#00a000}", r#"<rect class="a" width="64" height="64"/>"#),
            green,
        ),
        (
            "css_second_of_multiple_classes_applied",
            style_svg(".b{fill:#00a000}", r#"<rect class="unrelated b" width="64" height="64"/>"#),
            green,
        ),
        (
            "css_later_duplicate_rule_wins",
            style_svg(".a{fill:#ff0000}.a{fill:#00a000}", r#"<rect class="a" width="64" height="64"/>"#),
            green,
        ),
        (
            "css_inline_style_beats_class_rule",
            style_svg(".a{fill:#ff0000}", r#"<rect class="a" width="64" height="64" style="fill:#00a000"/>"#),
            green,
        ),
        (
            "css_comment_does_not_hide_following_rule",
            style_svg("/* comment */.a{fill:#00a000}", r#"<rect class="a" width="64" height="64"/>"#),
            green,
        ),
        (
            "css_commented_out_rule_is_ignored",
            style_svg("/*.a{fill:#ff0000}*/.a{fill:#00a000}", r#"<rect class="a" width="64" height="64"/>"#),
            green,
        ),
        (
            "css_at_media_contents_applied",
            style_svg("@media screen{.a{fill:#00a000}}", r#"<rect class="a" width="64" height="64"/>"#),
            green,
        ),
        (
            "css_important_lowercase_stripped",
            style_svg(".a{fill:#00a000 !important}", r#"<rect class="a" width="64" height="64"/>"#),
            green,
        ),
        (
            "css_important_inline_lowercase_stripped",
            style_svg("", r#"<rect width="64" height="64" style="fill:#00a000 !important"/>"#),
            green,
        ),
    ];

    for (name, svg, expected) in cases {
        report.begin_case(name);
        match dll::try_render(dll_handle, svg.as_bytes(), 64) {
            Ok(t) => {
                let p = t.pixel(32, 32);
                let ok = near(p[0], expected[0]) && near(p[1], expected[1]) && near(p[2], expected[2]) && p[3] > 200;
                report.check(
                    name,
                    ok,
                    format!(
                        "centre BGRA={p:?}, expected ~[{:#04x},{:#04x},{:#04x},0xFF]",
                        expected[0], expected[1], expected[2]
                    ),
                );
            }
            Err(hr) => report.fail(name, format!("render failed: hr=0x{:08X}", hr.0 as u32)),
        }
    }

    // Mixed-case !important. The provider decides whether to run the expensive
    // inline-style pass with a case-INSENSITIVE search for "!important", but
    // strips the token with a case-SENSITIVE replace. An SVG written with
    // "!IMPORTANT" therefore pays the cost of the pass and keeps the token
    // anyway - and Direct2D is documented not to render declarations carrying
    // it, which is the whole reason the stripping exists.
    //
    // CSS keywords are case-insensitive, so the correct behaviour is for the
    // declaration to be applied whichever way it is spelled. This is asserted
    // rather than merely observed: if the two spellings disagree, that is a
    // real rendering difference a user would hit with a real file.
    for (tag, token) in [("uppercase", "!IMPORTANT"), ("mixedcase", "!ImPoRtAnT")] {
        let name = format!("css_important_{tag}_is_stripped");
        report.begin_case(&name);
        let svg = style_svg(
            &format!(".a{{fill:#00a000 {token}}}"),
            r##"<rect class="a" width="64" height="64"/>"##,
        );
        match dll::try_render(dll_handle, svg.as_bytes(), 64) {
            Ok(t) => {
                let p = t.pixel(32, 32);
                let applied = near(p[0], green[0]) && near(p[1], green[1]) && near(p[2], green[2]) && p[3] > 200;
                report.check(
                    &name,
                    applied,
                    format!(
                        "centre BGRA={p:?}, expected ~[0x00,0xa0,0x00,0xFF]. CSS keywords are \
                         case-insensitive, so `{token}` must be stripped exactly like \
                         `!important`; the detection scan is case-insensitive but the strip is a \
                         case-sensitive replace, so this spelling survives into the document \
                         handed to Direct2D."
                    ),
                );
            }
            Err(hr) => report.fail(&name, format!("render failed: hr=0x{:08X}", hr.0 as u32)),
        }
    }
}

fn style_svg(style: &str, body: &str) -> String {
    if style.is_empty() {
        format!(r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64">{body}</svg>"#)
    } else {
        format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><style>{style}</style>{body}</svg>"#
        )
    }
}

// ---------------------------------------------------------------
//                      Scaling behaviour
// ---------------------------------------------------------------

fn scaling_behaviour(dll_handle: &Dll, report: &mut Report) {
    report.begin_case("artwork_scales_to_fill_requested_size");
    // The provider strips width/height so Direct2D scales the viewBox to the
    // thumbnail. A full-bleed rect must therefore cover the whole bitmap at
    // every size, not sit in a corner at its original 64px.
    let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16"><rect width="16" height="16" fill="#0a0"/></svg>"##;
    match dll::try_render(dll_handle, svg.as_bytes(), 256) {
        Ok(t) => {
            let coverage = t.coverage();
            report.check(
                "artwork_scales_to_fill_requested_size",
                coverage > 0.95,
                format!(
                    "coverage={coverage:.3} at 256x256 for a 16x16 source - low coverage means the \
                     artwork was not scaled up to the thumbnail"
                ),
            );
        }
        Err(hr) => report.fail("artwork_scales_to_fill_requested_size", format!("hr=0x{:08X}", hr.0 as u32)),
    }

    report.begin_case("viewbox_synthesised_when_absent");
    // No viewBox, only width/height: the provider synthesises "0 0 w h" before
    // removing the attributes. If that failed, the artwork would not scale.
    let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16"><rect width="16" height="16" fill="#0a0"/></svg>"##;
    match dll::try_render(dll_handle, svg.as_bytes(), 256) {
        Ok(t) => {
            let coverage = t.coverage();
            report.check(
                "viewbox_synthesised_when_absent",
                coverage > 0.95,
                format!("coverage={coverage:.3} at 256x256 for a viewBox-less 16x16 source"),
            );
        }
        Err(hr) => report.fail("viewbox_synthesised_when_absent", format!("hr=0x{:08X}", hr.0 as u32)),
    }

    report.begin_case("aspect_ratio_handling_is_consistent");
    // A wide source rendered into a square thumbnail: whatever the policy
    // (letterbox or stretch), it must be stable and must not produce an empty
    // or fully-covered-by-accident image.
    let wide = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 256 16"><rect width="256" height="16" fill="#0a0"/></svg>"##;
    match dll::try_render(dll_handle, wide.as_bytes(), 128) {
        Ok(t) => {
            let coverage = t.coverage();
            report.check(
                "aspect_ratio_handling_is_consistent",
                coverage > 0.02 && t.classify() == Rendering::Real,
                format!("coverage={coverage:.3} classification={:?}", t.classify()),
            );
        }
        Err(hr) => report.fail("aspect_ratio_handling_is_consistent", format!("hr=0x{:08X}", hr.0 as u32)),
    }
}

// ---------------------------------------------------------------
//                     Fallback detection
// ---------------------------------------------------------------

fn fallback_detection(dll_handle: &Dll, report: &mut Report) {
    report.begin_case("invalid_input_produces_the_fallback");
    // This validates the detector itself. Every other suite relies on
    // `classify()` being able to tell a fallback from real artwork, so if
    // garbage input does not classify as a fallback, the "no unexpected
    // fallbacks" assertions elsewhere are meaningless.
    match dll::try_render(dll_handle, b"this is definitely not an svg", 128) {
        Ok(t) => {
            let c = t.classify();
            report.check(
                "invalid_input_produces_the_fallback",
                c.is_fallback() || c == Rendering::Transparent,
                format!(
                    "garbage input classified as {c:?} - the fallback detector must recognise this \
                     for the other suites' no-fallback assertions to mean anything"
                ),
            );
        }
        Err(hr) => report.pass(
            "invalid_input_produces_the_fallback",
            format!("garbage input failed outright (hr=0x{:08X}), which is also acceptable", hr.0 as u32),
        ),
    }

    report.begin_case("valid_input_is_never_a_fallback");
    match dll::try_render(dll_handle, BASE_SVG.as_bytes(), 128) {
        Ok(t) => report.check(
            "valid_input_is_never_a_fallback",
            t.classify() == Rendering::Real,
            format!("classification={:?}", t.classify()),
        ),
        Err(hr) => report.fail("valid_input_is_never_a_fallback", format!("hr=0x{:08X}", hr.0 as u32)),
    }
}

fn near(a: u8, b: u8) -> bool {
    (a as i32 - b as i32).abs() <= 20
}
