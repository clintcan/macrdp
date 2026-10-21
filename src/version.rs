use std::sync::OnceLock;

/// Version for `--version` and the startup log: the Cargo version, plus
/// `git describe` output when the build is not exactly the release tag.
pub fn version() -> &'static str {
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION.get_or_init(|| format_version(env!("CARGO_PKG_VERSION"), env!("MACRDP_GIT_DESCRIBE")))
}

fn format_version(pkg: &str, describe: &str) -> String {
    if describe.is_empty() || describe == format!("v{pkg}") {
        pkg.to_string()
    } else {
        format!("{pkg} ({describe})")
    }
}

#[cfg(test)]
mod tests {
    use super::format_version;

    #[test]
    fn release_build_shows_plain_version() {
        assert_eq!(format_version("0.1.0", "v0.1.0"), "0.1.0");
    }

    #[test]
    fn dev_build_shows_describe() {
        assert_eq!(
            format_version("0.2.0-dev", "v0.1.0-3-gabc1234"),
            "0.2.0-dev (v0.1.0-3-gabc1234)"
        );
    }

    #[test]
    fn no_git_shows_plain_version() {
        assert_eq!(format_version("0.1.0", ""), "0.1.0");
    }
}
