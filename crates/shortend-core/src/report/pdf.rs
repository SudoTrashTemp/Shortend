//! PDF offload report (A4): summary page(s) followed by the full file manifest.
//!
//! Fonts are DejaVu Sans / Sans Bold / Sans Mono, embedded and subset, so the report renders the
//! same everywhere. Characters the fonts don't cover (e.g. CJK) are written as `[U+XXXX]` so a
//! name is never silently altered; the CSV, TXT and MHL keep the exact names.

use printpdf::{
    Color, FontId, Line, LinePoint, Mm, Op, PaintMode, ParsedFont, PdfDocument, PdfFontHandle,
    PdfPage, PdfSaveOptions, Point, Polygon, PolygonRing, Pt, RawImage, Rgb, TextItem,
    WindingOrder, XObjectTransform,
};

use super::{human_bytes, human_duration, human_rate, local_time};
use crate::model::{FileStatus, JobReport, JobState};

const REGULAR: &[u8] = include_bytes!("../../assets/fonts/DejaVuSans.ttf");
const BOLD: &[u8] = include_bytes!("../../assets/fonts/DejaVuSans-Bold.ttf");
const MONO: &[u8] = include_bytes!("../../assets/fonts/DejaVuSansMono.ttf");

const PAGE_W: f32 = 595.28;
const PAGE_H: f32 = 841.89;
const MARGIN: f32 = 40.0;
const FOOTER_H: f32 = 24.0;
const CONTENT_W: f32 = PAGE_W - 2.0 * MARGIN;

#[derive(Clone, Copy)]
enum Face {
    Regular,
    Bold,
    Mono,
}

struct Font {
    id: FontId,
    parsed: ParsedFont,
}

impl Font {
    fn load(doc: &mut PdfDocument, bytes: &[u8]) -> Result<Self, String> {
        let parsed = ParsedFont::from_bytes(bytes, 0, &mut Vec::new())
            .ok_or_else(|| "embedded font failed to parse".to_owned())?;
        Ok(Self {
            id: doc.add_font(&parsed),
            parsed,
        })
    }

    fn has(&self, c: char) -> bool {
        self.parsed.lookup_glyph_index(c as u32).is_some()
    }

    fn width(&self, s: &str, size: f32) -> f32 {
        let upem = f32::from(self.parsed.units_per_em.max(1));
        s.chars()
            .map(|c| {
                self.parsed
                    .lookup_glyph_index(c as u32)
                    .and_then(|g| self.parsed.get_glyph_width(g))
                    .map_or(f32::from(self.parsed.units_per_em) * 0.6, f32::from)
            })
            .sum::<f32>()
            * size
            / upem
    }
}

struct Writer {
    doc: PdfDocument,
    regular: Font,
    bold: Font,
    mono: Font,
    /// Characters drawn with each face (regular, bold, mono), for subsetting.
    used: [std::collections::BTreeSet<char>; 3],
    pages: Vec<Vec<Op>>,
    ops: Vec<Op>,
    /// Distance of the next line's top from the top of the page.
    y: f32,
}

const BLACK: (f32, f32, f32) = (0.1, 0.1, 0.1);
const GREY: (f32, f32, f32) = (0.42, 0.42, 0.45);
const GREEN: (f32, f32, f32) = (0.10, 0.50, 0.25);
const RED: (f32, f32, f32) = (0.75, 0.12, 0.12);
const RULE: (f32, f32, f32) = (0.80, 0.80, 0.82);

fn rgb((r, g, b): (f32, f32, f32)) -> Color {
    Color::Rgb(Rgb::new(r, g, b, None))
}

fn pt(x: f32, y_from_top: f32) -> Point {
    Point {
        x: Pt(x),
        y: Pt(PAGE_H - y_from_top),
    }
}

impl Writer {
    fn new(title: &str, fonts: &[Vec<u8>; 3]) -> Result<Self, String> {
        let mut doc = PdfDocument::new(title);
        let regular = Font::load(&mut doc, &fonts[0])?;
        let bold = Font::load(&mut doc, &fonts[1])?;
        let mono = Font::load(&mut doc, &fonts[2])?;
        Ok(Self {
            doc,
            regular,
            bold,
            mono,
            used: Default::default(),
            pages: Vec::new(),
            ops: Vec::new(),
            y: MARGIN,
        })
    }

    fn font(&self, f: Face) -> &Font {
        match f {
            Face::Regular => &self.regular,
            Face::Bold => &self.bold,
            Face::Mono => &self.mono,
        }
    }

