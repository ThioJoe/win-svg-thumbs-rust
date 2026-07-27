//! Breadth over a real-world SVG corpus.
//!
//! Synthetic cases probe the edges deliberately; a real corpus finds the things
//! nobody thought to write a case for. Production icon sets carry the artefacts
//! of a dozen different authoring tools: unusual attribute orderings, generated
//! class names, `<defs>` blocks, clip paths, masks, tool-specific metadata
//! namespaces and inconsistent whitespace.
//!
//! The corpus is fetched by the workflow (pinned to a commit and cached), not
//! vendored, so the repository stays free of third-party assets. If it is
//! absent the suite skips rather than fails - a missing corpus is an
//! environment problem, not a defect in the DLL.
//!
//! Every file is also round-tripped through gzip and re-rendered, which gives
//! the .svgz path the same breadth for free and asserts the two paths agree.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::dll::{self, Dll, Rendering};
use crate::metrics::Snapshot;
use crate::report::Report;

/// Per-file budget. Real icons are small; anything taking longer than this is a
/// pathological input worth naming individually.
const PER_FILE_BUDGET: Duration = Duration::from_secs(20);

fn collect(dir: &Path, limit: usize) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>, limit: usize) {
        if out.len() >= limit {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        let mut entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
        // Sorted so a given corpus always produces the same selection, which
        // keeps failures reproducible between runs.
        entries.sort_by_key(|e| e.path());
        for entry in entries {
            if out.len() >= limit {
                return;
            }
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out, limit);
            } else if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("svg")) {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, &mut out, limit);
    out
}

