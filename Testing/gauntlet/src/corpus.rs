//! Synthetic adversarial input corpus.
//!
//! Every case is generated deterministically at run time rather than checked in,
//! so the repository stays free of binary blobs and a case can be described by
//! its name alone. The supervisor can re-run any single case in isolation with
//! `gauntlet exec <suite> --only <case-name>`, which is how a crash found in a
//! batch run gets minimised to one input.
//!
//! Expectations are deliberately asymmetric. For hostile input the contract is
//! only "return, fail or fall back within budget; never crash, hang or corrupt
//! the host". Only inputs that are unambiguously valid SVG using primitives
//! Direct2D is documented to support are required to actually render.

use std::io::Write as _;

/// What a case is allowed to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expect {
    /// Any outcome is acceptable except a crash, a hang, or a malformed bitmap.
    /// Hostile and malformed inputs use this.
    Survive,
    /// Must produce genuine artwork - not the red-X fallback, not the black
    /// square, not a fully transparent image. Used for inputs that are valid
    /// SVG built from primitives Direct2D supports.
    RealRender,
    /// Must be rejected or fall back, never rendered as if it were valid. Used
    /// for inputs that exceed a documented limit.
    RejectOrFallback,
}

pub struct Case {
    pub name: String,
    pub bytes: Vec<u8>,
    pub expect: Expect,
}

fn case(name: &str, bytes: impl Into<Vec<u8>>, expect: Expect) -> Case {
    Case { name: name.to_string(), bytes: bytes.into(), expect }
}

/// A minimal, unambiguously valid SVG used as the base for mutation cases.
pub const BASE_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><rect x="4" y="4" width="56" height="56" fill="#2d89ef"/><circle cx="32" cy="32" r="14" fill="#ffb900"/></svg>"##;

/// Endpoints an XXE case may try to reach. The suite fills these in with a live
/// local listener and a real on-disk sentinel so any resolution attempt is
/// observable rather than theoretical.
pub struct XxeProbe {
    /// e.g. "http://127.0.0.1:53535/dtd"
    pub http_url: String,
    /// e.g. "C:/.../sentinel.txt", already forward-slashed for a file: URI.
    pub sentinel_path: String,
}

// =====================================================================
//                     Structure and encoding
// =====================================================================

