fn main() {
    // SQLx embeds migrations in both the production migrator and test binaries.
    // Adding/changing a migration must invalidate both, including cached builds.
    println!("cargo:rerun-if-changed=enterprise_migrations");
    println!("cargo:rerun-if-changed=migrations");
}