    /// Replaces characters the font cannot draw.
    fn printable(&self, f: Face, s: &str) -> String {
        let font = self.font(f);
        let mut out = String::with_capacity(s.len());
        for c in s.chars() {
            if c.is_control() {
                out.push_str(&format!("[U+{:04X}]", c as u32));
            } else if font.has(c) {
                out.push(c);
            } else {
                out.push_str(&format!("[U+{:04X}]", c as u32));
            }
        }
        out
    }

    fn width(&self, f: Face, s: &str, size: f32) -> f32 {
        self.font(f).width(s, size)
    }

    /// Draws `s` with its baseline `size` below the current line top.
    fn text_at(&mut self, x: f32, f: Face, size: f32, color: (f32, f32, f32), s: &str) {
        let s = self.printable(f, s);
        self.used[f as usize].extend(s.chars());
        let id = self.font(f).id.clone();
        self.ops.extend([
            Op::StartTextSection,
            Op::SetFont {
                font: PdfFontHandle::External(id),
                size: Pt(size),
            },
            Op::SetFillColor { col: rgb(color) },
            Op::SetTextCursor {
                pos: pt(x, self.y + size),
            },
            Op::ShowText {
                items: vec![TextItem::Text(s)],
            },
            Op::EndTextSection,
        ]);
    }

    /// Splits `s` into lines no wider than `width`, breaking anywhere (paths have no spaces
    /// to rely on). Nothing is dropped.
    fn wrap(&self, f: Face, size: f32, s: &str, width: f32) -> Vec<String> {
        let s = self.printable(f, s);
        let mut lines = Vec::new();
        let mut cur = String::new();
        for c in s.chars() {
            let mut next = cur.clone();
            next.push(c);
            if !cur.is_empty() && self.width(f, &next, size) > width {
                lines.push(std::mem::take(&mut cur));
                cur.push(c);
            } else {
                cur = next;
            }
        }
        if !cur.is_empty() || lines.is_empty() {
            lines.push(cur);
        }
        lines
    }

    fn ensure(&mut self, height: f32) {
        if self.y + height > PAGE_H - MARGIN - FOOTER_H {
            self.new_page();
        }
    }

    fn new_page(&mut self) {
        let ops = std::mem::take(&mut self.ops);
        self.pages.push(ops);
        self.y = MARGIN;
    }

    fn rule(&mut self) {
        let y = PAGE_H - self.y;
        self.ops.extend([
            Op::SetOutlineColor { col: rgb(RULE) },
            Op::SetOutlineThickness { pt: Pt(0.6) },
            Op::DrawLine {
                line: Line {
                    points: vec![
                        LinePoint {
                            p: Point {
                                x: Pt(MARGIN),
                                y: Pt(y),
                            },
                            bezier: false,
                        },
                        LinePoint {
                            p: Point {
                                x: Pt(PAGE_W - MARGIN),
                                y: Pt(y),
                            },
                            bezier: false,
                        },
                    ],
                    is_closed: false,
                },
            },
        ]);
    }

    fn rect(&mut self, x: f32, w: f32, h: f32, color: (f32, f32, f32)) {
        let (top, bottom) = (PAGE_H - self.y, PAGE_H - self.y - h);
        let p = |x: f32, y: f32| LinePoint {
            p: Point { x: Pt(x), y: Pt(y) },
            bezier: false,
        };
        self.ops.extend([
            Op::SetFillColor { col: rgb(color) },
            Op::DrawPolygon {
                polygon: Polygon {
                    rings: vec![PolygonRing {
                        points: vec![p(x, top), p(x + w, top), p(x + w, bottom), p(x, bottom)],
                    }],
                    mode: PaintMode::Fill,
                    winding_order: WindingOrder::NonZero,
                },
            },
        ]);
    }

    fn heading(&mut self, s: &str) {
        self.ensure(30.0);
        self.y += 8.0;
        self.text_at(MARGIN, Face::Bold, 11.0, BLACK, s);
        self.y += 16.0;
        self.rule();
        self.y += 5.0;
    }

    /// `label: value` rows; long values wrap.
    fn fields(&mut self, rows: &[(&str, String)]) {
        let label_w = 92.0;
        for (k, v) in rows {
            let lines = self.wrap(Face::Regular, 8.5, v, CONTENT_W - label_w);
            self.ensure(12.0 * lines.len() as f32);
            self.text_at(MARGIN, Face::Regular, 8.5, GREY, k);
            for (i, l) in lines.iter().enumerate() {
                if i > 0 {
                    self.y += 11.0;
                }
                self.text_at(MARGIN + label_w, Face::Regular, 8.5, BLACK, l);
            }
            self.y += 12.0;
        }
    }

