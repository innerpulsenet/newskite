//! Runs against the real desktop of the machine running the tests.
//! `cargo test -p newskite-theme -- --ignored --nocapture`

#[test]
#[ignore = "depends on the host desktop; run manually"]
fn resolve_on_this_machine() {
    println!("kdeglobals path: {:?}", newskite_theme::kdeglobals_path());
    let start = std::time::Instant::now();
    let t = newskite_theme::resolve();
    println!(
        "source={:?} dark={} scheme={:?} ({:?})",
        t.source,
        t.dark,
        t.scheme_name,
        start.elapsed()
    );
    for (name, c) in t.tokens.iter() {
        println!("  {name:<15} {c}");
    }
    assert!(t.tokens.accent_text.contrast(t.tokens.accent) >= 4.5);
}
