// Rebuild when a migration is added, since `sqlx::migrate!` embeds them,
// and when the islands are rebuilt, since `rust-embed` embeds `web/dist`
// (see src/islands.rs).
fn main() {
    println!("cargo:rerun-if-changed=migrations");
    println!("cargo:rerun-if-changed=../web/dist");
}
