//! Lens pairs, stage 1 (design/DESIGN-ETL.md §11) — reversible PySpell ETL.
//!
//! The headline test is a REFUSAL. `celsius ⇄ fahrenheit` is the pair everyone's
//! intuition calls reversible, and it is not: float64 loses the round trip, and
//! in mpedb's value domain it does not even preserve the type. A verifier that
//! accepts it is a rubber stamp, and then every other guarantee in #52 is
//! decoration. So the contract is tested from both ends — a real bijection is
//! accepted, and the plausible-looking impostor is refused with a named
//! counter-example.

use mpedb::lens::LensClass;
use mpedb::spellfn::SpellLang;
use mpedb::{Config, Database};

fn db(tag: &str) -> (Database, String) {
    let path = format!(
        "{}/lens-{tag}-{}.mpedb",
        mpedb_testkit::scratch_base_str(),
        std::process::id()
    );
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{path}"
size_mb = 32
max_readers = 8
durability = "none"

[[table]]
name = "t"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
"#
    );
    (Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap(), path)
}

fn def(d: &Database, src: &str) {
    d.create_function(SpellLang::Python, src).unwrap();
}

#[test]
fn celsius_fahrenheit_is_refused_as_bijective_and_named() {
    let (d, path) = db("celsius");
    def(&d, "def c_to_f(c):\n    return c * 9 / 5 + 32\n");
    def(&d, "def f_to_c(f):\n    return (f - 32) * 5 / 9\n");

    let e = d
        .create_lens("temp", "c_to_f", "f_to_c", LensClass::Bijective)
        .expect_err("celsius/fahrenheit is not a bijection and must not register as one");
    let msg = e.to_string();
    assert!(msg.contains("not bijective"), "{msg}");
    // A refusal without a counter-example is an opinion. The message must show
    // the input, what it forwarded to, and what came back.
    assert!(msg.contains("forward("), "no counter-example named: {msg}");
    assert!(msg.contains("but inverse of that is"), "no counter-example named: {msg}");

    // And it is refused for good: nothing was written.
    assert!(d.list_lenses().unwrap().is_empty(), "a refused pair must not be registered");

    // Reclassified as lossy — the honest escape hatch — it registers, and says
    // so. Nothing is verified, and `samples` reports exactly that.
    let samples = d.create_lens("temp", "c_to_f", "f_to_c", LensClass::Lossy).unwrap();
    assert_eq!(samples, 0);
    let listed = d.list_lenses().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].class, LensClass::Lossy);
    assert_eq!(listed[0].samples, 0);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_real_bijection_is_accepted_and_reports_its_sample_count() {
    let (d, path) = db("accept");
    def(&d, "def keep(x):\n    return x\n");
    let samples = d.create_lens("ident", "keep", "keep", LensClass::Bijective).unwrap();
    // Identity round-trips every probe input, so the sample count is the corpus
    // size — and it is REPORTED, not summarised as "verified". The evidence is
    // statistical (DESIGN-ETL §5).
    assert!(samples > 100, "identity should clear the whole corpus, got {samples}");

    let listed = d.list_lenses().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "ident");
    assert_eq!(listed[0].class, LensClass::Bijective);
    assert_eq!(listed[0].samples, samples);
    assert!(listed[0].healthy);
    assert_eq!(listed[0].forward_hash, listed[0].inverse_hash);

    // Re-verifying is read-only and reproducible: same corpus, same count.
    assert_eq!(d.verify_lens("ident").unwrap(), samples);

    assert!(d.drop_lens("ident").unwrap());
    assert!(!d.drop_lens("ident").unwrap());
    assert!(d.list_lenses().unwrap().is_empty());

    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_non_injective_pair_is_refused_as_a_collision() {
    let (d, path) = db("collide");
    // Squaring loses the sign; no inverse can recover it. This is a different
    // bug from losing precision, and it is reported differently.
    def(&d, "def sq(x):\n    return x * x\n");
    def(&d, "def unsq(x):\n    return x\n");
    let e = d.create_lens("sq", "sq", "unsq", LensClass::Bijective).unwrap_err();
    let msg = e.to_string();
    assert!(msg.contains("not bijective"), "{msg}");
    // Specifically a COLLISION, not a generic round-trip failure: the message
    // must name both inputs that landed on the same output.
    assert!(msg.contains("are both"), "expected a collision diagnosis, got: {msg}");
    assert!(msg.contains("at most one of them can be recovered"), "{msg}");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn stage_one_registers_one_to_one_pairs_only() {
    let (d, path) = db("arity");
    def(&d, "def one(x):\n    return x\n");
    def(&d, "def two(x, y):\n    return x + y\n");
    let e = d.create_lens("wide", "two", "one", LensClass::Bijective).unwrap_err();
    let msg = e.to_string();
    assert!(msg.contains("1\u{2192}1"), "{msg}");
    assert!(msg.contains("residual"), "the refusal must say what a wider pair needs: {msg}");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn residual_is_refused_because_its_format_is_not_written_yet() {
    // The residual format is an eternity promise (commitment 9). Stage 1 writes
    // no byte of it, so declaring the class must fail loudly rather than
    // register a pair whose format does not exist.
    let e = LensClass::parse("residual").unwrap_err().to_string();
    assert!(e.contains("stage 2"), "{e}");

    let (d, path) = db("residual");
    def(&d, "def keep(x):\n    return x\n");
    let e = d.create_lens("r", "keep", "keep", LensClass::Residual).unwrap_err().to_string();
    assert!(e.contains("not built"), "{e}");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn an_unknown_function_is_refused_by_name() {
    let (d, path) = db("missing");
    def(&d, "def keep(x):\n    return x\n");
    let e = d.create_lens("x", "keep", "nope", LensClass::Bijective).unwrap_err().to_string();
    assert!(e.contains("nope"), "{e}");
    assert!(e.contains("mpedb fn define"), "the refusal should say how to fix it: {e}");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn the_pair_is_pinned_by_hash_so_a_redefinition_cannot_change_what_was_verified() {
    let (d, path) = db("pin");
    def(&d, "def keep(x):\n    return x\n");
    d.create_lens("ident", "keep", "keep", LensClass::Bijective).unwrap();
    let before = d.list_lenses().unwrap()[0].forward_hash.clone();

    // Redefine the NAME to something that is not an involution at all. The pair
    // was verified against specific blobs and must still name them — a
    // verification an unrelated `fn define` could invalidate would be worthless
    // (DESIGN-TRIGGERS §5.1, inherited).
    def(&d, "def keep(x):\n    return x + 1\n");
    let after = d.list_lenses().unwrap();
    assert_eq!(after[0].forward_hash, before, "the pair must still name the verified blob");
    assert!(after[0].healthy, "the old blob is content-addressed and still readable");
    // And it still verifies, because it is still the pair that was verified.
    assert!(d.verify_lens("ident").unwrap() > 100);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn dropping_the_function_name_does_not_break_the_pair() {
    let (d, path) = db("dropfn");
    def(&d, "def keep(x):\n    return x\n");
    d.create_lens("ident", "keep", "keep", LensClass::Bijective).unwrap();

    // `drop_function` removes the NAME binding only; the blob is
    // content-addressed and stays. The pair names the blob, so it survives —
    // and it must still VERIFY, not merely still be listed.
    assert!(d.drop_function("keep").unwrap());
    assert!(d.list_functions().unwrap().is_empty());

    let after = d.list_lenses().unwrap();
    assert_eq!(after.len(), 1);
    assert!(after[0].healthy, "the blob outlives the name binding");
    assert!(d.verify_lens("ident").unwrap() > 100);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn another_process_sees_the_pair_through_the_generation_bump() {
    let (d, path) = db("crossproc");
    def(&d, "def keep(x):\n    return x\n");

    let d2 = Database::open_from_file(std::path::Path::new(&path)).unwrap();
    assert!(d2.list_lenses().unwrap().is_empty());

    d.create_lens("ident", "keep", "keep", LensClass::Bijective).unwrap();

    let seen = d2.list_lenses().unwrap();
    assert_eq!(seen.len(), 1, "a second handle must see the registered pair");
    assert_eq!(seen[0].name, "ident");
    assert!(d2.verify_lens("ident").unwrap() > 100);

    assert!(d.drop_lens("ident").unwrap());
    assert!(d2.list_lenses().unwrap().is_empty(), "and must see the drop");

    drop(d2);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_pair_that_refuses_the_whole_corpus_proves_nothing_and_is_refused() {
    let (d, path) = db("empty");
    // A body that errors on every input: indexing a scalar is never valid, so
    // every probe is refused and the round trip is never exercised.
    def(&d, "def bad(x):\n    return x[0]\n");
    let e = d.create_lens("b", "bad", "bad", LensClass::Bijective).unwrap_err().to_string();
    assert!(e.contains("nothing was verified"), "{e}");
    let _ = std::fs::remove_file(&path);
}
