//! The web app decodes the serialized watched field on its own
//! (apps/web/src/common/watchedBitfield.js, for the Cache window's per-episode
//! view), so its decoder is checked against strings THIS crate produced, not
//! against itself. The fixture lives with the web tests
//! (apps/web/tests/fixtures/watchedBitfield.json) and both suites read it:
//! here, that the core decodes every case to the stated answer; there, that
//! the JS decoder gives the same answer.
//!
//! Regenerate the fixture (only when adding cases) with
//! `cargo test -p rillio-watched-bitfield --test js_fixtures -- --ignored --nocapture`
//! and paste the printed JSON over the file.

use rillio_watched_bitfield::{WatchedBitField, WatchedField};
use serde_json::{json, Value};

fn ids(prefix: &str, season: u32, count: u32) -> Vec<String> {
    (1..=count).map(|e| format!("{prefix}:{season}:{e}")).collect()
}

fn encode(video_ids: &[String], watched: &[bool]) -> String {
    let bitfield = WatchedBitField::construct_from_array(watched.to_vec(), video_ids.to_vec());
    WatchedField::from(bitfield).to_string()
}

fn case(name: &str, encode_ids: Vec<String>, watched: Vec<bool>, decode_ids: Vec<String>, anchor_missing: bool) -> Value {
    let serialized = encode(&encode_ids, &watched);
    let decoded = WatchedBitField::construct_and_resize(&serialized, decode_ids.clone()).expect("decodes");
    let expected: Vec<bool> = decode_ids.iter().map(|id| decoded.get_video(id)).collect();
    json!({
        "name": name,
        "serialized": serialized,
        "videoIds": decode_ids,
        "anchorMissing": anchor_missing,
        "expected": expected,
    })
}

#[test]
#[ignore = "prints the fixture; run by hand when adding cases"]
fn print_js_fixtures() {
    let season = ids("tt1", 1, 8);
    let mut with_specials = vec!["tt1:0:1".to_owned()];
    with_specials.extend(ids("tt1", 1, 4));
    let mut appended = ids("tt1", 1, 5);
    appended.push("tt1:1:6".to_owned());
    let long = ids("tt2", 1, 70);
    let cases = vec![
        case("two of eight watched", season.clone(), vec![true, true, false, false, false, false, false, false], season.clone(), false),
        case("an episode released after the encoding", ids("tt1", 1, 5), vec![true, false, true, false, false], appended, false),
        case(
            "a special prepended (season 0 sorts first) shifts the list",
            ids("tt1", 1, 4),
            vec![false, true, false, true],
            with_specials,
            false,
        ),
        case("the anchor video is not in the list", ids("tt1", 1, 4), vec![true, true, true, false], ids("tt9", 1, 4), true),
        case("seventy episodes, every third watched", long.clone(), (0..70).map(|i| i % 3 == 0).collect(), long, false),
        case("ids that contain colons (kitsu)", ids("kitsu:44042", 1, 12), (0..12).map(|i| i < 5).collect(), ids("kitsu:44042", 1, 12), false),
        case("nothing watched", ids("tt1", 1, 3), vec![false, false, false], ids("tt1", 1, 3), false),
    ];
    println!("{}", serde_json::to_string_pretty(&json!({ "cases": cases })).unwrap());
}

#[test]
fn the_core_decodes_every_js_fixture_to_its_stated_answer() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/web/tests/fixtures/watchedBitfield.json");
    let fixture: Value = serde_json::from_slice(&std::fs::read(&path).expect("fixture exists")).expect("fixture parses");
    let cases = fixture["cases"].as_array().expect("cases");
    assert!(cases.len() >= 7);
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let ids: Vec<String> = case["videoIds"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_owned()).collect();
        let expected: Vec<bool> = case["expected"].as_array().unwrap().iter().map(|v| v.as_bool().unwrap()).collect();
        let decoded = WatchedBitField::construct_and_resize(case["serialized"].as_str().unwrap(), ids.clone())
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        let got: Vec<bool> = ids.iter().map(|id| decoded.get_video(id)).collect();
        assert_eq!(got, expected, "{name}");
        if case["anchorMissing"].as_bool().unwrap() {
            // The core cannot place the bits without the anchor and answers
            // "nothing watched"; the web reads that case as unknown instead.
            assert!(got.iter().all(|w| !w), "{name}");
        }
    }
}