fn structural_cases() -> Vec<Case> {
    let mut v = vec![
        case("empty-file", Vec::new(), Expect::Survive),
        case("single-nul", vec![0u8], Expect::Survive),
        case("whitespace-only", "   \r\n\t  ", Expect::Survive),
        case("plain-text-not-xml", "this is not an svg at all", Expect::Survive),
        case("html-not-svg", "<html><body><p>hi</p></body></html>", Expect::Survive),
        case("xml-but-not-svg", r#"<?xml version="1.0"?><root><a/></root>"#, Expect::Survive),
        case("bare-svg-no-namespace", "<svg><rect width='10' height='10'/></svg>", Expect::Survive),
        case("svg-empty-element", "<svg/>", Expect::Survive),
        case("svg-no-children", r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"></svg>"#, Expect::Survive),
        case("unclosed-root", r#"<svg xmlns="http://www.w3.org/2000/svg"><rect/>"#, Expect::Survive),
        case("mismatched-tags", "<svg><rect></circle></svg>", Expect::Survive),
        case("only-closing-tag", "</svg>", Expect::Survive),
        case("doctype-svg", format!("<!DOCTYPE svg PUBLIC \"-//W3C//DTD SVG 1.1//EN\" \"http://www.w3.org/Graphics/SVG/1.1/DTD/svg11.dtd\">{BASE_SVG}"), Expect::Survive),
        case("xml-declaration", format!("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"no\"?>{BASE_SVG}"), Expect::RealRender),
        case("processing-instruction", format!("<?xml-stylesheet type=\"text/css\" href=\"x.css\"?>{BASE_SVG}"), Expect::Survive),
        case("comment-before-root", format!("<!-- leading comment -->{BASE_SVG}"), Expect::RealRender),
        case("unterminated-comment", format!("{}<!-- never closed", BASE_SVG), Expect::Survive),
        case("cdata-section", r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><desc><![CDATA[<not><xml>]]></desc><rect width="10" height="10" fill="red"/></svg>"#, Expect::Survive),
        case("unterminated-cdata", r#"<svg xmlns="http://www.w3.org/2000/svg"><desc><![CDATA[unterminated</svg>"#, Expect::Survive),
    ];

    // Encodings. The provider hands bytes straight to MSXML (or, for .svgz, to
    // Direct2D), so byte-order marks and wide encodings are a genuine input
    // class rather than a theoretical one.
    let utf8_bom = {
        let mut b = vec![0xEF, 0xBB, 0xBF];
        b.extend_from_slice(BASE_SVG.as_bytes());
        b
    };
    v.push(case("utf8-bom", utf8_bom, Expect::Survive));

    let utf16le = {
        let mut b = vec![0xFF, 0xFE];
        for u in BASE_SVG.encode_utf16() {
            b.extend_from_slice(&u.to_le_bytes());
        }
        b
    };
    v.push(case("utf16le-bom", utf16le, Expect::Survive));

    let utf16be = {
        let mut b = vec![0xFE, 0xFF];
        for u in BASE_SVG.encode_utf16() {
            b.extend_from_slice(&u.to_be_bytes());
        }
        b
    };
    v.push(case("utf16be-bom", utf16be, Expect::Survive));

    // Declared encoding that contradicts the actual bytes.
    v.push(case(
        "encoding-mismatch-declared-utf16",
        format!("<?xml version=\"1.0\" encoding=\"UTF-16\"?>{BASE_SVG}"),
        Expect::Survive,
    ));

    // Invalid UTF-8 in the middle of an attribute value.
    let mut bad_utf8 = BASE_SVG.as_bytes().to_vec();
    let insert_at = bad_utf8.len() / 2;
    bad_utf8.splice(insert_at..insert_at, [0xC3, 0x28, 0xA0, 0xFF, 0xFE]);
    v.push(case("invalid-utf8-midway", bad_utf8, Expect::Survive));

    // Embedded NUL bytes, which terminate C strings but are legal in a byte
    // stream - a classic mismatch between a Rust slice and a Win32 API.
    let mut nul_embedded = BASE_SVG.as_bytes().to_vec();
    nul_embedded.splice(30..30, [0u8, 0u8, 0u8]);
    v.push(case("embedded-nul-bytes", nul_embedded, Expect::Survive));

    // Truncation at many offsets: every one leaves the parser in a different
    // partial state, and a single mishandled state is enough to fault.
    let base = BASE_SVG.as_bytes();
    for pct in [1usize, 5, 10, 17, 25, 33, 50, 66, 75, 90, 95, 99] {
        let cut = base.len() * pct / 100;
        v.push(case(&format!("truncated-{pct:02}pct"), base[..cut].to_vec(), Expect::Survive));
    }
    // Off-by-one truncations around the very end, where the closing tag lives.
    for back in [1usize, 2, 3, 6, 7] {
        if base.len() > back {
            let cut = base.len() - back;
            v.push(case(&format!("truncated-minus{back}"), base[..cut].to_vec(), Expect::Survive));
        }
    }

    v
}

// =====================================================================
//                       XML shape and scale
// =====================================================================

fn xml_scale_cases() -> Vec<Case> {
    let mut v = Vec::new();

    // Deep element nesting. MSXML builds a DOM, and the provider then walks
    // every element with XPath, so depth costs both parse and traversal.
    for depth in [64usize, 512, 5_000, 50_000] {
        let mut s = String::from(r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64">"#);
        s.push_str(&"<g>".repeat(depth));
        s.push_str(r##"<rect width="64" height="64" fill="#0a0"/>"##);
        s.push_str(&"</g>".repeat(depth));
        s.push_str("</svg>");
        v.push(case(&format!("deep-nesting-{depth}"), s, Expect::Survive));
    }

    // Unbalanced deep nesting: opens without closes.
    let mut unbalanced = String::from(r#"<svg xmlns="http://www.w3.org/2000/svg">"#);
    unbalanced.push_str(&"<g>".repeat(10_000));
    v.push(case("deep-nesting-unclosed", unbalanced, Expect::Survive));

    // Enormous single attribute value.
    for kb in [64usize, 1024] {
        let filler = "a".repeat(kb * 1024);
        v.push(case(
            &format!("huge-attribute-{kb}kb"),
            format!(r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10" data-x="{filler}"><rect width="10" height="10" fill="red"/></svg>"#),
            Expect::Survive,
        ));
    }

    // Very many attributes on one element.
    let many_attrs: String = (0..20_000).map(|i| format!(" data-a{i}=\"{i}\"")).collect();
    v.push(case(
        "20k-attributes-on-one-element",
        format!(r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><rect{many_attrs} width="10" height="10" fill="red"/></svg>"#),
        Expect::Survive,
    ));

    // Very many sibling elements: this is the realistic "huge icon" shape and
    // the main driver of the provider's per-element XPath walk.
    for n in [1_000usize, 50_000] {
        let body: String = (0..n)
            .map(|i| format!(r##"<rect x="{}" y="{}" width="1" height="1" fill="#00f"/>"##, i % 64, (i / 64) % 64))
            .collect();
        v.push(case(
            &format!("{n}-sibling-elements"),
            format!(r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64">{body}</svg>"#),
            Expect::Survive,
        ));
    }

    // Hundreds of namespace declarations, each of which MSXML must track.
    let namespaces: String = (0..500).map(|i| format!(" xmlns:n{i}=\"urn:test:{i}\"")).collect();
    v.push(case(
        "500-namespace-declarations",
        format!(r#"<svg xmlns="http://www.w3.org/2000/svg"{namespaces} viewBox="0 0 10 10"><rect width="10" height="10" fill="red"/></svg>"#),
        Expect::Survive,
    ));

    // A namespace-prefixed root: the provider strips prefixes when matching CSS
    // element selectors, so this exercises that path.
    v.push(case(
        "prefixed-svg-elements",
        r##"<s:svg xmlns:s="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><s:rect width="10" height="10" fill="#0f0"/></s:svg>"##,
        Expect::Survive,
    ));

    // Thousands of classes on a single element: the provider splits the class
    // attribute on whitespace and does a map lookup per class, per element.
    let classes: String = (0..5_000).map(|i| format!("c{i} ")).collect();
    v.push(case(
        "5k-classes-on-one-element",
        format!(r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><style>.c0{{fill:red}}</style><rect class="{classes}" width="10" height="10"/></svg>"#),
        Expect::Survive,
    ));

    // Self-referential <use>, the classic SVG recursion bomb.
    v.push(case(
        "recursive-use",
        r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" viewBox="0 0 10 10"><g id="a"><use xlink:href="#a"/></g><use xlink:href="#a"/></svg>"##,
        Expect::Survive,
    ));

    // Mutually recursive <use>.
    v.push(case(
        "mutually-recursive-use",
        r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" viewBox="0 0 10 10"><g id="a"><use xlink:href="#b"/></g><g id="b"><use xlink:href="#a"/></g><use xlink:href="#a"/></svg>"##,
        Expect::Survive,
    ));

    v
}

// =====================================================================
//                          CSS torture
// =====================================================================
//
// The provider extracts <style> content, strips !important, parses the CSS with
// a hand-written brace matcher, and applies matching rules as inline styles.
// That parser has an explicit iterative work-stack capped at 256 and a HashMap
// specifically to avoid quadratic behaviour, so these cases aim straight at
// those limits and at the states a hand-written matcher tends to get wrong.

fn css_cases() -> Vec<Case> {
    fn with_style(style: &str, body: &str) -> String {
        format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><style>{style}</style>{body}</svg>"#
        )
    }
    let rect = r#"<rect class="a" width="64" height="64"/>"#;

    let mut v = vec![
        case("css-basic-class", with_style(".a{fill:#0a0}", rect), Expect::RealRender),
        case("css-element-selector", with_style("rect{fill:#0a0}", r#"<rect width="64" height="64"/>"#), Expect::RealRender),
        case("css-empty-style", with_style("", rect), Expect::Survive),
        case("css-only-comment", with_style("/* nothing */", rect), Expect::Survive),
        case("css-unmatched-open-brace", with_style(".a{fill:#0a0", rect), Expect::Survive),
        case("css-unmatched-close-brace", with_style(".a}fill:#0a0}", rect), Expect::Survive),
        case("css-only-braces", with_style("{{{{{{{{", rect), Expect::Survive),
        case("css-only-close-braces", with_style("}}}}}}}}", rect), Expect::Survive),
        case("css-unterminated-comment", with_style(".a{fill:#0a0} /* never closed", rect), Expect::Survive),
        case("css-unterminated-string", with_style(r#".a{content:"unterminated; fill:#0a0}"#, rect), Expect::Survive),
        case("css-brace-inside-string", with_style(r#".a{content:"}"; fill:#0a0}"#, rect), Expect::Survive),
        case("css-escaped-quote", with_style(r#".a{content:"\""; fill:#0a0}"#, rect), Expect::Survive),
        case("css-trailing-backslash", with_style(r".a{fill:#0a0}\", rect), Expect::Survive),
        case("css-empty-selector-list", with_style(",,,,{fill:#0a0}", rect), Expect::Survive),
        case("css-empty-rule-body", with_style(".a{}", rect), Expect::Survive),
        case("css-semicolon-spam", with_style(".a{;;;;fill:#0a0;;;;}", rect), Expect::Survive),
        case("css-no-semicolons", with_style(".a{fill:#0a0}", rect), Expect::Survive),
        case("css-at-rule-bare", with_style("@charset \"utf-8\";.a{fill:#0a0}", rect), Expect::Survive),
        case("css-nul-in-style", with_style(".a{fill:#0a0}\u{0}", rect), Expect::Survive),
        case("css-unicode-selector", with_style(".é{fill:#0a0}.a{fill:#00a}", rect), Expect::Survive),
    ];

    // !important handling in every casing, plus in inline styles. The provider
    // does a case-insensitive scan to decide whether to do the expensive inline
    // pass, but then strips with a case-SENSITIVE replace of "!important".
    // Mixed case therefore takes the expensive path and still leaves the token
    // in place, which is exactly the kind of asymmetry worth pinning.
    for (tag, token) in [
        ("lower", "!important"),
        ("upper", "!IMPORTANT"),
        ("mixed", "!ImPoRtAnT"),
        ("spaced", "! important"),
    ] {
        v.push(case(
            &format!("css-important-{tag}"),
            with_style(&format!(".a{{fill:#0a0 {token}}}"), rect),
            Expect::Survive,
        ));
        v.push(case(
            &format!("css-important-inline-{tag}"),
            format!(
                r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><rect width="64" height="64" style="fill:#0a0 {token}"/></svg>"#
            ),
            Expect::Survive,
        ));
    }

    // At-rule nesting right around the parser's MAX_DEPTH of 256. Nesting
    // unwinds one level per pop so the stack stays shallow, but 255/256/257 is
    // where an off-by-one in the cap would show up.
    for depth in [1usize, 255, 256, 257, 1_000] {
        let css = format!(
            "{}.a{{fill:#0a0}}{}",
            "@media screen{".repeat(depth),
            "}".repeat(depth)
        );
        v.push(case(&format!("css-at-rule-nested-{depth}"), with_style(&css, rect), Expect::Survive));
    }

    // Sibling at-rules all land on the work stack at once, so this is the shape
    // that actually reaches the 256 cap.
    for n in [255usize, 256, 257, 2_000] {
        let css = "@media screen{.a{fill:#0a0}}".repeat(n);
        v.push(case(&format!("css-at-rule-siblings-{n}"), with_style(&css, rect), Expect::Survive));
    }

    // Unclosed at-rule blocks.
    v.push(case("css-at-rule-unclosed", with_style(&"@media screen{".repeat(300), rect), Expect::Survive));

    // Rule-count scale, aimed at the HashMap that exists to stop quadratic
    // behaviour on repeated selectors.
    let distinct: String = (0..20_000).map(|i| format!(".c{i}{{fill:#000}}")).collect();
    v.push(case("css-20k-distinct-rules", with_style(&distinct, rect), Expect::Survive));

    let duplicated = ".a{fill:#000}".repeat(20_000);
    v.push(case("css-20k-duplicate-rules", with_style(&duplicated, rect), Expect::Survive));

    // Long selector lists: the property block is normalised once per selector,
    // so this is the parser's worst-case output amplification.
    let selectors: String = (0..5_000).map(|i| format!(".s{i},")).collect();
    let block = "fill:#000000;stroke:#ffffff;".repeat(20);
    v.push(case(
        "css-5k-selector-list",
        with_style(&format!("{selectors}.a{{{block}}}"), rect),
        Expect::Survive,
    ));

    // Many <style> elements rather than one big one.
    let many_styles: String = (0..2_000).map(|i| format!("<style>.c{i}{{fill:#111}}</style>")).collect();
    v.push(case(
        "css-2k-style-elements",
        format!(r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64">{many_styles}{rect}</svg>"#),
        Expect::Survive,
    ));

    // Style content wrapped in CDATA, which is how many authoring tools emit it.
    v.push(case(
        "css-in-cdata",
        format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><style><![CDATA[.a{{fill:#0a0}}]]></style>{rect}</svg>"#
        ),
        Expect::Survive,
    ));

    // Inline style must win over a class rule (standard CSS cascade, and the
    // provider explicitly prepends class styles so inline ones take precedence).
    v.push(case(
        "css-inline-overrides-class",
        format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><style>.a{{fill:#ff0000}}</style><rect class="a" width="64" height="64" style="fill:#00ff00"/></svg>"#
        ),
        Expect::RealRender,
    ));

    v
}

// =====================================================================
//                        Numeric extremes
// =====================================================================

fn numeric_cases() -> Vec<Case> {
    let mut v = Vec::new();

    // viewBox values that are degenerate, inverted, enormous or non-numeric.
    for (tag, vb) in [
        ("zero", "0 0 0 0"),
        ("zero-width", "0 0 0 64"),
        ("zero-height", "0 0 64 0"),
        ("negative-size", "0 0 -64 -64"),
        ("negative-origin", "-1000 -1000 64 64"),
        ("huge", "0 0 1e30 1e30"),
        ("tiny", "0 0 1e-30 1e-30"),
        ("nan", "0 0 NaN NaN"),
        ("inf", "0 0 Infinity Infinity"),
        ("too-few-values", "0 0 64"),
        ("too-many-values", "0 0 64 64 64"),
        ("non-numeric", "a b c d"),
        ("empty", ""),
        ("comma-separated", "0,0,64,64"),
        ("extra-whitespace", "  0   0   64   64  "),
    ] {
        v.push(case(
            &format!("viewbox-{tag}"),
            format!(r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="{vb}"><rect width="64" height="64" fill="#0a0"/></svg>"##),
            Expect::Survive,
        ));
    }

    // width/height attribute variants. The provider removes these before
    // drawing, and synthesises a viewBox from them when none is present, so the
    // combinations here drive that logic directly.
    for (tag, wh) in [
        ("percent", r#"width="100%" height="100%""#),
        ("em-units", r#"width="10em" height="10em""#),
        ("pt-units", r#"width="72pt" height="72pt""#),
        ("px-units", r#"width="64px" height="64px""#),
        ("zero", r#"width="0" height="0""#),
        ("negative", r#"width="-64" height="-64""#),
        ("huge", r#"width="1e30" height="1e30""#),
        ("nan", r#"width="NaN" height="NaN""#),
        ("only-width", r#"width="64""#),
        ("only-height", r#"height="64""#),
        ("empty", r#"width="" height="""#),
        ("whitespace", r#"width="  " height="  ""#),
    ] {
        // Without a viewBox, so the provider's synthesise-a-viewBox branch runs.
        v.push(case(
            &format!("dimensions-{tag}-no-viewbox"),
            format!(r##"<svg xmlns="http://www.w3.org/2000/svg" {wh}><rect width="64" height="64" fill="#0a0"/></svg>"##),
            Expect::Survive,
        ));
    }

    // A width/height string long enough to overflow the fixed 32-u16 buffer the
    // provider reads attributes into, and a viewBox longer than its 64-u16
    // buffer. Truncation there must not produce a malformed synthesised value
    // that then crashes the SVG document.
    v.push(case(
        "dimensions-overflowing-attribute-buffer",
        format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="{}" height="{}"><rect width="64" height="64" fill="#0a0"/></svg>"##,
            "1".repeat(200),
            "2".repeat(200)
        ),
        Expect::Survive,
    ));
    v.push(case(
        "viewbox-overflowing-attribute-buffer",
        format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="{}"><rect width="64" height="64" fill="#0a0"/></svg>"##,
            (0..40).map(|i| format!("{i}.00000")).collect::<Vec<_>>().join(" ")
        ),
        Expect::Survive,
    ));

    // Coordinate and colour extremes on the shapes themselves.
    for (tag, attrs) in [
        ("nan-coords", r#"x="NaN" y="NaN" width="NaN" height="NaN""#),
        ("inf-coords", r#"x="Infinity" y="-Infinity" width="Infinity" height="Infinity""#),
        ("huge-exponent", r#"x="1e308" y="-1e308" width="1e308" height="1e308""#),
        ("denormal", r#"x="1e-320" y="1e-320" width="1e-320" height="1e-320""#),
        ("many-digits", r#"x="0.123456789012345678901234567890" y="0" width="64" height="64""#),
        ("leading-plus", r#"x="+4" y="+4" width="+56" height="+56""#),
        ("hex-numbers", r#"x="0x10" y="0x10" width="0x10" height="0x10""#),
    ] {
        v.push(case(
            &format!("coords-{tag}"),
            format!(r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><rect {attrs} fill="#0a0"/></svg>"##),
            Expect::Survive,
        ));
    }

    // Path data extremes: an enormous single path, and malformed commands.
    let long_path: String = (0..50_000).map(|i| format!("L{} {}", i % 64, (i * 7) % 64)).collect();
    v.push(case(
        "path-50k-segments",
        format!(r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><path d="M0 0 {long_path}" stroke="#0a0" fill="none"/></svg>"##),
        Expect::Survive,
    ));
    for (tag, d) in [
        ("garbage", "QQQQQQQQ"),
        ("truncated-command", "M0 0 L"),
        ("nan-in-path", "M NaN NaN L Infinity Infinity"),
        ("empty", ""),
        ("only-spaces", "     "),
        ("huge-numbers", "M1e308 1e308 L-1e308 -1e308"),
    ] {
        v.push(case(
            &format!("path-{tag}"),
            format!(r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><path d="{d}" stroke="#0a0" fill="none"/></svg>"##),
            Expect::Survive,
        ));
    }

    // Colour syntax variants, including invalid ones.
    for (tag, fill) in [
        ("named", "rebeccapurple"),
        ("hex3", "#0a0"),
        ("hex6", "#00aa00"),
        ("hex8-with-alpha", "#00aa0080"),
        ("rgb", "rgb(0,170,0)"),
        ("rgb-percent", "rgb(0%,66%,0%)"),
        ("rgba", "rgba(0,170,0,0.5)"),
        ("hsl", "hsl(120,100%,33%)"),
        ("invalid-hex", "#zzzzzz"),
        ("invalid-name", "notacolour"),
        ("empty", ""),
        ("currentcolor", "currentColor"),
        ("url-reference-missing", "url(#nonexistent)"),
    ] {
        v.push(case(
            &format!("fill-{tag}"),
            format!(r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><rect width="64" height="64" fill="{fill}"/></svg>"#),
            Expect::Survive,
        ));
    }

    // Opacity extremes, which feed straight into the alpha un-premultiply path.
    for (tag, op) in [
        ("zero", "0"), ("one", "1"), ("half", "0.5"),
        ("negative", "-1"), ("over-one", "2"), ("nan", "NaN"),
        ("tiny", "0.0000001"), ("almost-one", "0.99999999"),
    ] {
        v.push(case(
            &format!("opacity-{tag}"),
            format!(r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><rect width="64" height="64" fill="#0a0" opacity="{op}"/></svg>"##),
            Expect::Survive,
        ));
    }

    v
}

// =====================================================================
//                    Entity expansion and XXE
// =====================================================================
//
// A thumbnail handler processes files that arrive from anywhere - email
// attachments, downloads, network shares - so any XML feature that can cause
// outbound network traffic or local file access during parsing is a security
// concern, not just a robustness one. MSXML's defaults are safe today; these
// cases exist so that a future change to the parser configuration is caught.

fn entity_cases(probe: &XxeProbe) -> Vec<Case> {
    let url = &probe.http_url;
    let sentinel = &probe.sentinel_path;

    let mut v = vec![
        case(
            "xxe-external-dtd",
            format!(r#"<?xml version="1.0"?><!DOCTYPE svg SYSTEM "{url}/external.dtd"><svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><rect width="10" height="10" fill="red"/></svg>"#),
            Expect::Survive,
        ),
        case(
            "xxe-external-general-entity-http",
            format!(r#"<?xml version="1.0"?><!DOCTYPE svg [<!ENTITY xxe SYSTEM "{url}/entity">]><svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><desc>&xxe;</desc><rect width="10" height="10" fill="red"/></svg>"#),
            Expect::Survive,
        ),
        case(
            "xxe-external-entity-file",
            format!(r#"<?xml version="1.0"?><!DOCTYPE svg [<!ENTITY xxe SYSTEM "file:///{sentinel}">]><svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><desc>&xxe;</desc><rect width="10" height="10" fill="red"/></svg>"#),
            Expect::Survive,
        ),
        case(
            "xxe-parameter-entity",
            format!(r#"<?xml version="1.0"?><!DOCTYPE svg [<!ENTITY % pe SYSTEM "{url}/param.dtd">%pe;]><svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><rect width="10" height="10" fill="red"/></svg>"#),
            Expect::Survive,
        ),
        case(
            "xxe-unc-path",
            r#"<?xml version="1.0"?><!DOCTYPE svg [<!ENTITY xxe SYSTEM "\\127.0.0.1\share\probe.dtd">]><svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><desc>&xxe;</desc><rect width="10" height="10" fill="red"/></svg>"#.to_string(),
            Expect::Survive,
        ),
        case(
            "xxe-entity-in-attribute",
            format!(r#"<?xml version="1.0"?><!DOCTYPE svg [<!ENTITY xxe SYSTEM "{url}/attr">]><svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><rect width="10" height="10" fill="red" data-x="&xxe;"/></svg>"#),
            Expect::Survive,
        ),
        case(
            "xxe-stylesheet-href",
            format!(r#"<?xml version="1.0"?><?xml-stylesheet type="text/css" href="{url}/style.css"?><svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><rect width="10" height="10" fill="red"/></svg>"#),
            Expect::Survive,
        ),
        case(
            "external-image-href",
            format!(r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" viewBox="0 0 10 10"><image xlink:href="{url}/image.png" width="10" height="10"/></svg>"#),
            Expect::Survive,
        ),
    ];

    // Billion laughs: nested internal entities that expand exponentially.
    // Entirely local, so it tests memory bounding rather than network access.
    let mut laughs = String::from(r#"<?xml version="1.0"?><!DOCTYPE svg [<!ENTITY l0 "aaaaaaaaaa">"#);
    for i in 1..=9 {
        laughs.push_str(&format!(
            "<!ENTITY l{i} \"{}\">",
            format!("&l{};", i - 1).repeat(10)
        ));
    }
    laughs.push_str(r#"]><svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><desc>&l9;</desc><rect width="10" height="10" fill="red"/></svg>"#);
    v.push(case("entity-billion-laughs", laughs, Expect::Survive));

    // Quadratic blowup: one large entity referenced many times. Cheaper to
    // detect than billion laughs and often missed by expansion-limit checks.
    let big = "b".repeat(50_000);
    let refs = "&big;".repeat(2_000);
    v.push(case(
        "entity-quadratic-blowup",
        format!(r#"<?xml version="1.0"?><!DOCTYPE svg [<!ENTITY big "{big}">]><svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><desc>{refs}</desc><rect width="10" height="10" fill="red"/></svg>"#),
        Expect::Survive,
    ));

    // Recursive entity definition, which is illegal XML and must be rejected
    // rather than expanded forever.
    v.push(case(
        "entity-recursive",
        r#"<?xml version="1.0"?><!DOCTYPE svg [<!ENTITY a "&b;"><!ENTITY b "&a;">]><svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><desc>&a;</desc></svg>"#,
        Expect::Survive,
    ));

    // Undefined entity reference.
    v.push(case(
        "entity-undefined",
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><desc>&nope;</desc><rect width="10" height="10" fill="red"/></svg>"#,
        Expect::Survive,
    ));

    v
}

// =====================================================================
//                    Size-limit boundary cases
// =====================================================================

fn size_limit_cases() -> Vec<Case> {
    // The provider rejects inputs over 101 MiB (ERROR_FILE_TOO_LARGE). Build
    // padded-but-valid documents that sit just under and just over that line, so
    // both the accept and the reject branch are proven rather than assumed.
    const MAX: usize = 101 * 1024 * 1024;

    let build = |total: usize| -> Vec<u8> {
        let head = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><rect width="64" height="64" fill="#0a0"/><desc>"##;
        let tail = "</desc></svg>";
        let pad = total.saturating_sub(head.len() + tail.len());
        let mut s = Vec::with_capacity(total);
        s.extend_from_slice(head.as_bytes());
        s.extend(std::iter::repeat(b'x').take(pad));
        s.extend_from_slice(tail.as_bytes());
        s
    };

    vec![
        // Comfortably under the cap: must still be processed normally.
        case("size-8mib-under-cap", build(8 * 1024 * 1024), Expect::Survive),
        // One byte under and one byte over the documented boundary.
        case("size-one-byte-under-cap", build(MAX - 1), Expect::Survive),
        case("size-one-byte-over-cap", build(MAX + 1), Expect::RejectOrFallback),
        case("size-well-over-cap", build(MAX + 32 * 1024 * 1024), Expect::RejectOrFallback),
    ]
}

// =====================================================================
//                       Valid reference art
// =====================================================================
//
// These must render for real. They are the control group: if they ever fall
// back, something has broken in the ordinary path, and the "hostile input
// survived" results above become meaningless.

pub fn valid_cases() -> Vec<Case> {
    vec![
        case("valid-basic-shapes", BASE_SVG, Expect::RealRender),
        case(
            "valid-opaque-fill",
            r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><rect width="64" height="64" fill="#2d89ef"/></svg>"##,
            Expect::RealRender,
        ),
        case(
            "valid-semi-transparent",
            r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><rect width="64" height="64" fill="#ffffff" fill-opacity="0.5"/></svg>"##,
            Expect::RealRender,
        ),
        case(
            "valid-linear-gradient",
            r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><defs><linearGradient id="g"><stop offset="0" stop-color="#ff0000"/><stop offset="1" stop-color="#0000ff"/></linearGradient></defs><rect width="64" height="64" fill="url(#g)"/></svg>"##,
            Expect::RealRender,
        ),
        case(
            "valid-transform",
            r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><g transform="translate(32,32) rotate(45)"><rect x="-16" y="-16" width="32" height="32" fill="#0a0"/></g></svg>"##,
            Expect::RealRender,
        ),
        case(
            "valid-polygon",
            r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><polygon points="32,4 60,60 4,60" fill="#e81123"/></svg>"##,
            Expect::RealRender,
        ),
        case(
            "valid-path",
            r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><path d="M8 32 L32 8 L56 32 L32 56 Z" fill="#8764b8"/></svg>"##,
            Expect::RealRender,
        ),
        case(
            "valid-no-viewbox-with-dimensions",
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="64"><rect width="64" height="64" fill="#0a0"/></svg>"##,
            Expect::RealRender,
        ),
        case(
            "valid-wide-aspect",
            r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 512 16"><rect width="512" height="16" fill="#0a0"/></svg>"##,
            Expect::RealRender,
        ),
        case(
            "valid-tall-aspect",
            r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 512"><rect width="16" height="512" fill="#0a0"/></svg>"##,
            Expect::RealRender,
        ),
        case(
            "valid-offset-viewbox-origin",
            r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="100 200 64 64"><rect x="100" y="200" width="64" height="64" fill="#0a0"/></svg>"##,
            Expect::RealRender,
        ),
        case(
            "valid-nested-svg",
            r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><svg x="0" y="0" width="64" height="64" viewBox="0 0 32 32"><rect width="32" height="32" fill="#0a0"/></svg></svg>"##,
            Expect::RealRender,
        ),
    ]
}

/// The full synthetic corpus.
pub fn all_cases(probe: &XxeProbe) -> Vec<Case> {
    let mut v = Vec::new();
    v.extend(valid_cases());
    v.extend(structural_cases());
    v.extend(xml_scale_cases());
    v.extend(css_cases());
    v.extend(numeric_cases());
    v.extend(entity_cases(probe));
    v
}

/// Cases that are large enough to be worth running separately, because they
/// dominate wall-clock time and memory.
pub fn heavy_cases() -> Vec<Case> {
    size_limit_cases()
}

// =====================================================================
//                        .svgz (gzip) corpus
// =====================================================================
//
// Compressed SVG deserves its own suite because the provider's 101 MiB limit
// applies to the *compressed* stream it is handed, not to whatever that expands
// into. The provider also skips its own CSS processing for gzip input and
// forwards the bytes straight to Direct2D, so this is a genuinely different code
// path from plain .svg rather than a re-run of the same one.

fn gzip(data: &[u8], level: flate2::Compression) -> Vec<u8> {
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), level);
    let _ = enc.write_all(data);
    enc.finish().unwrap_or_default()
}

pub fn svgz_cases() -> Vec<Case> {
    let valid = gzip(BASE_SVG.as_bytes(), flate2::Compression::default());
    let mut v = vec![
        case("svgz-valid", valid.clone(), Expect::RealRender),
        case("svgz-valid-stored", gzip(BASE_SVG.as_bytes(), flate2::Compression::none()), Expect::RealRender),
        case("svgz-valid-best", gzip(BASE_SVG.as_bytes(), flate2::Compression::best()), Expect::RealRender),
        // A bare gzip magic number with nothing behind it.
        case("svgz-magic-only", vec![0x1F, 0x8B], Expect::Survive),
        case("svgz-magic-plus-garbage", {
            let mut b = vec![0x1F, 0x8B];
            b.extend_from_slice(b"not actually gzip data at all");
            b
        }, Expect::Survive),
        // Empty gzip member (valid gzip, zero-length payload).
        case("svgz-empty-payload", gzip(b"", flate2::Compression::default()), Expect::Survive),
        // Gzip of something that is not XML.
        case("svgz-of-plain-text", gzip(b"just some text, not svg", flate2::Compression::default()), Expect::Survive),
        // Gzip of a gzip: decompresses to binary, not XML.
        case("svgz-double-compressed", gzip(&valid, flate2::Compression::default()), Expect::Survive),
    ];

    // Header truncation at every byte of the 10-byte gzip header, then body
    // truncation at a spread of offsets. Each leaves the inflater in a
    // different partial state.
    for cut in 1..=9usize {
        if valid.len() > cut {
            v.push(case(&format!("svgz-truncated-header-{cut}"), valid[..cut].to_vec(), Expect::Survive));
        }
    }
    for pct in [20usize, 40, 60, 80, 95] {
        let cut = valid.len() * pct / 100;
        if cut > 10 {
            v.push(case(&format!("svgz-truncated-body-{pct}pct"), valid[..cut].to_vec(), Expect::Survive));
        }
    }
    // Missing only the 8-byte trailer (CRC + size).
    if valid.len() > 8 {
        v.push(case("svgz-missing-trailer", valid[..valid.len() - 8].to_vec(), Expect::Survive));
    }

    // Corrupt the CRC32 in the trailer while leaving everything else intact:
    // the payload inflates cleanly but fails verification.
    if valid.len() > 8 {
        let mut bad_crc = valid.clone();
        let n = bad_crc.len();
        bad_crc[n - 8] ^= 0xFF;
        v.push(case("svgz-bad-crc", bad_crc, Expect::Survive));

        // Corrupt the uncompressed-size field (ISIZE), the last four bytes.
        let mut bad_isize = valid.clone();
        let n = bad_isize.len();
        bad_isize[n - 4] ^= 0xFF;
        bad_isize[n - 3] ^= 0xFF;
        v.push(case("svgz-bad-isize", bad_isize, Expect::Survive));
    }

    // Corrupt a byte in the middle of the DEFLATE stream.
    if valid.len() > 20 {
        let mut bad_body = valid.clone();
        let mid = bad_body.len() / 2;
        bad_body[mid] ^= 0xFF;
        v.push(case("svgz-corrupt-deflate-stream", bad_body, Expect::Survive));
    }

    // Bad header fields: unknown compression method, reserved FLG bits set.
    if valid.len() > 10 {
        let mut bad_cm = valid.clone();
        bad_cm[2] = 0x07; // CM must be 8 (deflate)
        v.push(case("svgz-unknown-compression-method", bad_cm, Expect::Survive));

        let mut bad_flg = valid.clone();
        bad_flg[3] = 0xE0; // reserved bits
        v.push(case("svgz-reserved-flag-bits", bad_flg, Expect::Survive));
    }

    // Concatenated members: legal gzip, and a decoder that stops after the
    // first member silently drops data.
    let mut concatenated = valid.clone();
    concatenated.extend_from_slice(&valid);
    v.push(case("svgz-concatenated-members", concatenated, Expect::Survive));

    // Trailing garbage after a complete member.
    let mut trailing = valid.clone();
    trailing.extend_from_slice(&[0xDEu8, 0xAD, 0xBE, 0xEF].repeat(64));
    v.push(case("svgz-trailing-garbage", trailing, Expect::Survive));

    // Decompression bombs. The compressed form is tiny, so the provider's
    // 101 MiB input check passes trivially and only whatever bounds Direct2D
    // applies stand between this and the runner's memory. The child process runs
    // under a job-object memory cap so a genuine blowup fails the test cleanly
    // instead of taking down the machine.
    for (tag, mb) in [("64mb", 64usize), ("512mb", 512)] {
        // Highly compressible filler inside a valid SVG comment, so the
        // decompressed result is still well-formed XML.
        let mut payload = Vec::with_capacity(mb * 1024 * 1024 + 256);
        payload.extend_from_slice(
            br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><rect width="64" height="64" fill="#0a0"/><!--"##,
        );
        payload.extend(std::iter::repeat(b'A').take(mb * 1024 * 1024));
        payload.extend_from_slice(b"--></svg>");
        v.push(case(
            &format!("svgz-decompression-bomb-{tag}"),
            gzip(&payload, flate2::Compression::best()),
            Expect::Survive,
        ));
    }

    // Deeply nested XML that only appears after decompression, so the nesting is
    // invisible to any check performed on the compressed bytes.
    let mut deep = String::from(r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64">"#);
    deep.push_str(&"<g>".repeat(20_000));
    deep.push_str(r##"<rect width="64" height="64" fill="#0a0"/>"##);
    deep.push_str(&"</g>".repeat(20_000));
    deep.push_str("</svg>");
    v.push(case("svgz-deep-nesting-after-inflate", gzip(deep.as_bytes(), flate2::Compression::best()), Expect::Survive));

    // A gzip whose payload is a huge CSS block: compressed input is small, but
    // the provider skips CSS processing for gzip, so this also pins that the
    // skip actually happens rather than the parser being handed a bomb.
    let css_bomb = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><style>{}</style><rect class="a" width="64" height="64"/></svg>"#,
        ".a{fill:#0a0}".repeat(200_000)
    );
    v.push(case("svgz-css-bomb-after-inflate", gzip(css_bomb.as_bytes(), flate2::Compression::best()), Expect::Survive));

    v
}
