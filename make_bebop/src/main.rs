fn main() {
    use bebop_tools as bebop;
    use std::path::PathBuf;
    bebop::download_bebopc(PathBuf::from("target").join("bebopc"));
    // build all `.bop` schemas in `schemas` dir and make a new module `generated` in `src` with all of them.
    bebop::build_schema_dir(
        "schemas",
        "../src/generated",
        &bebop::BuildConfig::default(),
    );
}

#[test]
fn make_bebop() {
    use bebop_tools as bebop;
    use std::path::PathBuf;
    bebop::download_bebopc(PathBuf::from("target").join("bebopc"));
    // build all `.bop` schemas in `schemas` dir and make a new module `generated` in `src` with all of them.
    bebop::build_schema_dir(
        "schemas",
        "../src/generated",
        &bebop::BuildConfig::default(),
    );
}