    /// A paragraph of wrapped lines.
    fn para(&mut self, f: Face, size: f32, color: (f32, f32, f32), indent: f32, s: &str) {
        for l in self.wrap(f, size, s, CONTENT_W - indent) {
            self.ensure(size + 3.0);
            self.text_at(MARGIN + indent, f, size, color, &l);
            self.y += size + 3.0;
        }
    }

    fn finish(mut self, footer: &str) -> (Vec<u8>, Used) {
        if !self.ops.is_empty() || self.pages.is_empty() {
            self.new_page();
        }
        let total = self.pages.len();
        let pages: Vec<PdfPage> = std::mem::take(&mut self.pages)
            .into_iter()
            .enumerate()
            .map(|(i, mut ops)| {
                let s = self.printable(
                    Face::Regular,
                    &format!("{footer}    page {} of {total}", i + 1),
                );
                self.used[0].extend(s.chars());
                ops.extend([
                    Op::StartTextSection,
                    Op::SetFont {
                        font: PdfFontHandle::External(self.regular.id.clone()),
                        size: Pt(7.0),
                    },
                    Op::SetFillColor { col: rgb(GREY) },
                    Op::SetTextCursor {
                        pos: Point {
                            x: Pt(MARGIN),
                            y: Pt(MARGIN - 10.0),
                        },
                    },
                    Op::ShowText {
                        items: vec![TextItem::Text(s)],
                    },
                    Op::EndTextSection,
                ]);
                PdfPage::new(Mm(210.0), Mm(297.0), ops)
            })
            .collect();
        // Fonts are subset by `render` itself (printpdf only subsets with its layout engine).
        let opts = PdfSaveOptions {
            subset_fonts: false,
            ..Default::default()
        };
        let bytes = self.doc.with_pages(pages).save(&opts, &mut Vec::new());
        (bytes, self.used)
    }
}

/// Reduces a TrueType font to the glyphs for `chars` (plus .notdef), keeping a Unicode cmap,
/// so the embedded font costs kilobytes instead of hundreds of kilobytes.
fn subset_font(bytes: &[u8], chars: &std::collections::BTreeSet<char>) -> Result<Vec<u8>, String> {
    use allsorts::binary::read::ReadScope;
    use allsorts::font_data::FontData;
    use allsorts::subset::{CmapTarget, SubsetProfile, subset};

    let full = ParsedFont::from_bytes(bytes, 0, &mut Vec::new())
        .ok_or_else(|| "font failed to parse".to_owned())?;
    let mut ids: Vec<u16> = chars
        .iter()
        .filter_map(|c| full.lookup_glyph_index(*c as u32))
        .filter(|g| *g != 0)
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids.insert(0, 0);
    let scope = ReadScope::new(bytes);
    let data = scope.read::<FontData<'_>>().map_err(|e| e.to_string())?;
    let provider = data.table_provider(0).map_err(|e| e.to_string())?;
    subset(&provider, &ids, &SubsetProfile::Pdf, CmapTarget::Unicode).map_err(|e| e.to_string())
}

/// Renders the report. Returns the PDF bytes and any warnings (e.g. an unreadable logo).
///
/// Two passes: the first records which characters each face draws, the second renders with
/// fonts subset to exactly those characters (glyph widths are unchanged, so is the layout).
pub fn render(rep: &JobReport) -> Result<(Vec<u8>, Vec<String>), String> {
    let full = [REGULAR.to_vec(), BOLD.to_vec(), MONO.to_vec()];
    let (_, used, _) = render_with(rep, &full)?;
    let subset: Result<Vec<Vec<u8>>, String> = full
        .iter()
        .zip(&used)
        .map(|(f, u)| subset_font(f, u))
        .collect();
    let fonts = match subset {
        Ok(v) => [v[0].clone(), v[1].clone(), v[2].clone()],
        Err(_) => full, // never fail a report over subsetting; embed the full fonts
    };
    let (bytes, _, warnings) = render_with(rep, &fonts)?;
    Ok((bytes, warnings))
}

type Used = [std::collections::BTreeSet<char>; 3];

