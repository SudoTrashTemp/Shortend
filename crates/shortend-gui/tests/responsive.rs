//! The window stays responsive (≥30 fps) through a 10,000-file offload during which one
//! destination hangs and is dropped by the stall watchdog — so its 10,000 failures land in
//! the Failures list too. A separate test binary because it sets `SHORTEND_FAULT`.

mod common;

use std::time::{Duration, Instant};

use common::*;
use egui_kittest::kittest::Queryable;
use shortend_core::JobState;
use shortend_gui::app::Check;
use shortend_gui::live::lock;

#[test]
fn stays_responsive_through_10k_files_and_a_stalled_destination() {
    dirs();
    // SAFETY: the only test in this binary; set before the engine starts.
    #[allow(unsafe_code)]
    unsafe {
        std::env::set_var("SHORTEND_FAULT", "hang-dest=1");
    }
    let tmp = tempfile::tempdir().unwrap();
    let card = tmp.path().join("DNG");
    for i in 0..10_000u64 {
        write(
            &card.join(format!("A001_C{:03}/frame_{i:06}.dng", i / 1000)),
            &noise(512, i),
        );
    }
    let (good, hung) = (tmp.path().join("good"), tmp.path().join("hung"));
    std::fs::create_dir_all(&good).unwrap();
    std::fs::create_dir_all(&hung).unwrap();

    let mut h = harness();
    fill_form(h.state_mut(), &card, &[&good, &hung]);
    h.state_mut().form.stall_timeout = 2;
    h.state_mut().form.mhl_v2 = false;
    h.state_mut().form.csv = false;
    h.state_mut().form.txt = false;
    h.run();
    h.get_by_label("Check").click();
    wait_for(&mut h, 120, "the check", |a| {
        matches!(a.check, Check::Ready(..))
    });
    h.get_by_label("Start offload").click();

    // Render continuously, as a visible window would, timing every frame.
    let mut frames: Vec<Duration> = Vec::new();
    let mut ui_time: Vec<Duration> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(600);
    let mut saw_stall = false;
    loop {
        let t = Instant::now();
        h.step();
        frames.push(t.elapsed());
        ui_time.push(h.state().last_frame);
        let app = h.state();
        let live = lock(&app.running.as_ref().unwrap().live);
        saw_stall |= live.dest_failed[1].is_some();
        if live.result.is_some() {
            break;
        }
        drop(live);
        assert!(Instant::now() < deadline, "job did not finish");
        std::thread::sleep(Duration::from_millis(2));
    }
    h.run();
    {
        let app = h.state();
        let live = lock(&app.running.as_ref().unwrap().live);
        let rep = live.result.as_ref().unwrap().as_ref().unwrap();
        assert_eq!(rep.state, JobState::CompletedWithFailures);
        assert!(saw_stall, "the hung destination was reported while running");
        assert_eq!(
            live.failures.len(),
            10_000,
            "every file failed on the hung drive"
        );
    }
    h.get_by_label("FAIL");
    h.get_by_label_contains("Failures (10000)");

    // A few more frames with the full failure list on screen.
    for _ in 0..30 {
        let t = Instant::now();
        h.step();
        frames.push(t.elapsed());
    }
    frames.sort_unstable();
    let p95 = frames[frames.len() * 95 / 100];
    let max = *frames.last().unwrap();
    let ui_max = ui_time.iter().max().copied().unwrap_or_default();
    eprintln!(
        "{} frames: p95 {p95:.1?}, max {max:.1?} (app code max {ui_max:.1?})",
        frames.len()
    );
    assert!(p95 < Duration::from_millis(33), "p95 frame {p95:?}");
    assert!(max < Duration::from_millis(200), "slowest frame {max:?}");
}