pub fn run(dll_handle: &Dll, report: &mut Report, corpus_dir: &Path, limit: usize) {
    if !corpus_dir.is_dir() {
        report.skip(
            "corpus_available",
            format!("corpus directory {} does not exist", corpus_dir.display()),
        );
        return;
    }

    let files = collect(corpus_dir, limit);
    if files.is_empty() {
        report.skip("corpus_available", format!("no .svg files under {}", corpus_dir.display()));
        return;
    }
    report.pass("corpus_available", format!("{} files from {}", files.len(), corpus_dir.display()));

    // Sizes rotate per file so the whole corpus is not rendered at one size,
    // without multiplying the run time by a full sweep.
    const SIZES: [u32; 6] = [16, 32, 48, 96, 256, 1024];

    let before = Snapshot::take();
    let mut rendered = 0u32;
    let mut fell_back = Vec::new();
    let mut failed = Vec::new();
    let mut slow = Vec::new();
    let mut geometry_bad = Vec::new();
    let mut gz_disagreed = Vec::new();
    let mut worst = (Duration::ZERO, String::new());

    for (i, path) in files.iter().enumerate() {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        report.begin_case(&name);

        let Ok(bytes) = std::fs::read(path) else {
            failed.push(format!("{name}: could not read file"));
            continue;
        };
        let size = SIZES[i % SIZES.len()];

        let start = Instant::now();
        let result = dll::try_render(dll_handle, &bytes, size);
        let elapsed = start.elapsed();
        if elapsed > worst.0 {
            worst = (elapsed, name.clone());
        }
        if elapsed > PER_FILE_BUDGET {
            slow.push(format!("{name}: {elapsed:?} at {size}px"));
        }

        match result {
            Ok(thumb) => {
                rendered += 1;
                if thumb.width != size || thumb.height != size {
                    geometry_bad.push(format!(
                        "{name}: asked for {size}x{size}, got {}x{}",
                        thumb.width, thumb.height
                    ));
                }
                match thumb.classify() {
                    // Every file in a curated production icon set is valid SVG,
                    // so a fallback here means the provider could not handle
                    // something real-world - the most valuable signal in this
                    // whole suite.
                    r if r.is_fallback() => fell_back.push(format!("{name} ({r:?}) at {size}px")),
                    _ => {}
                }

                // Same file, gzipped: the .svgz path skips CSS processing
                // entirely, so agreement between the two is a real property
                // rather than a tautology.
                if i % 5 == 0 {
                    let mut enc = flate2::write::GzEncoder::new(
                        Vec::new(),
                        flate2::Compression::default(),
                    );
                    let _ = enc.write_all(&bytes);
                    if let Ok(gz) = enc.finish() {
                        match dll::try_render(dll_handle, &gz, size) {
                            Ok(gz_thumb) => {
                                // Compare classification rather than exact
                                // pixels: CSS rewriting legitimately changes the
                                // document the renderer sees, so identical
                                // output is not guaranteed for files that carry
                                // a <style> block.
                                let plain_real = thumb.classify() == Rendering::Real;
                                let gz_real = gz_thumb.classify() == Rendering::Real;
                                if plain_real != gz_real {
                                    gz_disagreed.push(format!(
                                        "{name}: svg={:?} svgz={:?}",
                                        thumb.classify(),
                                        gz_thumb.classify()
                                    ));
                                }
                            }
                            Err(hr) => gz_disagreed.push(format!(
                                "{name}: svg rendered but svgz failed with 0x{:08X}",
                                hr.0 as u32
                            )),
                        }
                    }
                }
            }
            Err(hr) => failed.push(format!("{name}: hr=0x{:08X} at {size}px", hr.0 as u32)),
        }
    }

    let d = Snapshot::take().delta(&before);

    // ---- Verdicts ----

    report.check(
        "every_real_world_file_renders",
        failed.is_empty(),
        if failed.is_empty() {
            format!("all {} corpus files produced a bitmap", files.len())
        } else {
            format!(
                "{} of {} corpus files failed to render:\n      {}",
                failed.len(),
                files.len(),
                sample(&failed)
            )
        },
    );

    report.check(
        "no_real_world_file_falls_back",
        fell_back.is_empty(),
        if fell_back.is_empty() {
            format!("none of the {rendered} rendered files produced a fallback thumbnail")
        } else {
            format!(
                "{} of {rendered} files fell back to the placeholder thumbnail, meaning the \
                 provider could not render valid production artwork:\n      {}",
                fell_back.len(),
                sample(&fell_back)
            )
        },
    );

    report.check(
        "corpus_geometry_always_matches_request",
        geometry_bad.is_empty(),
        if geometry_bad.is_empty() {
            "every returned bitmap matched the requested size".to_string()
        } else {
            format!("{} size mismatches:\n      {}", geometry_bad.len(), sample(&geometry_bad))
        },
    );

    report.check(
        "corpus_files_render_within_budget",
        slow.is_empty(),
        if slow.is_empty() {
            format!("slowest file was {} at {:?}", worst.1, worst.0)
        } else {
            format!(
                "{} file(s) exceeded the {PER_FILE_BUDGET:?} budget:\n      {}",
                slow.len(),
                sample(&slow)
            )
        },
    );

    report.check(
        "svg_and_svgz_agree_on_real_corpus",
        gz_disagreed.is_empty(),
        if gz_disagreed.is_empty() {
            "gzip round-trips of the sampled corpus files rendered equivalently".to_string()
        } else {
            format!(
                "{} file(s) rendered differently as .svgz than as .svg:\n      {}",
                gz_disagreed.len(),
                sample(&gz_disagreed)
            )
        },
    );

    report.check(
        "corpus_run_does_not_leak_handles",
        d.gdi_objects <= 32 && d.handles <= 128,
        format!("across the whole corpus run: {}", d.describe()),
    );
}

/// Caps a failure list so one systemic problem cannot flood the CI log, while
/// still making clear how much was truncated.
fn sample(items: &[String]) -> String {
    const MAX: usize = 12;
    if items.len() <= MAX {
        items.join("\n      ")
    } else {
        format!(
            "{}\n      ... and {} more",
            items[..MAX].join("\n      "),
            items.len() - MAX
        )
    }
}
