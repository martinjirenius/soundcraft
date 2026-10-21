//! Fade edits keep the playlist sorted, with overlap only between adjacent clips (at a
//! crossfade) and no clip shrunk to nothing. Regression for a proptest-found sequence on the
//! demo session's Bass track: a crossfade range over three whole clips stacked them, and
//! Delete Fades then healed the stack into an unsorted playlist with a one-sample clip.

use serde_json::json;
use soundcraft_engine::Engine;

fn assert_playlist_sane(e: &Engine, step: &str) {
    let bass = e.session().tracks.iter().find(|t| t.name == "Bass").expect("demo has a Bass track");
    let clips = bass.clips();
    for (i, c) in clips.iter().enumerate() {
        assert!(c.length > 1, "{step}: clip {i} shrank to {} samples", c.length);
        if let Some(next) = clips.get(i + 1) {
            assert!(c.start <= next.start, "{step}: playlist not sorted at {i}");
        }
        if let Some(after) = clips.get(i + 2) {
            assert!(c.end() <= after.start, "{step}: clip {i} overlaps beyond its neighbour");
        }
    }
}

fn run(e: &mut Engine, id: &str, a: i64, b: i64) {
    e.execute("edit.select", &json!({"tracks": ["Bass"], "start": a.min(b).max(0), "end": a.max(b).max(0)})).expect("select");
    e.execute(id, &json!({"tracks": ["Bass"], "start": a.min(b), "end": a.max(b)})).expect(id);
}

#[test]
fn crossfade_over_three_clips_then_cut_time_then_delete_fades_stays_sorted() {
    let mut e = soundcraft_engine::demo::demo_engine();
    run(&mut e, "edit.fades_create", 1_772_309, 0);
    assert_playlist_sane(&e, "fades_create");
    run(&mut e, "event.cut_time", 443_078, 0);
    assert_playlist_sane(&e, "cut_time");
    run(&mut e, "edit.fades_delete", 0, 0);
    assert_playlist_sane(&e, "fades_delete");
}
