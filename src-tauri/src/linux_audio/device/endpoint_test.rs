//! Endpoint listing and the default pick over a port list measured on a desktop PipeWire graph
//! (pipewire-jack), with no JACK server.
use super::*;

const CAPTURE: &[(&str, bool)] = &[
    ("EVO 4 In 2 (Mic):capture_MONO", false),
    ("C920 Webcam Mic:capture_FL", true),
    ("C920 Webcam Mic:capture_FR", true),
    ("EVO 4 In 1 (Guitar):capture_MONO", false),
    ("EVO4 0:capture_AUX2", true),
    ("EVO4 0:capture_AUX0", true),
    ("EVO4 0:capture_AUX3", true),
    ("EVO4 0:capture_AUX1", true),
    ("EVO 4 In 1+2 (Guitar + Mic):capture_FL", false),
    ("EVO 4 In 1+2 (Guitar + Mic):capture_FR", false),
    ("System sound:output_FL", false),
    ("System sound:output_FR", false),
    ("System sound for sharing:capture_FL", false),
    ("System sound for sharing:capture_FR", false),
    ("Split EVO 4 Speakers / Headphones:output_AUX0", false),
    ("Split EVO 4 Speakers / Headphones:output_AUX1", false),
    ("Slay the Spire 2:output_FL", false),
    ("Slay the Spire 2:output_FR", false),
    ("FMOD Audio:output_FL", false),
    ("FMOD Audio:output_FR", false),
    ("System sound:monitor_FL", false),
    ("System sound:monitor_FR", false),
    ("EVO 4 Speakers / Headphones:monitor_FL", false),
    ("EVO 4 Speakers / Headphones:monitor_FR", false),
    ("EVO4 0:monitor_AUX0", false),
    ("EVO4 0:monitor_AUX1", false),
    ("Samsung Monitor:monitor_FL", false),
    ("Samsung Monitor:monitor_FR", false),
    ("BleepLoop:out_L", false),
];

const PLAYBACK: &[(&str, bool)] = &[
    ("System sound:playback_FL", false),
    ("System sound:playback_FR", false),
    ("EVO 4 Speakers / Headphones:playback_FL", false),
    ("EVO 4 Speakers / Headphones:playback_FR", false),
    ("EVO4 0:playback_AUX0", true),
    ("EVO4 0:playback_AUX1", true),
    ("Samsung Monitor:playback_FL", true),
    ("Samsung Monitor:playback_FR", true),
    ("System sound for sharing:input_FL", false),
    ("System sound for sharing:input_FR", false),
    ("Split EVO 4 In 2 (Mic):input_AUX1", false),
    ("EVO 4 In 1+2 (Guitar + Mic) input:input_MONO", false),
    ("BleepLoop:in_1", false),
];

fn graph(ports: &[(&str, bool)], capture: bool) -> Vec<Group> {
    let name = |p: &str| CString::new(p).unwrap();
    let physical = ports.iter().filter(|p| p.1).map(|p| name(p.0)).collect();
    group_ports(ports.iter().map(|p| name(p.0)).collect(), &physical, capture)
}

fn listed(groups: &[Group]) -> Vec<(&str, u32)> {
    groups
        .iter()
        .map(|g| (g.info.id.as_str(), g.info.channels))
        .collect()
}

fn set(names: &[&str]) -> HashSet<String> {
    names.iter().map(|n| n.to_string()).collect()
}

fn default_id(groups: Vec<Group>, webcams: &[&str]) -> Option<String> {
    selected(groups, &None, "capture", || set(webcams))
        .unwrap()
        .map(|g| g.info.id)
}

