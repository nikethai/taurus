pub mod errors;

/// Crate version, as declared in `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    #[test]
    fn version_matches_cargo() {
        assert_eq!(super::VERSION, env!("CARGO_PKG_VERSION"));
    }
}
