//! Development aid: opens ShortEnd on a temporary card with the form filled in, optionally
//! checking or running the job, for looking at the views. `cargo run --example demo -- <step>`
//! where step is `form`, `check`, `run`, `history` or `presets`.

use std::io::Write;

use shortend_gui::form::DestRow;
use shortend_gui::{ShortEndApp, View, egui};

fn main() -> eframe::Result {
    let step = std::env::args().nth(1).unwrap_or_else(|| "form".into());
    // Cards from earlier runs (the demo is usually killed, so it can't clean up after itself).
    for e in std::fs::read_dir(std::env::temp_dir())
        .into_iter()
        .flatten()
        .flatten()
    {
        if e.file_name()
            .to_string_lossy()
            .starts_with("shortend-demo-")
        {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
    let tmp = std::env::temp_dir().join(format!("shortend-demo-{}", std::process::id()));
    let card = tmp.join("A001_C004");
    for i in 0..400 {
        let p = card.join(format!("CLIP/A001C{i:03}_240307_R1AB.mxf"));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let mut f = std::fs::File::create(p).unwrap();
        f.write_all(&vec![i as u8; 3_000_000]).unwrap();
    }
    for d in ["RAID", "SHUTTLE_A"] {
        std::fs::create_dir_all(tmp.join(d)).unwrap();
    }

    let mut app = ShortEndApp::new();
    let f = &mut app.form;
    f.source = card.display().to_string();
    f.dests = vec![
        DestRow {
            label: "RAID".into(),
            root: tmp.join("RAID").display().to_string(),
        },
        DestRow {
            label: "Shuttle A".into(),
            root: tmp.join("SHUTTLE_A").display().to_string(),
        },
    ];
    f.folder = "{show}/{date}_D{day}/{camroll}_{seq:3}".into();
    f.show = "Big Show".into();
    f.day = "07".into();
    f.camroll = "A001".into();
    f.operator = "Jane Doe".into();
    f.extra = vec![shortend_core::HashAlgorithm::Md5];
    app.view = match step.as_str() {
        "history" => View::History,
        "presets" => View::Presets,
        _ => View::Offload,
    };
    eframe::run_native(
        "ShortEnd",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default().with_inner_size([980.0, 760.0]),
            ..Default::default()
        },
        Box::new(move |_| {
            Ok(Box::new(Demo {
                app,
                step,
                started: false,
            }))
        }),
    )
}

struct Demo {
    app: ShortEndApp,
    step: String,
    started: bool,
}

impl eframe::App for Demo {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        if !self.started && (self.step == "check" || self.step == "run") {
            self.started = true;
            let spec = self.app.form.to_spec().unwrap();
            let job = shortend_core::plan(spec).unwrap();
            if self.step == "run" {
                self.app.running = Some(shortend_gui::live::Running::start(ctx, job, false));
                self.app.view = View::Progress;
            } else {
                self.app.check =
                    shortend_gui::app::Check::Ready(Box::new(job), self.app.form.clone());
            }
        }
        self.app.ui(ctx);
    }
}
