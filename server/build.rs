// Rebuild when a migration is added, since `sqlx::migrate!` embeds them.
fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
