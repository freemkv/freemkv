//! The const field builders, run at runtime (the table itself is built at compile time).
use super::*;

#[test]
fn the_field_builders_set_what_they_name() {
    let f = field("k", "Label", Group::Disc, Kind::Text, Def::Str("d"), "Help");
    assert_eq!(
        (f.key, f.label, f.help, f.placeholder),
        ("k", "Label", "Help", "")
    );
    assert!(f.show_if.is_none() && f.hide_if.is_none());
    let f = f
        .placeholder("p")
        .show_if("mode", "on")
        .hide_if("other", "x");
    assert_eq!(f.placeholder, "p");
    let show = f.show_if.as_ref().unwrap();
    assert_eq!((show.key, show.value), ("mode", "on"));
    let hide = f.hide_if.as_ref().unwrap();
    assert_eq!((hide.key, hide.value), ("other", "x"));
}
