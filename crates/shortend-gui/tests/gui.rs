//! The GUI driven through its widgets (egui_kittest + AccessKit): check, start, cancel and
//! resume an offload; inline preflight refusals; preset editing; no idle repaint.

mod common;

use common::*;
use egui_kittest::kittest::Queryable;
use shortend_core::JobState;
use shortend_gui::View;
use shortend_gui::app::Check;
use shortend_gui::live::lock;

fn result_state(app: &shortend_gui::ShortEndApp) -> Option<JobState> {
    let r = app.running.as_ref()?;
    let l = lock(&r.live);
    l.result
        .as_ref()
        .map(|r| r.as_ref().expect("engine result").state)
}

#[test]
fn idle_window_does_not_repaint() {
    let mut h = harness();
    let steps = h.run();
    assert!(steps <= 3, "{steps} frames without input");
    for tab in ["Presets", "Offload"] {
        h.get_by_label(tab).click();
        let steps = h.run();
        assert!(steps <= 4, "{tab}: {steps} frames after one click");
    }
    assert!(
        !h.ctx.has_requested_repaint(),
        "no repaint pending while idle"
    );
}

#[test]
fn offload_cancel_and_resume_from_the_window() {
    let tmp = tempfile::tempdir().unwrap();
    let card = tmp.path().join("A001");
    for i in 0..60 {
        write(
            &card.join(format!("CLIP/C{i:03}.mov")),
            &noise(2_000_000, i),
        );
    }
    write(&card.join("Ünïcödé.wav"), &noise(5_000, 99));
    let (d1, d2) = (tmp.path().join("raid"), tmp.path().join("shuttle"));
    std::fs::create_dir_all(&d1).unwrap();
    std::fs::create_dir_all(&d2).unwrap();

    let mut h = harness();
    fill_form(h.state_mut(), &card, &[&d1, &d2]);
    h.state_mut().form.folder = "{card}_{seq:2}".into();
    h.state_mut().form.parallel = 1;
    h.run();
    h.get_by_label("Check").click();
    wait_for(&mut h, 30, "the check", |a| {
        matches!(a.check, Check::Ready(..))
    });
    h.get_by_label("Ready to offload");
    assert_eq!(
        h.get_all_by_label_contains("A001_01").count(),
        2,
        "both destinations"
    );

    h.get_by_label("Start offload").click();
    h.step();
    assert_eq!(h.state().view, View::Progress);
    wait_for(&mut h, 30, "copying to start", |a| {
        lock(&a.running.as_ref().unwrap().live).files_done > 0
    });
    h.get_by_label("Cancel").click();
    wait_for(&mut h, 30, "the cancel", |a| result_state(a).is_some());
    assert_eq!(result_state(h.state()), Some(JobState::Cancelled));
    h.get_by_label("Cancelled");

    h.get_by_label("History").click();
    wait_for(&mut h, 30, "history", |a| {
        a.jobs.as_ref().is_ok_and(|j| !j.is_empty())
    });
    h.get_by_label("Resume").click();
    h.step();
    assert_eq!(h.state().view, View::Progress);
    wait_for(&mut h, 60, "the resume", |a| result_state(a).is_some());
    assert_eq!(result_state(h.state()), Some(JobState::Completed));
    h.get_by_label("PASS");
    h.get_by_label_contains("already verified earlier");
    for d in [&d1, &d2] {
        assert_eq!(tree(&d.join("A001_01")), tree(&card));
    }
    assert!(h.run() <= 4, "idle again after the job");
}

#[test]
fn preflight_refusal_is_shown_inline_and_blocks_start() {
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("out");
    std::fs::create_dir(&dest).unwrap();
    let mut h = harness();
    fill_form(h.state_mut(), &tmp.path().join("missing-card"), &[&dest]);
    h.run();
    h.get_by_label("Check").click();
    wait_for(&mut h, 30, "the check", |a| {
        matches!(a.check, Check::Refused(..))
    });
    h.get_by_label_contains("Refused");
    assert!(h.get_all_by_label_contains("missing-card").count() >= 1);
    h.get_by_label("Start offload").click();
    h.run();
    assert!(h.state().running.is_none(), "start stays disabled");

    // Editing the form clears the old result; an empty value in the template is caught.
    let card = tmp.path().join("CARD");
    write(&card.join("a.mov"), b"x");
    h.state_mut().form.source = card.display().to_string();
    h.state_mut().form.folder = "{show}/{card}".into();
    h.run();
    assert!(matches!(h.state().check, Check::None));
    h.get_by_label_contains("{show}, which is not set");
    h.get_by_label("Check").click();
    wait_for(&mut h, 30, "the check", |a| {
        matches!(a.check, Check::Refused(..))
    });
    h.get_by_label_contains("-  the folder template uses {show}");
}

#[test]
fn presets_are_validated_saved_and_applied() {
    let mut h = harness();
    h.state_mut().form.show = "Big Show".into();
    h.state_mut().form.folder = "{show}/{camrol}_{seq:3}".into();
    h.state_mut().form.extra = vec![shortend_core::HashAlgorithm::Md5];
    h.get_by_label("Presets").click();
    h.run();
    h.get_by_label("New from Offload form").click();
    h.run();
    h.get_by_label("Valid preset");
    assert!(
        h.state().editor.text.contains("{camroll}"),
        "canonical form"
    );
    h.state_mut().editor.name = "bigshow".into();
    h.run();
    h.get_by_label("Save").click();
    h.run();
    let saved = dirs().join("config/presets/bigshow.toml");
    let text = std::fs::read_to_string(&saved).unwrap();
    assert!(text.contains("md5") && text.contains("Big Show"), "{text}");
    h.get_by_label("bigshow"); // listed

    // Invalid text: the reason is shown and Save does nothing.
    h.state_mut().editor.text = "format_version = 1\nbogus = 1\n".into();
    h.state_mut().editor.status = None;
    h.run();
    h.get_by_label_contains("unknown field `bogus`");
    h.get_by_label("Save").click();
    h.run();
    assert_eq!(
        std::fs::read_to_string(&saved).unwrap(),
        text,
        "not overwritten"
    );

    // Applying a preset fills the Offload form.
    h.state_mut().editor.text = text;
    h.state_mut().editor.status = None;
    h.state_mut().form = Default::default();
    h.run();
    h.get_by_label("Use in Offload").click();
    h.run();
    let app = h.state();
    assert_eq!(app.view, View::Offload);
    assert_eq!(app.form.show, "Big Show");
    assert_eq!(app.form.folder, "{show}/{camroll}_{seq:3}");
    assert_eq!(app.form.extra, [shortend_core::HashAlgorithm::Md5]);
}