#[test]
fn capture_lists_only_sources_physical_first() {
    let groups = graph(CAPTURE, true);
    assert_eq!(
        listed(&groups),
        [
            ("C920 Webcam Mic", 2),
            ("EVO4 0", 4),
            ("EVO 4 In 1 (Guitar)", 1),
            ("EVO 4 In 1+2 (Guitar + Mic)", 2),
            ("EVO 4 In 2 (Mic)", 1),
            ("System sound for sharing", 2),
        ]
    );
    let evo = &groups[1];
    assert_eq!(evo.info.name, "EVO4 0");
    let ports: Vec<_> = evo.ports.iter().map(|p| p.to_str().unwrap()).collect();
    assert_eq!(
        ports,
        [
            "EVO4 0:capture_AUX0",
            "EVO4 0:capture_AUX1",
            "EVO4 0:capture_AUX2",
            "EVO4 0:capture_AUX3"
        ]
    );
    assert_eq!(groups[0].ports[0].to_str().unwrap(), "C920 Webcam Mic:capture_FL");
}

#[test]
fn the_default_input_skips_a_webcam() {
    // Listed first among the physical groups, the webcam is what the skip has to pass over.
    assert_eq!(listed(&graph(CAPTURE, true))[0].0, "C920 Webcam Mic");
    assert_eq!(
        default_id(graph(CAPTURE, true), &["C920 Webcam Mic", "alsa_input.c920"]).as_deref(),
        Some("EVO4 0")
    );
    assert_eq!(
        default_id(graph(CAPTURE, true), &[]).as_deref(),
        Some("C920 Webcam Mic")
    );
    // Without the webcam in the graph the raw interface group is the pick either way.
    let no_cam: Vec<_> = CAPTURE
        .iter()
        .copied()
        .filter(|p| !p.0.starts_with("C920"))
        .collect();
    assert_eq!(default_id(graph(&no_cam, true), &[]).as_deref(), Some("EVO4 0"));
    // A virtual source stands in when the only physical one is a webcam.
    let virtual_only = [
        ("C920 Webcam Mic:capture_FL", true),
        ("Desk Mic:capture_MONO", false),
    ];
    assert_eq!(
        default_id(graph(&virtual_only, true), &["C920 Webcam Mic"]).as_deref(),
        Some("Desk Mic")
    );
}

#[test]
fn a_webcam_alone_opens_no_input() {
    let only = [
        ("C920 Webcam Mic:capture_FL", true),
        ("C920 Webcam Mic:capture_FR", true),
        ("Slay the Spire 2:output_FL", false),
        ("System sound:monitor_FL", false),
    ];
    assert_eq!(listed(&graph(&only, true)), [("C920 Webcam Mic", 2)]);
    assert_eq!(default_id(graph(&only, true), &["C920 Webcam Mic"]), None);
    let mut asked = false;
    let none = selected(graph(&only[2..], true), &None, "capture", || {
        asked = true;
        HashSet::new()
    });
    assert!(none.unwrap().is_none());
    assert!(!asked, "an empty list needs no webcam lookup");
}

#[test]
fn an_explicit_input_resolves_among_listed_groups_only() {
    for id in ["EVO4 0", "C920 Webcam Mic"] {
        let found = selected(graph(CAPTURE, true), &Some(id.into()), "capture", || {
            panic!("an explicit id needs no webcam lookup")
        });
        assert_eq!(found.unwrap().unwrap().info.id, id);
    }
    for id in ["Slay the Spire 2", "System sound", "EVO4 0:monitor", "BleepLoop"] {
        let missing = selected(graph(CAPTURE, true), &Some(id.into()), "capture", HashSet::new);
        assert!(
            matches!(&missing, Err(e) if e.contains("unavailable")),
            "{id}: {:?}",
            missing.map(|g| g.map(|g| g.info.id))
        );
    }
}

#[test]
fn playback_lists_only_sinks() {
    let groups = graph(PLAYBACK, false);
    assert_eq!(
        listed(&groups),
        [
            ("EVO4 0", 2),
            ("Samsung Monitor", 2),
            ("EVO 4 Speakers / Headphones", 2),
            ("System sound", 2),
        ]
    );
    let output = selected(groups, &None, "playback", HashSet::new).unwrap();
    assert_eq!(output.unwrap().info.id, "EVO4 0");
}
