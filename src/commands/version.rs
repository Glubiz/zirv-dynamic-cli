use std::io::Write;

pub fn get_version<W: Write>(writer: &mut W) -> Result<(), Box<dyn std::error::Error>> {
    writeln!(writer, "Version: {}", env!("CARGO_PKG_VERSION"))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_get_version_output() {
        let mut buffer = Cursor::new(Vec::new());
        get_version(&mut buffer).unwrap();
        let output = String::from_utf8(buffer.into_inner()).unwrap();
        // The output should contain the version from Cargo.toml.
        // For example, if Cargo.toml has version = "0.3.3", the output should be "Version: 0.3.3"
        assert!(
            output.contains(env!("CARGO_PKG_VERSION")),
            "Output should contain the version"
        );
    }

    #[test]
    fn test_version_is_at_least_2_5_0() {
        let version = env!("CARGO_PKG_VERSION");
        let parts: Vec<u32> = version.split('.').map(|p| p.parse().unwrap_or(0)).collect();
        assert!(parts.len() >= 2, "semantic version expected, got {version}");
        assert!(
            (parts[0], parts[1]) >= (2, 5),
            "zirv ctx ships in 2.5.0, got {version}"
        );
    }
}