fn render_with(
    rep: &JobReport,
    fonts: &[Vec<u8>; 3],
) -> Result<(Vec<u8>, Used, Vec<String>), String> {
    let job = &rep.job;
    let mut warnings = Vec::new();
    let mut w = Writer::new(&format!("ShortEnd offload report — {}", job.folder), fonts)?;

    // --- header: logo, title, verdict -----------------------------------------------------
    let mut header_h: f32 = 34.0;
    if let Some(logo) = job
        .settings
        .reports
        .pdf
        .as_ref()
        .and_then(|p| p.logo.as_ref())
    {
        match std::fs::read(logo)
            .map_err(|e| e.to_string())
            .and_then(|b| RawImage::decode_from_bytes(&b, &mut Vec::new()))
        {
            Ok(img) => {
                let (max_w, max_h) = (150.0_f32, 48.0_f32);
                let (pw, ph) = (img.width.max(1) as f32, img.height.max(1) as f32);
                let scale = (max_w / pw).min(max_h / ph);
                let (dw, dh) = (pw * scale, ph * scale);
                let id = w.doc.add_image(&img);
                w.ops.push(Op::UseXobject {
                    id,
                    transform: XObjectTransform {
                        translate_x: Some(Pt(PAGE_W - MARGIN - dw)),
                        translate_y: Some(Pt(PAGE_H - MARGIN - dh)),
                        dpi: Some(72.0 / scale),
                        ..Default::default()
                    },
                });
                header_h = header_h.max(dh);
            }
            Err(e) => warnings.push(format!("PDF logo {} not used: {e}", logo.display())),
        }
    }
    w.text_at(MARGIN, Face::Bold, 18.0, BLACK, "Offload Report");
    w.y += 24.0;
    let card = format!("{}  →  {}", job.source.label, job.folder);
    w.text_at(MARGIN, Face::Regular, 10.0, GREY, &card);
    w.y = MARGIN + header_h + 10.0;

    let n_files = job.files.len();
    let n_dest = job.destinations.len();
    let failed = rep.failed_files();
    let (verdict, color) = match rep.state {
        JobState::Completed => (
            format!(
                "PASS — {n_files} files verified on {}",
                if n_dest == 1 {
                    "the destination".to_owned()
                } else {
                    format!("all {n_dest} destinations")
                }
            ),
            GREEN,
        ),
        _ => (
            format!(
                "FAIL — {failed} of {n_files} files not verified on every destination ({})",
                rep.state
            ),
            RED,
        ),
    };
    w.rect(MARGIN, CONTENT_W, 24.0, color);
    w.y += 7.0;
    w.text_at(MARGIN + 10.0, Face::Bold, 11.0, (1.0, 1.0, 1.0), &verdict);
    w.y += 24.0;

    // --- production & job -----------------------------------------------------------------
    let production: Vec<(&str, String)> = job
        .production
        .fields()
        .into_iter()
        .map(|(k, v)| (k, v.to_owned()))
        .collect();
    if !production.is_empty() {
        w.heading("Production");
        w.fields(&production);
    }
    w.heading("Offload");
    let mut verification = "read-back from each destination".to_owned();
    if job.settings.source_reread {
        verification.push_str(" + second source read");
    }
    w.fields(&[
        ("Job", job.id.to_string()),
        (
            "Source",
            format!(
                "{} ({})",
                job.source.root.display(),
                job.source.volume.fs_type
            ),
        ),
        ("Started", local_time(rep.started)),
        ("Finished", local_time(rep.finished)),
        ("Duration", human_duration(rep.elapsed())),
        (
            "Files",
            format!("{n_files} ({})", human_bytes(job.total_bytes())),
        ),
        ("Checksums", job.settings.hashes.to_string()),
        ("Verification", verification),
        (
            "Manifest",
            job.settings
                .reports
                .mhl
                .map_or_else(|| "none".to_owned(), |v| v.to_string()),
        ),
    ]);

    // --- destinations ------------------------------------------------------------------------
    w.heading("Destinations");
    for (d, st) in job.destinations.iter().zip(&rep.dest_stats) {
        let ok = rep
            .outcomes
            .iter()
            .filter(|o| o.dests[d.id.index()].status.is_ok())
            .count();
        let good = ok == n_files && st.failure.is_none();
        w.ensure(40.0);
        w.text_at(
            MARGIN,
            Face::Bold,
            9.0,
            if good { GREEN } else { RED },
            &format!("{}  {}", if good { "✓" } else { "✗" }, d.label),
        );
        w.y += 13.0;
        w.para(
            Face::Regular,
            8.0,
            BLACK,
            14.0,
            &format!("{}  ({})", d.target.display(), d.volume.fs_type),
        );
        let mut line = format!(
            "{ok}/{n_files} files verified · {} written at {} · {} read back",
            human_bytes(st.bytes_written),
            human_rate(st.bytes_written, rep.elapsed()),
            human_bytes(st.bytes_verified)
        );
        if let Some(f) = &st.failure {
            line.push_str(&format!(" · FAILED: {f}"));
        }
        w.para(Face::Regular, 8.0, GREY, 14.0, &line);
        if let Some(m) = rep.manifests.iter().find(|m| m.starts_with(&d.target)) {
            w.para(
                Face::Regular,
                8.0,
                GREY,
                14.0,
                &format!("manifest: {}", m.display()),
            );
        }
        w.y += 4.0;
    }

    // --- problems ----------------------------------------------------------------------------
    if !rep.manifest_issues.is_empty() {
        w.heading("Card vs. its own ASC MHL");
        for m in &rep.manifest_issues {
            w.para(Face::Regular, 8.0, RED, 0.0, m);
        }
    }
    if failed > 0 {
        w.heading("Failures");
        for o in rep.outcomes.iter().filter(|o| !o.all_ok()) {
            for d in o.dests.iter().filter(|d| !d.status.is_ok()) {
                let label = &job.destinations[d.dest.index()].label;
                let why = match &d.status {
                    FileStatus::Failed(f) => f.to_string(),
                    _ => "not copied".to_owned(),
                };
                w.para(
                    Face::Regular,
                    8.0,
                    RED,
                    0.0,
                    &format!("{}  [{label}]  {why}", o.entry.rel),
                );
            }
        }
    }
    if !rep.warnings.is_empty() {
        w.heading("Warnings");
        for x in &rep.warnings {
            w.para(Face::Regular, 8.0, GREY, 0.0, x);
        }
    }

    // --- manifest table ----------------------------------------------------------------------
    w.new_page();
    let primary = job.settings.hashes.primary();
    w.heading(&format!(
        "Files ({n_files}) — size, {primary} checksum, status per destination"
    ));
    let size_w = 62.0;
    let sum_w = w.width(Face::Mono, &"0".repeat(32), 6.5) + 6.0;
    let status_w = (n_dest as f32 * 9.0).max(18.0);
    let path_w = CONTENT_W - size_w - sum_w - status_w - 8.0;
    for o in &rep.outcomes {
        let path_lines = w.wrap(Face::Regular, 7.5, o.entry.rel.as_str(), path_w);
        let sum = o
            .source
            .as_ref()
            .map_or_else(|| "—".to_owned(), |d| d.primary().value());
        let sum_lines = w.wrap(Face::Mono, 6.5, &sum, sum_w - 6.0);
        let rows = path_lines.len().max(sum_lines.len());
        w.ensure(10.0 * rows as f32);
        let top = w.y;
        for (i, l) in path_lines.iter().enumerate() {
            w.y = top + 10.0 * i as f32;
            w.text_at(MARGIN, Face::Regular, 7.5, BLACK, l);
        }
        w.y = top;
        let size = o.entry.size.to_string();
        let sx = MARGIN + path_w + 4.0 + size_w - w.width(Face::Regular, &size, 7.5) - 6.0;
        w.text_at(sx, Face::Regular, 7.5, BLACK, &size);
        for (i, l) in sum_lines.iter().enumerate() {
            w.y = top + 10.0 * i as f32 + 0.8;
            w.text_at(MARGIN + path_w + 4.0 + size_w, Face::Mono, 6.5, GREY, l);
        }
        w.y = top;
        for (k, d) in o.dests.iter().enumerate() {
            let (mark, col) = if d.status.is_ok() {
                ("✓", GREEN)
            } else {
                ("✗", RED)
            };
            let x = MARGIN + CONTENT_W - status_w + 9.0 * k as f32;
            w.text_at(x, Face::Bold, 7.5, col, mark);
        }
        w.y = top + 10.0 * rows as f32;
    }
    w.y += 6.0;
    w.para(
        Face::Regular,
        7.0,
        GREY,
        0.0,
        &format!(
            "Status columns: {}. Characters the report font cannot draw are shown as [U+XXXX]; the CSV, TXT and manifest files keep exact names and every checksum.",
            job.destinations
                .iter()
                .enumerate()
                .map(|(i, d)| format!("{} = {}", i + 1, d.label))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    );

    let footer = format!(
        "ShortEnd {} · {} · job {}",
        env!("CARGO_PKG_VERSION"),
        job.folder,
        &job.id.to_string()[..8]
    );
    let (bytes, used) = w.finish(&footer);
    Ok((bytes, used, warnings))
}
